use std::convert::TryInto;

use anyhow::{anyhow, Result};
use plain::Plain;
use smallvec::SmallVec;

#[repr(u8)]
#[derive(Clone, Copy, Debug, Default)]
pub enum HidClassType {
    #[default]
    HID = 0x21,
    Report,
    Physical,
}

#[repr(C, packed)]
#[derive(Clone, Copy, Debug, Default)]
pub struct HidClassDescriptor {
    pub desc_type: HidClassType,
    pub desc_len: u16,
}

unsafe impl Plain for HidClassDescriptor {}

#[derive(Clone, Debug, Default)]
pub struct HidDescriptor {
    pub length: u8,
    pub kind: u8,
    pub hid_spec_release: u16,
    pub country_code: u8,
    pub num_descriptors: u8,
    pub descriptors: SmallVec<[HidClassDescriptor; 1]>,
}

impl HidDescriptor {
    // Size of the fixed part of HidDescriptor
    const HID_DESC_FIXED_SIZE: u8 = 6;
    const HID_CLASS_DESC_SIZE: u8 = 3;

    pub fn from_bytes(bytes: &[u8]) -> Result<Self> {
        let length = bytes[0];
        let kind = bytes[1];

        // A valid descriptor has at least one class descriptor
        if length < Self::HID_DESC_FIXED_SIZE + Self::HID_CLASS_DESC_SIZE {
            return Err(anyhow!("Invalid length"));
        }

        if kind != HidClassType::HID as u8 {
            return Err(anyhow!("This is not a hid descriptor"));
        }

        let num_descriptors = bytes[5];

        if length != Self::HID_DESC_FIXED_SIZE + num_descriptors * Self::HID_CLASS_DESC_SIZE {
            return Err(anyhow!(
                "Len doesn't match the given number of descriptors ({num_descriptors})"
            ));
        }

        let mut descriptors =
            SmallVec::<[HidClassDescriptor; 1]>::with_capacity(num_descriptors as usize);

        for i in 0..num_descriptors {
            match HidClassDescriptor::from_bytes(
                &bytes[(Self::HID_DESC_FIXED_SIZE + i * Self::HID_CLASS_DESC_SIZE) as usize
                    ..(Self::HID_DESC_FIXED_SIZE + (i + 1) * Self::HID_CLASS_DESC_SIZE) as usize],
            ) {
                Ok(desc) => descriptors.push(*desc),
                Err(e) => return Err(anyhow!("{e:?}")),
            }
        }

        Ok(Self {
            length,
            kind,
            hid_spec_release: u16::from_ne_bytes(bytes[2..4].try_into()?),
            country_code: bytes[4],
            num_descriptors,
            descriptors,
        })
    }

    pub fn get_report_desc(&self) -> Result<HidClassDescriptor> {
        for desc in self.descriptors.iter() {
            match desc.desc_type {
                HidClassType::Report => return Ok(*desc),
                _ => (),
            }
        }
        return Err(anyhow!("No Report descriptor found"));
    }
}
