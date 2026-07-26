use event::EventQueue;
use inputd::ConsumerHandleEvent;
use orbclient::Event;
use redox_scheme::{Response, SignalBehavior, Socket};
use scheme_utils::ReadinessBased;
use std::env;
use syscall::EVENT_READ;

use crate::scheme::{FbconScheme, Handle, VtIndex};

mod display;
mod scheme;
mod text;

fn main() {
    daemon::SchemeDaemon::new(daemon);
}
fn daemon(daemon: daemon::SchemeDaemon) -> ! {
    let vt_ids = env::args()
        .skip(1)
        .map(|arg| arg.parse().expect("invalid vt number"))
        .collect::<Vec<_>>();

    common::setup_logging(
        "graphics",
        "fbcond",
        "fbcond",
        common::output_level(),
        common::file_level(),
    );
    let mut event_queue = EventQueue::new().expect("fbcond: failed to create event queue");

    // FIXME listen for resize events from inputd and handle them

    let socket = Socket::nonblock().expect("fbcond: failed to create fbcon scheme");
    event_queue
        .subscribe(
            socket.inner().raw(),
            VtIndex::SCHEMA_SENTINEL,
            event::EventFlags::READ,
        )
        .expect("fbcond: failed to subscribe to scheme events");

    let mut scheme = FbconScheme::new(&vt_ids, &mut event_queue);
    let mut readiness = ReadinessBased::new(&socket, 16);

    let _ = daemon.ready_sync_scheme(&socket, &mut scheme);

    // This is not possible for now as fbcond needs to open new displays at runtime for graphics
    // driver handoff. In the future inputd may directly pass a handle to the display instead.
    // libredox::call::setrens(0, 0).expect("fbcond: failed to enter null namespace");

    // Handle all events that could have happened before registering with the event queue.
    handle_event(
        &socket,
        &mut scheme,
        &mut readiness,
        VtIndex::SCHEMA_SENTINEL,
    );
    for vt_i in scheme.vts.keys().copied().collect::<Vec<_>>() {
        handle_event(&socket, &mut scheme, &mut readiness, vt_i);
    }

    for event in event_queue {
        let event = event.expect("fbcond: failed to read event from event queue");
        handle_event(&socket, &mut scheme, &mut readiness, event.user_data);
    }

    std::process::exit(0);
}

fn handle_event(
    socket: &Socket,
    scheme: &mut FbconScheme,
    readiness: &mut ReadinessBased,
    event: VtIndex,
) {
    match event {
        VtIndex::SCHEMA_SENTINEL => {
            readiness
                .read_and_process_requests(scheme)
                .expect("fbcond: failed to read from socket");
        }
        vt_i => {
            let vt = scheme.vts.get_mut(&vt_i).unwrap();

            let mut events = [Event::new(); 16];
            loop {
                match vt
                    .display
                    .input_handle
                    .read_events(&mut events)
                    .expect("fbcond: Error while reading events")
                {
                    ConsumerHandleEvent::Events(&[]) => break,

                    ConsumerHandleEvent::Events(events) => {
                        for event in events {
                            vt.input(event)
                        }
                    }
                    ConsumerHandleEvent::Handoff => vt.handle_handoff(),
                }
            }
        }
    }

    readiness
        .poll_all_requests(scheme)
        .expect("fbcond: error occured in poll_all_requests");
    readiness
        .write_responses()
        .expect("fbcond: failed to write to socket");

    for (handle_id, handle) in scheme.handles.iter_mut() {
        let handle = match handle {
            Handle::SchemeRoot => continue,
            Handle::Vt(handle) => handle,
        };

        if !handle.events.contains(EVENT_READ) {
            continue;
        }

        let can_read = scheme
            .vts
            .get(&handle.vt_i)
            .map_or(false, |console| console.can_read());

        if can_read {
            if !handle.notified_read {
                handle.notified_read = true;
                let response = Response::post_fevent(*handle_id, EVENT_READ.bits());
                socket
                    .write_response(response, SignalBehavior::Restart)
                    .expect("fbcond: failed to write display event");
            }
        } else {
            handle.notified_read = false;
        }
    }
}
