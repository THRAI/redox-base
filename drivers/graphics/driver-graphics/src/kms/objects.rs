use std::collections::{BTreeSet, HashMap};
use std::fmt::Debug;
use std::marker::PhantomData;
use std::sync::Mutex;
use std::sync::mpsc::{self, Receiver, Sender};

use drm_sys::{
    DRM_MODE_OBJECT_BLOB, DRM_MODE_OBJECT_CONNECTOR, DRM_MODE_OBJECT_CRTC, DRM_MODE_OBJECT_ENCODER,
    DRM_MODE_OBJECT_FB, DRM_MODE_OBJECT_PLANE, DRM_MODE_OBJECT_PROPERTY,
};
use syscall::{ENOENT, Error, Result};

use crate::GraphicsAdapter;
use crate::kms::connector::{KmsConnector, KmsEncoder};
use crate::kms::crtc::KmsCrtc;
use crate::kms::framebuffer::KmsFramebuffer;
use crate::kms::plane::KmsPlane;
use crate::kms::properties::{KmsBlob, KmsProperty, init_standard_props};
use crate::kms::rc_object::KmsRcObject;

#[derive(Debug)]
pub struct KmsObjects<T: GraphicsAdapter> {
    next_id: KmsObjectId,
    pub(super) remove_tx: Sender<KmsObjectId>,
    pub(super) remove_rx: Receiver<KmsObjectId>,
    pub(super) connectors: Vec<KmsObjectId>,
    pub(super) encoders: Vec<KmsObjectId>,
    pub(super) crtcs: Vec<KmsObjectId>,
    pub(super) planes: Vec<KmsObjectId>,
    pub(super) framebuffers: BTreeSet<KmsObjectId>,
    pub(super) objects: HashMap<KmsObjectId, KmsObject<T>>,
    _marker: PhantomData<T>,
}

impl<T: GraphicsAdapter> KmsObjects<T> {
    pub(crate) fn new() -> Self {
        let (remove_tx, remove_rx) = mpsc::channel();
        let mut objects = KmsObjects {
            next_id: KmsObjectId(1),
            remove_tx,
            remove_rx,
            connectors: vec![],
            encoders: vec![],
            crtcs: vec![],
            planes: vec![],
            framebuffers: BTreeSet::new(),
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

    pub(super) fn add_with<U: KmsObjectKind<T>, V>(
        &mut self,
        data: impl FnOnce(KmsObjectId) -> (U, V),
    ) -> (KmsObjectId, V) {
        let id = self.next_id;
        let (data, ret) = data(id);
        self.objects.insert(id, data.into_object());
        self.next_id.0 += 1;
        (id, ret)
    }

    pub(super) fn get<U: KmsObjectKind<T>>(&self, id: KmsObjectId) -> Result<&U> {
        let object = self.objects.get(&id).ok_or(Error::new(ENOENT))?;
        if let Some(object) = U::try_from_object(object) {
            Ok(object)
        } else {
            Err(Error::new(ENOENT))
        }
    }

    /// Remove all objects which had their last [`KmsRcObjectRef`] dropped.
    pub(crate) fn remove_all_deferred(&mut self) {
        while let Ok(id) = self.remove_rx.try_recv() {
            let obj = self.objects.remove(&id).unwrap();
            match obj {
                KmsObject::Crtc(_)
                | KmsObject::Connector(_)
                | KmsObject::Encoder(_)
                | KmsObject::Property(_)
                | KmsObject::Plane(_) => {
                    unreachable!("object shouldn't use deferred remove")
                }
                KmsObject::Framebuffer(fb) => {
                    self.framebuffers.remove(&id);
                    KmsRcObject::assert_removed(fb);
                }
                KmsObject::Blob(blob) => {
                    KmsRcObject::assert_removed(blob);
                }
            }
        }
    }

    pub(crate) fn object_type(&self, id: KmsObjectId) -> Result<u32> {
        let object = self.objects.get(&id).ok_or(Error::new(ENOENT))?;
        Ok(object.object_type())
    }
}

#[derive(Debug, Copy, Clone, Eq, PartialEq, Hash, Ord, PartialOrd)]
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
    Framebuffer(KmsRcObject<KmsFramebuffer<T>>) = DRM_MODE_OBJECT_FB,
    Blob(KmsRcObject<KmsBlob>) = DRM_MODE_OBJECT_BLOB,
}

#[derive(Debug, Clone)]
pub struct KmsRect<T> {
    pub x: T,
    pub y: T,
    pub width: u32,
    pub height: u32,
}
