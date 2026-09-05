use std::thread;
use std::time::{Duration, Instant};

use console_draw::V2DisplayMap;
use drm::control::ClipRect;
use graphics_ipc::DrmHandle;
use inputd::ConsumerHandle;

fn main() {
    let input_handle = ConsumerHandle::new_vt().unwrap();
    let display_handle = DrmHandle::from_file(input_handle.open_display().unwrap()).unwrap();
    let mut map = V2DisplayMap::new(display_handle).unwrap();

    loop {
        let start = Instant::now();
        for _ in 0..100 {
            map.dirty_fb(ClipRect::new(0, 0, 512, 512)).unwrap();
        }
        println!("100 frames took {:?}", start.elapsed());
        thread::sleep(Duration::from_millis(500));
    }
}
