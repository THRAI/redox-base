//! All types and constants in this module are Redox OS specific uapi extensions.
//! They should not be used outside of Redox OS specific programs.

use drm_sys::drm_event;

// random number to reduce chance of conflict
pub const REDOX_DRM_CLIENT_CAP_HOTPLUG_EVENTS: u32 = 0x299fc90c;

pub const REDOX_DRM_EVENT_CONNECTOR_HOTPLUG: u32 = 0x299fc90d;
#[repr(C)]
#[derive(Debug, Copy, Clone)]
pub struct RedoxDrmEventConnectorHotplug {
    pub base: drm_event,
    pub connector: u32,
}
