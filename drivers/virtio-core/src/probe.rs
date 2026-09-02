use std::sync::{Arc, Mutex};

use common::MemoryType;
use pcid_interface::irq_helpers::{InterruptVector, Msix};
use pcid_interface::PciFunctionHandle;

use crate::spec::*;
use crate::transport::{Error, Queue, StandardTransport, Transport};

pub struct Device {
    pub transport: Arc<dyn Transport>,
    pub device_space: *const u8,
    msix: Mutex<Msix>,
}

// FIXME(andypython): `device_space` should not be `Send` nor `Sync`. Take
// it out of `Device`.
unsafe impl Send for Device {}
unsafe impl Sync for Device {}

impl Device {
    /// Next MSI-X vector and a virtqueue on it.
    /// Later allocs reuse vector 0 if the MSI-X table is exhausted.
    pub fn setup_queue(&self) -> Result<Arc<Queue>, Error> {
        let irq = self.alloc_irq();
        self.transport.setup_queue(irq)
    }

    /// Next MSI-X vector without a virtqueue.
    ///
    /// For device-config interrupts (`setup_config_notify`).
    /// This does not spawn an IRQ thread. (see virtio-gpud).
    pub fn alloc_irq(&self) -> InterruptVector {
        self.msix.lock().unwrap().alloc()
    }
}

/// VirtIO Device Probe
///
/// ## Device State
/// After this function, the device will have been successfully reseted and is ready for use.
///
/// The caller is required to do the following:
/// * Negotiate the device and driver supported features (finialize via [`StandardTransport::finalize_features`])
/// * Create the device specific virtio queues (via [`Device::setup_queue`]). This is *required* to be done
///   before starting the device.
/// * Finally start the device (via [`StandardTransport::run_device`]). At this point, the device
///   is alive.
///
/// ## Panics
/// This function panics if the device is not a virtio device.
pub fn probe_device(pcid_handle: &mut PciFunctionHandle) -> Result<Device, Error> {
    let pci_config = pcid_handle.config();

    assert_eq!(
        pci_config.func.full_device_id.vendor_id, 6900,
        "virtio_core::probe_device: not a virtio device"
    );

    let mut common_addr = None;
    let mut notify_addr = None;
    let mut device_addr = None;

    for raw_capability in pcid_handle.get_vendor_capabilities() {
        // SAFETY: We have verified that the length of the data is correct.
        let capability = unsafe { &*(raw_capability.data.as_ptr() as *const PciCapability) };

        match capability.cfg_type {
            CfgType::Common | CfgType::Notify | CfgType::Device => {}
            _ => continue,
        }

        let mapped_bar = unsafe { pcid_handle.map_bar(capability.bar, MemoryType::Uncacheable) };
        let address = mapped_bar.ptr.expose_provenance().get() + capability.offset as usize;

        match capability.cfg_type {
            CfgType::Common => {
                debug_assert!(common_addr.is_none());
                common_addr = Some(address);
            }

            CfgType::Notify => {
                debug_assert!(notify_addr.is_none());

                // SAFETY: The capability type is `Notify`, so its safe to access
                //         the `notify_multiplier` field.
                let multiplier = unsafe {
                    (&*(raw_capability.data.as_ptr() as *const PciCapability
                        as *const PciCapabilityNotify))
                        .notify_off_multiplier()
                };
                notify_addr = Some((address, multiplier));
            }

            CfgType::Device => {
                debug_assert!(device_addr.is_none());
                device_addr = Some(address);
            }

            _ => unreachable!(),
        }
    }

    let common_addr = common_addr.expect("virtio common capability missing");
    let device_addr = device_addr.expect("virtio device capability missing");
    let (notify_addr, notify_multiplier) = notify_addr.expect("virtio notify capability missing");

    // FIXME this is explicitly allowed by the virtio specification to happen
    assert!(
        notify_multiplier != 0,
        "virtio-core::device_probe: device uses the same Queue Notify addresses for all queues"
    );

    let common = unsafe { &mut *(common_addr as *mut CommonCfg) };
    let device_space = unsafe { &mut *(device_addr as *mut u8) };

    let transport = StandardTransport::new(
        common,
        notify_addr as *const u8,
        notify_multiplier,
        device_space,
    );

    // Setup interrupts.
    let all_pci_features = pcid_handle.fetch_all_features();
    let has_msix = all_pci_features.iter().any(|feature| feature.is_msix());

    // According to the virtio specification, the device REQUIRED to support MSI-X.
    assert!(has_msix, "virtio: device does not support MSI-X");
    let msix = Msix::enable(pcid_handle, "virtio");

    log::debug!("virtio: using standard PCI transport");

    let device = Device {
        transport,
        device_space,
        msix: Mutex::new(msix),
    };

    device.transport.reset();
    reinit(&device)?;

    Ok(device)
}

pub fn reinit(device: &Device) -> Result<(), Error> {
    // XXX: According to the virtio specification v1.2, setting the ACKNOWLEDGE and DRIVER bits
    //      in `device_status` is required to be done in two steps.
    device
        .transport
        .insert_status(DeviceStatusFlags::ACKNOWLEDGE);

    device.transport.insert_status(DeviceStatusFlags::DRIVER);
    Ok(())
}
