use std::env;
use std::os::fd::AsRawFd;

use event::{user_data, EventQueue};
use inputd::{ConsumerHandle, ConsumerHandleEvent};
use orbclient::Event;
use redox_scheme::{Response, SignalBehavior, Socket};
use scheme_utils::ReadinessBased;
use syscall::EVENT_READ;

use crate::scheme::{FbconResource, FbconScheme, FbconSchemeData, SchemeRoot};

mod scheme;
mod text;

user_data! {
    enum Source {
        Scheme,
        Vt,
    }
}

fn main() {
    daemon::SchemeDaemon::new(daemon);
}
fn daemon(daemon: daemon::SchemeDaemon) -> ! {
    let vt_id = env::args().skip(1).next().unwrap();

    common::setup_logging(
        "graphics",
        "fbcond",
        "fbcond",
        common::output_level(),
        common::file_level(),
    );

    let event_queue = EventQueue::new().expect("fbcond: failed to create event queue");

    let socket = Socket::nonblock().expect("fbcond: failed to create fbcon scheme");
    event_queue
        .subscribe(
            socket.inner().raw(),
            Source::Scheme,
            event::EventFlags::READ,
        )
        .expect("fbcond: failed to subscribe to scheme events");

    let input_handle = ConsumerHandle::new_vt().expect("Failed to open display for vt");
    event_queue
        .subscribe(
            input_handle.event_handle().as_raw_fd() as usize,
            Source::Vt,
            event::EventFlags::READ,
        )
        .expect("Failed to subscribe to input events for vt");

    let mut scheme = FbconScheme::new(
        format!("fbcon.{vt_id}"),
        FbconSchemeData::new(input_handle),
        FbconResource::SchemeRoot(SchemeRoot),
    );
    let mut readiness = ReadinessBased::new(Box::new(socket), 16);

    let _ = daemon.ready_sync_scheme(readiness.socket(), &mut scheme);

    libredox::call::setns(0).expect("fbcond: failed to enter null namespace");

    // Handle all events that could have happened before registering with the event queue.
    handle_event(&mut scheme, &mut readiness, Source::Scheme);
    handle_event(&mut scheme, &mut readiness, Source::Vt);

    for event in event_queue {
        let event = event.expect("fbcond: failed to read event from event queue");
        handle_event(&mut scheme, &mut readiness, event.user_data);
    }

    std::process::exit(0);
}

fn handle_event(
    scheme: &mut FbconScheme,
    readiness: &mut ReadinessBased<Box<Socket>>,
    event: Source,
) {
    match event {
        Source::Scheme => {
            readiness
                .read_and_process_requests(scheme)
                .expect("fbcond: failed to read from socket");
        }
        Source::Vt => {
            let vt = &mut scheme.scheme_data_mut().console;

            let mut events = [Event::new(); 16];
            loop {
                match vt
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

    let (handles, scheme_data) = scheme.handles_mut_and_scheme_data();
    for (handle_id, handle) in handles {
        let handle = match handle {
            FbconResource::SchemeRoot(SchemeRoot) => continue,
            FbconResource::Vt(handle) => handle,
        };

        if !handle.events.contains(EVENT_READ) {
            continue;
        }

        if scheme_data.console.can_read() {
            if !handle.notified_read {
                handle.notified_read = true;
                let response = Response::post_fevent(*handle_id, EVENT_READ.bits());
                readiness
                    .socket()
                    .write_response(response, SignalBehavior::Restart)
                    .expect("fbcond: failed to write display event");
            }
        } else {
            handle.notified_read = false;
        }
    }
}
