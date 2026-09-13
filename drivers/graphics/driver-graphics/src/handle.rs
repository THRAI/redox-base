use std::collections::{BTreeMap, HashMap};
use std::fmt::Debug;
use std::sync::Arc;

use drm_sys::DRM_CLIENT_NAME_MAX_LEN;
use redox_scheme::CallerCtx;
use scheme_utils::{FpathWriter, ResourceSync};
use syscall::{EINVAL, Error, MapFlags, Result};

use crate::kms::framebuffer::{KmsFramebuffer, disable_planes_with_fb};
use crate::kms::objects::KmsObjectId;
use crate::kms::rc_object::KmsRcObjectRef;
use crate::{
    GraphicsAdapter, GraphicsResource, GraphicsSchemeData, MAP_FAKE_OFFSET_MULTIPLIER, ioctl,
};

#[derive(Debug)]
pub(crate) struct DrmHandle<T: GraphicsAdapter> {
    pub(crate) vt: usize,
    pub(crate) client_name: [u8; DRM_CLIENT_NAME_MAX_LEN as usize],
    pub(crate) unique: Option<String>,
    pub(crate) supports_universal_planes: bool,
    pub(crate) supports_cursor_hotspot: bool,
    pub(crate) fbs: BTreeMap<KmsObjectId, KmsRcObjectRef<KmsFramebuffer<T>>>,
    pub(crate) next_buffer_id: u32,
    pub(crate) buffers: HashMap<u32, Arc<T::Buffer>>,
}

impl<T: GraphicsAdapter> DrmHandle<T> {
    pub(crate) fn new(vt: usize) -> Self {
        DrmHandle {
            vt,
            client_name: [0; _],
            unique: None,
            supports_universal_planes: false,
            supports_cursor_hotspot: false,
            fbs: BTreeMap::new(),
            next_buffer_id: 0,
            buffers: HashMap::new(),
        }
    }
}

impl<T: GraphicsAdapter> ResourceSync for DrmHandle<T> {
    type SchemeData = GraphicsSchemeData<T>;
    type ResourceEnum = GraphicsResource<T>;

    fn fstat(
        &mut self,
        _scheme_data: &mut Self::SchemeData,
        stat: &mut syscall::Stat,
    ) -> Result<()> {
        stat.st_dev = 226 /*DRM_MAJOR*/ << 8;
        Ok(())
    }

    fn fpath(
        &mut self,
        _scheme_data: &mut Self::SchemeData,
        w: &mut FpathWriter,
    ) -> syscall::Result<()> {
        write!(w, "{}", self.vt).unwrap();
        Ok(())
    }

    fn call(
        &mut self,
        scheme_data: &mut Self::SchemeData,
        payload: &mut [u8],
        metadata: &[u64],
        _ctx: &CallerCtx,
    ) -> Result<usize> {
        let res = ioctl::call_ioctl(
            &mut scheme_data.adapter,
            &mut scheme_data.objects,
            scheme_data.active_vt,
            &mut scheme_data.vts,
            self,
            metadata[0],
            payload,
        );
        scheme_data.objects.remove_all_deferred();
        res
    }

    fn mmap_prep(
        &mut self,
        scheme_data: &mut Self::SchemeData,
        offset: u64,
        _size: usize,
        _flags: MapFlags,
    ) -> syscall::Result<usize> {
        // log::trace!("KSMSG MMAP {} {:?} {} {}", id, _flags, _offset, _size);
        let framebuffer = self
            .buffers
            .get(&((offset as usize / MAP_FAKE_OFFSET_MULTIPLIER) as u32))
            .ok_or(Error::new(EINVAL))
            .unwrap();
        let offset = offset & (MAP_FAKE_OFFSET_MULTIPLIER as u64 - 1);
        let ptr = T::map_dumb_buffer(&mut scheme_data.adapter, framebuffer);
        Ok(unsafe { ptr.add(offset as usize) } as usize)
    }

    fn on_close(self, scheme_data: &mut Self::SchemeData) {
        for &fb_id in self.fbs.keys() {
            disable_planes_with_fb(
                &mut scheme_data.adapter,
                &mut scheme_data.objects,
                scheme_data.active_vt,
                &mut scheme_data.vts,
                fb_id,
            );
        }
        scheme_data.objects.remove_all_deferred();
    }
}
