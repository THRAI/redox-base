use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};

use drm_fourcc::DrmFourcc;
use syscall::{EINVAL, Error};

use crate::kms::objects::{KmsFramebuffer, KmsObjectId, KmsObjects};
use crate::{Damage, DrmHandle, GraphicsAdapter, VtState};

pub(super) fn mode_get_fb<T: GraphicsAdapter>(
    objects: &mut KmsObjects<T>,
    handle: &mut DrmHandle<T>,
    mut data: redox_ioctl::drm::DrmModeFbCmd<'_>,
) -> Result<usize, Error> {
    let fb = objects.get_framebuffer_maybe_closed(KmsObjectId(data.fb_id()))?;

    let (bpp, depth) = match fb.pixel_format {
        DrmFourcc::Xrgb8888 => (32, 24),
        DrmFourcc::Argb8888 => (32, 32),
        _ => todo!(),
    };

    handle.next_buffer_id += 1;
    handle.buffers.insert(handle.next_buffer_id, fb.buffer.clone());

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

    let fb = adapter.create_framebuffer(buffer);

    let id = objects.add_framebuffer(KmsFramebuffer {
        closed: AtomicBool::new(false),
        width: data.width(),
        height: data.height(),
        pixel_format,
        pitch: data.pitch(),
        buffer: buffer.clone(),
        driver_data: fb,
    });

    data.set_fb_id(id.0);

    Ok(0)
}

pub(super) fn mode_rm_fb<T: GraphicsAdapter>(
    adapter: &mut T,
    objects: &mut KmsObjects<T>,
    active_vt: usize,
    vts: &mut HashMap<usize, VtState<T>>,
    data: redox_ioctl::drm::StandinForUint<'_>,
) -> Result<usize, Error> {
    let fb_id = KmsObjectId(data.inner());
    objects.remove_framebuffer(fb_id)?;

    // Disable planes that use this framebuffer.
    for (vt, vt_data) in vts {
        for (plane_idx, plane_state) in vt_data.plane_state.iter_mut().enumerate() {
            if plane_state.fb_id != Some(fb_id) {
                continue;
            }
            plane_state.fb_id = None;

            if *vt != active_vt {
                continue;
            }
            let plane = objects.planes().nth(plane_idx).unwrap();
            adapter
                .set_plane(&objects, plane, plane_state.clone(), None)
                .unwrap();
        }
    }

    Ok(0)
}

pub(super) fn mode_dirtyfb<T: GraphicsAdapter>(
    adapter: &mut T,
    objects: &mut KmsObjects<T>,
    active_vt: usize,
    handle: &mut DrmHandle<T>,
    data: redox_ioctl::drm::DrmModeFbDirtyCmd<'_>,
) -> Result<usize, Error> {
    let fb = objects.get_framebuffer(KmsObjectId(data.fb_id()))?;

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
            if state.fb_id == Some(KmsObjectId(data.fb_id())) {
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

    let fb = adapter.create_framebuffer(buffer);

    let id = objects.add_framebuffer(KmsFramebuffer {
        closed: AtomicBool::new(false),
        width: data.width(),
        height: data.height(),
        pixel_format: DrmFourcc::try_from(data.pixel_format()).map_err(|_| Error::new(EINVAL))?,
        pitch: data.pitches()[0],
        buffer: buffer.clone(),
        driver_data: fb,
    });

    data.set_fb_id(id.0);

    Ok(0)
}

pub(super) fn mode_get_fb2<T: GraphicsAdapter>(
    objects: &mut KmsObjects<T>,
    handle: &mut DrmHandle<T>,
    mut data: redox_ioctl::drm::DrmModeFbCmd2<'_>,
) -> Result<usize, Error> {
    let fb = objects.get_framebuffer_maybe_closed(KmsObjectId(data.fb_id()))?;

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
    objects: &mut KmsObjects<T>,
    vts: &mut HashMap<usize, VtState<T>>,
    data: redox_ioctl::drm::DrmModeClosefb<'_>,
) -> Result<usize, Error> {
    let fb_id = KmsObjectId(data.fb_id());
    let fb = objects.get_framebuffer(fb_id)?;
    fb.closed.store(true, Ordering::SeqCst);

    if !VtState::fb_has_any_use(vts, fb_id) {
        objects.remove_framebuffer(fb_id).unwrap();
    }

    Ok(0)
}
