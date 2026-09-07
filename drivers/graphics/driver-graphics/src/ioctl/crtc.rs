use std::collections::HashMap;

use syscall::Error;

use crate::kms::objects::{KmsObjectId, KmsObjects};
use crate::{DrmHandle, GraphicsAdapter, VtState};

pub(super) fn get_crtc<T: GraphicsAdapter>(
    objects: &mut KmsObjects<T>,
    mut data: redox_ioctl::drm::DrmModeCrtc<'_>,
) -> Result<usize, Error> {
    let crtc = objects.get_crtc(KmsObjectId(data.crtc_id()))?;
    // Don't touch set_connectors, that is only used by MODE_SET_CRTC
    let primary_plane_state = objects
        .get_plane(crtc.primary_plane)
        .unwrap()
        .state
        .lock()
        .unwrap();
    data.set_fb_id(primary_plane_state.fb_id.unwrap_or(KmsObjectId::INVALID).0);
    data.set_x(primary_plane_state.src_rect.x >> 16);
    data.set_y(primary_plane_state.src_rect.y >> 16);
    data.set_gamma_size(crtc.gamma_size);
    if let Some(mode) = crtc.state.lock().unwrap().mode {
        data.set_mode_valid(1);
        data.set_mode(mode);
    } else {
        data.set_mode_valid(0);
        data.set_mode(Default::default());
    }
    Ok(0)
}

pub(super) fn set_crtc<T: GraphicsAdapter>(
    adapter: &mut T,
    objects: &mut KmsObjects<T>,
    active_vt: usize,
    vts: &mut HashMap<usize, VtState<T>>,
    handle: &mut DrmHandle<T>,
    data: redox_ioctl::drm::DrmModeCrtc<'_>,
) -> Result<usize, Error> {
    let crtc_id = KmsObjectId(data.crtc_id());
    let crtc = objects.get_crtc(crtc_id)?;
    let connector_ids: Vec<KmsObjectId> = data
        .set_connectors_ptr()
        .iter()
        .take(data.count_connectors() as usize)
        .map(|&id| KmsObjectId(id))
        .collect();
    let fb_id = if data.fb_id() != 0 {
        let fb_id = KmsObjectId(data.fb_id());
        objects.get_framebuffer(fb_id)?;
        Some(fb_id)
    } else {
        None
    };
    let mode = if data.mode_valid() != 0 {
        Some(data.mode())
    } else {
        None
    };

    let plane = objects.get_plane(crtc.primary_plane)?;
    let vt_state = vts.get_mut(&handle.vt).unwrap();
    let new_crtc_state = &mut vt_state.crtc_state[crtc.crtc_index as usize];
    let new_plane_state = &mut vt_state.plane_state[plane.plane_index as usize];

    new_crtc_state.mode = mode;
    let old_fb_id = new_plane_state.fb_id;
    new_plane_state.fb_id = fb_id;
    new_plane_state.crtc_id = Some(crtc_id);
    if handle.vt == active_vt {
        adapter.set_crtc(&objects, crtc, new_crtc_state.clone())?;
        adapter.set_plane(&objects, plane, new_plane_state.clone(), None)?;
        for connector in connector_ids {
            objects
                .get_connector(connector)?
                .lock()
                .unwrap()
                .state
                .crtc_id = crtc_id
        }
    }

    if let Some(old_fb_id) = old_fb_id {
        if !VtState::fb_has_any_use(vts, old_fb_id) {
            objects.remove_framebuffer_if_closed(old_fb_id);
        }
    }

    Ok(0)
}
