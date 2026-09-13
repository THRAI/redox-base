use std::cell::RefCell;
use std::collections::VecDeque;
use std::convert::TryFrom;
use std::rc::Rc;
use std::{cmp, io, mem, ptr};

pub use alacritty_terminal;
use alacritty_terminal::event::{Event, EventListener};
use alacritty_terminal::grid::{Dimensions, Indexed, Scroll};
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::color::Colors;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{
    self, point_to_viewport, viewport_to_point, RenderableContent, TermDamage,
};
use alacritty_terminal::vte::ansi::{Color, NamedColor, Rgb};
use alacritty_terminal::{vte, Term};
use drm::buffer::{Buffer, DrmFourcc};
use drm::control::{connector, crtc, framebuffer, ClipRect, Device};
use graphics_ipc::{CpuBackedBuffer, DrmHandle};
use orbclient::FONT;

pub struct V2DisplayMap {
    display_handle: DrmHandle,
    connector: connector::Handle,
    crtc: crtc::Handle,
    fb: framebuffer::Handle,
    buffer: CpuBackedBuffer,
}

impl V2DisplayMap {
    pub fn new(display_handle: DrmHandle) -> io::Result<Self> {
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

    pub fn size(&self) -> (u32, u32) {
        self.buffer.buffer().size()
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

    fn dirty_fb(&mut self, damage: ClipRect) -> io::Result<()> {
        self.buffer.sync_rect(
            u32::from(damage.x1()),
            u32::from(damage.y1()),
            u32::from(damage.x2() - damage.x1()),
            u32::from(damage.y2() - damage.y1()),
        );

        self.display_handle.dirty_framebuffer(self.fb, &[damage])
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

// Need an Rc here because Term doesn't allow access to the inner event listener
struct TextScreenListener(Rc<RefCell<Vec<u8>>>);

impl EventListener for TextScreenListener {
    fn send_event(&self, event: Event) {
        match event {
            Event::PtyWrite(text) => self.0.borrow_mut().extend_from_slice(text.as_bytes()),
            //Event::ColorRequest(_, _) => todo!(),
            _ => {}
        }
    }
}

pub struct TextScreen {
    vte_parser: vte::ansi::Processor,
    term: Term<TextScreenListener>,
    last_cursor: Point,
    colors: Colors,
    font: ConsoleFont,
    term_input: Rc<RefCell<Vec<u8>>>,
}

impl TextScreen {
    pub fn new(font: Option<ConsoleFont>, config: term::Config) -> TextScreen {
        // Color palette derived from ransid crate
        let rgb = |r, g, b| Rgb { r, g, b };
        let mut colors = Colors::default();
        for value in 0u8..=255 {
            colors[usize::from(value)] = Some(match value {
                0 => rgb(0x00, 0x00, 0x00),
                1 => rgb(0x80, 0x00, 0x00),
                2 => rgb(0x00, 0x80, 0x00),
                3 => rgb(0x80, 0x80, 0x00),
                4 => rgb(0x00, 0x00, 0x80),
                5 => rgb(0x80, 0x00, 0x80),
                6 => rgb(0x00, 0x80, 0x80),
                7 => rgb(0xc0, 0xc0, 0xc0),
                8 => rgb(0x80, 0x80, 0x80),
                9 => rgb(0xff, 0x00, 0x00),
                10 => rgb(0x00, 0xff, 0x00),
                11 => rgb(0xff, 0xff, 0x00),
                12 => rgb(0x00, 0x00, 0xff),
                13 => rgb(0xff, 0x00, 0xff),
                14 => rgb(0x00, 0xff, 0xff),
                15 => rgb(0xff, 0xff, 0xff),
                16..=231 => {
                    let convert = |value: u8| -> u8 {
                        match value {
                            0 => 0,
                            _ => value * 0x28 + 0x28,
                        }
                    };
                    let r = convert((value - 16) / 36 % 6);
                    let g = convert((value - 16) / 6 % 6);
                    let b = convert((value - 16) % 6);
                    rgb(r, g, b)
                }
                232..=255 => {
                    let gray = (value - 232) * 10 + 8;
                    rgb(gray, gray, gray)
                }
            });
        }
        colors[NamedColor::Foreground] = colors[NamedColor::White];
        colors[NamedColor::Background] = colors[NamedColor::Black];
        colors[NamedColor::Cursor] = Some(Rgb { r: 0, g: 0, b: 0 });
        colors[NamedColor::DimBlack] = colors[NamedColor::Black];
        colors[NamedColor::DimRed] = colors[NamedColor::Red];
        colors[NamedColor::DimGreen] = colors[NamedColor::Green];
        colors[NamedColor::DimYellow] = colors[NamedColor::Yellow];
        colors[NamedColor::DimBlue] = colors[NamedColor::Blue];
        colors[NamedColor::DimMagenta] = colors[NamedColor::Magenta];
        colors[NamedColor::DimCyan] = colors[NamedColor::Cyan];
        colors[NamedColor::DimWhite] = colors[NamedColor::White];
        colors[NamedColor::BrightForeground] = colors[NamedColor::BrightWhite];
        colors[NamedColor::DimForeground] = colors[NamedColor::DimWhite];

        let term_input = Rc::new(RefCell::new(Vec::new()));

        TextScreen {
            vte_parser: vte::ansi::Processor::new(),
            colors,
            term: Term::new(
                config,
                // Width and height will be filled in on the next write to the console
                &TermSize::new(1, 1),
                TextScreenListener(term_input.clone()),
            ),
            last_cursor: Point::new(Line(0), Column(0)),
            font: font.unwrap_or_else(|| ConsoleFont::new(FONT.to_vec(), 8, 16)),
            term_input,
        }
    }

    pub fn scroll_display(&mut self, scroll: Scroll) {
        self.term.scroll_display(scroll);
    }

    fn lookup_color(term_colors: &Colors, default_colors: &Colors, color: Color) -> u32 {
        let rgb = match color {
            Color::Named(name) => term_colors[name].unwrap_or(default_colors[name].unwrap()),
            Color::Spec(rgb) => rgb,
            Color::Indexed(index) => term_colors[usize::from(index)]
                .unwrap_or(default_colors[usize::from(index)].unwrap()),
        };

        0xFF000000 | u32::from(rgb.r) << 16 | u32::from(rgb.g) << 8 | u32::from(rgb.b)
    }

    fn draw_cell(
        map: &mut DisplayMap,
        font: &ConsoleFont,
        default_colors: &Colors,
        term_content: &RenderableContent,
        screen_lines: usize,
        cell: Indexed<&Cell>,
    ) -> Option<Point<usize>> {
        let Some(point) = point_to_viewport(term_content.display_offset, cell.point) else {
            return None;
        };
        if point.line >= screen_lines {
            return None;
        }

        let x = point.column.0 * font.width;
        let y = point.line * font.height;

        let mut bg_color = Self::lookup_color(term_content.colors, default_colors, cell.bg);
        let mut fg_color = Self::lookup_color(term_content.colors, default_colors, cell.fg);
        if cell.flags.contains(Flags::INVERSE) {
            mem::swap(&mut bg_color, &mut fg_color);
        }

        let _bold = cell.flags.contains(Flags::BOLD);
        let _italic = cell.flags.contains(Flags::ITALIC);

        let c = if cell.c == '\t' { ' ' } else { cell.c };

        if x + font.width <= map.width && y + font.height <= map.height {
            let mut dst = map.offscreen as *mut u8 as usize + (y * map.width + x) * 4;

            let font_i = font.height * (c as usize);
            if font_i + font.height <= font.glyphs.len() {
                for row in 0..font.height {
                    let row_data = font.glyphs[font_i + row];
                    for col in 0..font.width {
                        if (row_data >> (7 - col)) & 1 == 1 {
                            unsafe { *((dst + col * 4) as *mut u32) = fg_color };
                        } else {
                            unsafe { *((dst + col * 4) as *mut u32) = bg_color };
                        }
                    }
                    dst += map.width * 4;
                }
            }
        }

        Some(point)
    }

    pub fn write(&mut self, map: &mut V2DisplayMap, buf: &[u8], input: &mut VecDeque<u8>) {
        let console_map = unsafe { &mut map.console_map() };

        self.term.resize(TermSize::new(
            console_map.width / self.font.width,
            console_map.height / self.font.height,
        ));

        self.vte_parser.advance(&mut self.term, buf);
        self.vte_parser.stop_sync(&mut self.term); // FIXME

        input.extend(self.term_input.borrow_mut().drain(..));

        let damage = self.redraw(console_map);

        map.dirty_fb(damage).unwrap();
    }

    fn redraw(&mut self, map: &mut DisplayMap) -> ClipRect {
        let mut min_changed_x = map.width / self.font.width;
        let mut max_changed_x = 0;
        let mut min_changed_y = map.height / self.font.height;
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

        // FIXME handle column damage
        let changed_lines = match self.term.damage() {
            TermDamage::Full => (0..self.term.screen_lines()).collect::<Vec<_>>(),
            TermDamage::Partial(term_damage_iterator) => term_damage_iterator
                .map(|damage| damage.line)
                .collect::<Vec<_>>(),
        };

        let term_content = self.term.renderable_content();
        let screen_lines = self.term.grid().screen_lines();
        for line in changed_lines {
            let last_column = self.term.grid().last_column();
            for cell in self
                .term
                .grid()
                .iter_from(viewport_to_point(
                    term_content.display_offset,
                    // For whatever reason iter_from skips the point you give it:
                    // https://github.com/alacritty/alacritty/issues/9038
                    Point::new(line - 1, last_column),
                ))
                .take(self.term.grid().columns())
            {
                if let Some(point) = Self::draw_cell(
                    map,
                    &self.font,
                    &self.colors,
                    &term_content,
                    screen_lines,
                    cell,
                ) {
                    col_changed(point.column.0);
                    line_changed(point.line);
                }
            }
        }

        // Hide old cursor if the cursor moved
        if self.last_cursor != term_content.cursor.point {
            let point = self.last_cursor;
            self.last_cursor = term_content.cursor.point;
            let cell = &self.term.grid()[point];
            if let Some(point) = Self::draw_cell(
                map,
                &self.font,
                &self.colors,
                &term_content,
                screen_lines,
                Indexed { point, cell },
            ) {
                col_changed(point.column.0);
                line_changed(point.line);
            }
        }

        {
            let point = term_content.cursor.point;
            let mut cell = self.term.grid()[point].clone();
            cell.flags ^= Flags::INVERSE;
            if let Some(point) = Self::draw_cell(
                map,
                &self.font,
                &self.colors,
                &term_content,
                screen_lines,
                Indexed { point, cell: &cell },
            ) {
                col_changed(point.column.0);
                line_changed(point.line);
            }
        }
        self.term.reset_damage();

        ClipRect::new(
            u16::try_from(min_changed_x).unwrap() * self.font.width as u16,
            u16::try_from(min_changed_y).unwrap() * self.font.height as u16,
            u16::try_from(cmp::max(min_changed_x, max_changed_x + 1)).unwrap()
                * self.font.width as u16,
            u16::try_from(cmp::max(min_changed_y, max_changed_y + 1)).unwrap()
                * self.font.height as u16,
        )
    }

    pub fn resize_to_preferred(&mut self, map: &mut V2DisplayMap) -> io::Result<bool> {
        let mode = match map
            .display_handle
            .get_connector(map.connector, false)
            .and_then(|info| {
                info.modes()
                    .get(0)
                    .map(|m| *m)
                    .ok_or(io::Error::other("unable to get default mode for connector"))
            }) {
            Ok(mode) => mode,
            Err(err) => {
                return Err(io::Error::other(format!(
                    "failed to get display size: {}",
                    err
                )));
            }
        };

        if (u32::from(mode.size().0), u32::from(mode.size().1)) == map.buffer.buffer().size() {
            return Ok(false);
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

        let old_buffer = mem::replace(&mut map.buffer, new_buffer);
        let old_fb = mem::replace(&mut map.fb, new_fb);

        self.redraw(&mut unsafe { map.console_map() });

        map.display_handle.set_crtc(
            map.crtc,
            Some(map.fb),
            (0, 0),
            &[map.connector],
            Some(mode),
        )?;

        old_buffer.destroy(&map.display_handle)?;
        let _ = map.display_handle.destroy_framebuffer(old_fb);

        Ok(true)
    }
}
