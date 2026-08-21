use std::rc::Rc;

use redox_scheme::Socket;
use scheme_utils::Blocking;

use crate::scheme::{LogResource, LogScheme, LogSchemeData, SchemeRoot};

mod scheme;

fn daemon(daemon: daemon::SchemeDaemon) -> ! {
    let socket = Rc::new(Socket::create().expect("logd: failed to create log scheme"));

    let mut scheme = LogScheme::new(
        "log".to_owned(),
        LogSchemeData::new(socket.clone()),
        LogResource::SchemeRoot(SchemeRoot),
    );
    let handler = Blocking::new(&*socket, 16);

    let _ = daemon.ready_sync_scheme(&socket, &mut scheme);

    libredox::call::setrens(0, 0).expect("logd: failed to enter null namespace");

    handler
        .process_requests_blocking(scheme)
        .expect("logd: failed to process requests");
}

fn main() {
    daemon::SchemeDaemon::new(daemon);
}
