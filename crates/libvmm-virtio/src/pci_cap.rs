//! virtio modern PCI capability layout — §2.2.
//!
//! ```text
//! struct virtio_pci_cap { u8 cap_vndr; u8 cap_next; u8 cap_len;
//!     u8 cfg_type; // 1=COMMON 2=NOTIFY 3=ISR 4=DEVICE 5=PCI
//!     u8 bar; u8 pad[3]; u32 offset; u32 length; }
//! ```
//!
//! There is no transitional/legacy I/O-port BAR: every device here is a
//! modern (virtio 1.x) PCI device with MSI-X (§2).

/// `cfg_type` values.
pub const VIRTIO_PCI_CAP_COMMON_CFG: u8 = 1;
pub const VIRTIO_PCI_CAP_NOTIFY_CFG: u8 = 2;
pub const VIRTIO_PCI_CAP_ISR_CFG: u8 = 3;
pub const VIRTIO_PCI_CAP_DEVICE_CFG: u8 = 4;
pub const VIRTIO_PCI_CAP_PCI_CFG: u8 = 5;

/// virtio device IDs used by this machine.
pub const VIRTIO_ID_NET: u16 = 1;
pub const VIRTIO_ID_CONSOLE: u16 = 3;
pub const VIRTIO_ID_RNG: u16 = 4;
pub const VIRTIO_ID_SCSI: u16 = 8;
pub const VIRTIO_ID_GPU: u16 = 16;
pub const VIRTIO_ID_INPUT: u16 = 18;
pub const VIRTIO_ID_SOUND: u16 = 25;
/// §6: virtio-tpm at 02:04.0.
pub const VIRTIO_ID_TPM: u16 = 45;

/// PCI vendor ID for virtio devices.
pub const VIRTIO_VENDOR_ID: u16 = 0x1AF4;
/// Modern devices use device ID 0x1040 + virtio device ID.
pub const fn modern_device_id(virtio_id: u16) -> u16 {
    0x1040 + virtio_id
}

/// Offsets within `COMMON_CFG` (virtio 1.2 §4.1.4.3).
pub mod common_cfg {
    pub const DEVICE_FEATURE_SELECT: usize = 0x00;
    pub const DEVICE_FEATURE: usize = 0x04;
    pub const DRIVER_FEATURE_SELECT: usize = 0x08;
    pub const DRIVER_FEATURE: usize = 0x0C;
    pub const MSIX_CONFIG: usize = 0x10;
    pub const NUM_QUEUES: usize = 0x12;
    pub const DEVICE_STATUS: usize = 0x14;
    pub const CONFIG_GENERATION: usize = 0x15;
    pub const QUEUE_SELECT: usize = 0x16;
    pub const QUEUE_SIZE: usize = 0x18;
    pub const QUEUE_MSIX_VECTOR: usize = 0x1A;
    pub const QUEUE_ENABLE: usize = 0x1C;
    pub const QUEUE_NOTIFY_OFF: usize = 0x1E;
    pub const QUEUE_DESC: usize = 0x20;
    pub const QUEUE_DRIVER: usize = 0x28;
    pub const QUEUE_DEVICE: usize = 0x30;
    pub const LEN: usize = 0x38;
}

/// The MMIO BAR layout used by every virtio device in this VMM.
///
/// One 64-bit BAR holds all four capability regions at fixed offsets, so a
/// device's BAR can be sized once and the capabilities emitted mechanically.
pub struct BarLayout {
    pub common_offset: u32,
    pub common_length: u32,
    pub notify_offset: u32,
    pub notify_length: u32,
    /// Per-queue doorbell stride: `doorbell = notify_base + qidx * mult`.
    pub notify_off_multiplier: u32,
    pub isr_offset: u32,
    pub isr_length: u32,
    pub device_offset: u32,
    pub device_length: u32,
    pub msix_table_offset: u32,
    pub msix_pba_offset: u32,
    pub bar_size: u64,
}

impl BarLayout {
    /// Lay out a BAR for `num_queues` queues and a `device_cfg_len`-byte
    /// device-specific configuration structure.
    pub fn new(num_queues: u16, device_cfg_len: u32) -> Self {
        const PAGE: u32 = 0x1000;
        const NOTIFY_MULTIPLIER: u32 = 4;

        let common_offset = 0;
        let common_length = PAGE;
        let notify_offset = common_offset + common_length;
        // One doorbell per queue, spaced by the multiplier, rounded to a page.
        let notify_length =
            ((num_queues as u32 * NOTIFY_MULTIPLIER).max(PAGE) + PAGE - 1) & !(PAGE - 1);
        let isr_offset = notify_offset + notify_length;
        let isr_length = PAGE;
        let device_offset = isr_offset + isr_length;
        let device_length = (device_cfg_len.max(1) + PAGE - 1) & !(PAGE - 1);
        let msix_table_offset = device_offset + device_length;
        // 16 bytes per MSI-X table entry, one entry per queue plus one for
        // the configuration-change vector.
        let msix_table_len = (((num_queues as u32 + 1) * 16).max(PAGE) + PAGE - 1) & !(PAGE - 1);
        let msix_pba_offset = msix_table_offset + msix_table_len;
        let total = msix_pba_offset + PAGE;

        BarLayout {
            common_offset,
            common_length,
            notify_offset,
            notify_length,
            notify_off_multiplier: NOTIFY_MULTIPLIER,
            isr_offset,
            isr_length,
            device_offset,
            device_length,
            msix_table_offset,
            msix_pba_offset,
            bar_size: (total as u64).next_power_of_two(),
        }
    }

    /// Guest physical address of queue `q`'s doorbell, given the BAR base.
    ///
    /// This is the address bound to an eventfd with `KVM_IOEVENTFD` (§2.2),
    /// so a guest kick wakes the queue worker with no userspace VM exit.
    pub fn doorbell_address(&self, bar_base: u64, queue: u16) -> u64 {
        bar_base + self.notify_offset as u64 + queue as u64 * self.notify_off_multiplier as u64
    }
}
