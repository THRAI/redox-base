use std::convert::TryFrom;
use std::mem;
use std::ops::ControlFlow;
use std::sync::Arc;

use ::acpi::aml::op_region::{RegionHandler, RegionSpace};
use event::{EventFlags, RawEventQueue};
use redox_scheme::{scheme::register_sync_scheme, Socket};
use scheme_utils::Blocking;

mod acpi;
mod aml_physmem;
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
mod ec;

mod scheme;

use libredox::Fd;
use syscall::flag::{AcpiVerb, CallFlags};

fn daemon(daemon: daemon::Daemon) -> ! {
    common::setup_logging(
        "misc",
        "acpi",
        "acpid",
        common::output_level(),
        common::file_level(),
    );

    log::info!("acpid start");

    let kernel_acpi_handle = Fd::open("/scheme/kernel.acpi", libredox::flag::O_CLOEXEC, 0)
        .expect("acpid: failed to open kernel ACPI handle");

    let rxsdt_raw_data: Arc<[u8]> = {
        let len = kernel_acpi_handle
            .call_ro(&mut [], CallFlags::READ, &[AcpiVerb::ReadRxsdt as u64])
            .expect("acpid: failed to get rxsdt length");
        let mut buf = vec![0_u8; len];
        kernel_acpi_handle
            .call_ro(&mut buf, CallFlags::READ, &[AcpiVerb::ReadRxsdt as u64])
            .expect("acpid: failed to read rxsdt");
        buf.into()
    };

    if rxsdt_raw_data.is_empty() {
        log::info!("System doesn't use ACPI");
        daemon.ready();
        std::process::exit(0);
    }

    let sdt = self::acpi::Sdt::new(rxsdt_raw_data).expect("acpid: failed to parse [RX]SDT");

    let mut thirty_two_bit;
    let mut sixty_four_bit;

    let physaddrs_iter = match &sdt.signature {
        b"RSDT" => {
            thirty_two_bit = sdt
                .data()
                .chunks(mem::size_of::<u32>())
                // TODO: With const generics, the compiler has some way of doing this for static sizes.
                .map(|chunk| <[u8; mem::size_of::<u32>()]>::try_from(chunk).unwrap())
                .map(|chunk| u32::from_le_bytes(chunk))
                .map(u64::from);

            &mut thirty_two_bit as &mut dyn Iterator<Item = u64>
        }
        b"XSDT" => {
            sixty_four_bit = sdt
                .data()
                .chunks(mem::size_of::<u64>())
                .map(|chunk| <[u8; mem::size_of::<u64>()]>::try_from(chunk).unwrap())
                .map(|chunk| u64::from_le_bytes(chunk));

            &mut sixty_four_bit as &mut dyn Iterator<Item = u64>
        }
        _ => panic!("acpid: expected [RX]SDT from kernel to be either of those"),
    };

    let region_handlers: Vec<(RegionSpace, Box<dyn RegionHandler + 'static>)> = vec![
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        (RegionSpace::EmbeddedControl, Box::new(ec::Ec::new())),
    ];
    let acpi_context = self::acpi::AcpiContext::init(physaddrs_iter, region_handlers);

    // TODO: I/O permission bitmap?
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    common::acquire_port_io_rights().expect("acpid: failed to set I/O privilege level to Ring 3");

    let shutdown_pipe = kernel_acpi_handle
        .openat("kstop", libredox::flag::O_CLOEXEC, 0)
        .expect("acpid: failed to open kstop handle");

    let mut event_queue = RawEventQueue::new().expect("acpid: failed to create event queue");
    let socket = Socket::nonblock().expect("acpid: failed to create disk scheme");

    let mut scheme = self::scheme::AcpiScheme::new(&acpi_context, &socket);
    let mut handler = Blocking::new(&socket, 16);

    event_queue
        .subscribe(shutdown_pipe.raw() as usize, 0, EventFlags::READ)
        .expect("acpid: failed to register shutdown pipe for event queue");
    event_queue
        .subscribe(socket.inner().raw(), 1, EventFlags::READ)
        .expect("acpid: failed to register scheme socket for event queue");

    register_sync_scheme(&socket, "acpi", &mut scheme)
        .expect("acpid: failed to register acpi scheme to namespace");

    libredox::call::setrens(0, 0).expect("acpid: failed to enter null namespace");

    daemon.ready();
    log::info!("acpid ready");

    let mut mounted = true;
    while mounted {
        let Some(event) = event_queue
            .next()
            .transpose()
            .expect("acpid: failed to read event file")
        else {
            break;
        };

        if event.fd == socket.inner().raw() {
            loop {
                match handler
                    .process_requests_nonblocking(&mut scheme)
                    .expect("acpid: failed to process requests")
                {
                    ControlFlow::Continue(()) => {}
                    ControlFlow::Break(()) => break,
                }
            }
        } else if event.fd == shutdown_pipe.raw() {
            if shutdown_pipe
                .call_ro(
                    &mut [],
                    CallFlags::empty(),
                    &[AcpiVerb::CheckShutdown as u64],
                )
                .expect("acpid: failed to get shutdown status")
                == 0
            {
                continue;
            }
            log::info!("Received shutdown request from kernel.");
            mounted = false;
        } else {
            log::debug!("Received request to unknown fd: {}", event.fd);
            continue;
        }
    }

    drop(shutdown_pipe);
    drop(event_queue);

    acpi_context.set_global_s_state(5);

    unreachable!("System should have shut down before this is entered");
}

fn main() {
    common::init();
    daemon::Daemon::new(daemon);
}
