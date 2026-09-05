use std::io;

use console_draw::V2DisplayMap;
use drm::buffer::Buffer;
use drm::control::Device;
use graphics_ipc::DrmHandle;
use inputd::ConsumerHandle;

pub struct Display {
    pub input_handle: ConsumerHandle,
    pub map: Option<V2DisplayMap>,
}

impl Display {
    pub fn open_new_vt() -> io::Result<Self> {
        let mut display = Self {
            input_handle: ConsumerHandle::new_vt()?,
            map: None,
        };

        display.reopen_for_handoff();

        Ok(display)
    }

    /// Re-open the display after a handoff.
    pub fn reopen_for_handoff(&mut self) {
        let display_file = match self.input_handle.open_display() {
            Ok(display_file) => display_file,
            Err(err) => {
                log::error!("fbcond: No display present yet: {err}");
                return;
            }
        };
        let new_display_handle = DrmHandle::from_file(display_file).unwrap();

        log::debug!("fbcond: Opened new display");

        match V2DisplayMap::new(new_display_handle) {
            Ok(map) => {
                log::debug!(
                    "fbcond: Mapped new display with size {}x{}",
                    map.buffer.buffer().size().0,
                    map.buffer.buffer().size().1,
                );
                self.map = Some(map)
            }
            Err(err) => {
                log::error!("fbcond: failed to map new display: {err}");
                return;
            }
        }
    }

    pub fn handle_resize(map: &mut V2DisplayMap) -> Option<drm::control::Mode> {
        let mode = match map
            .display_handle
            .get_connector(map.connector, false)
            .map(|info| info.modes()[0])
        {
            Ok(mode) => mode,
            Err(err) => {
                eprintln!("fbcond: failed to get display size: {}", err);
                return None;
            }
        };

        if (u32::from(mode.size().0), u32::from(mode.size().1)) != map.buffer.buffer().size() {
            return Some(mode);
        }

        None
    }
}
