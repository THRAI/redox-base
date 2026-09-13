use std::fmt::Debug;
use std::sync::Mutex;

use drm_sys::drm_mode_modeinfo;
use syscall::Result;

use crate::GraphicsAdapter;
use crate::kms::objects::{KmsObject, KmsObjectId, KmsObjects};
use crate::kms::plane::{KmsPlaneDriver, KmsPlaneType};
use crate::kms::properties::{
    ACTIVE, GAMMA_LUT_SIZE, KmsBlob, KmsPropertyData, define_object_props,
};
use crate::kms::rc_object::KmsRcObjectRef;

impl<T: GraphicsAdapter> KmsObjects<T> {
    pub fn add_crtc(
        &mut self,
        driver_data: T::Crtc,
        driver_data_state: <T::Crtc as KmsCrtcDriver>::State,
        primary_plane_data: T::Plane,
        primary_plane_data_state: <T::Plane as KmsPlaneDriver>::State,
        cursor_plane: Option<(T::Plane, <T::Plane as KmsPlaneDriver>::State)>,
    ) -> (KmsObjectId, KmsObjectId) {
        let primary_plane = self.add_plane(
            &[],
            KmsPlaneType::Primary,
            false,
            primary_plane_data,
            primary_plane_data_state,
        );

        let cursor_plane = if let Some((cursor_plane_data, cursor_plane_data_state)) = cursor_plane
        {
            Some(self.add_plane(
                &[],
                KmsPlaneType::Cursor,
                true,
                cursor_plane_data,
                cursor_plane_data_state,
            ))
        } else {
            None
        };

        let crtc_index = self.crtcs.len() as u32;
        let id = self.add(KmsCrtc {
            crtc_index,
            gamma_size: FIXED_GAMMA_LUT_SIZE,
            properties: KmsCrtc::base_properties(),
            primary_plane,
            cursor_plane,
            state: Mutex::new(KmsCrtcState {
                mode: None,
                gamma_lut: None,
                driver_data: driver_data_state,
            }),
            driver_data,
        });
        self.crtcs.push(id);

        match self.objects.get_mut(&primary_plane).unwrap() {
            KmsObject::Plane(data) => data.possible_crtcs = 1 << crtc_index,
            _ => unreachable!(),
        }
        if let Some(cursor_plane) = cursor_plane {
            match self.objects.get_mut(&cursor_plane).unwrap() {
                KmsObject::Plane(data) => data.possible_crtcs = 1 << crtc_index,
                _ => unreachable!(),
            }
        }

        (id, primary_plane)
    }

    pub fn crtc_ids(&self) -> &[KmsObjectId] {
        &self.crtcs
    }

    pub fn crtcs(&self) -> impl Iterator<Item = &KmsCrtc<T>> + use<'_, T> {
        self.crtcs
            .iter()
            .map(|&id| self.get::<KmsCrtc<T>>(id).unwrap())
    }

    pub fn get_crtc(&self, id: KmsObjectId) -> Result<&KmsCrtc<T>> {
        self.get(id)
    }
}

pub trait KmsCrtcDriver: Debug {
    type State: Clone + Debug;
}

impl KmsCrtcDriver for () {
    type State = ();
}

// Fine to hard code for now. libdrm modetest only supports 256 as gamma lut size anyway.
const FIXED_GAMMA_LUT_SIZE: u32 = 256;

#[derive(Debug)]
pub struct KmsCrtc<T: GraphicsAdapter> {
    pub crtc_index: u32,
    pub gamma_size: u32,
    pub properties: Vec<KmsPropertyData<Self>>,
    pub primary_plane: KmsObjectId,
    pub cursor_plane: Option<KmsObjectId>,
    pub state: Mutex<KmsCrtcState<T>>,
    pub driver_data: T::Crtc,
}

#[derive(Debug)]
pub struct KmsCrtcState<T: GraphicsAdapter> {
    pub mode: Option<drm_mode_modeinfo>,
    /// Blob of [drm_color_lut; gamma_size]
    pub gamma_lut: Option<KmsRcObjectRef<KmsBlob>>,
    pub driver_data: <T::Crtc as KmsCrtcDriver>::State,
}

impl<T: GraphicsAdapter> Clone for KmsCrtcState<T> {
    fn clone(&self) -> Self {
        Self {
            mode: self.mode.clone(),
            gamma_lut: self.gamma_lut.clone(), // FIXME is cloning this correct?
            driver_data: self.driver_data.clone(),
        }
    }
}

define_object_props!(object, KmsCrtc<T: GraphicsAdapter> {
    ACTIVE {
        get => u64::from(object.state.lock().unwrap().mode.is_some()),
    }
    // FIXME commented out to force libdrm modetest to use the legacy DRM_IOCTL_MODE_SETGAMMA
    // instead while we don't support setting properties yet.
    // GAMMA_LUT {
    //     get => u64::from(object.state.lock().unwrap().gamma_lut.map_or(0, |blob| blob.id().0)),
    // }
    GAMMA_LUT_SIZE {
        get => u64::from(object.gamma_size * 4),
    }
});
