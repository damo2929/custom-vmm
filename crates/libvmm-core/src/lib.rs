//! `libvmm-core` — KVM setup, guest memory, ACPI/SMBIOS, PCIe transport and
//! the boot lifecycle (§1, §2.1, §3).
//!
//! Datapath rule (§0.1): code on the vCPU loop and queue workers must not
//! allocate, lock, or use `unwrap`/`expect`. Errors are returned as values.

// §0.1: datapath code MUST NOT unwrap, expect or panic. Test code may.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod acpi;
pub mod cloudhv;
pub mod devices;
pub mod display;
pub mod error;
pub mod ich9;
pub mod kvm;
pub mod lifecycle;
pub mod memory;
pub mod pci;
pub mod pvh;
pub mod smbios;
pub mod vcpu;

pub use error::{
    BackupError, ControlError, KvmError, MediaError, StorageError, UsbipError, VirtioError,
    VmmError, VmmResult,
};
pub use lifecycle::{Event, Lifecycle, Phase, State};
pub use memory::GuestMemoryMap;
