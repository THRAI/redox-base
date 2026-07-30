//! "ptsname" resource for the `pty` scheme.
//! Returns the id of the pseudoterminal subsidiary (PTS) as a u32.
use std::cell::RefCell;
use std::rc::Weak;

use syscall::error::{Error, Result, EBADF, EINVAL, EIO, EPIPE};
use syscall::flag::{EventFlags, F_GETFL, F_SETFL, O_ACCMODE};

use crate::pty::Pty;
use crate::resource::Resource;

/// Read side of a pipe
pub struct PtsName {
    pty: Weak<RefCell<Pty>>,
    flags: usize,
}

impl PtsName {
    pub fn new(pty: Weak<RefCell<Pty>>, flags: usize) -> Self {
        PtsName { pty, flags }
    }
}

impl Resource for PtsName {
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

    fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        if let Some(pty_lock) = self.pty.upgrade() {
            let pty = pty_lock.borrow();
            if pty.locked {
                Err(Error::new(EIO))
            } else {
                let id_buf = buf
                    .get_mut(..4)
                    .and_then(|b| <&mut [u8; 4]>::try_from(b).ok())
                    .ok_or(Error::new(EBADF))?;
                *id_buf = (pty.id as u32).to_ne_bytes();
                Ok(4)
            }
        } else {
            Err(Error::new(EPIPE))
        }
    }

    fn write(&mut self, _buf: &[u8]) -> Result<usize> {
        Err(Error::new(EBADF))
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
