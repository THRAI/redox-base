use std::cell::{RefCell, RefMut};
use std::collections::BTreeMap;
use std::rc::Rc;
use std::str;

use libredox::protocol::TtyCall;
use redox_scheme::scheme::SchemeSync;
use redox_scheme::{CallerCtx, OpenResult, Socket};
use syscall::data::Stat;
use syscall::error::{Error, Result, EACCES, EBADF, EINVAL, ENOENT};
use syscall::flag::{EventFlags, MODE_CHR};
use syscall::schemev2::NewFdFlags;
use syscall::FobtainFdFlags;

use crate::controlterm::PtyControlTerm;
use crate::pgrp::PtyPgrp;
use crate::ptflow::PtFlow;
use crate::ptflush::PtFlush;
use crate::ptlock::PtyLock;
use crate::ptname::PtsName;
use crate::ptsendbreak::PtSendbreak;
use crate::pty::Pty;
use crate::resource::Resource;
use crate::subterm::PtySubTerm;
use crate::termios::PtyTermios;
use crate::winsize::PtyWinsize;

pub enum Handle {
    Resource(Box<dyn Resource>),
    SchemeRoot,
}

pub struct PtyScheme {
    socket: Rc<Socket>,
    next_id: usize,
    pub handles: BTreeMap<usize, Handle>,
}

impl PtyScheme {
    pub fn new(socket: Rc<Socket>) -> Self {
        PtyScheme {
            socket,
            next_id: 0,
            handles: BTreeMap::new(),
        }
    }

    fn get_resource_mut(&mut self, id: usize) -> Result<&mut Box<dyn Resource>> {
        match self.handles.get_mut(&id).ok_or(Error::new(EBADF))? {
            Handle::Resource(res) => Ok(res),
            Handle::SchemeRoot => Err(Error::new(EBADF)),
        }
    }
}

impl SchemeSync for PtyScheme {
    fn scheme_root(&mut self) -> Result<usize> {
        let id = self.next_id;
        self.next_id += 1;
        self.handles.insert(id, Handle::SchemeRoot);
        Ok(id)
    }

    fn openat(
        &mut self,
        dirfd: usize,
        path: &str,
        flags: usize,
        fcntl_flags: u32,
        _ctx: &CallerCtx,
    ) -> Result<OpenResult> {
        if !matches!(
            self.handles.get(&dirfd).ok_or(Error::new(EBADF))?,
            Handle::SchemeRoot
        ) {
            return Err(Error::new(EACCES));
        }

        let path = path.trim_matches('/');

        // This happens if we are passed "/scheme/pty" and not "/scheme/pty/ptmx".
        if path.is_empty() {
            return Err(Error::new(ENOENT));
        }

        let id = self.next_id;

        if path == "ptmx" {
            let pty = Rc::new(RefCell::new(Pty::new(id)));
            self.handles.insert(
                id,
                Handle::Resource(Box::new(PtyControlTerm::new(pty, flags))),
            );
        } else {
            let control_term_id = path.parse::<usize>().or(Err(Error::new(EINVAL)))?;
            let pty = {
                let handle = self
                    .handles
                    .get(&control_term_id)
                    .ok_or(Error::new(ENOENT))?;

                match handle {
                    Handle::Resource(res) => res.pty(),
                    Handle::SchemeRoot => return Err(Error::new(ENOENT)),
                }
            };

            self.handles.insert(
                id,
                Handle::Resource(Box::new(PtySubTerm::new(pty, flags | fcntl_flags as usize))),
            );
        }

        self.next_id += 1;

        Ok(OpenResult::ThisScheme {
            number: id,
            flags: NewFdFlags::empty(),
        })
    }

    fn dup(&mut self, old_id: usize, buf: &[u8], _ctx: &CallerCtx) -> Result<OpenResult> {
        let handle: Box<dyn Resource> = {
            let old_handle = self.handles.get(&old_id).ok_or(Error::new(EBADF))?;

            let old_resource = match old_handle {
                Handle::Resource(res) => res,
                Handle::SchemeRoot => return Err(Error::new(EBADF)),
            };

            if buf == b"pgrp" {
                Box::new(PtyPgrp::new(old_resource.pty(), old_resource.flags()))
            } else if buf == b"termios" {
                Box::new(PtyTermios::new(old_resource.pty(), old_resource.flags()))
            } else if buf == b"winsize" {
                Box::new(PtyWinsize::new(old_resource.pty(), old_resource.flags()))
            } else if buf == b"ptlock" {
                Box::new(PtyLock::new(old_resource.pty(), old_resource.flags()))
            } else if buf == b"ptsname" {
                Box::new(PtsName::new(old_resource.pty(), old_resource.flags()))
            } else if buf == b"flush" {
                Box::new(PtFlush::new(old_resource.pty(), old_resource.flags()))
            } else if buf == b"sendbreak" {
                Box::new(PtSendbreak::new(old_resource.pty(), old_resource.flags()))
            } else if buf == b"flow" {
                Box::new(PtFlow::new(old_resource.pty(), old_resource.flags()))
            } else {
                return Err(Error::new(EINVAL));
            }
        };

        let id = self.next_id;
        self.next_id += 1;
        self.handles.insert(id, Handle::Resource(handle));

        Ok(OpenResult::ThisScheme {
            number: id,
            flags: NewFdFlags::empty(),
        })
    }

    fn read(
        &mut self,
        id: usize,
        buf: &mut [u8],
        _offset: u64,
        _fcntl_flags: u32,
        _ctx: &CallerCtx,
    ) -> Result<usize> {
        let handle = self.get_resource_mut(id)?;
        handle.read(buf)
    }

    fn write(
        &mut self,
        id: usize,
        buf: &[u8],
        _offset: u64,
        _fcntl_flags: u32,
        _ctx: &CallerCtx,
    ) -> Result<usize> {
        let handle = self.get_resource_mut(id)?;
        handle.write(buf)
    }

    fn fcntl(&mut self, id: usize, cmd: usize, arg: usize, _ctx: &CallerCtx) -> Result<usize> {
        let handle = self.get_resource_mut(id)?;
        handle.fcntl(cmd, arg)
    }

    fn fevent(&mut self, id: usize, _flags: EventFlags, _ctx: &CallerCtx) -> Result<EventFlags> {
        let handle = self.get_resource_mut(id)?;
        handle.fevent()
    }

    fn fpath(&mut self, id: usize, buf: &mut [u8], _ctx: &CallerCtx) -> Result<usize> {
        let handle = self.get_resource_mut(id)?;
        handle.path(buf)
    }

    fn fstat(&mut self, id: usize, stat: &mut Stat, _ctx: &CallerCtx) -> Result<()> {
        let handle = self.handles.get(&id).ok_or(Error::new(EBADF))?;

        match handle {
            Handle::SchemeRoot => return Err(Error::new(EBADF)),
            Handle::Resource(_res) => {
                *stat = Stat {
                    st_mode: MODE_CHR | 0o666,
                    ..Default::default()
                };
            }
        }

        Ok(())
    }

    fn fsync(&mut self, id: usize, _ctx: &CallerCtx) -> Result<()> {
        let handle = self.get_resource_mut(id)?;
        handle.sync()
    }

    fn on_close(&mut self, id: usize) {
        let _ = self.handles.remove(&id);
    }

    fn call(
        &mut self,
        id: usize,
        payload: &mut [u8],
        metadata: &[u64],
        _ctx: &CallerCtx, // Only pid and id are correct here, uid/gid are not used
    ) -> Result<usize> {
        const REQ_READ: u64 = 0;
        const REQ_WRITE: u64 = 1;
        let &[verb_raw, request] = metadata.get(0..2).ok_or(Error::new(EINVAL))? else {
            return Err(Error::new(EINVAL));
        };
        let verb = TtyCall::try_from_raw(verb_raw as usize).ok_or(Error::new(EINVAL))?;

        let old_handle = self.handles.get(&id).ok_or(Error::new(EBADF))?;

        let old_resource = match old_handle {
            Handle::Resource(res) => res,
            Handle::SchemeRoot => return Err(Error::new(EBADF)),
        };

        match verb {
            TtyCall::Termios => {
                let mut termios = PtyTermios::new(old_resource.pty(), old_resource.flags());
                match request {
                    REQ_READ => termios.read(payload),
                    REQ_WRITE => termios.write(payload),
                    _ => Err(Error::new(EINVAL)),
                }
            }
            TtyCall::Flush => {
                let mut flush = PtFlush::new(old_resource.pty(), old_resource.flags());
                match request {
                    REQ_READ => flush.read(payload),
                    REQ_WRITE => flush.write(payload),
                    _ => Err(Error::new(EINVAL)),
                }
            }
            TtyCall::SendBreak => {
                let mut sendbreak = PtSendbreak::new(old_resource.pty(), old_resource.flags());
                match request {
                    REQ_READ => sendbreak.read(payload),
                    REQ_WRITE => sendbreak.write(payload),
                    _ => Err(Error::new(EINVAL)),
                }
            }
            TtyCall::Flow => {
                let mut flow = PtFlow::new(old_resource.pty(), old_resource.flags());
                match request {
                    REQ_READ => flow.read(payload),
                    REQ_WRITE => flow.write(payload),
                    _ => Err(Error::new(EINVAL)),
                }
            }
            TtyCall::PtsName => {
                let mut ptsname = PtsName::new(old_resource.pty(), old_resource.flags());
                match request {
                    REQ_READ => ptsname.read(payload),
                    REQ_WRITE => ptsname.write(payload),
                    _ => Err(Error::new(EINVAL)),
                }
            }
            TtyCall::PtLock => {
                let mut ptlock = PtyLock::new(old_resource.pty(), old_resource.flags());
                match request {
                    REQ_READ => ptlock.read(payload),
                    REQ_WRITE => ptlock.write(payload),
                    _ => Err(Error::new(EINVAL)),
                }
            }
            TtyCall::Pgrp => {
                let mut pgrp = PtyPgrp::new(old_resource.pty(), old_resource.flags());
                match request {
                    REQ_READ => pgrp.read(payload),
                    REQ_WRITE => pgrp.write(payload),
                    _ => Err(Error::new(EINVAL)),
                }
            }
            TtyCall::Winsize => {
                let mut winsize = PtyWinsize::new(old_resource.pty(), old_resource.flags());
                match request {
                    REQ_READ => winsize.read(payload),
                    REQ_WRITE => winsize.write(payload),
                    _ => Err(Error::new(EINVAL)),
                }
            }
        }
    }
    fn on_sendfd(&mut self, sendfd_request: &redox_scheme::SendFdRequest) -> Result<usize> {
        let handle = self
            .handles
            .get(&sendfd_request.id())
            .ok_or(Error::new(EBADF))?;
        match handle {
            Handle::Resource(resource) => {
                let mut new_fds = [usize::MAX];
                sendfd_request.obtain_fd(&self.socket, FobtainFdFlags::empty(), &mut new_fds)?;
                let object_handle = libredox::Fd::new(new_fds[0]);
                let pty_lock = resource.pty().upgrade().expect("all resources have a pty");
                let mut pty: RefMut<Pty> = pty_lock.borrow_mut();
                pty.pgrp_handle = Some(object_handle);
                Ok(new_fds.len())
            }
            Handle::SchemeRoot => Err(Error::new(EBADF)),
        }
    }
}
