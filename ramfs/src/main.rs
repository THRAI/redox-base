use std::{convert::TryInto, env, ops::ControlFlow};

mod filesystem;
mod scheme;

use event::{EventFlags, RawEventQueue};
use redox_rings::raw::RingPopError;
use scheme_utils::Blocking;

use crate::scheme::{Handle, RingState};
use redox_rings::op::FsOpCqe;

use self::scheme::Scheme;

fn main() {
    daemon::SchemeDaemon::new(daemon);
}

fn daemon(daemon: daemon::SchemeDaemon) -> ! {
    env_logger::init();

    let mut args = env::args().skip(1);
    let mut scheme_name = None;
    let mut root_mode = 0o755;

    while let Some(arg) = args.next() {
        if arg == "-p" {
            let mode_str = args.next().expect("ramfs: expected mode after -p");
            let clean_str = mode_str.trim_start_matches("0o");
            root_mode = u16::from_str_radix(clean_str, 8).expect("ramfs: invalid octal mode");
        } else if scheme_name.is_none() {
            scheme_name = Some(arg);
        } else {
            panic!("Usage:\n\tramfs [-p 0oMODE] SCHEME_NAME");
        }
    }

    let scheme_name = scheme_name.expect("Usage:\n\tramfs [-p 0oMODE] SCHEME_NAME");

    if scheme_name.starts_with("tmp") {
        // TODO: move this to service definition?
        root_mode = 0o1777;
    }

    let socket = redox_scheme::Socket::nonblock().expect("ramfs: failed to create socket");

    let event_queue = RawEventQueue::new().unwrap();
    event_queue
        .subscribe(socket.inner().raw(), 0, EventFlags::READ)
        .unwrap();

    let mut scheme = Scheme::new(&socket, scheme_name.clone(), &event_queue, root_mode)
        .expect("ramfs: failed to initialize scheme");

    let mut handler = Blocking::new(&socket, 16);

    let _ = daemon.ready_sync_scheme(&socket, &mut scheme);

    libredox::call::setrens(0, 0).expect("ramfs: failed to enter null namespace");

    loop {
        let event = event_queue.next_event().unwrap();

        if event.user_data == 0 {
            match handler.process_requests_nonblocking(&mut scheme).unwrap() {
                ControlFlow::Continue(()) => {}
                ControlFlow::Break(()) => {
                    // Spurious event.
                    continue;
                }
            }
        } else {
            let Ok(handle) = scheme.handles.get_mut(event.user_data) else {
                continue;
            };

            let Handle::Ring(RingState::Active {
                sq,
                cq,
                shm,
                fixed_ftbl,
                ..
            }) = handle
            else {
                unreachable!("only uring events are subscribed to the event queue");
            };

            loop {
                let (user_data, res) = match sq.try_pop() {
                    Ok(sqe) => (
                        sqe.user_data,
                        scheme::handle_ring_req(shm, fixed_ftbl, &mut scheme.filesystem, sqe),
                    ),
                    Err(RingPopError::Empty) => break,
                    Err(RingPopError::Broken) => {
                        unreachable!("uring is in an inconsistent state")
                    }
                };

                cq.push(
                    FsOpCqe {
                        user_data,
                        res: match res {
                            Ok(val) => val.try_into().unwrap(),
                            Err(err) => err.errno,
                        },
                        pad: 0,
                    },
                    None,
                )
                .unwrap();
            }
        }
    }
}
