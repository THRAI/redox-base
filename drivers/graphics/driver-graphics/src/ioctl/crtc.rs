use std::collections::HashMap;
use std::ptr;
use std::sync::Mutex;

use drm_sys::drm_color_lut;
use syscall::{EINVAL, Error};

use crate::kms::connector::KmsConnector;
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
    data.set_fb_id(primary_plane_state.fb.as_ref().map_or(0, |fb| fb.id().0));
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
    let connector_ids = data
        .set_connectors_ptr()
        .iter()
        .take(data.count_connectors() as usize)
        .map(|&id| KmsObjectId(id))
        .collect::<Vec<KmsObjectId>>();
    let connectors = connector_ids
        .iter()
        .map(|&id| objects.get_connector(id))
        .collect::<Result<Vec<&Mutex<KmsConnector<T>>>, _>>()?;
    let fb = if data.fb_id() != 0 {
        Some(objects.get_framebuffer(KmsObjectId(data.fb_id()))?)
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

    for &connector in &connectors {
        let new_connector_state =
            &mut vt_state.connector_state[connector.lock().unwrap().connector_index];
        new_connector_state.crtc_id = crtc_id;
    }
    new_crtc_state.mode = mode;
    new_plane_state.fb = fb;
    new_plane_state.crtc_id = Some(crtc_id);
    if handle.vt == active_vt {
        for &connector in &connectors {
            let mut connector = connector.lock().unwrap();
            connector.state = vt_state.connector_state[connector.connector_index].clone();
            // FIXME adapter.set_connector()?
        }
        adapter.set_crtc(&objects, crtc, new_crtc_state.clone(), &connector_ids)?;
        adapter.set_plane(&objects, plane, new_plane_state.clone(), None)?;
    }

    Ok(0)
}

pub(super) fn set_gamma<T: GraphicsAdapter>(
    adapter: &mut T,
    objects: &mut KmsObjects<T>,
    vts: &mut HashMap<usize, VtState<T>>,
    handle: &mut DrmHandle<T>,
    data: redox_ioctl::drm::DrmModeCrtcLut<'_>,
) -> Result<usize, Error> {
    let crtc = objects.get_crtc(KmsObjectId(data.crtc_id()))?;

    if data.red().len() != crtc.gamma_size as usize
        || data.blue().len() != crtc.gamma_size as usize
        || data.green().len() != crtc.gamma_size as usize
    {
        return Err(Error::new(EINVAL));
    }

    let new_crtc_state = &mut vts.get_mut(&handle.vt).unwrap().crtc_state[crtc.crtc_index as usize];

    let mut gamma_data = vec![
        drm_color_lut {
            red: 0,
            green: 0,
            blue: 0,
            reserved: 0
        };
        crtc.gamma_size as usize
    ]
    .into_boxed_slice();

    for i in 0..crtc.gamma_size as usize {
        gamma_data[i].red = data.red()[i];
        gamma_data[i].green = data.green()[i];
        gamma_data[i].blue = data.blue()[i];
    }

    let gamma_data = unsafe {
        Box::from_raw(ptr::slice_from_raw_parts_mut(
            Box::into_raw(gamma_data).cast::<u8>(),
            crtc.gamma_size as usize * 4 * 2,
        ))
    };

    new_crtc_state.gamma_lut = Some(objects.add_blob(gamma_data.into_vec()));

    let crtc_id = KmsObjectId(data.crtc_id());
    let crtc = objects.get_crtc(crtc_id)?;
    let connectors = objects
        .connector_ids()
        .into_iter()
        .copied()
        .filter(|&connector_id| {
            objects
                .get_connector(connector_id)
                .unwrap()
                .lock()
                .unwrap()
                .state
                .crtc_id
                == crtc_id
        })
        .collect::<Vec<_>>();
    adapter.set_crtc(objects, crtc, new_crtc_state.clone(), &connectors)?;

    Ok(0)
}
