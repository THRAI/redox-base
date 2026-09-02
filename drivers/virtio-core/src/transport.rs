use core::mem::size_of;
use core::sync::atomic::{AtomicU16, Ordering};
use std::collections::HashMap;
use std::future::Future;
use std::io::{Read, Write};
use std::os::fd::AsRawFd;
use std::sync::{Arc, Mutex, Weak};
use std::task::{Poll, Waker};

use common::dma::Dma;
use event::RawEventQueue;
use pcid_interface::irq_helpers::InterruptVector;

use crate::spec::*;
use crate::utils::align;

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("syscall failed")]
    SyscallError(#[from] libredox::error::Error),
    #[error("the device is incapable of {0:?}")]
    InCapable(CfgType),
}

/// Returns the queue part sizes in bytes.
///
/// ## Reference
/// Section 2.7 Split Virtqueues of the specfication v1.2 describes the alignment
/// and size of the queue parts.
///
/// ## Panics
/// If `queue_size` is not a power of two or is zero.
pub const fn queue_part_sizes(queue_size: usize) -> (usize, usize, usize) {
    assert!(queue_size.is_power_of_two() && queue_size != 0);

    const DESCRIPTOR_ALIGN: usize = 16;
    const AVAILABLE_ALIGN: usize = 2;
    const USED_ALIGN: usize = 4;

    let queue_size = queue_size as usize;
    let desc = size_of::<Descriptor>() * queue_size;

    // `avail_header`: Size of the available ring header and the footer.
    let avail_header = size_of::<AvailableRing>() + size_of::<AvailableRingExtra>();
    let avail = avail_header + size_of::<AvailableRingElement>() * queue_size;

    // `used_header`: Size of the used ring header and the footer.
    let used_header = size_of::<UsedRing>() + size_of::<UsedRingExtra>();
    let used = used_header + size_of::<UsedRingElement>() * queue_size;

    (
        align(desc, DESCRIPTOR_ALIGN).next_multiple_of(syscall::PAGE_SIZE),
        align(avail, AVAILABLE_ALIGN).next_multiple_of(syscall::PAGE_SIZE),
        align(used, USED_ALIGN).next_multiple_of(syscall::PAGE_SIZE),
    )
}

/// Queues that share one MSI-X table entry. One thread watches that vector
/// and drains every queue in the group.
struct IrqGroup {
    queues: Mutex<Vec<Arc<Queue>>>,
}

pub trait NotifyBell {
    fn ring(&self, queue_index: u16);
}

struct Completion {
    waker: Option<Waker>,
    written: Option<u32>,
}

enum PendingState {
    PendingSubmit(Vec<Buffer>),
    Submitted(u32),
}

pub struct PendingRequest {
    queue: Arc<Queue>,
    state: PendingState,
}

impl PendingRequest {
    fn try_submit_waiting(&self) -> Option<u32> {
        match &self.state {
            PendingState::PendingSubmit(chain) => self.queue.try_submit(chain, true),
            PendingState::Submitted(_) => None,
        }
    }
}

impl Future for PendingRequest {
    type Output = u32;

    fn poll(self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        let first_descriptor = match this.state {
            PendingState::Submitted(first) => first,
            PendingState::PendingSubmit(_) => {
                this.queue.drain_used_ring();
                if let Some(first) = this.try_submit_waiting() {
                    this.state = PendingState::Submitted(first);
                    first
                } else {
                    this.queue
                        .submit_waiters
                        .lock()
                        .unwrap()
                        .push(cx.waker().clone());
                    this.queue.drain_used_ring();
                    if let Some(first) = this.try_submit_waiting() {
                        this.state = PendingState::Submitted(first);
                        first
                    } else {
                        return Poll::Pending;
                    }
                }
            }
        };

        this.queue.drain_used_ring();

        // Race-safety note:
        // - IRQ fires before `poll` acquires the completions lock:
        //    `poll` blocks until the IRQ thread completes and releases the lock.
        //    Once acquired, `poll` observes the updated state and returns `Poll::Ready` (no waker needed).
        // - IRQ fires after `poll` acquires the lock:
        //    `poll` observes the incomplete state, registers the waker, releases the lock,
        //    and returns `Poll::Pending`. The IRQ thread then acquires the lock and invokes the waker.
        //
        // This guarantees that lost completions/wakeups cannot occur.
        let mut completions = this.queue.completions.lock().unwrap();
        let slot = completions
            .get_mut(&first_descriptor)
            .expect("virtio-core: missing completion slot");

        if let Some(written) = slot.written {
            completions.remove(&first_descriptor);
            return Poll::Ready(written);
        }

        slot.waker = Some(cx.waker().clone());
        Poll::Pending
    }
}

impl Drop for PendingRequest {
    fn drop(&mut self) {
        let PendingState::Submitted(first_descriptor) = &self.state else {
            return;
        };
        let first_descriptor = *first_descriptor;
        // clears the waker only, while the completion slot stays so drain
        // can still recycle the chain
        if let Some(slot) = self
            .queue
            .completions
            .lock()
            .unwrap()
            .get_mut(&first_descriptor)
        {
            slot.waker = None;
        }
    }
}

pub struct Queue {
    pub queue_index: u16,
    pub used: Used,
    pub descriptor: Dma<[Descriptor]>,
    pub available: Available,
    vector: u16,

    consumed_index: AtomicU16,
    completions: Mutex<HashMap<u32, Completion>>,
    drain_lock: Mutex<()>,
    submit_lock: Mutex<()>,
    submit_waiters: Mutex<Vec<Waker>>,

    notification_bell: Box<dyn NotifyBell>,
    descriptor_stack: crossbeam_queue::SegQueue<u16>,
    sref: Weak<Self>,
}

impl Queue {
    pub fn new<N>(
        descriptor: Dma<[Descriptor]>,
        available: Available,
        used: Used,

        notification_bell: N,
        queue_index: u16,
        vector: u16,
    ) -> Arc<Self>
    where
        N: NotifyBell + 'static,
    {
        let descriptor_stack = crossbeam_queue::SegQueue::new();
        (0..descriptor.len() as u16).for_each(|i| descriptor_stack.push(i));

        Arc::new_cyclic(|sref| Self {
            notification_bell: Box::new(notification_bell),
            available,
            descriptor,
            used,
            queue_index,
            descriptor_stack,
            consumed_index: AtomicU16::new(0),
            completions: Mutex::new(HashMap::new()),
            drain_lock: Mutex::new(()),
            submit_lock: Mutex::new(()),
            submit_waiters: Mutex::new(Vec::new()),
            sref: sref.clone(),
            vector,
        })
    }

    fn reinit(&self) {
        self.consumed_index.store(0, Ordering::SeqCst);
        self.completions.lock().unwrap().clear();
        self.submit_waiters.lock().unwrap().clear();
        self.available.set_head_idx(0);

        // Drain all of the available descriptors.
        while let Some(_) = self.descriptor_stack.pop() {}

        // Refill the descriptor stack.
        (0..self.descriptor.len() as u16).for_each(|i| self.descriptor_stack.push(i));
    }

    fn recycle_chain(&self, mut table_index: u32) {
        loop {
            let idx = table_index as usize;
            if idx >= self.descriptor.len() {
                log::error!("virtio-core: used ring table_index {table_index} out of range");
                return;
            }

            let has_next = self.descriptor[idx].flags().contains(DescriptorFlags::NEXT);
            let next = self.descriptor[idx].next();
            self.descriptor_stack.push(table_index as u16);
            if !has_next {
                break;
            }
            table_index = u32::from(next);
        }
    }

    fn check_chain(&self, chain: &[Buffer]) {
        if chain.is_empty() {
            panic!("virtio-core: submitted chain is empty");
        }
        if chain.len() > self.descriptor_len() {
            panic!("virtio-core: submitted chain is longer than the virtqueue");
        }
    }

    fn try_submit(&self, chain: &[Buffer], track_completion: bool) -> Option<u32> {
        let first_descriptor = {
            let _guard = self.submit_lock.lock().unwrap();

            let mut allocated = Vec::with_capacity(chain.len());
            for buffer in chain {
                let Some(descriptor) = self.descriptor_stack.pop() else {
                    for desc in allocated {
                        self.descriptor_stack.push(desc);
                    }
                    return None;
                };
                allocated.push(descriptor);

                let idx = descriptor as usize;
                self.descriptor[idx].set_addr(buffer.buffer as u64);
                self.descriptor[idx].set_flags(buffer.flags);
                self.descriptor[idx].set_size(buffer.size as u32);
            }

            for pair in allocated.windows(2) {
                self.descriptor[pair[0] as usize].set_next(Some(pair[1]));
            }
            let first_descriptor = u32::from(allocated[0]);
            let last_descriptor = allocated[allocated.len() - 1];
            self.descriptor[last_descriptor as usize].set_next(None);

            if track_completion {
                self.completions.lock().unwrap().insert(
                    first_descriptor,
                    Completion {
                        waker: None,
                        written: None,
                    },
                );
            }

            let avail_idx = self.available.head_index();
            self.available
                .get_element_at(avail_idx as usize)
                .set_table_index(allocated[0]);
            self.available.set_head_idx(avail_idx.wrapping_add(1));
            self.notification_bell.ring(self.queue_index);
            first_descriptor
        };

        self.drain_used_ring();
        Some(first_descriptor)
    }

    /// Harvest every newly-used descriptor chain. Individual futures must not
    /// advance the used-ring bookmark which can race and drop completions.
    pub fn drain_used_ring(&self) {
        let guard = self.drain_lock.lock().unwrap();
        let mut to_wake = Vec::new();
        let mut recycled = false;

        loop {
            let consumed = self.consumed_index.load(Ordering::SeqCst);
            let device_head = self.used.head_index();
            if consumed == device_head {
                break;
            }

            let element = self.used.get_element_at(consumed as usize);
            let table_index = element.table_index.get();
            let written = element.written.get();

            {
                let mut completions = self.completions.lock().unwrap();
                if let Some(slot) = completions.get_mut(&table_index) {
                    self.recycle_chain(table_index);
                    recycled = true;
                    slot.written = Some(written);
                    if let Some(waker) = slot.waker.take() {
                        to_wake.push(waker);
                    }
                }
            }

            self.consumed_index
                .store(consumed.wrapping_add(1), Ordering::SeqCst);
        }

        drop(guard);
        for waker in to_wake {
            waker.wake();
        }
        if recycled {
            let waiters: Vec<Waker> = self.submit_waiters.lock().unwrap().drain(..).collect();
            for waker in waiters {
                waker.wake();
            }
        }
    }

    /// Submit a chain without tracking completion. Used when the caller reads
    /// the used ring itself (virtio-net RX) and does not recycle via `send()`.
    ///
    /// ## Panics
    /// Empty chain, chain longer than the virtqueue, or no free descriptors.
    /// The RX path is expected to post at most [`Self::descriptor_len`] buffers.
    pub fn post(&self, chain: Vec<Buffer>) -> u32 {
        self.check_chain(&chain);
        self.try_submit(&chain, false)
            .expect("virtio-core: virtqueue is out of descriptors")
    }

    /// Submit a chain and wait until the device has used it.
    ///
    /// The chain is placed on the ring on first poll. If the virtqueue is
    /// temporarily out of descriptors, the future stays pending until drain
    /// recycles enough, then submits.
    ///
    /// ## Panics
    /// Empty chain, or chain longer than the virtqueue.
    #[must_use = "The function returns a future that must be awaited to ensure the sent request is completed."]
    pub fn send(&self, chain: Vec<Buffer>) -> PendingRequest {
        self.check_chain(&chain);
        PendingRequest {
            queue: self.sref.upgrade().unwrap(),
            state: PendingState::PendingSubmit(chain),
        }
    }

    /// Returns the number of descriptors in the descriptor table of this queue.
    pub fn descriptor_len(&self) -> usize {
        self.descriptor.len()
    }
}

unsafe impl Sync for Queue {}
unsafe impl Send for Queue {}

pub struct Available {
    mem: Dma<[u8]>,
    queue_size: usize,
}

impl<'a> Available {
    pub fn ring(&self) -> &AvailableRing {
        unsafe { &*self.mem.as_ptr().cast() }
    }
    pub fn ring_mut(&mut self) -> &mut AvailableRing {
        unsafe { &mut *self.mem.as_mut_ptr().cast() }
    }
    pub fn new(queue_size: usize) -> Result<Self, Error> {
        let (_, _, size) = queue_part_sizes(queue_size);
        let mem = unsafe {
            Dma::zeroed_slice(size)
                .map_err(Error::SyscallError)?
                .assume_init()
        };

        unsafe { Self::from_raw(mem, queue_size) }
    }

    /// `addr` is the physical address of the ring.
    pub unsafe fn from_raw(mem: Dma<[u8]>, queue_size: usize) -> Result<Self, Error> {
        let ring = Self { mem, queue_size };

        for i in 0..queue_size {
            // Setting them to `u16::MAX` helps with debugging since qemu reports them
            // as illegal values.
            ring.get_element_at(i)
                .table_index
                .store(u16::MAX, Ordering::SeqCst);
        }

        Ok(ring)
    }

    /// ## Panics
    /// This function panics if the index is out of bounds.
    pub fn get_element_at(&self, index: usize) -> &AvailableRingElement {
        // SAFETY: We have exclusive access to the elements and the number of elements
        //         is correct; same as the queue size.
        unsafe {
            self.ring()
                .elements
                .as_slice(self.queue_size)
                .get(index % self.queue_size)
                .expect("virtio-core::available: index out of bounds")
        }
    }

    pub fn head_index(&self) -> u16 {
        self.ring().head_index.load(Ordering::SeqCst)
    }

    pub fn set_head_idx(&self, index: u16) {
        self.ring().head_index.store(index, Ordering::SeqCst);
    }

    pub fn phys_addr(&self) -> usize {
        self.mem.physical()
    }
}

impl<'a> Drop for Available {
    fn drop(&mut self) {
        log::warn!(
            "virtio-core: dropping 'available' ring at {:#x}",
            self.phys_addr()
        );
    }
}

pub struct Used {
    mem: Dma<[u8]>,
    queue_size: usize,
}

impl Used {
    fn ring(&self) -> &UsedRing {
        unsafe { &*self.mem.as_ptr().cast() }
    }
    fn ring_mut(&mut self) -> &mut UsedRing {
        unsafe { &mut *self.mem.as_mut_ptr().cast() }
    }

    pub fn new(queue_size: usize) -> Result<Self, Error> {
        let (_, _, size) = queue_part_sizes(queue_size);
        let mem = unsafe {
            Dma::zeroed_slice(size)
                .map_err(Error::SyscallError)?
                .assume_init()
        };

        unsafe { Self::from_raw(mem, queue_size) }
    }

    /// `addr` is the physical address of the ring.
    pub unsafe fn from_raw(mem: Dma<[u8]>, queue_size: usize) -> Result<Self, Error> {
        let mut ring = Self { mem, queue_size };

        for i in 0..queue_size {
            // Setting them to `u32::MAX` helps with debugging since qemu reports them
            // as illegal values.
            ring.get_mut_element_at(i).table_index.set(u32::MAX);
        }

        Ok(ring)
    }

    /// ## Panics
    /// This function panics if the index is out of bounds.
    pub fn get_element_at(&self, index: usize) -> &UsedRingElement {
        // SAFETY: We have exclusive access to the elements and the number of elements
        //         is correct; same as the queue size.
        unsafe {
            self.ring()
                .elements
                .as_slice(self.queue_size)
                .get(index % self.queue_size)
                .expect("virtio-core::used: index out of bounds")
        }
    }

    /// ## Panics
    /// This function panics if the index is out of bounds.
    pub fn get_mut_element_at(&mut self, index: usize) -> &mut UsedRingElement {
        // SAFETY: We have exclusive access to the elements and the number of elements
        //         is correct; same as the queue size.
        let queue_size = self.queue_size;
        unsafe {
            self.ring_mut()
                .elements
                .as_mut_slice(queue_size)
                .get_mut(index % queue_size)
                .expect("virtio-core::used: index out of bounds")
        }
    }

    pub fn flags(&self) -> u16 {
        self.ring().flags.get()
    }

    pub fn head_index(&self) -> u16 {
        self.ring().head_index.get()
    }

    pub fn phys_addr(&self) -> usize {
        self.mem.physical()
    }
}

impl Drop for Used {
    fn drop(&mut self) {
        log::warn!(
            "virtio-core: dropping 'used' ring at {:#x}",
            self.phys_addr()
        );
    }
}

pub trait Transport: Sync + Send {
    /// `size` specifies the size of the read in bytes.
    ///
    /// ## Panics
    /// This function panics if the provided `size` is more than `size_of::<u64>()`.
    fn load_config(&self, offset: u8, size: u8) -> u64;

    /// Resets the device.
    fn reset(&self);

    /// Returns whether the device supports the specified feature.
    fn check_device_feature(&self, feature: u32) -> bool;

    /// Acknowledges the specified feature.
    ///
    /// **Note**: [`Transport::check_device_feature`] must be used to check whether
    /// the device supports the feature before acknowledging it.
    fn ack_driver_feature(&self, feature: u32);

    /// Finalizes the acknowledged features by setting the `FEATURES_OK` bit in the
    /// device status flags.
    fn finalize_features(&self);

    /// Sets `DRIVER_OK`. Queues must already exist and features must be finalized.
    fn run_device(&self) {
        self.insert_status(DeviceStatusFlags::DRIVER_OK);
    }

    /// Ask the device to fire `vector` on configuration changes.
    ///
    /// Writes `config_msix_vector` only; no IRQ thread. Take `vector` from
    /// [`crate::Device::alloc_irq`] and wait on that file yourself.
    fn setup_config_notify(&self, irq_vec: &InterruptVector);

    /// Each time the device configuration changes this number will be updated.
    fn config_generation(&self) -> u32;

    /// Creates a virtqueue on `irq`'s vector and watches that vector.
    ///
    /// Prefer [`crate::Device::setup_queue`], which allocates `irq` from this
    /// device's [`pcid_interface::irq_helpers::Msix`] table. `InterruptVector`
    /// is what keeps the table index and IRQ file paired.
    fn setup_queue(&self, irq: InterruptVector) -> Result<Arc<Queue>, Error>;

    // TODO(andypython): Should this function be unsafe?
    fn reinit_queue(&self, queue: Arc<Queue>);
    fn insert_status(&self, status: DeviceStatusFlags);
}

struct StandardBell<'a>(&'a mut AtomicU16);

impl NotifyBell for StandardBell<'_> {
    #[inline]
    fn ring(&self, queue_index: u16) {
        self.0.store(queue_index, Ordering::SeqCst);
    }
}

pub struct StandardTransport<'a> {
    pub(crate) common: Mutex<&'a mut CommonCfg>,
    notify: *const u8,
    notify_mul: u32,
    device_space: *const u8,

    queue_index: AtomicU16,
    irq_groups: Mutex<HashMap<u16, Arc<IrqGroup>>>,
}

impl<'a> StandardTransport<'a> {
    pub fn new(
        common: &'a mut CommonCfg,
        notify: *const u8,
        notify_mul: u32,
        device_space: *const u8,
    ) -> Arc<Self> {
        Arc::new(Self {
            common: Mutex::new(common),
            notify,
            notify_mul,

            queue_index: AtomicU16::new(0),
            irq_groups: Mutex::new(HashMap::new()),
            device_space,
        })
    }

    fn attach_irq(&self, irq: InterruptVector, queue: Arc<Queue>) {
        let group = {
            let vector = queue.vector;
            let mut groups = self.irq_groups.lock().unwrap();

            if let Some(group) = groups.get(&vector) {
                group.queues.lock().unwrap().push(queue);
                return;
            }

            let group = Arc::new(IrqGroup {
                queues: Mutex::new(vec![queue]),
            });
            groups.insert(vector, group.clone());

            group
        };

        // `Device::setup_queue` drops `InterruptVector` on return, the thread
        // still needs this fd to subscribe and ACK
        let mut irq_file = irq
            .irq_handle()
            .try_clone()
            .expect("virtio-core: failed to clone IRQ handle");

        let event_queue = RawEventQueue::new().unwrap();
        event_queue
            .subscribe(irq_file.as_raw_fd() as usize, 0, event::EventFlags::READ)
            .unwrap();

        std::thread::spawn(move || {
            for _ in event_queue.map(Result::unwrap) {
                let queues = group.queues.lock().unwrap().clone();
                for queue in &queues {
                    queue.drain_used_ring();
                }

                let mut buf = [0u8; size_of::<usize>()];
                if irq_file.read(&mut buf).unwrap_or(0) == size_of::<usize>() {
                    let _ = irq_file.write(&buf);
                }

                // ACK can unmask completions that arrived during the read.
                for queue in &queues {
                    queue.drain_used_ring();
                }
            }
        });
    }
}

impl Transport for StandardTransport<'_> {
    fn load_config(&self, offset: u8, size: u8) -> u64 {
        unsafe {
            let ptr = self.device_space.add(offset as usize);
            let size = size as usize;

            if size == size_of::<u8>() {
                ptr.cast::<u8>().read() as u64
            } else if size == size_of::<u16>() {
                ptr.cast::<u16>().read() as u64
            } else if size == size_of::<u32>() {
                ptr.cast::<u32>().read() as u64
            } else if size == size_of::<u64>() {
                ptr.cast::<u64>().read() as u64
            } else {
                unreachable!()
            }
        }
    }

    fn reset(&self) {
        let mut common = self.common.lock().unwrap();

        common.device_status.set(DeviceStatusFlags::empty());
        // Upon reset, the device must initialize device status to 0.
        assert_eq!(common.device_status.get(), DeviceStatusFlags::empty());
    }

    fn check_device_feature(&self, feature: u32) -> bool {
        let mut common = self.common.lock().unwrap();

        common.device_feature_select.set(feature >> 5);
        (common.device_feature.get() & (1 << (feature & 31))) != 0
    }

    fn ack_driver_feature(&self, feature: u32) {
        let mut common = self.common.lock().unwrap();

        common.driver_feature_select.set(feature >> 5);

        let current = common.driver_feature.get();
        common.driver_feature.set(current | (1 << (feature & 31)));
    }

    fn finalize_features(&self) {
        // Check VirtIO version 1 compliance.
        assert!(self.check_device_feature(VIRTIO_F_VERSION_1));
        self.ack_driver_feature(VIRTIO_F_VERSION_1);

        let mut common = self.common.lock().unwrap();

        let status = common.device_status.get();
        common
            .device_status
            .set(status | DeviceStatusFlags::FEATURES_OK);

        // Re-read device status to ensure the `FEATURES_OK` bit is still set: otherwise,
        // the device does not support our subset of features and the device is unusable.
        let confirm = common.device_status.get();
        assert!((confirm & DeviceStatusFlags::FEATURES_OK) == DeviceStatusFlags::FEATURES_OK);
    }

    fn setup_config_notify(&self, irq_vec: &InterruptVector) {
        self.common
            .lock()
            .unwrap()
            .config_msix_vector
            .set(irq_vec.vector());
    }

    fn config_generation(&self) -> u32 {
        u32::from(self.common.lock().unwrap().config_generation.get())
    }

    fn setup_queue(&self, irq: InterruptVector) -> Result<Arc<Queue>, Error> {
        let mut common = self.common.lock().unwrap();

        let queue_index = self.queue_index.fetch_add(1, Ordering::SeqCst);
        common.queue_select.set(queue_index);

        let queue_size = common.queue_size.get() as usize;
        let queue_notify_idx = common.queue_notify_off.get();

        // Allocate memory for the queue structues.
        let descriptor = unsafe {
            Dma::<[Descriptor]>::zeroed_slice(queue_size)
                .map_err(Error::SyscallError)?
                .assume_init()
        };

        let avail = Available::new(queue_size)?;
        let used = Used::new(queue_size)?;

        common.queue_desc.set(descriptor.physical() as u64);
        common.queue_driver.set(avail.phys_addr() as u64);
        common.queue_device.set(used.phys_addr() as u64);

        // Set the MSI-X vector.
        let vector = irq.vector();
        common.queue_msix_vector.set(vector);
        assert!(common.queue_msix_vector.get() == vector);

        // Enable the queue.
        common.queue_enable.set(1);

        let notification_bell = unsafe {
            let offset = self.notify_mul * queue_notify_idx as u32;
            &mut *(self.notify.add(offset as usize) as *mut AtomicU16)
        };

        log::debug!(
            "virtio-core: enabled queue #{queue_index} (size={queue_size} vector={vector})"
        );

        let queue = Queue::new(
            descriptor,
            avail,
            used,
            StandardBell(notification_bell),
            queue_index,
            vector,
        );

        drop(common);
        self.attach_irq(irq, queue.clone());
        Ok(queue)
    }

    fn insert_status(&self, status: DeviceStatusFlags) {
        let mut common = self.common.lock().unwrap();
        let old = common.device_status.get();

        common.device_status.set(old | status);
    }

    /// Re-initializes a queue; usually done after a device reset.
    fn reinit_queue(&self, queue: Arc<Queue>) {
        let mut common = self.common.lock().unwrap();
        queue.reinit();

        common.queue_select.set(queue.queue_index);

        common.queue_desc.set(queue.descriptor.physical() as u64);
        common.queue_driver.set(queue.available.phys_addr() as u64);
        common.queue_device.set(queue.used.phys_addr() as u64);

        // Set the MSI-X vector.
        common.queue_msix_vector.set(queue.vector);
        assert!(common.queue_msix_vector.get() == queue.vector);

        // Enable the queue.
        common.queue_enable.set(1);
    }
}

unsafe impl Send for StandardTransport<'_> {}
unsafe impl Sync for StandardTransport<'_> {}
