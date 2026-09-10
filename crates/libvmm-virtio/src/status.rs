//! Device status handshake — §2.3.
//!
//! `ACKNOWLEDGE -> DRIVER -> FEATURES_OK -> DRIVER_OK`. If the guest clears
//! FEATURES_OK the device MUST refuse to run and log `Virtio(FeatureMismatch)`.

use libvmm_core::{VirtioError, VmmResult};

pub const ACKNOWLEDGE: u8 = 0x01;
pub const DRIVER: u8 = 0x02;
pub const DRIVER_OK: u8 = 0x04;
pub const FEATURES_OK: u8 = 0x08;
pub const DEVICE_NEEDS_RESET: u8 = 0x40;
pub const FAILED: u8 = 0x80;

/// Tracks the handshake for one device.
#[derive(Debug, Clone, Copy, Default)]
pub struct DeviceStatus {
    bits: u8,
    /// Set once the driver has cleared FEATURES_OK after setting it. The
    /// device is then wedged until reset (§2.3).
    features_rejected: bool,
}

impl DeviceStatus {
    pub const fn new() -> Self {
        DeviceStatus {
            bits: 0,
            features_rejected: false,
        }
    }

    pub const fn bits(&self) -> u8 {
        self.bits
    }

    /// The device may service the datapath only after DRIVER_OK, and never
    /// once the driver has rejected the feature set.
    pub const fn is_running(&self) -> bool {
        self.bits & DRIVER_OK != 0 && !self.features_rejected && self.bits & FAILED == 0
    }

    pub const fn features_rejected(&self) -> bool {
        self.features_rejected
    }

    /// Apply a guest write to the status register.
    ///
    /// A write of 0 is the device reset, which is always legal. Otherwise
    /// bits may only be added, and only in the order §2.3 gives.
    pub fn write(&mut self, device: &'static str, value: u8) -> VmmResult<()> {
        if value == 0 {
            *self = DeviceStatus::new();
            return Ok(());
        }

        let previous = self.bits;

        // The driver clearing FEATURES_OK after setting it is the explicit
        // "I reject your features" signal.
        if previous & FEATURES_OK != 0 && value & FEATURES_OK == 0 && value & FAILED == 0 {
            self.features_rejected = true;
            self.bits = value;
            return Err(VirtioError::FeatureMismatch {
                device,
                detail: "driver cleared FEATURES_OK; the device refuses to run".to_string(),
            }
            .into());
        }

        // FAILED can be set at any point.
        if value & FAILED != 0 {
            self.bits = value;
            return Ok(());
        }

        // Other bits may only be added, never removed.
        if previous & !value != 0 {
            return Err(VirtioError::BadStatusTransition {
                from: previous,
                to: value,
            }
            .into());
        }

        let added = value & !previous;
        // Each new bit requires its predecessor to already be present.
        let required = |bit: u8, prerequisite: u8| -> bool {
            added & bit == 0 || value & prerequisite == prerequisite
        };

        let ok = required(DRIVER, ACKNOWLEDGE)
            && required(FEATURES_OK, ACKNOWLEDGE | DRIVER)
            && required(DRIVER_OK, ACKNOWLEDGE | DRIVER | FEATURES_OK);
        if !ok {
            return Err(VirtioError::BadStatusTransition {
                from: previous,
                to: value,
            }
            .into());
        }

        self.bits = value;
        Ok(())
    }
}

/// Render a status byte for the log.
pub fn describe(bits: u8) -> String {
    let mut v = Vec::new();
    for (bit, name) in [
        (ACKNOWLEDGE, "ACKNOWLEDGE"),
        (DRIVER, "DRIVER"),
        (DRIVER_OK, "DRIVER_OK"),
        (FEATURES_OK, "FEATURES_OK"),
        (DEVICE_NEEDS_RESET, "DEVICE_NEEDS_RESET"),
        (FAILED, "FAILED"),
    ] {
        if bits & bit != 0 {
            v.push(name);
        }
    }
    if v.is_empty() {
        "RESET".to_string()
    } else {
        v.join("|")
    }
}
