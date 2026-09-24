use std::ffi::c_char;
use std::mem;

use drm_sys::{
    DRM_CAP_CURSOR_HEIGHT, DRM_CAP_CURSOR_WIDTH, DRM_CAP_DUMB_BUFFER, DRM_CAP_DUMB_PREFER_SHADOW,
    DRM_CAP_DUMB_PREFERRED_DEPTH, DRM_CAP_TIMESTAMP_MONOTONIC, DRM_CLIENT_CAP_CURSOR_PLANE_HOTSPOT,
    DRM_CLIENT_CAP_UNIVERSAL_PLANES,
};
use graphics_ipc::redox_uapi_exts::REDOX_DRM_CLIENT_CAP_HOTPLUG_EVENTS;
use syscall::{EINVAL, EOPNOTSUPP, Error};

use crate::{DrmHandle, GraphicsAdapter};

pub(super) fn version<T: GraphicsAdapter>(
    adapter: &mut T,
    mut data: redox_ioctl::drm::DrmVersion<'_>,
) -> Result<usize, Error> {
    data.set_version_major(1);
    data.set_version_minor(4);
    data.set_version_patchlevel(0);

    data.set_name(unsafe { mem::transmute(adapter.name()) });
    data.set_date(unsafe { mem::transmute(&b"0"[..]) });
    data.set_desc(unsafe { mem::transmute(adapter.desc()) });

    Ok(0)
}

pub(super) fn get_unique<T: GraphicsAdapter>(
    handle: &mut DrmHandle<T>,
    mut data: redox_ioctl::drm::DrmUnique<'_>,
) -> Result<usize, Error> {
    if let Some(unique) = &handle.unique {
        data.set_unique(unsafe { mem::transmute::<&[u8], &[c_char]>(unique.as_bytes()) });
    } else {
        data.set_unique_len(0);
    }
    Ok(0)
}

pub(super) fn set_version<T: GraphicsAdapter>(
    adapter: &mut T,
    handle: &mut DrmHandle<T>,
    mut data: redox_ioctl::drm::DrmSetVersion<'_>,
) -> Result<usize, Error> {
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
}

pub(super) fn get_cap<T: GraphicsAdapter>(
    adapter: &mut T,
    mut data: redox_ioctl::drm::DrmGetCap<'_>,
) -> Result<usize, Error> {
    let cap: u32 = data
        .capability()
        .try_into()
        .map_err(|_| Error::new(EINVAL))?;
    let value = match cap {
        DRM_CAP_DUMB_BUFFER => u64::from(adapter.dumb_buffer_config().is_some()),
        DRM_CAP_DUMB_PREFERRED_DEPTH => adapter
            .dumb_buffer_config()
            .map_or(0, |config| u64::from(config.preferred_depth)),
        DRM_CAP_DUMB_PREFER_SHADOW => u64::from(
            adapter
                .dumb_buffer_config()
                .map_or(false, |config| config.prefer_shadow),
        ),
        DRM_CAP_TIMESTAMP_MONOTONIC => 1,
        DRM_CAP_CURSOR_WIDTH => {
            if let Some((width, _height)) = adapter.cursor_size() {
                width
            } else {
                0
            }
        }
        DRM_CAP_CURSOR_HEIGHT => {
            if let Some((_width, height)) = adapter.cursor_size() {
                height
            } else {
                0
            }
        }
        _ => return Err(Error::new(EINVAL)),
    };
    data.set_value(value);
    Ok(0)
}

pub(super) fn set_client_cap<T: GraphicsAdapter>(
    adapter: &mut T,
    handle: &mut DrmHandle<T>,
    data: redox_ioctl::drm::DrmSetClientCap<'_>,
) -> Result<usize, Error> {
    let cap: u32 = data
        .capability()
        .try_into()
        .map_err(|_| Error::new(EINVAL))?;
    let enable = match data.value() {
        0 => false,
        1 => true,
        _ => return Err(Error::new(EINVAL)),
    };
    match cap {
        DRM_CLIENT_CAP_UNIVERSAL_PLANES => handle.supports_universal_planes = enable,
        DRM_CLIENT_CAP_CURSOR_PLANE_HOTSPOT => {
            if enable && !adapter.cursor_plane_needs_hotspot() {
                return Err(Error::new(EOPNOTSUPP));
            }
            handle.supports_cursor_hotspot = enable;
        }
        REDOX_DRM_CLIENT_CAP_HOTPLUG_EVENTS => handle.supports_redox_hotplug_events = enable,
        _ => return Err(Error::new(EINVAL)),
    }
    Ok(0)
}
