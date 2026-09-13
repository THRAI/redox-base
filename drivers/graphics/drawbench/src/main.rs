use std::thread;
use std::time::{Duration, Instant};

use drm::buffer::DrmFourcc;
use drm::control::connector::State;
use drm::control::{ClipRect, Device};
use graphics_ipc::{CpuBackedBuffer, DrmHandle};
use inputd::ConsumerHandle;

fn main() {
    let input_handle = ConsumerHandle::new_vt().unwrap();
    let display_handle = DrmHandle::from_file(input_handle.open_display().unwrap()).unwrap();

    let connector_info = display_handle
        .resource_handles()
        .unwrap()
        .connectors()
        .iter()
        .find_map(|&connector| {
            let info = display_handle.get_connector(connector, true).unwrap();
            if info.state() == State::Connected {
                Some(info)
            } else {
                None
            }
        })
        .expect("no connected display");

    let mode = connector_info.modes()[0];
    let (width, height) = mode.size();
    let encoder = connector_info.encoders()[0];

    let possible_crtcs = display_handle
        .get_encoder(encoder)
        .unwrap()
        .possible_crtcs();
    let crtc = display_handle
        .resource_handles()
        .unwrap()
        .filter_crtcs(possible_crtcs)[0];

    let mut buffer = CpuBackedBuffer::new(
        &display_handle,
        (width.into(), height.into()),
        DrmFourcc::Argb8888,
        32,
    )
    .unwrap();
    let fb = display_handle
        .add_framebuffer(buffer.buffer(), 32, 32)
        .unwrap();

    display_handle
        .set_crtc(
            crtc,
            Some(fb),
            (0, 0),
            &[connector_info.handle()],
            Some(mode),
        )
        .unwrap();

    loop {
        let start = Instant::now();
        for _ in 0..100 {
            let damage = ClipRect::new(0, 0, 512, 512);
            buffer.sync_rect(
                u32::from(damage.x1()),
                u32::from(damage.y1()),
                u32::from(damage.x2() - damage.x1()),
                u32::from(damage.y2() - damage.y1()),
            );
            display_handle.dirty_framebuffer(fb, &[damage]).unwrap()
        }
        println!("100 frames took {:?}", start.elapsed());
        thread::sleep(Duration::from_millis(500));
    }
}
