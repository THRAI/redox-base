use std::fmt::Debug;
use std::sync::Mutex;

use drm_sys::{DRM_PLANE_TYPE_CURSOR, DRM_PLANE_TYPE_OVERLAY, DRM_PLANE_TYPE_PRIMARY};
use syscall::Result;

use crate::GraphicsAdapter;
use crate::kms::framebuffer::KmsFramebuffer;
use crate::kms::objects::{KmsObjectId, KmsObjects, KmsRect};
use crate::kms::properties::{
    CRTC_H, CRTC_ID, CRTC_W, CRTC_X, CRTC_Y, FB_ID, KmsPropertyData, SRC_H, SRC_W, SRC_X, SRC_Y,
    define_object_props, type_,
};
use crate::kms::rc_object::KmsRcObjectRef;

impl<T: GraphicsAdapter> KmsObjects<T> {
    pub fn add_plane(
        &mut self,
        crtcs: &[KmsObjectId],
        plane_type: KmsPlaneType,
        has_hotspot: bool,
        driver_data: T::Plane,
        driver_data_state: <T::Plane as KmsPlaneDriver>::State,
    ) -> KmsObjectId {
        if has_hotspot {
            assert_eq!(plane_type, KmsPlaneType::Cursor);
        }

        let mut possible_crtcs = 0u32;
        for &crtc in crtcs {
            possible_crtcs |= 1 << self.get_crtc(crtc).unwrap().crtc_index
        }
        let plane_index = self.planes.len() as u32;
        let id = self.add(KmsPlane {
            plane_index,
            possible_crtcs,
            plane_type,
            properties: KmsPlane::base_properties(),
            state: Mutex::new(KmsPlaneState {
                fb: None,
                crtc_id: None,
                src_rect: KmsRect {
                    x: 0u32,
                    y: 0,
                    width: 0,
                    height: 0,
                },
                crtc_rect: KmsRect {
                    x: 0i32,
                    y: 0,
                    width: 0,
                    height: 0,
                },
                hotspot: has_hotspot.then_some((0, 0)),
                driver_data: driver_data_state,
            }),
            driver_data,
        });
        self.planes.push(id);

        id
    }

    pub fn plane_ids(&self) -> &[KmsObjectId] {
        &self.planes
    }

    pub fn planes(&self) -> impl Iterator<Item = &KmsPlane<T>> + use<'_, T> {
        self.planes
            .iter()
            .map(|&id| self.get::<KmsPlane<T>>(id).unwrap())
    }

    pub fn get_plane(&self, id: KmsObjectId) -> Result<&KmsPlane<T>> {
        self.get(id)
    }
}

pub trait KmsPlaneDriver: Debug {
    type State: Clone + Debug;
}

impl KmsPlaneDriver for () {
    type State = ();
}

#[derive(Debug)]
pub struct KmsPlane<T: GraphicsAdapter> {
    pub plane_index: u32,
    pub possible_crtcs: u32,
    pub plane_type: KmsPlaneType,
    pub properties: Vec<KmsPropertyData<Self>>,
    pub state: Mutex<KmsPlaneState<T>>,
    pub driver_data: T::Plane,
}

#[derive(Debug)]
pub struct KmsPlaneState<T: GraphicsAdapter> {
    pub fb: Option<KmsRcObjectRef<KmsFramebuffer<T>>>,
    pub crtc_id: Option<KmsObjectId>,
    pub src_rect: KmsRect<u32>,
    pub crtc_rect: KmsRect<i32>,
    pub hotspot: Option<(i32, i32)>,
    pub driver_data: <T::Plane as KmsPlaneDriver>::State,
}

impl<T: GraphicsAdapter> Clone for KmsPlaneState<T> {
    fn clone(&self) -> Self {
        Self {
            fb: self.fb.clone(),
            crtc_id: self.crtc_id.clone(),
            src_rect: self.src_rect.clone(),
            crtc_rect: self.crtc_rect.clone(),
            hotspot: self.hotspot,
            driver_data: self.driver_data.clone(),
        }
    }
}

define_object_props!(object, KmsPlane<T: GraphicsAdapter> {
    type_ {
        get => object.plane_type as u64,
    }
    FB_ID {
        get => u64::from(object.state.lock().unwrap().fb.as_ref().map_or(0, |fb| fb.id().0)),
    }
    CRTC_ID {
        get => u64::from(object.state.lock().unwrap().crtc_id.map_or(0, |id| id.0)),
    }
    CRTC_X {
        get => u64::from(object.state.lock().unwrap().crtc_rect.x.cast_unsigned()),
    }
    CRTC_Y {
        get => u64::from(object.state.lock().unwrap().crtc_rect.y.cast_unsigned()),
    }
    CRTC_W {
        get => u64::from(object.state.lock().unwrap().crtc_rect.width),
    }
    CRTC_H {
        get => u64::from(object.state.lock().unwrap().crtc_rect.height),
    }
    SRC_X {
        get => u64::from(object.state.lock().unwrap().src_rect.x),
    }
    SRC_Y {
        get => u64::from(object.state.lock().unwrap().src_rect.y),
    }
    SRC_W {
        get => u64::from(object.state.lock().unwrap().src_rect.width),
    }
    SRC_H {
        get => u64::from(object.state.lock().unwrap().src_rect.height),
    }
    // FIXME HOTSPOT_X and HOTSPOT_Y if supported by graphics card
});

#[derive(Copy, Clone, Debug, PartialEq)]
#[repr(u32)]
pub enum KmsPlaneType {
    Primary = DRM_PLANE_TYPE_PRIMARY,
    Overlay = DRM_PLANE_TYPE_OVERLAY,
    Cursor = DRM_PLANE_TYPE_CURSOR,
}
