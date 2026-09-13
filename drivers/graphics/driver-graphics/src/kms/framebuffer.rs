use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use drm_fourcc::DrmFourcc;
use syscall::Result;

use crate::{GraphicsAdapter, VtState};
use crate::kms::objects::{KmsObjectId, KmsObjects};

impl<T: GraphicsAdapter> KmsObjects<T> {
    pub fn add_framebuffer(&mut self, fb: KmsFramebuffer<T>) -> KmsObjectId {
        let id = self.add(fb);
        self.framebuffers.push(id);
        id
    }

    pub fn remove_framebuffer(&mut self, id: KmsObjectId) -> Result<()> {
        self.remove::<KmsFramebuffer<T>>(id)
    }

    pub fn remove_framebuffer_if_closed(&mut self, id: KmsObjectId) {
        if self
            .get_framebuffer(id)
            .unwrap()
            .closed
            .load(Ordering::SeqCst)
        {
            self.remove::<KmsFramebuffer<T>>(id).unwrap();
        }
    }

    pub fn fb_ids(&self) -> &[KmsObjectId] {
        &self.framebuffers
    }

    pub fn get_framebuffer(&self, id: KmsObjectId) -> Result<&KmsFramebuffer<T>> {
        Ok(self.get::<KmsFramebuffer<T>>(id)?)
    }
}

#[derive(Debug)]
pub struct KmsFramebuffer<T: GraphicsAdapter> {
    /// Was this framebuffer closed using the CLOSEFB ioctl or implicitly
    /// created by the CURSOR or CURSOR2 ioctls or similar?
    ///
    /// A closed framebuffer will be destroyed as soon as the last plane that
    /// uses it switches to a different framebuffer. In the mean time the GETFB
    /// and GETFB2 ioctls still function on it, but anything else will result
    /// in ENOENT, including another CLOSEFB call.
    pub closed: AtomicBool,

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
}
