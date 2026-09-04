use std::collections::HashMap;
use std::sync::atomic::AtomicBool;

use drm_fourcc::DrmFourcc;
use drm_sys::{DRM_MODE_CURSOR_BO, DRM_MODE_CURSOR_MOVE};
use syscall::{EINVAL, ENXIO, Error};

use crate::kms::objects::{KmsFramebuffer, KmsObjectId, KmsObjects, KmsRect};
use crate::{Damage, DrmHandle, GraphicsAdapter, VtState};

pub(super) fn mode_cursor<T: GraphicsAdapter>(
    adapter: &mut T,
    objects: &mut KmsObjects<T>,
    active_vt: usize,
    vts: &mut HashMap<usize, VtState<T>>,
    handle: &mut DrmHandle<T>,
    data: redox_ioctl::drm::DrmModeCursor<'_>,
) -> Result<usize, Error> {
    cursor_inner(
        adapter,
        objects,
        active_vt,
        vts,
        handle,
        data.flags(),
        data.crtc_id(),
        data.x(),
        data.y(),
        data.width(),
        data.height(),
        data.handle(),
        0,
        0,
    )
}

pub(super) fn mode_cursor2<T: GraphicsAdapter>(
    adapter: &mut T,
    objects: &mut KmsObjects<T>,
    active_vt: usize,
    vts: &mut HashMap<usize, VtState<T>>,
    handle: &mut DrmHandle<T>,
    data: redox_ioctl::drm::DrmModeCursor2<'_>,
) -> Result<usize, Error> {
    cursor_inner(
        adapter,
        objects,
        active_vt,
        vts,
        handle,
        data.flags(),
        data.crtc_id(),
        data.x(),
        data.y(),
        data.width(),
        data.height(),
        data.handle(),
        data.hot_x(),
        data.hot_y(),
    )
}

fn cursor_inner<T: GraphicsAdapter>(
    adapter: &mut T,
    objects: &mut KmsObjects<T>,
    active_vt: usize,
    vts: &mut HashMap<usize, VtState<T>>,
    handle: &mut DrmHandle<T>,

    flags: u32,
    crtc_id: u32,
    x: i32,
    y: i32,
    width: u32,
    height: u32,
    handle_id: u32,
    hot_x: i32,
    hot_y: i32,
) -> Result<usize, Error> {
    let crtc_id = KmsObjectId(crtc_id);
    let Some(plane) = objects.get_crtc(crtc_id)?.cursor_plane else {
        return Err(Error::new(ENXIO));
    };
    let plane_index = objects.get_plane(plane).unwrap().plane_index as usize;
    let new_state = &mut vts.get_mut(&handle.vt).unwrap().plane_state[plane_index];
    let old_fb_id = new_state.fb_id;
    new_state.crtc_id = Some(crtc_id);

    if flags & DRM_MODE_CURSOR_BO != 0 {
        if handle_id == 0 {
            new_state.fb_id = None;
        } else {
            let buffer = handle.buffers.get(&handle_id).ok_or(Error::new(EINVAL))?;
            let fb = adapter.create_framebuffer(buffer);
            let fb_id = objects.add_framebuffer(KmsFramebuffer {
                closed: AtomicBool::new(true),
                width,
                height,
                pixel_format: DrmFourcc::Argb8888,
                pitch: width * 4,
                buffer: buffer.clone(),
                driver_data: fb,
            });

            new_state.fb_id = Some(fb_id);
            new_state.src_rect = KmsRect {
                x: 0,
                y: 0,
                width,
                height,
            };
            new_state.crtc_rect.width = width;
            new_state.crtc_rect.height = height;
            if let Some(hotspot) = &mut new_state.hotspot {
                *hotspot = (hot_x, hot_y);
            }
        }
    }

    if flags & DRM_MODE_CURSOR_MOVE != 0 {
        new_state.crtc_rect.x = x;
        new_state.crtc_rect.y = y;
    }

    if handle.vt == active_vt {
        let plane = objects.get_plane(plane).unwrap();
        #[rustfmt::skip]
        let damage = if flags & DRM_MODE_CURSOR_BO != 0 {
            Damage { x: 0, y: 0, width, height }
        } else {
            Damage { x: 0, y: 0, width: 0, height: 0 }
        };
        adapter.set_plane(&objects, plane, new_state.clone(), damage)?;
    }

    if let Some(old_fb_id) = old_fb_id {
        if !VtState::fb_has_any_use(vts, old_fb_id) {
            objects.remove_framebuffer_if_closed(old_fb_id);
        }
    }

    Ok(0)
}
