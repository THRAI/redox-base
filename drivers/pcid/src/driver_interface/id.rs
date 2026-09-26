use std::fmt::Write;

use pci_types::device_type::{DeviceType, UsbType};
use serde::{Deserialize, Serialize};

/// All identifying information of a PCI function.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct FullDeviceId {
    pub vendor_id: u16,
    pub device_id: u16,
    pub class: u8,
    pub subclass: u8,
    pub interface: u8,
    pub revision: u8,
}

impl FullDeviceId {
    pub fn display(&self) -> String {
        let device_type = DeviceType::from((self.class, self.subclass));
        let mut string = format!(
            "{:>04X}:{:>04X} {:>02X}.{:>02X}.{:>02X}.{:>02X} {:?}",
            self.vendor_id,
            self.device_id,
            self.class,
            self.subclass,
            self.interface,
            self.revision,
            device_type,
        );
        match device_type {
            DeviceType::UsbController => match UsbType::try_from(self.interface) {
                Ok(usb_type) => {
                    let _ = write!(string, " {:?}", usb_type);
                }
                Err(_) => {}
            },
            _ => (),
        }
        string
    }
}
