use std::cell::{Cell, RefCell, UnsafeCell};
use std::future::Future;
use std::marker::PhantomData;
use std::mem::{self, ManuallyDrop};
use std::ops::Deref;
use std::pin::Pin;
use std::ptr::NonNull;
use std::rc::Rc;
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

use intrusive_collections::{LinkedList, LinkedListLink, UnsafeRef, intrusive_adapter};

use crate::{Hardware, LocalExecutor, WorkQueueAdapter};

struct TaskVtable {
    poll: unsafe fn(NonNull<TaskHeader>),
    cancel: unsafe fn(NonNull<TaskHeader>),
    dealloc: unsafe fn(NonNull<TaskHeader>),
}

#[derive(Debug, Copy, Clone)]
enum TaskState {
    Runable,
    Finished,
    Cancelled,
}

#[repr(C)]
enum FutureState<F: Future> {
    Runable(F),
    Finished(F::Output),
    Cancelled,
}

#[repr(C)]
pub(crate) struct TaskHeader {
    ready_link: LinkedListLink,
    pub(crate) wq_link: LinkedListLink,
    pub(crate) wq: Cell<Option<NonNull<RefCell<LinkedList<WorkQueueAdapter>>>>>,

    state: Cell<TaskState>,
    vtable: &'static TaskVtable,
    refcnt: Cell<usize>,
}

intrusive_adapter!(pub(crate) ReadyAdapter = UnsafeRef<TaskHeader>: TaskHeader { ready_link => LinkedListLink });

#[repr(C)]
pub(crate) struct Task<Hw: Hardware, F: Future> {
    header: TaskHeader,
    future: UnsafeCell<FutureState<F>>,
    _hw: PhantomData<Hw>,
}

impl<Hw: Hardware, F: Future> Task<Hw, F> {
    const TASK_VTABLE: &'static TaskVtable = &TaskVtable {
        poll: Self::poll,
        dealloc: Self::dealloc,
        cancel: Self::cancel,
    };

    pub fn alloc(future: F) -> TaskRef {
        let task = Box::new(Task {
            header: TaskHeader {
                ready_link: LinkedListLink::new(),
                wq_link: LinkedListLink::new(),
                wq: Cell::new(None),

                state: Cell::new(TaskState::Runable),
                vtable: Self::TASK_VTABLE,
                refcnt: Cell::new(1),
            },
            future: UnsafeCell::new(FutureState::Runable(future)),
            _hw: PhantomData::<Hw>,
        });

        let header = unsafe { NonNull::new_unchecked(Box::into_raw(task).cast::<TaskHeader>()) };
        unsafe { TaskRef::from_raw(header) }
    }

    /// # Safety
    /// Must not be the current task.
    unsafe fn cancel(header: NonNull<TaskHeader>) {
        let state: *mut FutureState<F> = unsafe { (*header.cast::<Self>().as_ptr()).future.get() };
        unsafe {
            *state = FutureState::Cancelled;
        }
    }

    unsafe fn dealloc(header: NonNull<TaskHeader>) {
        drop(unsafe { Box::from_raw(header.as_ptr().cast::<Self>()) });
    }

    unsafe fn poll(header: NonNull<TaskHeader>) {
        let waker = unsafe {
            ManuallyDrop::new(Waker::from_raw(RawWaker::new(
                header.as_ptr().cast::<()>(),
                Hw::vtable(),
            )))
        };
        let mut cx = Context::from_waker(&waker);

        let state: *mut FutureState<F> = unsafe { (*header.cast::<Self>().as_ptr()).future.get() };

        let poll = match unsafe { &mut *state } {
            // SAFETY: The task is never moved after being allocated.
            FutureState::Runable(future) => unsafe { Pin::new_unchecked(future) }.poll(&mut cx),
            FutureState::Finished(_) => {
                log::error!("poll was called on a ready future");
                return;
            }
            FutureState::Cancelled => {
                log::error!("poll was called on a cancelled future");
                return;
            }
        };

        if let Poll::Ready(output) = poll {
            unsafe {
                *state = FutureState::Finished(output);
                header.as_ref().state.set(TaskState::Finished);
            }
        }
    }
}

pub(crate) struct TaskRef(NonNull<TaskHeader>);

impl TaskRef {
    pub unsafe fn from_raw(header: NonNull<TaskHeader>) -> Self {
        Self(header)
    }

    unsafe fn clone_from_raw(header: NonNull<TaskHeader>) -> Self {
        let header_ref = unsafe { header.as_ref() };
        header_ref.refcnt.set(header_ref.refcnt.get() + 1);
        Self(header)
    }

    pub fn into_raw(self) -> NonNull<TaskHeader> {
        let header = self.0;
        mem::forget(self);
        header
    }

    pub fn as_ptr(&self) -> *const TaskHeader {
        self.0.as_ptr()
    }

    pub fn poll(&self) {
        use std::panic::{AssertUnwindSafe, catch_unwind};

        let header = self.0;
        let poll_fn = unsafe { header.as_ref().vtable.poll };

        if catch_unwind(AssertUnwindSafe(|| unsafe { poll_fn(header) })).is_err() {
            log::error!("Task panicked!");
        }
    }

    pub fn cancel(&self) {
        if matches!(self.state.get(), TaskState::Finished) {
            return;
        }
        let header = self.0;
        unsafe {
            let cancel_fn = header.as_ref().vtable.cancel;
            cancel_fn(header);
        }
        self.state.set(TaskState::Cancelled);
    }

    pub fn is_finished(&self) -> bool {
        matches!(self.state.get(), TaskState::Finished)
    }

    pub fn is_cancelled(&self) -> bool {
        matches!(self.state.get(), TaskState::Cancelled)
    }
}

impl Deref for TaskRef {
    type Target = TaskHeader;

    fn deref(&self) -> &Self::Target {
        unsafe { self.0.as_ref() }
    }
}

impl Clone for TaskRef {
    fn clone(&self) -> Self {
        unsafe { TaskRef::clone_from_raw(self.0) }
    }
}

impl Drop for TaskRef {
    fn drop(&mut self) {
        let header = unsafe { self.0.as_ref() };
        let refcnt = header.refcnt.get();
        header.refcnt.set(refcnt - 1);
        if refcnt == 1 {
            unsafe { (header.vtable.dealloc)(self.0) };
        }
    }
}

pub(crate) fn current_executor_and_task<Hw: Hardware>(
    cx: &mut Context<'_>,
) -> (Rc<LocalExecutor<Hw>>, TaskRef) {
    let executor = LocalExecutor::current();
    let task = unsafe {
        TaskRef::clone_from_raw(NonNull::new_unchecked(cx.waker().data() as *mut TaskHeader))
    };

    assert_eq!(
        cx.waker().vtable() as *const _,
        Hw::vtable(),
        "incompatible executor for CqeFuture"
    );

    (executor, task)
}

pub(crate) fn enqueue<Hw: Hardware>(task: TaskRef) {
    if task.ready_link.is_linked() {
        log::warn!("task has been already notified");
        return;
    }

    let executor = Hw::current();
    let header = task.into_raw();
    executor
        .ready_queue
        .borrow_mut()
        .push_back(unsafe { UnsafeRef::from_raw(header.as_ptr()) });
}

unsafe fn vt_clone<Hw: Hardware>(data: *const ()) -> RawWaker {
    let header = unsafe { NonNull::new_unchecked(data as *mut TaskHeader) };
    let _ = unsafe { TaskRef::clone_from_raw(header) }.into_raw();
    RawWaker::new(data, Hw::vtable())
}

unsafe fn vt_drop(data: *const ()) {
    drop(unsafe { TaskRef::from_raw(NonNull::new_unchecked(data as *mut TaskHeader)) });
}

unsafe fn vt_wake<Hw: Hardware>(data: *const ()) {
    let header = unsafe { NonNull::new_unchecked(data as *mut TaskHeader) };
    enqueue::<Hw>(unsafe { TaskRef::from_raw(header) });
}

unsafe fn vt_wake_by_ref<Hw: Hardware>(data: *const ()) {
    let header = unsafe { NonNull::new_unchecked(data as *mut TaskHeader) };
    enqueue::<Hw>(unsafe { TaskRef::clone_from_raw(header) });
}

pub const fn vtable<Hw: Hardware>() -> RawWakerVTable {
    RawWakerVTable::new(vt_clone::<Hw>, vt_wake::<Hw>, vt_wake_by_ref::<Hw>, vt_drop)
}
