use std::collections::HashMap;

use drm_fourcc::DrmFourcc;
use syscall::{EINVAL, ENOENT, Error};

use crate::kms::framebuffer::{KmsFramebuffer, disable_planes_with_fb};
use crate::kms::objects::{KmsObjectId, KmsObjects};
use crate::{Damage, DrmHandle, GraphicsAdapter, VtState};

pub(super) fn mode_get_fb<T: GraphicsAdapter>(
    objects: &mut KmsObjects<T>,
    handle: &mut DrmHandle<T>,
    mut data: redox_ioctl::drm::DrmModeFbCmd<'_>,
) -> Result<usize, Error> {
    let fb = objects.get_framebuffer(KmsObjectId(data.fb_id()))?;

    let (bpp, depth) = match fb.pixel_format {
        DrmFourcc::Xrgb8888 => (32, 24),
        DrmFourcc::Argb8888 => (32, 32),
        _ => todo!(),
    };

    handle.next_buffer_id += 1;
    handle
        .buffers
        .insert(handle.next_buffer_id, fb.buffer.clone());

    data.set_width(fb.width);
    data.set_height(fb.height);
    data.set_pitch(fb.pitch);
    data.set_bpp(bpp);
    data.set_depth(depth);
    data.set_handle(handle.next_buffer_id);
    Ok(0)
}

pub(super) fn mode_add_fb<T: GraphicsAdapter>(
    adapter: &mut T,
    objects: &mut KmsObjects<T>,
    handle: &mut DrmHandle<T>,
    mut data: redox_ioctl::drm::DrmModeFbCmd<'_>,
) -> Result<usize, Error> {
    let buffer = handle
        .buffers
        .get(&data.handle())
        .ok_or(Error::new(EINVAL))?;

    if data.bpp() != 32 {
        return Err(Error::new(EINVAL));
    }
    let pixel_format = match data.depth() {
        24 => DrmFourcc::Xrgb8888,
        32 => DrmFourcc::Argb8888,
        _ => return Err(Error::new(EINVAL)),
    };

    // FIXME enforce driver reported framebuffer size requirements

    let driver_data = adapter.create_framebuffer(buffer);
    let fb = objects.add_framebuffer(KmsFramebuffer {
        width: data.width(),
        height: data.height(),
        pixel_format,
        pitch: data.pitch(),
        buffer: buffer.clone(),
        driver_data,
    });
    let fb_id = fb.id();
    handle.fbs.insert(fb_id, fb);

    data.set_fb_id(fb_id.0);

    Ok(0)
}

pub(super) fn mode_rm_fb<T: GraphicsAdapter>(
    adapter: &mut T,
    objects: &mut KmsObjects<T>,
    active_vt: usize,
    vts: &mut HashMap<usize, VtState<T>>,
    handle: &mut DrmHandle<T>,
    data: redox_ioctl::drm::StandinForUint<'_>,
) -> Result<usize, Error> {
    let fb_id = KmsObjectId(data.inner());

    if handle.fbs.remove(&fb_id).is_none() {
        return Err(Error::new(ENOENT));
    }

    disable_planes_with_fb(adapter, objects, active_vt, vts, fb_id);

    Ok(0)
}

pub(super) fn mode_dirtyfb<T: GraphicsAdapter>(
    adapter: &mut T,
    objects: &mut KmsObjects<T>,
    active_vt: usize,
    handle: &mut DrmHandle<T>,
    data: redox_ioctl::drm::DrmModeFbDirtyCmd<'_>,
) -> Result<usize, Error> {
    let fb_id = KmsObjectId(data.fb_id());
    let fb = objects.get_framebuffer(fb_id)?;

    let damage = data
        .clips_ptr()
        .iter()
        .map(|rect| Damage {
            x: u32::from(rect.x1),
            y: u32::from(rect.y1),
            width: u32::from(rect.x2 - rect.x1),
            height: u32::from(rect.y2 - rect.y1),
        })
        .reduce(Damage::merge)
        .unwrap_or(Damage {
            x: 0,
            y: 0,
            width: fb.width,
            height: fb.height,
        })
        .clip(fb.width, fb.height);

    if handle.vt == active_vt {
        for plane in objects.planes() {
            let state = plane.state.lock().unwrap().clone();
            if state.fb.as_ref() == Some(&fb) {
                adapter.set_plane(&objects, plane, state, Some(damage))?;
            }
        }
    }

    Ok(0)
}

pub(super) fn mode_add_fb2<T: GraphicsAdapter>(
    adapter: &mut T,
    objects: &mut KmsObjects<T>,
    handle: &mut DrmHandle<T>,
    mut data: redox_ioctl::drm::DrmModeFbCmd2<'_>,
) -> Result<usize, Error> {
    // FIXME handle multi-plane framebuffers

    let buffer = handle
        .buffers
        .get(&data.handles()[0])
        .ok_or(Error::new(EINVAL))?;

    // FIXME enforce driver reported framebuffer size requirements

    let driver_data = adapter.create_framebuffer(buffer);
    let fb = objects.add_framebuffer(KmsFramebuffer {
        width: data.width(),
        height: data.height(),
        pixel_format: DrmFourcc::try_from(data.pixel_format()).map_err(|_| Error::new(EINVAL))?,
        pitch: data.pitches()[0],
        buffer: buffer.clone(),
        driver_data,
    });
    let fb_id = fb.id();
    handle.fbs.insert(fb_id, fb);

    data.set_fb_id(fb_id.0);

    Ok(0)
}

pub(super) fn mode_get_fb2<T: GraphicsAdapter>(
    objects: &mut KmsObjects<T>,
    handle: &mut DrmHandle<T>,
    mut data: redox_ioctl::drm::DrmModeFbCmd2<'_>,
) -> Result<usize, Error> {
    let fb = objects.get_framebuffer(KmsObjectId(data.fb_id()))?;

    handle.next_buffer_id += 1;
    handle
        .buffers
        .insert(handle.next_buffer_id, fb.buffer.clone());

    data.set_width(fb.width);
    data.set_height(fb.height);
    data.set_pixel_format(fb.pixel_format as u32);
    data.set_handles([handle.next_buffer_id, 0, 0, 0]);
    data.set_pitches([fb.pitch, 0, 0, 0]);
    data.set_offsets([0; 4]);
    data.set_modifier([0; 4]);
    Ok(0)
}

pub(super) fn mode_close_fb<T: GraphicsAdapter>(
    handle: &mut DrmHandle<T>,
    data: redox_ioctl::drm::DrmModeClosefb<'_>,
) -> Result<usize, Error> {
    let fb_id = KmsObjectId(data.fb_id());

    if handle.fbs.remove(&fb_id).is_none() {
        return Err(Error::new(ENOENT));
    }

    Ok(0)
}
