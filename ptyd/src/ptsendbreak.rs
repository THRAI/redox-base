//! "sendbreak" resource for the `pty` scheme.
//! For a PTY, this simply means to sleep for a short amount of time.
use std::cell::RefCell;
use std::rc::Weak;

use libc::{c_int, nanosleep, timespec};
use syscall::error::{Error, Result, EBADF, EINVAL, EPIPE};
use syscall::flag::{EventFlags, F_GETFL, F_SETFL, O_ACCMODE};

use crate::pty::Pty;
use crate::resource::Resource;

/// Read side of a pipe
pub struct PtSendbreak {
    pty: Weak<RefCell<Pty>>,
    flags: usize,
}

impl PtSendbreak {
    pub fn new(pty: Weak<RefCell<Pty>>, flags: usize) -> Self {
        PtSendbreak { pty, flags }
    }

    // The actual hardware break doesn't get sent here because
    // that is a tty feature, not a pty feature.
    fn sendbreak(&mut self, _duration: c_int) -> Result<usize> {
        let _ = unsafe {
            // POSIX specifies that we need to sleep for 0.25 to 0.5 seconds.
            // FreeBSD uses 0.4, and that seems reasonable.
            let tm = timespec {
                tv_sec: 0,
                tv_nsec: 400_000_000,
            };
            nanosleep(&raw const tm, core::ptr::null_mut())
        };
        Ok(4)
    }
}

impl Resource for PtSendbreak {
    fn pty(&self) -> Weak<RefCell<Pty>> {
        self.pty.clone()
    }

    fn flags(&self) -> usize {
        self.flags
    }

    fn path(&mut self, buf: &mut [u8]) -> Result<usize> {
        if let Some(pty_lock) = self.pty.upgrade() {
            pty_lock.borrow_mut().path(buf)
        } else {
            Err(Error::new(EPIPE))
        }
    }

    fn read(&mut self, _buf: &mut [u8]) -> Result<usize> {
        Err(Error::new(EBADF))
    }

    fn write(&mut self, buf: &[u8]) -> Result<usize> {
        let duration = c_int::from_ne_bytes(
            buf.get(..4)
                .and_then(|b| <[u8; 4]>::try_from(b).ok())
                .ok_or(Error::new(EINVAL))?,
        );
        self.sendbreak(duration)
    }

    fn sync(&mut self) -> Result<()> {
        Ok(())
    }

    fn fcntl(&mut self, cmd: usize, arg: usize) -> Result<usize> {
        match cmd {
            F_GETFL => Ok(self.flags),
            F_SETFL => {
                self.flags = (self.flags & O_ACCMODE) | (arg & !O_ACCMODE);
                Ok(0)
            }
            _ => Err(Error::new(EINVAL)),
        }
    }

    fn fevent(&mut self) -> Result<EventFlags> {
        Err(Error::new(EBADF))
    }

    fn events(&mut self) -> EventFlags {
        EventFlags::empty()
    }
}
