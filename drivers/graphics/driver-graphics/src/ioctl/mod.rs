use std::collections::HashMap;
use std::ffi::c_char;
use std::mem;
use std::sync::Arc;

use drm_fourcc::DrmFourcc;
use syscall::{EINVAL, ENOENT, Error};

use crate::kms::objects::{KmsObjectId, KmsObjects, KmsRect};
use crate::{Buffer, Damage, DrmHandle, GraphicsAdapter, MAP_FAKE_OFFSET_MULTIPLIER, VtState};

mod cursor;
mod framebuffer;
mod property;

pub(crate) fn call_ioctl<T: GraphicsAdapter>(
    adapter: &mut T,
    objects: &mut KmsObjects<T>,
    active_vt: usize,
    vts: &mut HashMap<usize, VtState<T>>,

    handle: &mut DrmHandle<T>,
    cmd: u64,
    payload: &mut [u8],
) -> syscall::Result<usize> {
    use redox_ioctl::drm as ipc;

    match cmd {
        ipc::VERSION => ipc::DrmVersion::with(payload, |mut data| {
            data.set_version_major(1);
            data.set_version_minor(4);
            data.set_version_patchlevel(0);

            data.set_name(unsafe { mem::transmute(adapter.name()) });
            data.set_date(unsafe { mem::transmute(&b"0"[..]) });
            data.set_desc(unsafe { mem::transmute(adapter.desc()) });

            Ok(0)
        }),
        ipc::GET_UNIQUE => ipc::DrmUnique::with(payload, |mut data| {
            if let Some(unique) = &handle.unique {
                data.set_unique(unsafe { mem::transmute::<&[u8], &[c_char]>(unique.as_bytes()) });
            } else {
                data.set_unique_len(0);
            }
            Ok(0)
        }),
        ipc::SET_VERSION => ipc::DrmSetVersion::with(payload, |mut data| {
            // We only support version 1.4 currently
            if data.drm_di_major() != 0 || data.drm_di_minor() != 4 {
                return Err(Error::new(EINVAL));
            }
            if data.drm_dd_major() != 0 || data.drm_dd_minor() != 4 {
                return Err(Error::new(EINVAL));
            }
            data.set_drm_di_major(1);
            data.set_drm_di_minor(4);
            data.set_drm_dd_major(1);
            data.set_drm_dd_minor(4);

            handle.unique = Some(adapter.get_unique());

            Ok(0)
        }),
        ipc::GET_CAP => ipc::DrmGetCap::with(payload, |mut data| {
            data.set_value(
                adapter.get_cap(
                    data.capability()
                        .try_into()
                        .map_err(|_| Error::new(EINVAL))?,
                )?,
            );
            Ok(0)
        }),
        ipc::SET_CLIENT_CAP => ipc::DrmSetClientCap::with(payload, |data| {
            adapter.set_client_cap(
                data.capability()
                    .try_into()
                    .map_err(|_| Error::new(EINVAL))?,
                data.value(),
            )?;
            Ok(0)
        }),
        ipc::MODE_CARD_RES => ipc::DrmModeCardRes::with(payload, |mut data| {
            let conn_ids = objects
                .connector_ids()
                .iter()
                .map(|id| id.0)
                .collect::<Vec<_>>();
            let crtc_ids = objects.crtc_ids().iter().map(|id| id.0).collect::<Vec<_>>();
            let enc_ids = objects
                .encoder_ids()
                .iter()
                .map(|id| id.0)
                .collect::<Vec<_>>();
            let fb_ids = objects.fb_ids().iter().map(|id| id.0).collect::<Vec<_>>();
            data.set_fb_id_ptr(&fb_ids);
            data.set_crtc_id_ptr(&crtc_ids);
            data.set_connector_id_ptr(&conn_ids);
            data.set_encoder_id_ptr(&enc_ids);
            data.set_min_width(0);
            data.set_max_width(16384);
            data.set_min_height(0);
            data.set_max_height(16384);
            Ok(0)
        }),
        ipc::MODE_GET_CRTC => ipc::DrmModeCrtc::with(payload, |mut data| {
            let crtc = objects
                .get_crtc(KmsObjectId(data.crtc_id()))?
                .lock()
                .unwrap();
            // Don't touch set_connectors, that is only used by MODE_SET_CRTC
            data.set_fb_id(
                objects
                    .get_plane(crtc.primary_plane)
                    .unwrap()
                    .lock()
                    .unwrap()
                    .state
                    .fb_id
                    .unwrap_or(KmsObjectId::INVALID)
                    .0,
            );
            // FIXME fill x and y with the data from the primary plane
            data.set_x(0);
            data.set_y(0);
            data.set_gamma_size(crtc.gamma_size);
            if let Some(mode) = crtc.state.mode {
                data.set_mode_valid(1);
                data.set_mode(mode);
            } else {
                data.set_mode_valid(0);
                data.set_mode(Default::default());
            }
            Ok(0)
        }),
        ipc::MODE_SET_CRTC => ipc::DrmModeCrtc::with(payload, |data| {
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

            let plane = objects.get_plane(crtc.lock().unwrap().primary_plane)?;
            let vt_state = vts.get_mut(&handle.vt).unwrap();
            let new_crtc_state = &mut vt_state.crtc_state[crtc.lock().unwrap().crtc_index as usize];
            let new_plane_state =
                &mut vt_state.plane_state[plane.lock().unwrap().plane_index as usize];

            new_crtc_state.mode = mode;
            let old_fb_id = new_plane_state.fb_id;
            new_plane_state.fb_id = fb_id;
            new_plane_state.crtc_id = Some(crtc_id);
            if handle.vt == active_vt {
                adapter.set_crtc(&objects, crtc, new_crtc_state.clone())?;
                adapter.set_plane(
                    &objects,
                    plane,
                    new_plane_state.clone(),
                    Damage {
                        x: data.x(),
                        y: data.y(),
                        width: mode.map_or(0, |m| m.hdisplay as u32),
                        height: mode.map_or(0, |m| m.vdisplay as u32),
                    },
                )?;
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
        }),
        ipc::MODE_CURSOR => ipc::DrmModeCursor::with(payload, |data| {
            cursor::mode_cursor(adapter, objects, active_vt, vts, handle, data)
        }),
        ipc::MODE_GET_ENCODER => ipc::DrmModeGetEncoder::with(payload, |mut data| {
            let encoder = objects.get_encoder(KmsObjectId(data.encoder_id()))?;
            data.set_crtc_id(encoder.crtc_id.0);
            data.set_possible_crtcs(encoder.possible_crtcs);
            data.set_possible_clones(encoder.possible_clones);
            Ok(0)
        }),
        ipc::MODE_GET_CONNECTOR => ipc::DrmModeGetConnector::with(payload, |mut data| {
            if data.count_modes() == 0 {
                adapter.probe_connector(objects, KmsObjectId(data.connector_id()));
            }
            let connector = objects
                .get_connector(KmsObjectId(data.connector_id()))?
                .lock()
                .unwrap();
            data.set_encoders_ptr(&[connector.encoder_id.0]);
            data.set_modes_ptr(&connector.modes);
            data.set_connector_type(data.connector_type());
            data.set_connector_type_id(data.connector_type_id());
            data.set_connection(connector.connection as u32);
            data.set_mm_width(connector.mm_width);
            data.set_mm_height(connector.mm_width);
            data.set_subpixel(connector.subpixel as u32);
            drop(connector);
            let (props, prop_vals) =
                objects.get_object_properties_data(KmsObjectId(data.connector_id()))?;
            data.set_props_ptr(&props);
            data.set_prop_values_ptr(&prop_vals);
            Ok(0)
        }),
        ipc::MODE_GET_PROPERTY => ipc::DrmModeGetProperty::with(payload, |data| {
            property::mode_get_property(objects, data)
        }),
        ipc::MODE_GET_PROP_BLOB => {
            ipc::DrmModeGetBlob::with(payload, |data| property::mode_get_prop_blob(objects, data))
        }
        ipc::MODE_GET_FB => ipc::DrmModeFbCmd::with(payload, |data| {
            framebuffer::mode_get_fb(objects, handle, data)
        }),
        ipc::MODE_ADD_FB => ipc::DrmModeFbCmd::with(payload, |data| {
            framebuffer::mode_add_fb(adapter, objects, handle, data)
        }),
        ipc::MODE_RM_FB => ipc::StandinForUint::with(payload, |data| {
            framebuffer::mode_rm_fb(adapter, objects, active_vt, vts, data)
        }),
        ipc::MODE_DIRTYFB => ipc::DrmModeFbDirtyCmd::with(payload, |data| {
            framebuffer::mode_dirtyfb(adapter, objects, active_vt, handle, data)
        }),
        ipc::MODE_CREATE_DUMB => ipc::DrmModeCreateDumb::with(payload, |mut data| {
            if data.bpp() != 32 || data.flags() != 0 {
                return Err(Error::new(EINVAL));
            }

            let (buffer, pitch) = adapter.create_dumb_buffer(data.width(), data.height());

            data.set_pitch(pitch);
            data.set_size(buffer.size() as u64);

            handle.next_id += 1;
            handle.buffers.insert(handle.next_id, Arc::new(buffer));
            data.set_handle(handle.next_id as u32);
            Ok(0)
        }),
        ipc::MODE_MAP_DUMB => ipc::DrmModeMapDumb::with(payload, |mut data| {
            if data.offset() != 0 {
                return Err(Error::new(EINVAL));
            }

            let buffer_id = data.handle();

            if !handle.buffers.contains_key(&buffer_id) {
                return Err(Error::new(ENOENT));
            }

            // FIXME use a better scheme for creating map offsets
            assert!(handle.buffers[&buffer_id].size() < MAP_FAKE_OFFSET_MULTIPLIER);

            data.set_offset((buffer_id as usize * MAP_FAKE_OFFSET_MULTIPLIER) as u64);

            Ok(0)
        }),
        ipc::MODE_DESTROY_DUMB => ipc::DrmModeDestroyDumb::with(payload, |data| {
            if handle.buffers.remove(&data.handle()).is_none() {
                return Err(Error::new(ENOENT));
            }
            Ok(0)
        }),
        ipc::MODE_GET_PLANE_RES => ipc::DrmModeGetPlaneRes::with(payload, |mut data| {
            let ids = objects
                .plane_ids()
                .iter()
                .map(|id| id.0)
                .collect::<Vec<_>>();
            data.set_plane_id_ptr(&ids);
            Ok(0)
        }),
        ipc::MODE_SET_PLANE => ipc::DrmModeSetPlane::with(payload, |data| {
            let plane_id = KmsObjectId(data.plane_id());
            let plane = objects.get_plane(plane_id)?;

            let crtc_id = KmsObjectId(data.crtc_id());
            let crtc_index = objects.get_crtc(crtc_id)?.lock().unwrap().crtc_index;

            let plane_index = {
                let plane = plane.lock().unwrap();
                if plane.possible_crtcs & (1 << crtc_index) == 0 {
                    return Err(Error::new(EINVAL));
                }
                plane.plane_index as usize
            };
            let new_state = &mut vts.get_mut(&handle.vt).unwrap().plane_state[plane_index];
            let fb_id = if data.fb_id() != 0 {
                let fb_id = KmsObjectId(data.fb_id());
                objects.get_framebuffer(fb_id)?;
                Some(fb_id)
            } else {
                None
            };
            let old_fb_id = new_state.fb_id;
            new_state.fb_id = fb_id;
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
                adapter.set_plane(
                    &objects,
                    plane,
                    new_state.clone(),
                    Damage {
                        x: 0,
                        y: 0,
                        width: data.src_w(),
                        height: data.src_h(),
                    },
                )?;
            }

            if let Some(old_fb_id) = old_fb_id {
                if !VtState::fb_has_any_use(vts, old_fb_id) {
                    objects.remove_framebuffer_if_closed(old_fb_id);
                }
            }

            Ok(0)
        }),
        ipc::MODE_GET_PLANE => ipc::DrmModeGetPlane::with(payload, |mut data| {
            let plane = objects
                .get_plane(KmsObjectId(data.plane_id()))
                .unwrap()
                .lock()
                .unwrap();
            data.set_crtc_id(plane.state.crtc_id.map_or(0, |id| id.0));
            data.set_fb_id(plane.state.fb_id.unwrap_or(KmsObjectId::INVALID).0);
            data.set_possible_crtcs(plane.possible_crtcs);
            data.set_format_type_ptr(&[DrmFourcc::Argb8888 as u32]);
            Ok(0)
        }),
        ipc::MODE_ADD_FB2 => ipc::DrmModeFbCmd2::with(payload, |data| {
            framebuffer::mode_add_fb2(adapter, objects, handle, data)
        }),
        ipc::MODE_OBJ_GET_PROPERTIES => ipc::DrmModeObjGetProperties::with(payload, |data| {
            property::mode_obj_get_properties(objects, data)
        }),
        ipc::MODE_CURSOR2 => ipc::DrmModeCursor2::with(payload, |data| {
            cursor::mode_cursor2(adapter, objects, active_vt, vts, handle, data)
        }),
        ipc::MODE_GET_FB2 => ipc::DrmModeFbCmd2::with(payload, |data| {
            framebuffer::mode_get_fb2(objects, handle, data)
        }),
        ipc::MODE_CLOSE_FB => ipc::DrmModeClosefb::with(payload, |data| {
            framebuffer::mode_close_fb(objects, vts, data)
        }),
        _ => return Err(Error::new(EINVAL)),
    }
}
