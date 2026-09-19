use std::fs::File;
use std::os::fd::{AsFd, BorrowedFd};
use std::{io, mem, ptr};

use drm::buffer::Buffer;
use drm::control::dumbbuffer::{DumbBuffer, DumbMapping};
use drm::control::Device as _;
use drm::{Device as _, DriverCapability};

pub mod redox_uapi_exts;

/// A graphics handle using the Linux DRM interface.
pub struct DrmHandle {
    file: File,
}

impl AsFd for DrmHandle {
    fn as_fd(&self) -> BorrowedFd<'_> {
        self.file.as_fd()
    }
}

impl drm::Device for DrmHandle {}
impl drm::control::Device for DrmHandle {}

impl DrmHandle {
    pub fn from_file(file: File) -> io::Result<Self> {
        let handle = DrmHandle { file };
        assert!(handle.get_driver_capability(DriverCapability::DumbBuffer)? == 1);
        Ok(handle)
    }
}

pub struct CpuBackedBuffer {
    buffer: DumbBuffer,
    map: DumbMapping<'static>,
    shadow: Option<Box<[u8]>>,
}

impl CpuBackedBuffer {
    pub fn new(
        display_handle: &DrmHandle,
        size: (u32, u32),
        format: drm::buffer::DrmFourcc,
        bpp: u32,
    ) -> io::Result<CpuBackedBuffer> {
        let mut buffer = display_handle.create_dumb_buffer(size, format, bpp)?;

        let map = display_handle.map_dumb_buffer(&mut buffer)?;
        let map = unsafe { mem::transmute::<DumbMapping<'_>, DumbMapping<'static>>(map) };

        let shadow = if display_handle
            .get_driver_capability(DriverCapability::DumbPreferShadow)
            .unwrap_or(1)
            == 0
        {
            None
        } else {
            Some(vec![0; map.len()].into_boxed_slice())
        };

        Ok(CpuBackedBuffer {
            buffer,
            map,
            shadow,
        })
    }

    pub fn buffer(&self) -> &DumbBuffer {
        &self.buffer
    }

    pub fn has_shadow_buf(&self) -> bool {
        self.shadow.is_some()
    }

    pub fn shadow_buf(&mut self) -> &mut [u8] {
        self.shadow.as_deref_mut().unwrap_or(&mut *self.map)
    }

    pub fn sync_rect(&mut self, x: u32, y: u32, width: u32, height: u32) {
        let Some(shadow) = &self.shadow else {
            return; // No shadow buffer; all writes are already propagated to the GPU.
        };

        assert!(x.checked_add(width).unwrap() <= self.buffer.size().0);
        assert!(y.checked_add(height).unwrap() <= self.buffer.size().1);

        let start_x: usize = x.try_into().unwrap();
        let start_y: usize = y.try_into().unwrap();
        let w: usize = width.try_into().unwrap();
        let h: usize = height.try_into().unwrap();

        let offscreen_ptr = shadow.as_ptr().cast::<u32>();
        let onscreen_ptr = self.map.as_mut_ptr().cast::<u32>();

        for row in start_y..start_y + h {
            unsafe {
                ptr::copy_nonoverlapping(
                    offscreen_ptr.add(row * self.buffer.pitch() as usize / 4 + start_x),
                    onscreen_ptr.add(row * self.buffer.pitch() as usize / 4 + start_x),
                    w,
                );
            }
        }

        // No need for a wbinvd to flush the write combining writes as they are
        // already flushed on the next syscall anyway. And the user will need
        // to do a DRM ioctl to actually present the changes on the display.
    }

    pub fn destroy(self, display_handle: &DrmHandle) -> io::Result<()> {
        display_handle.destroy_dumb_buffer(self.buffer)
    }
}
