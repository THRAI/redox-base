//! "flush" resource for the `pty` scheme.
//! Clear the pending output for the input, output, or both.
use std::cell::RefCell;
use std::rc::Weak;

use libc::{TCIFLUSH, TCIOFLUSH, TCOFLUSH};
use syscall::error::{Error, Result, EBADF, EINVAL, EPIPE};
use syscall::flag::{EventFlags, F_GETFL, F_SETFL, O_ACCMODE};

use crate::pty::Pty;
use crate::resource::Resource;

/// Read side of a pipe
pub struct PtFlush {
    pty: Weak<RefCell<Pty>>,
    flags: usize,
}

impl PtFlush {
    pub fn new(pty: Weak<RefCell<Pty>>, flags: usize) -> Self {
        PtFlush { pty, flags }
    }
    fn flush_write(&mut self) -> Result<usize> {
        if let Some(pty) = self.pty.upgrade() {
            pty.borrow_mut().miso.clear();
            Ok(0)
        } else {
            Err(Error::new(EPIPE))
        }
    }

    fn flush_read(&mut self) -> Result<usize> {
        if let Some(pty) = self.pty.upgrade() {
            pty.borrow_mut().mosi.clear();
            Ok(0)
        } else {
            Err(Error::new(EPIPE))
        }
    }

    fn flush(&mut self, buf: &[u8]) -> Result<usize> {
        let action = u32::from_ne_bytes(
            buf.get(..4)
                .and_then(|b| <[u8; 4]>::try_from(b).ok())
                .ok_or(Error::new(EINVAL))?,
        );

        match action.cast_signed() {
            TCIFLUSH => {
                self.flush_read()?;
            }
            TCOFLUSH => {
                self.flush_write()?;
            }
            TCIOFLUSH => {
                self.flush_read()?;
                self.flush_write()?;
            }
            _ => return Err(Error::new(EINVAL)),
        }
        Ok(4)
    }
}

impl Resource for PtFlush {
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
        self.flush(buf)
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
