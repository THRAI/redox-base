//! "flow" resource for the `pty` scheme.
//! Allows PTY flow control -- stop or restart PTY output.
use std::cell::RefCell;
use std::rc::Weak;

use libc::{TCIOFF, TCION, TCOOFF, TCOON};
use syscall::error::{Error, Result, EBADF, EINVAL, EPIPE};
use syscall::flag::{EventFlags, F_GETFL, F_SETFL, O_ACCMODE};

use crate::pty::Pty;
use crate::resource::Resource;

/// Read side of a pipe
pub struct PtFlow {
    pty: Weak<RefCell<Pty>>,
    flags: usize,
}

impl PtFlow {
    pub fn new(pty: Weak<RefCell<Pty>>, flags: usize) -> Self {
        PtFlow { pty, flags }
    }

    fn flow(&mut self, buf: &[u8]) -> Result<usize> {
        let action = u32::from_ne_bytes(
            buf.get(..4)
                .and_then(|b| <[u8; 4]>::try_from(b).ok())
                .ok_or(Error::new(EINVAL))?,
        );
        let action = action.cast_signed();

        match action {
            TCOON | TCOOFF => {
                let pty_lock = self.pty.upgrade().ok_or(Error::new(EPIPE))?;
                let mut pty = pty_lock.borrow_mut();
                let pty = &mut *pty;

                if action == TCOON {
                    pty.stopped = false;
                } else if action == TCOOFF {
                    pty.stopped = true;
                }
            }
            TCION | TCIOFF => {
                // We are a pty, and the start and stop characters
                // only have to be written if we are a tty
            }
            _ => return Err(Error::new(EINVAL)),
        }
        Ok(4)
    }
}

impl Resource for PtFlow {
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
        self.flow(buf)
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
