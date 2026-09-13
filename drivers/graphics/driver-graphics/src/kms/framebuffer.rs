use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use drm_fourcc::DrmFourcc;
use syscall::Result;

use crate::kms::objects::{KmsObjectId, KmsObjects};
use crate::kms::rc_object::{KmsRcObject, KmsRcObjectRef};
use crate::{GraphicsAdapter, VtState};

impl<T: GraphicsAdapter> KmsObjects<T> {
    pub fn add_framebuffer(&mut self, fb: KmsFramebuffer<T>) -> KmsRcObjectRef<KmsFramebuffer<T>> {
        let fb = KmsRcObject::new(self, fb);
        self.framebuffers.insert(fb.id());
        fb
    }

    pub fn fb_ids(&self) -> &BTreeSet<KmsObjectId> {
        &self.framebuffers
    }

    pub fn get_framebuffer(&self, id: KmsObjectId) -> Result<KmsRcObjectRef<KmsFramebuffer<T>>> {
        KmsRcObject::lookup(self, id)
    }
}

#[derive(Debug)]
pub struct KmsFramebuffer<T: GraphicsAdapter> {
    pub width: u32,
    pub height: u32,
    pub pixel_format: DrmFourcc,
    pub pitch: u32,
    pub buffer: Arc<T::Buffer>,
    pub driver_data: T::Framebuffer,
}

pub(crate) fn disable_planes_with_fb<T: GraphicsAdapter>(
    adapter: &mut T,
    objects: &mut KmsObjects<T>,
    active_vt: usize,
    vts: &mut HashMap<usize, VtState<T>>,
    fb_id: KmsObjectId,
) {
    // Disable planes that use this framebuffer.
    for (vt, vt_data) in vts {
        for (plane_idx, plane_state) in vt_data.plane_state.iter_mut().enumerate() {
            if plane_state.fb.as_ref().map(|fb| fb.id()) != Some(fb_id) {
                continue;
            }
            plane_state.fb = None;

            if *vt != active_vt {
                continue;
            }
            let plane = objects.planes().nth(plane_idx).unwrap();
            adapter
                .set_plane(&objects, plane, plane_state.clone(), None)
                .unwrap();
        }
    }
}
