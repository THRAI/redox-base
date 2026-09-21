use std::sync::Arc;

use common::io::{Io, MmioPtr};
use driver_graphics::kms::framebuffer::KmsFramebuffer;
use driver_graphics::kms::objects::KmsObjectId;
use drm_fourcc::DrmFourcc;
use ihdgd_macros::define_regs;
use range_alloc::RangeAllocator;
use syscall::error::Result;
use syscall::{Error, EIO};

use super::buffer::GpuBuffer;
use super::{Device, GlobalGtt, MmioRegion};
use crate::device::InterruptRegs;

pub const PLANE_WM_ENABLE: u32 = 1 << 31;
pub const PLANE_WM_LINES_SHIFT: u32 = 14;

define_regs! {
    pub struct Plane {
        pub let name: &'static str,
        pub let index: usize,
        pub let kms_id: Option<KmsObjectId>,
        pub reg buf_cfg: u32,
        pub reg color_ctl?: u32 {
            flag gamma_disable,
        },
        pub reg ctl: u32 {
            flag enable = 1 << 31,
            enum source {
                rgb_8888,
            }
        },
        pub reg offset: u32,
        pub reg pos: u32,
        pub reg size: u32,
        pub reg stride: u32,
        pub reg surf: u32,
        pub reg wm[8]: u32,
        pub reg wm_trans: u32,
    }
}

impl Plane {
    pub fn fetch_modeset(&self, alloc_buffers: &mut RangeAllocator<u32>) {
        let buf_cfg = self.buf_cfg.read();
        let buffer_start = buf_cfg & 0x7FF;
        let buffer_end = (buf_cfg >> 16) & 0x7FF;
        alloc_buffers
            .allocate_exact_range(buffer_start..(buffer_end + 1))
            .unwrap_or_else(|err| {
                panic!(
                    "failed to allocate pre-existing buffer blocks {} to {}: {:?}",
                    buffer_start, buffer_end, err
                );
            });
    }

    pub fn modeset(&mut self, alloc_buffers: &mut RangeAllocator<u32>) -> syscall::Result<()> {
        // FIXME handle runtime buffer reconfiguration
        //TODO: enable DBUF if more buffers needed
        //TODO: more blocks would mean better power usage
        // Minimum is 8 blocks for linear planes, 160 blocks is recommended for pre-OS init
        let buffer_size = 160;
        let buffer = alloc_buffers.allocate_range(buffer_size).map_err(|err| {
            log::warn!(
                "failed to allocate {} buffer blocks: {:?}",
                buffer_size,
                err
            );
            Error::new(EIO)
        })?;
        self.buf_cfg.write(buffer.start | (buffer.end << 16));

        //TODO: correct watermark calculation
        self.wm[0].write(PLANE_WM_ENABLE | (2 << PLANE_WM_LINES_SHIFT) | buffer.len() as u32);
        for i in 1..self.wm.len() {
            self.wm[i].writef(PLANE_WM_ENABLE, false);
        }
        self.wm_trans.writef(PLANE_WM_ENABLE, false);

        Ok(())
    }

    pub fn fetch_framebuffer(
        &self,
        gm: &MmioRegion,
        ggtt: &mut GlobalGtt,
    ) -> KmsFramebuffer<Device> {
        let size = self.size.read();
        let width = (size & 0xFFFF) + 1;
        let height = ((size >> 16) & 0xFFFF) + 1;
        let stride_64 = self.stride.read() & 0x7FF;
        //TODO: this will be wrong for tiled planes
        let stride = stride_64 * 64;
        let surf = self.surf.read() & 0xFFFFF000;
        //TODO: read bits per pixel
        let surf_size = (stride * height).next_multiple_of(4096);
        ggtt.reserve(surf, surf_size);

        let buffer = unsafe { GpuBuffer::new(gm, surf, stride * height, true) };

        KmsFramebuffer {
            width,
            height,
            pixel_format: DrmFourcc::Argb8888,
            pitch: stride,
            buffer: Arc::new(buffer),
            driver_data: (),
        }
    }

    pub fn set_framebuffer(&mut self, fb: Option<&KmsFramebuffer<Device>>) {
        let Some(fb) = fb else {
            self.ctl.write(|data| data); // Disable plane
            return;
        };

        //TODO: documentation on this is not great
        let stride_64 = fb.pitch / 64;

        self.size.write((fb.width - 1) | ((fb.height - 1) << 16));
        self.stride.write(stride_64);

        self.surf.write(fb.buffer.gm_offset);

        // Disable gamma
        if let Some(color_ctl) = &mut self.color_ctl {
            color_ctl.write(|data| data.set_gamma_disable(true));
        }

        //TODO: more PLANE_CTL bits
        self.ctl
            .write(|data| data.set_enable(true).set_source_rgb_8888());
    }

    pub fn dump(&self) {
        eprint!("Plane {}", self.name);
        eprint!(" buf_cfg {:08X}", self.buf_cfg.read());
        if let Some(color_ctl) = &self.color_ctl {
            eprint!(" color_ctl {:08X}", color_ctl.read().raw());
        }
        eprint!(" ctl {:08X}", self.ctl.read().raw());
        eprint!(" offset {:08X}", self.offset.read());
        eprint!(" pos {:08X}", self.offset.read());
        eprint!(" size {:08X}", self.size.read());
        eprint!(" stride {:08X}", self.stride.read());
        eprint!(" surf {:08X}", self.surf.read());
        for i in 0..self.wm.len() {
            eprint!(" wm_{} {:08X}", i, self.wm[i].read());
        }
        eprint!(" wm_trans {:08X}", self.wm_trans.read());
        eprintln!();
    }
}

define_regs! {
    pub struct Pipe {
        pub let name: &'static str,
        pub let index: usize,
        pub let planes: Vec<Plane>,
        pub reg bottom_color: u32,
        pub let display_int_ctl_pending: u32,
        pub let interrupt: InterruptRegs,
        pub reg misc: u32,
        pub reg srcsz: u32,
    }
}

impl Pipe {
    pub fn dump(&self) {
        eprint!("Pipe {}", self.name);
        eprint!(" bottom_color {:08X}", self.bottom_color.read());
        eprint!(" misc {:08X}", self.misc.read());
        eprint!(" srcsz {:08X}", self.srcsz.read());
        eprintln!();
    }

    pub fn kabylake(gttmm: &MmioRegion) -> Result<Vec<Self>> {
        let mut pipes = Vec::with_capacity(3);
        for (i, name) in ["A", "B", "C"].iter().enumerate() {
            let mut planes = Vec::new();
            //TODO: cursor plane
            for (j, name) in ["1", "2", "3"].iter().enumerate() {
                planes.push(Plane {
                    name,
                    index: j,
                    kms_id: None,
                    // IHD-OS-KBL-Vol 2c-1.17 PLANE_BUF_CFG
                    buf_cfg: unsafe { gttmm.mmio(0x7027C + i * 0x1000 + j * 0x100)? },
                    // N/A
                    color_ctl: None,
                    // IHD-OS-KBL-Vol 2c-1.17 PLANE_CTL
                    ctl: Plane_ctl {
                        reg: unsafe { gttmm.mmio(0x70180 + i * 0x1000 + j * 0x100)? },
                        source_rgb_8888: 0b0100,
                        source_mask: 0b1111 << 24,
                        source_shift: 24,
                    },
                    // IHD-OS-KBL-Vol 2c-1.17 PLANE_OFFSET
                    offset: unsafe { gttmm.mmio(0x701A4 + i * 0x1000 + j * 0x100)? },
                    // IHD-OS-KBL-Vol 2c-1.17 PLANE_POS
                    pos: unsafe { gttmm.mmio(0x7018C + i * 0x1000 + j * 0x100)? },
                    // IHD-OS-KBL-Vol 2c-1.17 PLANE_SIZE
                    size: unsafe { gttmm.mmio(0x70190 + i * 0x1000 + j * 0x100)? },
                    // IHD-OS-KBL-Vol 2c-1.17 PLANE_STRIDE
                    stride: unsafe { gttmm.mmio(0x70188 + i * 0x1000 + j * 0x100)? },
                    // IHD-OS-KBL-Vol 2c-1.17 PLANE_SURF
                    surf: unsafe { gttmm.mmio(0x7019C + i * 0x1000 + j * 0x100)? },
                    // IHD-OS-KBL-Vol 2c-1.17 PLANE_WM
                    wm: [
                        unsafe { gttmm.mmio(0x70240 + i * 0x1000 + j * 0x100)? },
                        unsafe { gttmm.mmio(0x70244 + i * 0x1000 + j * 0x100)? },
                        unsafe { gttmm.mmio(0x70248 + i * 0x1000 + j * 0x100)? },
                        unsafe { gttmm.mmio(0x7024C + i * 0x1000 + j * 0x100)? },
                        unsafe { gttmm.mmio(0x70250 + i * 0x1000 + j * 0x100)? },
                        unsafe { gttmm.mmio(0x70254 + i * 0x1000 + j * 0x100)? },
                        unsafe { gttmm.mmio(0x70258 + i * 0x1000 + j * 0x100)? },
                        unsafe { gttmm.mmio(0x7025C + i * 0x1000 + j * 0x100)? },
                    ],
                    wm_trans: unsafe { gttmm.mmio(0x70268 + i * 0x1000 + j * 0x100)? },
                });
            }
            pipes.push(Pipe {
                name,
                index: i,
                planes,
                // IHD-OS-KBL-Vol 2c-1.17 PIPE_BOTTOM_COLOR
                bottom_color: unsafe { gttmm.mmio(0x70034 + i * 0x1000)? },
                // IHD-OS-KBL-Vol 2c-1.17 MASTER_INT_CTL
                display_int_ctl_pending: 1 << (16 + i as u32),
                // IHD-OS-KBL-Vol 2c-1.17 DE_PIPE_INTERRUPT
                interrupt: unsafe { InterruptRegs::new(gttmm, 0x44400 + i * 0x10)? },
                // IHD-OS-KBL-Vol 2c-1.17 PIPE_MISC
                misc: unsafe { gttmm.mmio(0x70030 + i * 0x1000)? },
                // IHD-OS-KBL-Vol 2c-1.17 PIPE_SRCSZ
                srcsz: unsafe { gttmm.mmio(0x6001C + i * 0x1000)? },
            })
        }
        Ok(pipes)
    }

    pub fn tigerlake(gttmm: &MmioRegion) -> Result<Vec<Self>> {
        let mut pipes = Vec::with_capacity(4);
        for (i, name) in ["A", "B", "C", "D"].iter().enumerate() {
            let mut planes = Vec::new();
            //TODO: cursor plane
            for (j, name) in ["1", "2", "3", "4", "5", "6", "7"].iter().enumerate() {
                planes.push(Plane {
                    name,
                    index: j,
                    kms_id: None,
                    // IHD-OS-TGL-Vol 2c-12.21 PLANE_BUF_CFG
                    buf_cfg: unsafe { gttmm.mmio(0x7027C + i * 0x1000 + j * 0x100)? },
                    // IHD-OS-TGL-Vol 2c-12.21 PLANE_COLOR_CTL
                    color_ctl: Some(Plane_color_ctl {
                        reg: unsafe { gttmm.mmio(0x701CC + i * 0x1000 + j * 0x100)? },
                        gamma_disable: 1 << 13,
                    }),
                    // IHD-OS-TGL-Vol 2c-12.21 PLANE_CTL
                    ctl: Plane_ctl {
                        reg: unsafe { gttmm.mmio(0x70180 + i * 0x1000 + j * 0x100)? },
                        source_rgb_8888: 0b01000 << 23,
                        source_mask: 0b11111 << 23,
                        source_shift: 23,
                    },
                    // IHD-OS-TGL-Vol 2c-12.21 PLANE_OFFSET
                    offset: unsafe { gttmm.mmio(0x701A4 + i * 0x1000 + j * 0x100)? },
                    // IHD-OS-TGL-Vol 2c-12.21 PLANE_POS
                    pos: unsafe { gttmm.mmio(0x7018C + i * 0x1000 + j * 0x100)? },
                    // IHD-OS-TGL-Vol 2c-12.21 PLANE_SIZE
                    size: unsafe { gttmm.mmio(0x70190 + i * 0x1000 + j * 0x100)? },
                    // IHD-OS-TGL-Vol 2c-12.21 PLANE_STRIDE
                    stride: unsafe { gttmm.mmio(0x70188 + i * 0x1000 + j * 0x100)? },
                    // IHD-OS-TGL-Vol 2c-12.21 PLANE_SURF
                    surf: unsafe { gttmm.mmio(0x7019C + i * 0x1000 + j * 0x100)? },
                    // IHD-OS-TGL-Vol 2c-12.21 PLANE_WM
                    wm: [
                        unsafe { gttmm.mmio(0x70240 + i * 0x1000 + j * 0x100)? },
                        unsafe { gttmm.mmio(0x70244 + i * 0x1000 + j * 0x100)? },
                        unsafe { gttmm.mmio(0x70248 + i * 0x1000 + j * 0x100)? },
                        unsafe { gttmm.mmio(0x7024C + i * 0x1000 + j * 0x100)? },
                        unsafe { gttmm.mmio(0x70250 + i * 0x1000 + j * 0x100)? },
                        unsafe { gttmm.mmio(0x70254 + i * 0x1000 + j * 0x100)? },
                        unsafe { gttmm.mmio(0x70258 + i * 0x1000 + j * 0x100)? },
                        unsafe { gttmm.mmio(0x7025C + i * 0x1000 + j * 0x100)? },
                    ],
                    wm_trans: unsafe { gttmm.mmio(0x70268 + i * 0x1000 + j * 0x100)? },
                });
            }
            pipes.push(Pipe {
                name,
                index: i,
                planes,
                // IHD-OS-TGL-Vol 2c-12.21 PIPE_BOTTOM_COLOR
                bottom_color: unsafe { gttmm.mmio(0x70034 + i * 0x1000)? },
                // IHD-OS-TGL-Vol 2c-12.21 DISPLAY_INT_CTL
                display_int_ctl_pending: 1 << (16 + i as u32),
                // IHD-OS-TGL-Vol 2c-12.21 DE_PIPE_INTERRUPT
                interrupt: unsafe { InterruptRegs::new(gttmm, 0x44400 + i * 0x10)? },
                // IHD-OS-TGL-Vol 2c-12.21 PIPE_MISC
                misc: unsafe { gttmm.mmio(0x70030 + i * 0x1000)? },
                // IHD-OS-TGL-Vol 2c-12.21 PIPE_SRCSZ
                srcsz: unsafe { gttmm.mmio(0x6001C + i * 0x1000)? },
            })
        }
        Ok(pipes)
    }
}
