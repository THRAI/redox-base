use std::collections::VecDeque;
use std::os::fd::AsRawFd;

use console_draw::alacritty_terminal::term;
use console_draw::V2DisplayMap;
use event::EventQueue;
use graphics_ipc::DrmHandle;
use inputd::ConsumerHandle;
use orbclient::{Event, EventOption};
use syscall::error::*;

use crate::Source;

pub struct TextScreen {
    pub input_handle: ConsumerHandle,
    map: Option<V2DisplayMap>,
    inner: console_draw::TextScreen,
    ctrl: bool,
    input: VecDeque<u8>,
}

impl TextScreen {
    pub fn new(
        input_handle: ConsumerHandle,
        event_queue: &EventQueue<Source>,
        font: Option<console_draw::ConsoleFont>,
    ) -> TextScreen {
        let mut text_screen = TextScreen {
            input_handle,
            map: None,
            inner: console_draw::TextScreen::new(font, term::Config::default()),
            ctrl: false,
            input: VecDeque::new(),
        };
        text_screen.handle_handoff(event_queue);
        text_screen
    }

    pub fn handle_handoff(&mut self, event_queue: &EventQueue<Source>) {
        log::info!("fbcond: Performing handoff");

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
            Ok(mut map) => match self.inner.handle_handoff(&mut map) {
                Ok(()) => {
                    if let Some(old_map) = &self.map {
                        event_queue
                            .unsubscribe(old_map.event_handle().as_raw_fd() as usize)
                            .expect("fbcond: failed to unsubscribe from old drm events");
                    }
                    event_queue
                        .subscribe(
                            map.event_handle().as_raw_fd() as usize,
                            Source::DisplayHandle,
                            event::EventFlags::READ,
                        )
                        .expect("fbcond: failed to subscribe to drm events");
                    self.map = Some(map);
                }
                Err(err) => {
                    eprintln!("fbcond: failed to handle handoff: {err}");
                    return;
                }
            },
            Err(err) => {
                eprintln!("fbcond: failed to open display: {}", err);
                return;
            }
        };
    }

    pub fn handle_display_event(&mut self) {
        if let Some(map) = &mut self.map {
            match self.inner.handle_display_event(map) {
                Ok(()) => {}
                Err(err) => {
                    eprintln!("fbcond: failed to create or map framebuffer: {}", err);
                    return;
                }
            }
        }
    }

    pub fn input(&mut self, event: &Event) {
        let mut buf = vec![];

        match event.to_option() {
            EventOption::Key(key_event) => {
                if key_event.scancode == 0x1D {
                    self.ctrl = key_event.pressed;
                } else if key_event.pressed {
                    match key_event.scancode {
                        0x0E => {
                            // Backspace
                            buf.extend_from_slice(b"\x7F");
                        }
                        0x1C => {
                            // Newline
                            buf.extend_from_slice(b"\n");
                        }
                        0x47 => {
                            // Home
                            buf.extend_from_slice(b"\x1B[H");
                        }
                        0x48 => {
                            // Up
                            buf.extend_from_slice(b"\x1B[A");
                        }
                        0x49 => {
                            // Page up
                            buf.extend_from_slice(b"\x1B[5~");
                        }
                        0x4B => {
                            // Left
                            buf.extend_from_slice(b"\x1B[D");
                        }
                        0x4D => {
                            // Right
                            buf.extend_from_slice(b"\x1B[C");
                        }
                        0x4F => {
                            // End
                            buf.extend_from_slice(b"\x1B[F");
                        }
                        0x50 => {
                            // Down
                            buf.extend_from_slice(b"\x1B[B");
                        }
                        0x51 => {
                            // Page down
                            buf.extend_from_slice(b"\x1B[6~");
                        }
                        0x52 => {
                            // Insert
                            buf.extend_from_slice(b"\x1B[2~");
                        }
                        0x53 => {
                            // Delete
                            buf.extend_from_slice(b"\x1B[3~");
                        }
                        _ => {
                            let c = match key_event.character {
                                c @ 'A'..='Z' if self.ctrl => ((c as u8 - b'A') + b'\x01') as char,
                                c @ 'a'..='z' if self.ctrl => ((c as u8 - b'a') + b'\x01') as char,
                                c => c,
                            };

                            if c != '\0' {
                                let mut b = [0; 4];
                                buf.extend_from_slice(c.encode_utf8(&mut b).as_bytes());
                            }
                        }
                    }
                }
            }
            _ => (), //TODO: Mouse in terminal
        }

        self.input.extend(buf);
    }

    pub fn can_read(&self) -> bool {
        !self.input.is_empty()
    }
}

impl TextScreen {
    pub fn read(&mut self, buf: &mut [u8]) -> Result<usize> {
        let mut i = 0;

        while i < buf.len() && !self.input.is_empty() {
            buf[i] = self.input.pop_front().unwrap();
            i += 1;
        }

        Ok(i)
    }

    pub fn write(&mut self, buf: &[u8]) -> Result<usize> {
        if let Some(map) = &mut self.map {
            self.inner.write(map, buf, &mut self.input);
        }

        Ok(buf.len())
    }
}
