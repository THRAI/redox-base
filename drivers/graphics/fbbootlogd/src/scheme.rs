use std::collections::VecDeque;
use std::io;

use console_draw::alacritty_terminal::grid::Scroll;
use console_draw::alacritty_terminal::term;
use console_draw::{TextScreen, V2DisplayMap};
use drm::buffer::Buffer;
use drm::control::Device;
use graphics_ipc::DrmHandle;
use inputd::ConsumerHandle;
use orbclient::{Event, EventOption};
use redox_scheme::CallerCtx;
use scheme_utils::{resource_scheme, FpathWriter, ResourceOpenResult, ResourceSync};
use syscall::schemev2::NewFdFlags;
use syscall::{Error, Result, ENOENT};

resource_scheme! {
    pub(crate) FbbootlogScheme<>;
    type SchemeData = FbbootlogSchemeData;

    pub(crate) enum FbbootlogResource {
        SchemeRoot(SchemeRoot),
        Log(Log),
    }
}

pub struct FbbootlogSchemeData {
    pub input_handle: ConsumerHandle,
    display_map: Option<V2DisplayMap>,
    text_screen: console_draw::TextScreen,
    shift: bool,
}

impl FbbootlogSchemeData {
    pub fn new() -> Self {
        let mut config = term::Config::default();
        config.scrolling_history = 1000;

        let mut scheme_data = Self {
            input_handle: ConsumerHandle::bootlog_vt().expect("fbbootlogd: Failed to open vt"),
            display_map: None,
            text_screen: console_draw::TextScreen::new(None, config),
            shift: false,
        };

        scheme_data.handle_handoff();

        scheme_data
    }

    pub fn handle_handoff(&mut self) {
        let new_display_handle = match self.input_handle.open_display() {
            Ok(display) => DrmHandle::from_file(display).unwrap(),
            Err(err) => {
                eprintln!("fbbootlogd: No display present yet: {err}");
                return;
            }
        };

        match V2DisplayMap::new(new_display_handle) {
            Ok(display_map) => self.display_map = Some(display_map),
            Err(err) => {
                eprintln!("fbbootlogd: failed to open display: {}", err);
                return;
            }
        };

        eprintln!("fbbootlogd: mapped display");
    }

    pub fn handle_input(&mut self, ev: &Event) {
        match ev.to_option() {
            EventOption::Key(key_event) => {
                if key_event.scancode == 0x2A || key_event.scancode == 0x36 {
                    self.shift = key_event.pressed;
                } else if !key_event.pressed || !self.shift {
                    return;
                }
                match key_event.scancode {
                    0x48 => {
                        // Up
                        self.text_screen.scroll_display(Scroll::Delta(1));
                    }
                    0x49 => {
                        // Page up
                        self.text_screen.scroll_display(Scroll::PageUp);
                    }
                    0x50 => {
                        // Down
                        self.text_screen.scroll_display(Scroll::Delta(-1));
                    }
                    0x51 => {
                        // Page down
                        self.text_screen.scroll_display(Scroll::PageDown);
                    }
                    0x47 => {
                        // Home
                        self.text_screen.scroll_display(Scroll::Bottom);
                    }
                    0x4F => {
                        // End
                        self.text_screen.scroll_display(Scroll::Top);
                    }
                    _ => return,
                }
            }
            _ => return,
        }
        if let Some(map) = &mut self.display_map {
            let damage = self.text_screen.write(map, &[], &mut VecDeque::new());
            map.dirty_fb(damage).unwrap();
        }
    }

    fn handle_resize(map: &mut V2DisplayMap, text_screen: &mut TextScreen) {
        let mode = match map
            .display_handle
            .get_connector(map.connector, false)
            .and_then(|info| {
                info.modes()
                    .get(0)
                    .map(|m| *m)
                    .ok_or(io::Error::other("Unable to get first display connector"))
            }) {
            Ok(mode) => mode,
            Err(err) => {
                eprintln!("fbbootlogd: failed to get display size: {}", err);
                return;
            }
        };

        if (u32::from(mode.size().0), u32::from(mode.size().1)) != map.buffer.buffer().size() {
            match text_screen.resize(map, mode) {
                Ok(()) => eprintln!("fbbootlogd: mapped display"),
                Err(err) => {
                    eprintln!("fbbootlogd: failed to create or map framebuffer: {}", err);
                    return;
                }
            }
        }
    }
}

#[derive(Debug)]
pub(crate) struct SchemeRoot;

impl ResourceSync for SchemeRoot {
    type ResourceEnum = FbbootlogResource;
    type SchemeData = FbbootlogSchemeData;

    fn openat<'a>(
        &mut self,
        _scheme_data: &mut Self::SchemeData,
        path: &str,
        _flags: usize,
        _fcntl_flags: u32,
        _ctx: &CallerCtx,
    ) -> Result<ResourceOpenResult<Self::ResourceEnum>> {
        if !path.is_empty() {
            return Err(Error::new(ENOENT));
        }

        Ok(ResourceOpenResult::ThisScheme {
            data: FbbootlogResource::Log(Log),
            flags: NewFdFlags::empty(),
        })
    }
}

#[derive(Debug)]
pub(crate) struct Log;

impl ResourceSync for Log {
    type ResourceEnum = FbbootlogResource;
    type SchemeData = FbbootlogSchemeData;

    fn fpath(&mut self, _scheme_data: &mut Self::SchemeData, _w: &mut FpathWriter) -> Result<()> {
        Ok(())
    }

    fn fsync(&mut self, _scheme_data: &mut Self::SchemeData) -> Result<()> {
        Ok(())
    }

    fn write(
        &mut self,
        scheme_data: &mut Self::SchemeData,
        buf: &[u8],
        _offset: u64,
        _fcntl_flags: u32,
    ) -> Result<usize> {
        if let Some(map) = &mut scheme_data.display_map {
            FbbootlogSchemeData::handle_resize(map, &mut scheme_data.text_screen);

            let damage = scheme_data
                .text_screen
                .write(map, buf, &mut VecDeque::new());
            map.dirty_fb(damage).unwrap();
        }

        Ok(buf.len())
    }
}
