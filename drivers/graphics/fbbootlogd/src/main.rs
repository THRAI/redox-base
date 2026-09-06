//! Fbbootlogd renders the boot log and presents it on VT1.
//!
//! In the future it could display a boot splash like plymouth instead of a boot log when booting
//! in quiet mode.

use std::io::{self, Read};
use std::os::fd::AsRawFd;

use event::EventQueue;
use inputd::ConsumerHandleEvent;
use orbclient::Event;

use crate::scheme::FbbootlogSchemeData;

mod scheme;

fn main() {
    daemon::Daemon::new(daemon);
}
fn daemon(daemon: daemon::Daemon) -> ! {
    let event_queue = EventQueue::new().expect("fbbootlogd: failed to create event queue");

    event::user_data! {
        enum Source {
            LogPipe,
            Input,
        }
    }

    let (log_reader, log_writer) = io::pipe().expect("fbbootlogd: failed to create pipe");
    if unsafe { libc::fcntl(log_reader.as_raw_fd(), libc::F_SETFL, libc::O_NONBLOCK) != 0 } {
        panic!(
            "fbbootlogd: failed to set pipe as nonblocking: {}",
            io::Error::last_os_error()
        )
    };

    let mut scheme = FbbootlogSchemeData::new(log_reader);

    event_queue
        .subscribe(
            scheme.log_reader.as_raw_fd() as usize,
            Source::LogPipe,
            event::EventFlags::READ,
        )
        .expect("fbbootlogd: failed to subscribe to log pipe events");

    event_queue
        .subscribe(
            scheme.input_handle.event_handle().as_raw_fd() as usize,
            Source::Input,
            event::EventFlags::READ,
        )
        .expect("fbbootlogd: failed to subscribe to input events");

    {
        // Add ourself as log sink
        let log_file = libredox::Fd::open(
            "/scheme/log/add_sink",
            libredox::flag::O_WRONLY | libredox::flag::O_CLOEXEC,
            0,
        )
        .expect("fbbootlogd: failed to open log/add_sink");
        log_file
            .call_wo(
                &(log_writer.as_raw_fd() as usize).to_ne_bytes(),
                syscall::CallFlags::FD,
                &[],
            )
            .expect("fbbootlogd: failed to send log fd to log scheme.");
    }

    let _ = daemon.ready();

    libredox::call::setns(0).expect("fbbootlogd: failed to enter null namespace");

    for event in event_queue {
        match event.expect("fbbootlogd: failed to get event").user_data {
            Source::LogPipe => loop {
                let mut buf = [0; 4096];
                let n = match scheme.log_reader.read(&mut buf) {
                    Ok(n) => n,
                    Err(e) if e.raw_os_error() == Some(libc::EAGAIN) => break,
                    Err(e) => panic!("fbbootlogd: failed to read from log pipe: {e}"),
                };
                scheme.handle_logs(&buf[..n]);
            },
            Source::Input => {
                let mut events = [Event::new(); 16];
                loop {
                    match scheme
                        .input_handle
                        .read_events(&mut events)
                        .expect("fbbootlogd: error while reading events")
                    {
                        ConsumerHandleEvent::Events(&[]) => break,
                        ConsumerHandleEvent::Events(events) => {
                            for event in events {
                                scheme.handle_input(&event);
                            }
                        }
                        ConsumerHandleEvent::Handoff => {
                            eprintln!("fbbootlogd: handoff requested");
                            scheme.handle_handoff();
                        }
                    }
                }
            }
        }
    }

    std::process::exit(0);
}
