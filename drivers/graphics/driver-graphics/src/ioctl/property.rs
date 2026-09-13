use drm_sys::{
    DRM_MODE_PROP_ATOMIC, DRM_MODE_PROP_BITMASK, DRM_MODE_PROP_BLOB, DRM_MODE_PROP_ENUM,
    DRM_MODE_PROP_IMMUTABLE, DRM_MODE_PROP_OBJECT, DRM_MODE_PROP_RANGE, DRM_MODE_PROP_SIGNED_RANGE,
    drm_mode_property_enum,
};
use syscall::Error;

use crate::GraphicsAdapter;
use crate::kms::objects::{KmsObjectId, KmsObjects};
use crate::kms::properties::KmsPropertyKind;

pub(super) fn mode_get_property<T: GraphicsAdapter>(
    objects: &mut KmsObjects<T>,
    mut data: redox_ioctl::drm::DrmModeGetProperty<'_>,
) -> Result<usize, Error> {
    let property = objects.get_property(KmsObjectId(data.prop_id()))?;
    data.set_name(property.name.0);
    let mut flags = 0;
    if property.immutable {
        flags |= DRM_MODE_PROP_IMMUTABLE;
    }
    if property.atomic {
        flags |= DRM_MODE_PROP_ATOMIC;
    }
    match &property.kind {
        &KmsPropertyKind::Range(start, end) => {
            data.set_flags(flags | DRM_MODE_PROP_RANGE);
            data.set_values_ptr(&[start, end]);
            data.set_enum_blob_ptr(&[]);
        }
        KmsPropertyKind::Enum(variants) => {
            data.set_flags(flags | DRM_MODE_PROP_ENUM);
            data.set_values_ptr(&variants.iter().map(|&(_, value)| value).collect::<Vec<_>>());
            data.set_enum_blob_ptr(
                &variants
                    .iter()
                    .map(|&(name, value)| drm_mode_property_enum {
                        name: name.0,
                        value,
                    })
                    .collect::<Vec<_>>(),
            );
        }
        KmsPropertyKind::Blob => {
            data.set_flags(flags | DRM_MODE_PROP_BLOB);
            data.set_values_ptr(&[]);
            data.set_enum_blob_ptr(&[]);
        }
        KmsPropertyKind::Bitmask(bitmask_flags) => {
            data.set_flags(flags | DRM_MODE_PROP_BITMASK);
            data.set_values_ptr(
                &bitmask_flags
                    .iter()
                    .map(|&(_, value)| value)
                    .collect::<Vec<_>>(),
            );
            data.set_enum_blob_ptr(
                &bitmask_flags
                    .iter()
                    .map(|&(name, value)| drm_mode_property_enum {
                        name: name.0,
                        value,
                    })
                    .collect::<Vec<_>>(),
            );
        }
        KmsPropertyKind::Object { type_ } => {
            data.set_flags(flags | DRM_MODE_PROP_OBJECT);
            data.set_values_ptr(&[u64::from(*type_)]);
            data.set_enum_blob_ptr(&[]);
        }
        &KmsPropertyKind::SignedRange(start, end) => {
            data.set_flags(flags | DRM_MODE_PROP_SIGNED_RANGE);
            data.set_values_ptr(&[start as u64, end as u64]);
            data.set_enum_blob_ptr(&[]);
        }
    }
    Ok(0)
}

pub(super) fn mode_get_prop_blob<T: GraphicsAdapter>(
    objects: &mut KmsObjects<T>,
    mut data: redox_ioctl::drm::DrmModeGetBlob<'_>,
) -> Result<usize, Error> {
    let blob = objects.get_blob(KmsObjectId(data.blob_id()))?;
    data.set_data(blob.data());
    Ok(0)
}

pub(super) fn mode_obj_get_properties<T: GraphicsAdapter>(
    objects: &mut KmsObjects<T>,
    mut data: redox_ioctl::drm::DrmModeObjGetProperties<'_>,
) -> Result<usize, Error> {
    let (props, prop_vals) = objects.get_object_properties_data(KmsObjectId(data.obj_id()))?;
    data.set_props_ptr(&props);
    data.set_prop_values_ptr(&prop_vals);
    data.set_obj_type(objects.object_type(KmsObjectId(data.obj_id()))?);
    Ok(0)
}
