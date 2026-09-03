use std::collections::HashMap;
use std::fmt::Debug;
use std::marker::PhantomData;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use drm_fourcc::DrmFourcc;
use drm_sys::{
    DRM_MODE_OBJECT_BLOB, DRM_MODE_OBJECT_CONNECTOR, DRM_MODE_OBJECT_CRTC, DRM_MODE_OBJECT_ENCODER,
    DRM_MODE_OBJECT_FB, DRM_MODE_OBJECT_PLANE, DRM_MODE_OBJECT_PROPERTY, DRM_PLANE_TYPE_CURSOR,
    DRM_PLANE_TYPE_OVERLAY, DRM_PLANE_TYPE_PRIMARY, drm_mode_modeinfo,
};
use syscall::{ENOENT, Error, Result};

use crate::GraphicsAdapter;
use crate::kms::connector::{KmsConnector, KmsEncoder};
use crate::kms::properties::{
    ACTIVE, CRTC_H, CRTC_ID, CRTC_W, CRTC_X, CRTC_Y, FB_ID, KmsBlob, KmsProperty, KmsPropertyData,
    SRC_H, SRC_W, SRC_X, SRC_Y, define_object_props, init_standard_props, type_,
};

#[derive(Debug)]
pub struct KmsObjects<T: GraphicsAdapter> {
    next_id: KmsObjectId,
    pub(super) connectors: Vec<KmsObjectId>,
    pub(super) encoders: Vec<KmsObjectId>,
    crtcs: Vec<KmsObjectId>,
    planes: Vec<KmsObjectId>,
    framebuffers: Vec<KmsObjectId>,
    pub(super) objects: HashMap<KmsObjectId, KmsObject<T>>,
    _marker: PhantomData<T>,
}

impl<T: GraphicsAdapter> KmsObjects<T> {
    pub(crate) fn new() -> Self {
        let mut objects = KmsObjects {
            next_id: KmsObjectId(1),
            connectors: vec![],
            encoders: vec![],
            crtcs: vec![],
            planes: vec![],
            framebuffers: vec![],
            objects: HashMap::new(),
            _marker: PhantomData,
        };
        init_standard_props(&mut objects);
        objects
    }

    pub(super) fn add<U: KmsObjectKind<T>>(&mut self, data: U) -> KmsObjectId {
        let id = self.next_id;
        self.objects.insert(id, data.into_object());
        self.next_id.0 += 1;

        id
    }

    pub(super) fn get<U: KmsObjectKind<T>>(&self, id: KmsObjectId) -> Result<&U> {
        let object = self.objects.get(&id).ok_or(Error::new(ENOENT))?;
        if let Some(object) = U::try_from_object(object) {
            Ok(object)
        } else {
            Err(Error::new(ENOENT))
        }
    }

    pub(super) fn remove<U: KmsObjectKind<T>>(&mut self, id: KmsObjectId) -> Result<()> {
        let Some(object) = self.objects.get(&id) else {
            return Err(Error::new(ENOENT));
        };
        let Some(_) = U::try_from_object(object) else {
            return Err(Error::new(ENOENT));
        };
        self.objects.remove(&id).unwrap();

        Ok(())
    }

    pub(crate) fn object_type(&self, id: KmsObjectId) -> Result<u32> {
        let object = self.objects.get(&id).ok_or(Error::new(ENOENT))?;
        Ok(object.object_type())
    }

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
            gamma_size: 0,
            properties: KmsCrtc::base_properties(),
            primary_plane,
            cursor_plane,
            state: Mutex::new(KmsCrtcState {
                mode: None,
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
                fb_id: None,
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
            .get_framebuffer_maybe_closed(id)
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
        let fb = self.get::<KmsFramebuffer<T>>(id)?;
        if fb.closed.load(Ordering::SeqCst) {
            return Err(Error::new(ENOENT));
        }
        Ok(fb)
    }

    pub fn get_framebuffer_maybe_closed(&self, id: KmsObjectId) -> Result<&KmsFramebuffer<T>> {
        self.get::<KmsFramebuffer<T>>(id)
    }
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash)]
pub struct KmsObjectId(pub(crate) u32);

impl KmsObjectId {
    pub const INVALID: KmsObjectId = KmsObjectId(0);
}

impl From<KmsObjectId> for u64 {
    fn from(value: KmsObjectId) -> Self {
        value.0.into()
    }
}

pub(super) trait KmsObjectKind<T: GraphicsAdapter> {
    fn into_object(self) -> KmsObject<T>;
    fn try_from_object(object: &KmsObject<T>) -> Option<&Self>;
}

macro_rules! define_object_kinds {
    (<$T:ident> $(
        $variant:ident($data:ty) = $type:ident,
    )*) => {
        #[derive(Debug)]
        pub(super) enum KmsObject<$T: GraphicsAdapter> {
            $($variant($data),)*
        }

        impl<$T: GraphicsAdapter> KmsObject<$T> {
            fn object_type(&self) -> u32 {
                match self {
                    $(Self::$variant(_) => $type,)*
                }
            }
        }

        $(
            impl<$T: GraphicsAdapter> KmsObjectKind<$T> for $data {
                fn into_object(self) -> KmsObject<$T> {
                    KmsObject::$variant(self)
                }

                fn try_from_object(object: &KmsObject<$T>) -> Option<&$data> {
                    match object {
                        KmsObject::$variant(data) => Some(data),
                        _ => None,
                    }
                }
            }
        )*
    };
}

define_object_kinds! { <T>
    Crtc(KmsCrtc<T>) = DRM_MODE_OBJECT_CRTC,
    Connector(Mutex<KmsConnector<T>>) = DRM_MODE_OBJECT_CONNECTOR,
    Encoder(KmsEncoder) = DRM_MODE_OBJECT_ENCODER,
    Property(KmsProperty) = DRM_MODE_OBJECT_PROPERTY,
    Plane(KmsPlane<T>) = DRM_MODE_OBJECT_PLANE,
    Framebuffer(KmsFramebuffer<T>) = DRM_MODE_OBJECT_FB,
    Blob(KmsBlob) = DRM_MODE_OBJECT_BLOB,
}

pub trait KmsCrtcDriver: Debug {
    type State: Clone + Debug;
}

impl KmsCrtcDriver for () {
    type State = ();
}

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
    pub driver_data: <T::Crtc as KmsCrtcDriver>::State,
}

impl<T: GraphicsAdapter> Clone for KmsCrtcState<T> {
    fn clone(&self) -> Self {
        Self {
            mode: self.mode.clone(),
            driver_data: self.driver_data.clone(),
        }
    }
}

define_object_props!(object, KmsCrtc<T: GraphicsAdapter> {
    ACTIVE {
        get => u64::from(object.state.lock().unwrap().mode.is_some()),
    }
});

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
    pub fb_id: Option<KmsObjectId>,
    pub crtc_id: Option<KmsObjectId>,
    pub src_rect: KmsRect<u32>,
    pub crtc_rect: KmsRect<i32>,
    pub hotspot: Option<(i32, i32)>,
    pub driver_data: <T::Plane as KmsPlaneDriver>::State,
}

impl<T: GraphicsAdapter> Clone for KmsPlaneState<T> {
    fn clone(&self) -> Self {
        Self {
            fb_id: self.fb_id.clone(),
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
        get => u64::from(object.state.lock().unwrap().fb_id.map_or(0, |id| id.0)),
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

#[derive(Debug, Clone)]
pub struct KmsRect<T> {
    pub x: T,
    pub y: T,
    pub width: u32,
    pub height: u32,
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
