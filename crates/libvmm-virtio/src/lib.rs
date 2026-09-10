//! `libvmm-virtio` — modern virtio-pci transport, virtqueue engine, MSI-X
//! (§2).
//!
//! All devices are modern (virtio 1.x) PCI devices with MSI-X: there is no
//! transitional/legacy I/O-port BAR and no shared ISR line.

// §0.1: datapath code MUST NOT unwrap, expect or panic. Test code may.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod features;
pub mod gpu;
pub mod gpu_pci;
pub mod mem;
pub mod msix;
pub mod pci_cap;
pub mod queue;
pub mod status;
pub mod transport;

pub use features::{ring_layout, RingLayout};
pub use pci_cap::BarLayout;
pub use queue::{DescriptorChain, GuestMemory, Virtqueue};
pub use status::DeviceStatus;
