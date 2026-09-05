//! `:input`
//!
//! A seperate scheme is required since all of the input from different input devices is required
//! to be combined into a single stream which is later going to be processed by the "consumer"
//! which usually is Orbital.
//!
//! ## Input Device ("producer")
//! Write events to `input:producer`.
//!
//! ## Input Consumer ("consumer")
//! Read events from `input:consumer`. Optionally, set the `EVENT_READ` flag to be notified when
//! events are available.

use std::borrow::Cow;
use std::collections::BTreeSet;
use std::fs::File;
use std::ops::ControlFlow;
use std::os::fd::IntoRawFd;

use inputd::{ControlEvent, VtEvent, VtEventKind};
use libredox::errno::ESTALE;
use libredox::Fd;
use orbclient::{Event, EventOption};
use redox_scheme::scheme::SchemeSync;
use redox_scheme::{CallerCtx, OpenResult, Response, SignalBehavior, Socket};
use scheme_utils::{Blocking, FpathWriter, HandleMap};
use syscall::schemev2::NewFdFlags;
use syscall::{
    CallFlags, Error as SysError, EventFlags, FobtainFdFlags, EACCES, EBADF, EEXIST, EINVAL,
    EOPNOTSUPP,
};

pub mod keymap;

use keymap::KeymapKind;

use crate::keymap::KeymapData;

enum Handle {
    Producer,
    Consumer {
        events: EventFlags,
        pending: Vec<u8>,
        /// We return an ESTALE error once to indicate that a handoff to a different graphics driver
        /// is necessary.
        needs_handoff: bool,
        notified: bool,
        vt: usize,
    },
    Display {
        events: EventFlags,
        device: String,
        device_control: Option<Fd>,
        /// Control of all VT's gets handed over from earlyfb devices to the first non-earlyfb device.
        is_earlyfb: bool,
    },
    Control,
    SchemeRoot,
}

enum ActiveDisplay {
    Unknown,
    /// Control of all VT's gets handed over from earlyfb devices to the first non-earlyfb device.
    Early {
        name: String,
        id: usize,
    },
    Regular {
        name: String,
        id: usize,
    },
}

struct InputScheme<'a> {
    socket: &'a Socket,
    handles: HandleMap<Handle>,

    next_vt_id: usize,

    active_display: ActiveDisplay,
    vts: BTreeSet<usize>,
    super_key: bool,
    active_vt: Option<usize>,
    active_keymap: KeymapData,
    lshift: bool,
    rshift: bool,

    has_new_events: bool,
    pending_activate: Option<usize>,
}

impl<'a> InputScheme<'a> {
    fn new(socket: &'a Socket) -> Self {
        Self {
            socket,
            handles: HandleMap::new(),

            next_vt_id: 2, // VT 1 is reserved for the bootlog

            active_display: ActiveDisplay::Unknown,
            vts: BTreeSet::new(),
            super_key: false,
            active_vt: None,
            // TODO: configurable init?
            active_keymap: KeymapData::new(KeymapKind::US),
            lshift: false,
            rshift: false,
            has_new_events: false,
            pending_activate: None,
        }
    }

    fn send_vt_event_to_active_display(&mut self, event: VtEvent) {
        match self.active_display {
            ActiveDisplay::Unknown => {}
            ActiveDisplay::Early { id, .. } | ActiveDisplay::Regular { id, .. } => {
                match self.handles.get_mut(id).unwrap() {
                    Handle::Display { device_control, .. } => {
                        if let Some(device_control) = device_control {
                            libredox::call::call_wo(
                                device_control.raw(),
                                event.as_bytes(),
                                CallFlags::empty(),
                                &[],
                            )
                            .unwrap(); // FIXME
                        }
                    }
                    _ => unreachable!(),
                }
            }
        }
    }

    fn switch_vt(&mut self, new_active: usize) {
        if let Some(active_vt) = self.active_vt {
            if new_active == active_vt {
                return;
            }
        }

        if !self.vts.contains(&new_active) {
            log::warn!("switch to non-existent VT #{new_active} was requested");
            return;
        }

        log::debug!(
            "switching from VT #{} to VT #{new_active}",
            self.active_vt.unwrap_or(0)
        );

        self.send_vt_event_to_active_display(VtEvent {
            kind: VtEventKind::Activate,
            vt: new_active,
        });

        self.active_vt = Some(new_active);
    }

    fn switch_keymap(&mut self, new_active: usize) {
        if new_active == self.active_keymap.get_kind() as usize {
            return;
        }

        log::debug!(
            "switching from keymap #{} to keymap #{}",
            self.active_keymap.get_kind(),
            KeymapKind::from(new_active),
        );

        self.active_keymap = KeymapData::new(new_active.into());
    }
}

impl SchemeSync for InputScheme<'_> {
    fn scheme_root(&mut self) -> syscall::Result<usize> {
        Ok(self.handles.insert(Handle::SchemeRoot))
    }

    fn openat(
        &mut self,
        dirfd: usize,
        path: &str,
        _flags: usize,
        _fcntl_flags: u32,
        _ctx: &CallerCtx,
    ) -> syscall::Result<OpenResult> {
        match self.handles.get(dirfd)? {
            Handle::SchemeRoot => {}
            Handle::Consumer { vt, .. } => {
                let mut path_parts = path.split('/');

                let command = path_parts.next().ok_or(SysError::new(EINVAL))?;
                match command {
                    "display" => {
                        let display = match &self.active_display {
                            ActiveDisplay::Unknown => return Err(SysError::new(EINVAL)),
                            ActiveDisplay::Early { name, .. }
                            | ActiveDisplay::Regular { name, .. } => name,
                        };
                        // NOTE: File::open for another scheme is deadlock prone. In this case
                        // however care is taken for the target scheme to never make a request to us
                        // after initial registration. Instead we push commands to the target scheme.
                        // Also care is taken to not run this code when doing openat on the scheme
                        // root. That would currently deadlock due to initnsmgr not handling openat
                        // requests in parallel: https://gitlab.redox-os.org/redox-os/base/-/work_items/93
                        return Ok(OpenResult::OtherScheme {
                            fd: File::open(format!("/scheme/{display}/{vt}"))
                                .map_err(|err| SysError::new(err.raw_os_error().unwrap()))?
                                .into_raw_fd() as usize,
                        });
                    }
                    _ => {
                        log::error!("invalid path '{path}'");
                        return Err(SysError::new(EINVAL));
                    }
                }
            }
            _ => return Err(SysError::new(EACCES)),
        }

        if !matches!(self.handles.get(dirfd)?, Handle::SchemeRoot) {
            return Err(SysError::new(EACCES));
        }

        let mut path_parts = path.split('/');

        let command = path_parts.next().ok_or(SysError::new(EINVAL))?;

        let handle_ty = match command {
            "producer" => Handle::Producer,
            "consumer" => {
                let vt = self.next_vt_id;
                self.next_vt_id += 1;
                self.vts.insert(vt);

                if self.active_vt.is_none() {
                    self.switch_vt(vt);
                }
                Handle::Consumer {
                    events: EventFlags::empty(),
                    pending: Vec::new(),
                    needs_handoff: false,
                    notified: false,
                    vt,
                }
            }
            "consumer_bootlog" => {
                if !self.vts.insert(1) {
                    return Err(SysError::new(EEXIST));
                }

                self.switch_vt(1);
                Handle::Consumer {
                    events: EventFlags::empty(),
                    pending: Vec::new(),
                    needs_handoff: false,
                    notified: false,
                    vt: 1,
                }
            }
            "handle" | "handle_early" => {
                let display = path_parts.next().ok_or(SysError::new(EINVAL))?;

                Handle::Display {
                    events: EventFlags::empty(),
                    device: display.to_owned(),
                    device_control: None,
                    is_earlyfb: command == "handle_early",
                }
            }
            "control" => Handle::Control,

            _ => {
                log::error!("invalid path '{path}'");
                return Err(SysError::new(EINVAL));
            }
        };

        log::debug!("{path} channel has been opened");

        let fd = self.handles.insert(handle_ty);
        Ok(OpenResult::ThisScheme {
            number: fd,
            flags: NewFdFlags::empty(),
        })
    }

    fn fpath(&mut self, id: usize, buf: &mut [u8], _ctx: &CallerCtx) -> syscall::Result<usize> {
        let display = match &self.active_display {
            ActiveDisplay::Unknown => return Err(SysError::new(EINVAL)),
            ActiveDisplay::Early { name, .. } | ActiveDisplay::Regular { name, .. } => name,
        };
        FpathWriter::with(buf, display, |w| {
            let handle = self.handles.get(id)?;

            if let Handle::Consumer { vt, .. } = handle {
                write!(w, "{vt}").unwrap();
                Ok(())
            } else {
                Err(SysError::new(EINVAL))
            }
        })
    }

    fn read(
        &mut self,
        id: usize,
        buf: &mut [u8],
        _offset: u64,
        _fcntl_flags: u32,
        _ctx: &CallerCtx,
    ) -> syscall::Result<usize> {
        let handle = self.handles.get_mut(id)?;

        match handle {
            Handle::Consumer {
                pending,
                needs_handoff,
                ..
            } => {
                if *needs_handoff {
                    *needs_handoff = false;
                    // Indicates that handoff to a new graphics driver is necessary.
                    return Err(SysError::new(ESTALE));
                }

                let copy = core::cmp::min(pending.len(), buf.len());

                for (i, byte) in pending.drain(..copy).enumerate() {
                    buf[i] = byte;
                }

                Ok(copy)
            }

            Handle::Display { .. } => {
                log::error!("display tried to read");
                Err(SysError::new(EINVAL))
            }
            Handle::Producer => {
                log::error!("producer tried to read");
                Err(SysError::new(EINVAL))
            }
            Handle::Control => {
                log::error!("control tried to read");
                Err(SysError::new(EINVAL))
            }
            Handle::SchemeRoot => Err(SysError::new(EBADF)),
        }
    }

    fn write(
        &mut self,
        id: usize,
        buf: &[u8],
        _offset: u64,
        _fcntl_flags: u32,
        _ctx: &CallerCtx,
    ) -> syscall::Result<usize> {
        self.has_new_events = true;

        let handle = self.handles.get_mut(id)?;

        match handle {
            Handle::Control => {
                if buf.len() != size_of::<ControlEvent>() {
                    log::error!("control tried to write incorrectly sized command");
                    return Err(SysError::new(EINVAL));
                }

                // SAFETY: We have verified the size of the buffer above.
                let cmd = unsafe { &*buf.as_ptr().cast::<ControlEvent>() };

                match cmd.kind {
                    1 => self.switch_vt(cmd.data),
                    2 => self.switch_keymap(cmd.data),
                    k => {
                        log::warn!("unknown control {}", k);
                    }
                }

                return Ok(buf.len());
            }

            Handle::Consumer { .. } => {
                log::error!("consumer tried to write");
                return Err(SysError::new(EINVAL));
            }
            Handle::Display { .. } => {
                log::error!("display tried to write");
                return Err(SysError::new(EINVAL));
            }
            Handle::Producer => {}
            Handle::SchemeRoot => return Err(SysError::new(EBADF)),
        }

        if buf.len() == 1 && buf[0] > 0xf4 {
            return Ok(1);
        }

        let mut events = Cow::from(unsafe {
            core::slice::from_raw_parts(
                buf.as_ptr() as *const Event,
                buf.len() / size_of::<Event>(),
            )
        });

        for i in 0..events.len() {
            let mut new_active_opt = None;
            match events[i].to_option() {
                EventOption::Key(mut key_event) => match key_event.scancode {
                    f @ orbclient::K_F1..=orbclient::K_F10 if self.super_key => {
                        new_active_opt = Some((f - 0x3A) as usize);
                    }
                    orbclient::K_F11 if self.super_key => {
                        new_active_opt = Some(11);
                    }
                    orbclient::K_F12 if self.super_key => {
                        new_active_opt = Some(12);
                    }
                    orbclient::K_SUPER => {
                        self.super_key = key_event.pressed;
                    }
                    orbclient::K_LEFT_SHIFT => {
                        self.lshift = key_event.pressed;
                    }
                    orbclient::K_RIGHT_SHIFT => {
                        self.rshift = key_event.pressed;
                    }

                    key => {
                        let shift = self.lshift | self.rshift;
                        let ev = self.active_keymap.get_char(key, shift);
                        key_event.character = ev;
                        events.to_mut()[i] = key_event.to_event();
                    }
                },

                // Set ID for controller events
                EventOption::ControllerAxis(mut axis_event) => {
                    //TODO: what to do with overflow?
                    axis_event.id = id as u32;
                    events.to_mut()[i] = axis_event.to_event();
                }
                EventOption::ControllerButton(mut button_event) => {
                    //TODO: what to do with overflow?
                    button_event.id = id as u32;
                    events.to_mut()[i] = button_event.to_event();
                }

                _ => continue,
            }

            if let Some(new_active) = new_active_opt {
                self.switch_vt(new_active);
            }
        }

        let handle = self.handles.get_mut(id)?;
        assert!(matches!(handle, Handle::Producer));

        let buf = unsafe {
            core::slice::from_raw_parts(
                (events.as_ptr()) as *const u8,
                events.len() * size_of::<Event>(),
            )
        };

        if let Some(active_vt) = self.active_vt {
            for handle in self.handles.values_mut() {
                match handle {
                    Handle::Consumer {
                        pending,
                        notified,
                        vt,
                        ..
                    } => {
                        if *vt != active_vt {
                            continue;
                        }

                        pending.extend_from_slice(buf);
                        *notified = false;
                    }
                    _ => continue,
                }
            }
        }

        Ok(buf.len())
    }

    fn on_sendfd(
        &mut self,
        sendfd_request: &redox_scheme::SendFdRequest,
    ) -> syscall::Result<usize> {
        let handle = self.handles.get_mut(sendfd_request.id())?;

        let (device, is_earlyfb) = match handle {
            Handle::SchemeRoot | Handle::Producer | Handle::Consumer { .. } | Handle::Control => {
                return Err(SysError::new(EOPNOTSUPP))
            }
            Handle::Display {
                device,
                device_control,
                is_earlyfb,
                ..
            } => {
                let mut new_fds = [usize::MAX];
                sendfd_request.obtain_fd(self.socket, FobtainFdFlags::UPPER_TBL, &mut new_fds)?;
                *device_control = Some(Fd::new(new_fds[0]));
                (device.clone(), *is_earlyfb)
            }
        };

        let needs_handoff = match is_earlyfb {
            true => matches!(self.active_display, ActiveDisplay::Unknown),
            false => matches!(
                self.active_display,
                ActiveDisplay::Unknown | ActiveDisplay::Early { .. }
            ),
        };

        if needs_handoff {
            self.has_new_events = true;
            self.active_display = if is_earlyfb {
                ActiveDisplay::Early {
                    name: device,
                    id: sendfd_request.id(),
                }
            } else {
                ActiveDisplay::Regular {
                    name: device,
                    id: sendfd_request.id(),
                }
            };

            for handle in self.handles.values_mut() {
                match handle {
                    Handle::Consumer {
                        needs_handoff,
                        notified,
                        ..
                    } => {
                        *needs_handoff = true;
                        *notified = false;
                    }
                    _ => continue,
                }
            }
        }

        if let Some(vt) = self.active_vt {
            self.pending_activate = Some(vt);
        }

        Ok(0)
    }

    fn fevent(
        &mut self,
        id: usize,
        flags: syscall::EventFlags,
        _ctx: &CallerCtx,
    ) -> syscall::Result<syscall::EventFlags> {
        match self.handles.get_mut(id)? {
            Handle::Consumer {
                ref mut events,
                ref mut notified,
                ..
            } => {
                *events = flags;
                *notified = false;
                Ok(EventFlags::empty())
            }
            Handle::Display { ref mut events, .. } => {
                *events = flags;
                Ok(EventFlags::empty())
            }
            Handle::Producer | Handle::Control => {
                log::error!("producer or control tried to use an event queue");
                Err(SysError::new(EINVAL))
            }
            Handle::SchemeRoot => Err(SysError::new(EBADF)),
        }
    }

    fn on_close(&mut self, id: usize) {
        if let Handle::Consumer { vt, .. } = self.handles.remove(id).unwrap() {
            self.vts.remove(&vt);
            if self.active_vt == Some(vt) {
                if let Some(&new_vt) = self.vts.last() {
                    self.switch_vt(new_vt);
                } else {
                    self.active_vt = None;
                }
            }
        }
    }
}

fn daemon(daemon: daemon::SchemeDaemon) -> anyhow::Result<()> {
    // Create the ":input" scheme.
    let socket_file = Socket::create()?;
    let mut scheme = InputScheme::new(&socket_file);
    let mut handler = Blocking::new(&socket_file, 16);

    let _ = daemon.ready_sync_scheme(handler.socket(), &mut scheme);

    loop {
        scheme.has_new_events = false;
        match handler.process_requests_nonblocking(&mut scheme)? {
            ControlFlow::Continue(()) => {}
            ControlFlow::Break(()) => unreachable!("scheme should be blocking"),
        }

        if let Some(vt) = scheme.pending_activate.take() {
            scheme.send_vt_event_to_active_display(VtEvent {
                kind: VtEventKind::Activate,
                vt,
            });
        }

        if !scheme.has_new_events {
            continue;
        }

        for (id, handle) in scheme.handles.iter_mut() {
            match handle {
                Handle::Consumer {
                    events,
                    pending,
                    needs_handoff,
                    ref mut notified,
                    ..
                } => {
                    if (!*needs_handoff && pending.is_empty())
                        || *notified
                        || !events.contains(EventFlags::EVENT_READ)
                    {
                        continue;
                    }

                    // Notify the consumer that we have some events to read. Yum yum.
                    handler.socket().write_response(
                        Response::post_fevent(*id, EventFlags::EVENT_READ.bits()),
                        SignalBehavior::Restart,
                    )?;

                    *notified = true;
                }
                _ => {}
            }
        }
    }
}

fn daemon_runner(redox_daemon: daemon::SchemeDaemon) -> ! {
    daemon(redox_daemon).unwrap();
    unreachable!();
}

const HELP: &str = r#"
inputd [-K keymap|-A vt|--keymaps]
   -A vt       : set current virtual display
   -K keymap   : set keyboard mapping
   --keymaps   : list available keyboard mappings
"#;

fn main() {
    let mut args = std::env::args().skip(1);

    if let Some(val) = args.next() {
        // TODO: Get current VT or keymap
        match val.as_ref() {
            // Activates a VT.
            "-A" => {
                let vt = args.next().unwrap().parse::<usize>().unwrap();

                let mut handle =
                    inputd::ControlHandle::new().expect("inputd: failed to open control handle");
                handle
                    .activate_vt(vt)
                    .expect("inputd: failed to activate VT");
            }
            // Activates a keymap.
            "-K" => {
                let arg = if let Some(a) = args.next() {
                    a
                } else {
                    eprintln!("Error: Option -K requires a layout argument.");
                    std::process::exit(1);
                };

                let vt: KeymapKind = arg.to_ascii_lowercase().parse().unwrap_or_else(|_| {
                    eprintln!("inputd: unrecognized keymap code (see: inputd --keymaps)");
                    std::process::exit(1);
                });

                let mut handle =
                    inputd::ControlHandle::new().expect("inputd: failed to open control handle");
                handle
                    .activate_keymap(vt as usize)
                    .expect("inputd: failed to activate keymap");
            }
            // List available keymaps
            "--keymaps" => {
                // TODO: configurable KeymapKind using files
                for key in ["dvorak", "us", "gb", "azerty", "bepo", "it"] {
                    println!("{}", key);
                }
            }
            "--help" => {
                println!("{}", HELP);
            }

            _ => panic!("inputd: invalid argument: {}", val),
        }
    } else {
        common::setup_logging(
            "input",
            "inputd",
            "inputd",
            common::output_level(),
            common::file_level(),
        );

        daemon::SchemeDaemon::new(daemon_runner);
    }
}
