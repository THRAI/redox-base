use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::os::fd::{AsFd, AsRawFd, BorrowedFd, FromRawFd, RawFd};
use std::os::unix::fs::OpenOptionsExt;
use std::{mem, slice};

use libredox::flag::{O_CLOEXEC, O_NONBLOCK, O_RDWR};
use libredox::Fd;
use orbclient::Event;
use syscall::ESTALE;

fn read_to_slice<T: Copy>(
    file: BorrowedFd,
    buf: &mut [T],
) -> Result<usize, libredox::error::Error> {
    unsafe {
        libredox::call::read(
            file.as_raw_fd() as usize,
            slice::from_raw_parts_mut(buf.as_mut_ptr() as *mut u8, size_of_val(buf)),
        )
        .map(|count| count / size_of::<T>())
    }
}

unsafe fn any_as_u8_slice<T: Sized>(p: &T) -> &[u8] {
    slice::from_raw_parts((p as *const T) as *const u8, size_of::<T>())
}

pub struct ConsumerHandle(File);

pub enum ConsumerHandleEvent<'a> {
    Events(&'a [Event]),
    Handoff,
}

impl ConsumerHandle {
    pub fn new_vt() -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(O_NONBLOCK)
            .open("/scheme/input/consumer")?;
        Ok(Self(file))
    }

    pub fn bootlog_vt() -> io::Result<Self> {
        let file = OpenOptions::new()
            .read(true)
            .custom_flags(O_NONBLOCK)
            .open("/scheme/input/consumer_bootlog")?;
        Ok(Self(file))
    }

    pub fn event_handle(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }

    pub fn open_display_v2(&self) -> io::Result<File> {
        let display_file = libredox::call::openat(
            self.0.as_raw_fd() as usize,
            "display",
            O_CLOEXEC | O_NONBLOCK | O_RDWR,
            0,
        )
        .map(|socket| unsafe { File::from_raw_fd(socket as RawFd) })?;

        Ok(display_file)
    }

    pub fn read_events<'a>(&self, events: &'a mut [Event]) -> io::Result<ConsumerHandleEvent<'a>> {
        match read_to_slice(self.0.as_fd(), events) {
            Ok(count) => Ok(ConsumerHandleEvent::Events(&events[..count])),
            Err(err) if err.errno() == ESTALE => Ok(ConsumerHandleEvent::Handoff),
            Err(err) => Err(err.into()),
        }
    }
}

#[derive(Debug, Clone)]
#[repr(C)]
pub struct ControlEvent {
    pub kind: usize,
    pub data: usize,
}

impl From<VtActivate> for ControlEvent {
    fn from(value: VtActivate) -> Self {
        ControlEvent {
            kind: 1,
            data: value.vt,
        }
    }
}

impl From<KeymapActivate> for ControlEvent {
    fn from(value: KeymapActivate) -> Self {
        ControlEvent {
            kind: 2,
            data: value.keymap,
        }
    }
}

pub struct VtActivate {
    pub vt: usize,
}

pub struct KeymapActivate {
    pub keymap: usize,
}

pub struct DisplayHandle(File);

impl DisplayHandle {
    pub fn new<S: Into<String>>(scheme_name: S, control_cap: Fd, early: bool) -> io::Result<Self> {
        let path = if early {
            format!("/scheme/input/handle_early/{}", scheme_name.into())
        } else {
            format!("/scheme/input/handle/{}", scheme_name.into())
        };
        let handle = File::open(path)?;
        libredox::call::call_wo(
            handle.as_raw_fd() as usize,
            &control_cap.into_raw().to_ne_bytes(),
            syscall::CallFlags::FD,
            &[],
        )?;
        Ok(Self(handle))
    }

    pub fn inner(&self) -> BorrowedFd<'_> {
        self.0.as_fd()
    }
}

pub struct ControlHandle(File);

impl ControlHandle {
    pub fn new() -> io::Result<Self> {
        Ok(Self(File::open("/scheme/input/control")?))
    }

    /// Sent to Handle::Display
    pub fn activate_vt(&mut self, vt: usize) -> io::Result<usize> {
        let cmd = ControlEvent::from(VtActivate { vt });
        self.0.write(unsafe { any_as_u8_slice(&cmd) })
    }

    /// Sent to Handle::Producer
    pub fn activate_keymap(&mut self, keymap: usize) -> io::Result<usize> {
        let cmd = ControlEvent::from(KeymapActivate { keymap });
        self.0.write(unsafe { any_as_u8_slice(&cmd) })
    }
}

#[derive(Debug)]
#[repr(usize)]
pub enum VtEventKind {
    Activate,
}

#[derive(Debug)]
#[repr(C)]
pub struct VtEvent {
    pub kind: VtEventKind,
    pub vt: usize,
}

impl VtEvent {
    pub fn as_bytes(&self) -> &[u8] {
        unsafe { any_as_u8_slice(self) }
    }

    pub unsafe fn from_bytes(bytes: &[u8]) -> Option<Self> {
        let bytes: &[u8; size_of::<Self>()] = bytes.try_into().ok()?;
        Some(unsafe { mem::transmute::<[u8; _], Self>(*bytes) })
    }
}

pub struct ProducerHandle(File);

impl ProducerHandle {
    pub fn new() -> io::Result<Self> {
        File::open("/scheme/input/producer").map(ProducerHandle)
    }

    pub fn write_event(&mut self, event: orbclient::Event) -> io::Result<()> {
        let amount = self.0.write(&event)?;
        assert!(amount == size_of::<orbclient::Event>());
        Ok(())
    }
}
