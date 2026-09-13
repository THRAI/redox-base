use std::collections::HashMap;

use drm_fourcc::DrmFourcc;
use syscall::{EINVAL, Error};

use crate::kms::objects::{KmsObjectId, KmsObjects, KmsRect};
use crate::kms::plane::KmsPlaneType;
use crate::{DrmHandle, GraphicsAdapter, VtState};

pub(super) fn get_plane_res<T: GraphicsAdapter>(
    adapter: &mut T,
    objects: &mut KmsObjects<T>,
    handle: &mut DrmHandle<T>,
    mut data: redox_ioctl::drm::DrmModeGetPlaneRes<'_>,
) -> Result<usize, Error> {
    let ids = objects
        .plane_ids()
        .iter()
        .filter(|&&id| {
            // FIXME should unsupported planes also give ENOENT for get/set plane?

            let plane_type = objects.get_plane(id).unwrap().plane_type;

            if !handle.supports_universal_planes {
                // Universal planes not supported by client, only return primary planes.
                return plane_type == KmsPlaneType::Primary;
            }

            if plane_type == KmsPlaneType::Cursor
                && adapter.cursor_plane_needs_hotspot()
                && !handle.supports_cursor_hotspot
            {
                // Cursor hotspot not supported by client but required by driver,
                // omit cursor planes.
                return false;
            }

            true
        })
        .map(|id| id.0)
        .collect::<Vec<_>>();
    data.set_plane_id_ptr(&ids);
    Ok(0)
}

pub(super) fn set_plane<T: GraphicsAdapter>(
    adapter: &mut T,
    objects: &mut KmsObjects<T>,
    active_vt: usize,
    vts: &mut HashMap<usize, VtState<T>>,
    handle: &mut DrmHandle<T>,
    data: redox_ioctl::drm::DrmModeSetPlane<'_>,
) -> Result<usize, Error> {
    let plane_id = KmsObjectId(data.plane_id());
    let plane = objects.get_plane(plane_id)?;

    let crtc_id = KmsObjectId(data.crtc_id());
    let crtc_index = objects.get_crtc(crtc_id)?.crtc_index;

    if plane.possible_crtcs & (1 << crtc_index) == 0 {
        return Err(Error::new(EINVAL));
    }

    let new_state = &mut vts.get_mut(&handle.vt).unwrap().plane_state[plane.plane_index as usize];
    let fb = if data.fb_id() != 0 {
        Some(objects.get_framebuffer(KmsObjectId(data.fb_id()))?)
    } else {
        None
    };
    new_state.fb = fb;
    new_state.crtc_id = Some(crtc_id);
    new_state.src_rect = KmsRect {
        x: data.src_x(),
        y: data.src_y(),
        width: data.src_w(),
        height: data.src_h(),
    };
    new_state.crtc_rect = KmsRect {
        x: data.crtc_x() as i32,
        y: data.crtc_y() as i32,
        width: data.crtc_w(),
        height: data.crtc_h(),
    };

    if handle.vt == active_vt {
        adapter.set_plane(&objects, plane, new_state.clone(), None)?;
    }

    Ok(0)
}

pub(super) fn get_plane<T: GraphicsAdapter>(
    objects: &mut KmsObjects<T>,
    mut data: redox_ioctl::drm::DrmModeGetPlane<'_>,
) -> Result<usize, Error> {
    let plane = objects.get_plane(KmsObjectId(data.plane_id())).unwrap();
    let state = plane.state.lock().unwrap();

    data.set_crtc_id(state.crtc_id.map_or(0, |id| id.0));
    data.set_fb_id(state.fb.as_ref().map_or(0, |fb| fb.id().0));
    data.set_possible_crtcs(plane.possible_crtcs);
    data.set_format_type_ptr(&[DrmFourcc::Argb8888 as u32]);
    Ok(0)
}
