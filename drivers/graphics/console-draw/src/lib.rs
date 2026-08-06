use std::collections::VecDeque;
use std::convert::TryFrom;
use std::{cmp, io, mem, ptr};

use drm::buffer::{Buffer, DrmFourcc};
use drm::control::{connector, crtc, framebuffer, ClipRect, Device, Mode};
use graphics_ipc::{CpuBackedBuffer, V2GraphicsHandle};

use orbclient::FONT;

#[derive(Debug, Copy, Clone)]
pub struct Damage {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Damage {
    pub const NONE: Self = Damage {
        x: 0,
        y: 0,
        width: 0,
        height: 0,
    };

    pub fn merge(self, other: Self) -> Self {
        if self.width == 0 || self.height == 0 {
            return other;
        }

        if other.width == 0 || other.height == 0 {
            return self;
        }

        let x = cmp::min(self.x, other.x);
        let y = cmp::min(self.y, other.y);
        let x2 = cmp::max(self.x + self.width, other.x + other.width);
        let y2 = cmp::max(self.y + self.height, other.y + other.height);

        Damage {
            x,
            y,
            width: x2 - x,
            height: y2 - y,
        }
    }
}

pub struct V2DisplayMap {
    pub display_handle: V2GraphicsHandle,
    pub connector: connector::Handle,
    crtc: crtc::Handle,
    fb: framebuffer::Handle,
    pub buffer: CpuBackedBuffer,
}

impl V2DisplayMap {
    pub fn new(display_handle: V2GraphicsHandle) -> io::Result<Self> {
        let connector_info = display_handle.first_display()?;

        let Some(&mode) = connector_info.modes().get(0) else {
            return Err(io::Error::other("Unable to get first display connector"));
        };
        let (width, height) = mode.size();
        let Some(&encoder) = connector_info.encoders().get(0) else {
            return Err(io::Error::other("Unable to get first display encoder"));
        };

        // FIXME do something smarter that avoids conflicts
        let Some(&crtc) = display_handle
            .resource_handles()
            .unwrap()
            .filter_crtcs(display_handle.get_encoder(encoder)?.possible_crtcs())
            .get(0)
        else {
            return Err(io::Error::other("Unable to get first display ctrc"));
        };

        let buffer = CpuBackedBuffer::new(
            &display_handle,
            (width.into(), height.into()),
            DrmFourcc::Argb8888,
            32,
        )?;
        let fb = display_handle.add_framebuffer(buffer.buffer(), 32, 32)?;

        display_handle.set_crtc(
            crtc,
            Some(fb),
            (0, 0),
            &[connector_info.handle()],
            Some(mode),
        )?;

        Ok(Self {
            display_handle,
            connector: connector_info.handle(),
            crtc,
            fb,
            buffer,
        })
    }

    unsafe fn console_map(&mut self) -> DisplayMap {
        let size = self.buffer.buffer().size();
        let shadow_buf = self.buffer.shadow_buf();

        DisplayMap {
            offscreen: ptr::slice_from_raw_parts_mut(
                shadow_buf.as_mut_ptr() as *mut u32,
                shadow_buf.len() / 4,
            ),
            width: size.0 as usize,
            height: size.1 as usize,
        }
    }

    pub fn dirty_fb(&mut self, damage: Damage) -> io::Result<()> {
        self.buffer
            .sync_rect(damage.x, damage.y, damage.width, damage.height);

        self.display_handle.dirty_framebuffer(
            self.fb,
            &[ClipRect::new(
                damage.x as u16,
                damage.y as u16,
                (damage.x + damage.width) as u16,
                (damage.y + damage.height) as u16,
            )],
        )
    }
}

struct DisplayMap {
    offscreen: *mut [u32],
    width: usize,
    height: usize,
}

#[derive(Clone)]
pub struct ConsoleFont {
    glyphs: Vec<u8>,
    width: usize,
    height: usize,
}

impl ConsoleFont {
    pub fn new(glyphs: Vec<u8>, width: usize, height: usize) -> ConsoleFont {
        ConsoleFont {
            glyphs,
            width,
            height,
        }
    }

    pub fn from_psf(data: &[u8]) -> ConsoleFont {
        let font = psf_rs::Font::load(data);

        let width = font.header.glyph_width as usize;
        let height = font.header.glyph_height as usize;
        let bytes_per_row = (width + 7) / 8;
        let glyph_count = font.header.length as usize;

        let mut glyphs = vec![0u8; glyph_count * height * bytes_per_row];

        for i in 0..glyph_count {
            if let Some(c) = char::from_u32(i as u32) {
                let glyph_offset = i * height * bytes_per_row;

                font.display_glyph(c, |bit, x, y| {
                    if bit != 0 {
                        let byte_offset =
                            glyph_offset + (y as usize) * bytes_per_row + ((x as usize) / 8);
                        let bit_offset = 7 - (x % 8);
                        if byte_offset < glyphs.len() {
                            glyphs[byte_offset] |= 1 << bit_offset;
                        }
                    }
                });
            }
        }

        Self {
            glyphs,
            width,
            height,
        }
    }
}

pub struct TextScreen {
    console: ransid::Console,
    font: ConsoleFont,
}

impl TextScreen {
    pub fn new(font: Option<ConsoleFont>) -> TextScreen {
        TextScreen {
            // Width and height will be filled in on the next write to the console
            console: ransid::Console::new(0, 0),
            font: font.unwrap_or_else(|| ConsoleFont::new(FONT.to_vec(), 8, 16)),
        }
    }

    /// Draw a rectangle
    fn rect(map: &mut DisplayMap, x: usize, y: usize, w: usize, h: usize, color: u32) {
        let start_y = cmp::min(map.height, y);
        let end_y = cmp::min(map.height, y + h);

        let start_x = cmp::min(map.width, x);
        let len = cmp::min(map.width, x + w) - start_x;

        let mut offscreen_ptr = map.offscreen as *mut u8 as usize;

        let stride = map.width * 4;

        let offset = y * stride + start_x * 4;
        offscreen_ptr += offset;

        let mut rows = end_y - start_y;
        while rows > 0 {
            for i in 0..len {
                unsafe {
                    *(offscreen_ptr as *mut u32).add(i) = color;
                }
            }
            offscreen_ptr += stride;
            rows -= 1;
        }
    }

    /// Invert a rectangle
    fn invert(map: &mut DisplayMap, x: usize, y: usize, w: usize, h: usize) {
        let start_y = cmp::min(map.height, y);
        let end_y = cmp::min(map.height, y + h);

        let start_x = cmp::min(map.width, x);
        let len = cmp::min(map.width, x + w) - start_x;

        let mut offscreen_ptr = map.offscreen as *mut u8 as usize;

        let stride = map.width * 4;

        let offset = y * stride + start_x * 4;
        offscreen_ptr += offset;

        let mut rows = end_y - start_y;
        while rows > 0 {
            let mut row_ptr = offscreen_ptr;
            let mut cols = len;
            while cols > 0 {
                unsafe {
                    let color = *(row_ptr as *mut u32);
                    *(row_ptr as *mut u32) = !color;
                }
                row_ptr += 4;
                cols -= 1;
            }
            offscreen_ptr += stride;
            rows -= 1;
        }
    }

    /// Draw a character
    fn char(
        map: &mut DisplayMap,
        x: usize,
        y: usize,
        character: char,
        font: &ConsoleFont,
        color: u32,
        _bold: bool,
        _italic: bool,
    ) {
        if x + font.width <= map.width && y + font.height <= map.height {
            let mut dst = map.offscreen as *mut u8 as usize + (y * map.width + x) * 4;

            let font_i = font.height * (character as usize);
            if font_i + font.height <= font.glyphs.len() {
                for row in 0..font.height {
                    let row_data = font.glyphs[font_i + row];
                    for col in 0..font.width {
                        if (row_data >> (7 - col)) & 1 == 1 {
                            unsafe {
                                *((dst + col * 4) as *mut u32) = color;
                            }
                        }
                    }
                    dst += map.width * 4;
                }
            }
        }
    }
}

impl TextScreen {
    pub fn write(
        &mut self,
        map: &mut V2DisplayMap,
        buf: &[u8],
        input: &mut VecDeque<u8>,
    ) -> Damage {
        let map = unsafe { &mut map.console_map() };

        let mut min_changed_x = map.width;
        let mut max_changed_x = 0;
        let mut min_changed_y = map.height;
        let mut max_changed_y = 0;
        let mut col_changed = |col| {
            if col < min_changed_x {
                min_changed_x = col;
            }
            if col > max_changed_x {
                max_changed_x = col;
            }
        };
        let mut line_changed = |line| {
            if line < min_changed_y {
                min_changed_y = line;
            }
            if line > max_changed_y {
                max_changed_y = line;
            }
        };

        self.console
            .resize(map.width / self.font.width, map.height / self.font.height);
        if self.console.state.x >= self.console.state.w {
            self.console.state.x = self.console.state.w - 1;
        }
        if self.console.state.y >= self.console.state.h {
            self.console.state.y = self.console.state.h - 1;
        }

        if self.console.state.cursor
            && self.console.state.x < self.console.state.w
            && self.console.state.y < self.console.state.h
        {
            let x = self.console.state.x;
            let y = self.console.state.y;
            Self::invert(
                map,
                x * self.font.width,
                y * self.font.height,
                self.font.width,
                self.font.height,
            );
            col_changed(x);
            line_changed(y);
        }

        self.console.write(buf, |event| match event {
            ransid::Event::Char {
                x,
                y,
                c,
                color,
                bold,
                ..
            } => {
                Self::char(
                    map,
                    x * self.font.width,
                    y * self.font.height,
                    c,
                    &self.font,
                    color.as_rgb(),
                    bold,
                    false,
                );
                col_changed(x);
                line_changed(y);
            }
            ransid::Event::Input { data } => input.extend(data),
            ransid::Event::Rect { x, y, w, h, color } => {
                Self::rect(
                    map,
                    x * self.font.width,
                    y * self.font.height,
                    w * self.font.width,
                    h * self.font.height,
                    color.as_rgb(),
                );
                for y2 in y..y + h {
                    line_changed(y2);
                }
                for x2 in x..x + w {
                    col_changed(x2);
                }
            }
            ransid::Event::ScreenBuffer { .. } => (),
            ransid::Event::Move {
                from_x,
                from_y,
                to_x,
                to_y,
                w,
                h,
            } => {
                let width = map.width;
                let pixels = unsafe { &mut *map.offscreen };

                for raw_y in 0..h {
                    let y = if from_y > to_y { raw_y } else { h - raw_y - 1 };

                    for pixel_y in 0..self.font.height {
                        {
                            let off_from = ((from_y + y) * self.font.height + pixel_y) * width
                                + from_x * self.font.width;
                            let off_to = ((to_y + y) * self.font.height + pixel_y) * width
                                + to_x * self.font.width;
                            let len = w * self.font.width;

                            if off_from + len <= pixels.len() && off_to + len <= pixels.len() {
                                unsafe {
                                    let data_ptr = pixels.as_mut_ptr() as *mut u32;
                                    ptr::copy(
                                        data_ptr.offset(off_from as isize),
                                        data_ptr.offset(off_to as isize),
                                        len,
                                    );
                                }
                            }
                        }
                    }
                    for col in to_x..to_x + w {
                        col_changed(col);
                    }
                    line_changed(to_y + y);
                }
            }
            ransid::Event::Resize { .. } => (),
            ransid::Event::Title { .. } => (),
        });

        if self.console.state.cursor
            && self.console.state.x < self.console.state.w
            && self.console.state.y < self.console.state.h
        {
            let x = self.console.state.x;
            let y = self.console.state.y;
            Self::invert(
                map,
                x * self.font.width,
                y * self.font.height,
                self.font.width,
                self.font.height,
            );
            line_changed(y);
        }

        let damage = Damage {
            x: u32::try_from(min_changed_x).unwrap() * self.font.width as u32,
            y: u32::try_from(min_changed_y).unwrap() * self.font.height as u32,
            width: u32::try_from(max_changed_x.saturating_sub(min_changed_x) + 1).unwrap()
                * self.font.width as u32,
            height: u32::try_from(max_changed_y.saturating_sub(min_changed_y) + 1).unwrap()
                * self.font.height as u32,
        };

        damage
    }

    pub fn resize(&mut self, map: &mut V2DisplayMap, mode: Mode) -> io::Result<()> {
        // FIXME fold row when target is narrower and maybe unfold when it is wider
        fn copy_row(
            old_map: &mut DisplayMap,
            new_map: &mut DisplayMap,
            from_row: usize,
            to_row: usize,
        ) {
            for x in 0..cmp::min(old_map.width, new_map.width) {
                let old_idx = from_row * old_map.width + x;
                let new_idx = to_row * new_map.width + x;
                unsafe {
                    (*new_map.offscreen)[new_idx] = (*old_map.offscreen)[old_idx];
                }
            }
        }

        let mut new_buffer = CpuBackedBuffer::new(
            &map.display_handle,
            (u32::from(mode.size().0), u32::from(mode.size().1)),
            DrmFourcc::Argb8888,
            32,
        )?;
        let new_fb = map
            .display_handle
            .add_framebuffer(new_buffer.buffer(), 24, 32)?;

        new_buffer.shadow_buf().fill(0);

        {
            let old_map = unsafe { &mut map.console_map() };

            let new_size = new_buffer.buffer().size();
            let new_shadow_buf = new_buffer.shadow_buf();
            let new_map = &mut DisplayMap {
                offscreen: ptr::slice_from_raw_parts_mut(
                    new_shadow_buf.as_mut_ptr() as *mut u32,
                    new_shadow_buf.len() / 4,
                ),
                width: new_size.0 as usize,
                height: new_size.1 as usize,
            };

            if new_map.height >= old_map.height {
                for row in 0..old_map.height {
                    copy_row(old_map, new_map, row, row);
                }
            } else {
                let deleted_rows = (old_map.height - new_map.height).div_ceil(self.font.height);
                for row in 0..new_map.height {
                    if row + (deleted_rows + 1) * self.font.height >= old_map.height {
                        break;
                    }
                    copy_row(old_map, new_map, row + deleted_rows * self.font.height, row);
                }
                self.console.state.y = self.console.state.y.saturating_sub(deleted_rows);
            }
        }

        let old_buffer = mem::replace(&mut map.buffer, new_buffer);
        old_buffer.destroy(&map.display_handle)?;

        let old_fb = mem::replace(&mut map.fb, new_fb);
        map.display_handle.set_crtc(
            map.crtc,
            Some(map.fb),
            (0, 0),
            &[map.connector],
            Some(mode),
        )?;
        let _ = map.display_handle.destroy_framebuffer(old_fb);

        Ok(())
    }
}

pub struct TextBuffer {
    pub lines: VecDeque<Vec<u8>>,
    pub lines_max: usize,
}

impl TextBuffer {
    pub fn new(max: usize) -> Self {
        let mut lines = VecDeque::new();
        lines.push_back(Vec::new());
        Self {
            lines,
            lines_max: max,
        }
    }
    pub fn write(&mut self, buf: &[u8]) {
        if buf.is_empty() {
            return;
        }

        for &byte in buf {
            self.lines.back_mut().unwrap().push(byte);

            if byte == b'\n' {
                self.lines.push_back(Vec::new());
            }
        }

        let max_len = self.lines_max;
        while self.lines.len() > max_len {
            self.lines.pop_front();
        }
    }
}
