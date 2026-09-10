//! `libvmm-usbip` — the USB/IP client bridge (hypervisor side) and the
//! embedded server (client binary side), §9.
//!
//! All USB/IP structures are big-endian (§0.1, §9).

// §0.1: datapath code MUST NOT unwrap, expect or panic. Test code may.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod bridge;
pub mod wire;

pub use bridge::{ImportPolicy, PortState, XhciBridge};
pub use wire::{CmdSubmit, CmdUnlink, DeviceInfo, HeaderBasic, OpCommon, RetSubmit};
