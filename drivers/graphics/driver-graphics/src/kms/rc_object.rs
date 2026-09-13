use std::ops::Deref;
use std::sync::mpsc::Sender;
use std::sync::{Arc, Weak};

use syscall::{ENOENT, Error, Result};

use crate::GraphicsAdapter;
use crate::kms::objects::{KmsObjectId, KmsObjectKind, KmsObjects};

#[derive(Debug)]
struct KmsRcObjectInner<U> {
    id: KmsObjectId,
    data: U,
}

#[derive(Debug)]
pub(super) struct KmsRcObject<U>(Weak<KmsRcObjectInner<U>>);

impl<U> KmsRcObject<U> {
    pub(super) fn new<T: GraphicsAdapter>(objects: &mut KmsObjects<T>, data: U) -> KmsRcObjectRef<U>
    where
        Self: KmsObjectKind<T>,
    {
        let (_, inner) = objects.add_with(|id| {
            let inner = Arc::new(KmsRcObjectInner { id, data });
            (KmsRcObject(Arc::downgrade(&inner)), inner)
        });
        KmsRcObjectRef(inner, objects.remove_tx.clone())
    }

    pub(super) fn lookup<T: GraphicsAdapter>(
        objects: &KmsObjects<T>,
        id: KmsObjectId,
    ) -> Result<KmsRcObjectRef<U>>
    where
        Self: KmsObjectKind<T>,
    {
        let obj = objects.get::<Self>(id)?;
        Ok(KmsRcObjectRef(
            obj.0.upgrade().ok_or(Error::new(ENOENT))?,
            objects.remove_tx.clone(),
        ))
    }

    pub(super) fn assert_removed(self) {
        assert!(self.0.upgrade().is_none());
    }
}

#[derive(Debug)]
pub struct KmsRcObjectRef<U>(Arc<KmsRcObjectInner<U>>, Sender<KmsObjectId>);

impl<U> KmsRcObjectRef<U> {
    pub fn id(&self) -> KmsObjectId {
        self.0.id
    }
}

impl<U> Deref for KmsRcObjectRef<U> {
    type Target = U;

    fn deref(&self) -> &Self::Target {
        &self.0.data
    }
}

/// Compare object identity
impl<U> PartialEq for KmsRcObjectRef<U> {
    fn eq(&self, other: &Self) -> bool {
        // Compare pointer equality rather than KmsObjectId to handle multiple
        // drivers in the same process
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl<U> Clone for KmsRcObjectRef<U> {
    fn clone(&self) -> Self {
        Self(self.0.clone(), self.1.clone())
    }
}

impl<U> Drop for KmsRcObjectRef<U> {
    fn drop(&mut self) {
        // If we are the last remaining reference, perform a deferred removal.
        // The KmsObjectId will be removed in KmsObjects::remove_all_deferred.
        if Arc::strong_count(&self.0) == 1 {
            let _ = self.1.send(self.0.id);
        }
    }
}
