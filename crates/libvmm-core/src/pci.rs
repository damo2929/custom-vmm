//! Emulated PCIe ECAM configuration space — §2.1.
//!
//! `address = ECAM_BASE + (bus<<20) + (dev<<15) + (func<<12) + offset`.
//! Accesses trap as `KVM_EXIT_MMIO` and are served by [`PciBus::config_rw`].
//!
//! Every function exposes a standard header, an MSI-X capability, and the
//! four virtio PCI capabilities (§2.2). There is deliberately no INTx line:
//! §1.4 requires that all interrupts be MSI-X delivered via irqfd.

use crate::memory::ECAM_BASE;
use std::collections::BTreeMap;

pub const CONFIG_SPACE_SIZE: usize = 4096;

// Standard header offsets.
pub const VENDOR_ID: usize = 0x00;
pub const DEVICE_ID: usize = 0x02;
pub const COMMAND: usize = 0x04;
pub const STATUS: usize = 0x06;
pub const REVISION_ID: usize = 0x08;
pub const CLASS_CODE: usize = 0x09;
pub const HEADER_TYPE: usize = 0x0E;
pub const BAR0: usize = 0x10;
pub const SUBSYSTEM_VENDOR_ID: usize = 0x2C;
pub const SUBSYSTEM_ID: usize = 0x2E;
pub const CAPABILITY_LIST: usize = 0x34;
/// Interrupt Line / Interrupt Pin. `INTERRUPT_PIN` MUST stay 0 (§1.4).
pub const INTERRUPT_LINE: usize = 0x3C;
pub const INTERRUPT_PIN: usize = 0x3D;

/// Where capabilities start; leaves the standard header intact.
pub const FIRST_CAP_OFFSET: u8 = 0x40;

pub const STATUS_CAP_LIST: u16 = 1 << 4;

/// A bus/device/function address on the emulated fabric.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Bdf {
    pub bus: u8,
    pub device: u8,
    pub function: u8,
}

impl Bdf {
    pub const fn new(bus: u8, device: u8, function: u8) -> Self {
        Bdf {
            bus,
            device,
            function,
        }
    }

    /// Decode an ECAM offset back into a BDF plus register offset.
    pub fn from_ecam_offset(offset: u64) -> (Bdf, usize) {
        let bus = ((offset >> 20) & 0xFF) as u8;
        let device = ((offset >> 15) & 0x1F) as u8;
        let function = ((offset >> 12) & 0x07) as u8;
        let reg = (offset & 0xFFF) as usize;
        (Bdf::new(bus, device, function), reg)
    }

    /// The ECAM offset of this function's config space.
    pub const fn ecam_offset(&self) -> u64 {
        ((self.bus as u64) << 20) | ((self.device as u64) << 15) | ((self.function as u64) << 12)
    }

    pub const fn ecam_address(&self) -> u64 {
        ECAM_BASE + self.ecam_offset()
    }
}

impl std::fmt::Display for Bdf {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:02x}:{:02x}.{}", self.bus, self.device, self.function)
    }
}

/// PCI capability IDs we emit.
pub const CAP_ID_MSIX: u8 = 0x11;
pub const CAP_ID_VENDOR: u8 = 0x09;

/// One emulated PCI function's configuration space.
pub struct PciFunction {
    pub bdf: Bdf,
    space: [u8; CONFIG_SPACE_SIZE],
    /// Per-BAR size, used to answer the write-all-ones sizing probe.
    bar_sizes: [u64; 6],
    /// Offset of the last capability emitted, so the next one can be chained.
    last_cap: Option<u8>,
    next_cap_offset: u8,
}

impl PciFunction {
    /// Create a function with a standard header.
    pub fn new(
        bdf: Bdf,
        vendor_id: u16,
        device_id: u16,
        class_code: u32,
        subsystem_id: u16,
    ) -> Self {
        let mut f = PciFunction {
            bdf,
            space: [0u8; CONFIG_SPACE_SIZE],
            bar_sizes: [0; 6],
            last_cap: None,
            next_cap_offset: FIRST_CAP_OFFSET,
        };
        f.write_u16(VENDOR_ID, vendor_id);
        f.write_u16(DEVICE_ID, device_id);
        f.write_u8(REVISION_ID, 1);
        // class_code is 24 bits: prog-if, subclass, base class.
        f.space[CLASS_CODE] = (class_code & 0xFF) as u8;
        f.space[CLASS_CODE + 1] = ((class_code >> 8) & 0xFF) as u8;
        f.space[CLASS_CODE + 2] = ((class_code >> 16) & 0xFF) as u8;
        f.write_u8(HEADER_TYPE, 0x00);
        f.write_u16(SUBSYSTEM_VENDOR_ID, vendor_id);
        f.write_u16(SUBSYSTEM_ID, subsystem_id);
        f.write_u16(STATUS, STATUS_CAP_LIST);
        f.write_u8(CAPABILITY_LIST, FIRST_CAP_OFFSET);
        // §1.4: no device may request a legacy INTx line.
        f.write_u8(INTERRUPT_PIN, 0);
        f.write_u8(INTERRUPT_LINE, 0xFF);
        f
    }

    /// Program a 64-bit memory BAR. `size` must be a power of two.
    pub fn set_bar64(&mut self, index: usize, base: u64, size: u64) {
        debug_assert!(index < 5, "a 64-bit BAR occupies two slots");
        debug_assert!(size.is_power_of_two());
        // bit 0 = 0 (memory), bits 2:1 = 10b (64-bit), bit 3 = prefetchable.
        let low = ((base & 0xFFFF_FFF0) as u32) | 0b0100 | 0b1000;
        let high = (base >> 32) as u32;
        self.write_u32(BAR0 + index * 4, low);
        self.write_u32(BAR0 + (index + 1) * 4, high);
        self.bar_sizes[index] = size;
    }

    /// Append the MSI-X capability (§2.1). Returns its config-space offset.
    pub fn add_msix_cap(
        &mut self,
        table_size: u16,
        bar: u8,
        table_offset: u32,
        pba_offset: u32,
    ) -> u8 {
        let at = self.begin_cap(CAP_ID_MSIX, 12);
        // Message Control: table size is encoded as N-1; function unmasked,
        // MSI-X disabled until the driver enables it.
        self.write_u16(at as usize + 2, table_size.saturating_sub(1) & 0x07FF);
        self.write_u32(at as usize + 4, (table_offset & !0x7) | (bar as u32 & 0x7));
        self.write_u32(at as usize + 8, (pba_offset & !0x7) | (bar as u32 & 0x7));
        at
    }

    /// Append one virtio PCI capability (§2.2).
    pub fn add_virtio_cap(
        &mut self,
        cfg_type: u8,
        bar: u8,
        offset: u32,
        length: u32,
        extra: Option<u32>,
    ) -> u8 {
        let len = if extra.is_some() { 20 } else { 16 };
        let at = self.begin_cap(CAP_ID_VENDOR, len);
        let base = at as usize;
        self.space[base + 2] = len; // cap_len
        self.space[base + 3] = cfg_type;
        self.space[base + 4] = bar;
        // bytes 5..8 are padding
        self.write_u32(base + 8, offset);
        self.write_u32(base + 12, length);
        if let Some(v) = extra {
            // NOTIFY_CFG carries notify_off_multiplier immediately after.
            self.write_u32(base + 16, v);
        }
        at
    }

    /// Reserve `len` bytes for a capability and chain it to the previous one.
    fn begin_cap(&mut self, cap_id: u8, len: u8) -> u8 {
        let at = self.next_cap_offset;
        self.space[at as usize] = cap_id;
        self.space[at as usize + 1] = 0; // cap_next: patched when the next one lands
        if let Some(prev) = self.last_cap {
            self.space[prev as usize + 1] = at;
        }
        self.last_cap = Some(at);
        // Capabilities are DWORD-aligned.
        self.next_cap_offset = at + ((len + 3) & !3);
        at
    }

    pub fn read(&self, offset: usize, len: usize) -> u64 {
        let mut v = 0u64;
        for i in 0..len.min(8) {
            let byte = self.space.get(offset + i).copied().unwrap_or(0xFF);
            v |= (byte as u64) << (i * 8);
        }
        v
    }

    pub fn write(&mut self, offset: usize, len: usize, value: u64) {
        // A write of all-ones to a BAR is the sizing probe: answer with the
        // two's complement of the region size rather than storing the value.
        if (BAR0..BAR0 + 24).contains(&offset) && len == 4 {
            let index = (offset - BAR0) / 4;
            if value as u32 == 0xFFFF_FFFF {
                let size = self.bar_sizes.get(index).copied().unwrap_or(0);
                if size != 0 {
                    let mask = (!(size - 1)) as u32;
                    // Preserve the low type bits of the BAR.
                    let kept = self.read(offset, 4) as u32 & 0xF;
                    self.write_u32(offset, (mask & !0xF) | kept);
                    return;
                }
            }
        }
        for i in 0..len.min(8) {
            if let Some(slot) = self.space.get_mut(offset + i) {
                *slot = ((value >> (i * 8)) & 0xFF) as u8;
            }
        }
    }

    fn write_u8(&mut self, at: usize, v: u8) {
        self.space[at] = v;
    }
    fn write_u16(&mut self, at: usize, v: u16) {
        self.space[at..at + 2].copy_from_slice(&v.to_le_bytes());
    }
    fn write_u32(&mut self, at: usize, v: u32) {
        self.space[at..at + 4].copy_from_slice(&v.to_le_bytes());
    }
}

/// The set of emulated functions behind ECAM.
#[derive(Default)]
pub struct PciBus {
    functions: BTreeMap<Bdf, PciFunction>,
}

impl PciBus {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, f: PciFunction) {
        self.functions.insert(f.bdf, f);
    }

    pub fn get(&self, bdf: Bdf) -> Option<&PciFunction> {
        self.functions.get(&bdf)
    }

    pub fn get_mut(&mut self, bdf: Bdf) -> Option<&mut PciFunction> {
        self.functions.get_mut(&bdf)
    }

    pub fn bdfs(&self) -> impl Iterator<Item = &Bdf> {
        self.functions.keys()
    }

    /// Serve a `KVM_EXIT_MMIO` in the ECAM window.
    ///
    /// A read of an absent function returns all-ones, which is how PCI
    /// signals "no device here" — that is what makes enumeration terminate.
    pub fn config_rw(&mut self, ecam_offset: u64, len: usize, write: Option<u64>) -> u64 {
        let (bdf, reg) = Bdf::from_ecam_offset(ecam_offset);
        match (self.functions.get_mut(&bdf), write) {
            (Some(f), Some(v)) => {
                f.write(reg, len, v);
                0
            }
            (Some(f), None) => f.read(reg, len),
            (None, Some(_)) => 0,
            (None, None) => match len {
                1 => 0xFF,
                2 => 0xFFFF,
                4 => 0xFFFF_FFFF,
                _ => u64::MAX,
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ecam_address_decoding_round_trips() {
        let bdf = Bdf::new(0x02, 0x04, 0);
        let (back, reg) = Bdf::from_ecam_offset(bdf.ecam_offset() + 0x34);
        assert_eq!(back, bdf);
        assert_eq!(reg, 0x34);
        assert_eq!(bdf.ecam_address(), ECAM_BASE + (2 << 20) + (4 << 15));
    }

    #[test]
    fn absent_function_reads_all_ones() {
        let mut bus = PciBus::new();
        let absent = Bdf::new(0x0A, 0x1F, 7).ecam_offset();
        assert_eq!(bus.config_rw(absent, 4, None), 0xFFFF_FFFF);
    }

    #[test]
    fn no_function_requests_a_legacy_intx_line() {
        // §1.4: all interrupts are MSI-X; INTERRUPT_PIN must read back 0.
        let f = PciFunction::new(Bdf::new(1, 0, 0), 0x1AF4, 0x1050, 0x030000, 0x0040);
        assert_eq!(f.read(INTERRUPT_PIN, 1), 0);
    }

    #[test]
    fn capabilities_chain_in_order() {
        let mut f = PciFunction::new(Bdf::new(3, 0, 0), 0x1AF4, 0x1048, 0x010000, 0x0008);
        let msix = f.add_msix_cap(8, 1, 0, 0x1000);
        let common = f.add_virtio_cap(1, 2, 0, 0x1000, None);
        assert_eq!(f.read(CAPABILITY_LIST, 1) as u8, msix);
        assert_eq!(f.read(msix as usize, 1) as u8, CAP_ID_MSIX);
        // The MSI-X cap's next pointer must reach the virtio common cfg cap.
        assert_eq!(f.read(msix as usize + 1, 1) as u8, common);
        assert_eq!(f.read(common as usize, 1) as u8, CAP_ID_VENDOR);
        assert_eq!(f.read(common as usize + 3, 1) as u8, 1); // cfg_type COMMON
    }

    #[test]
    fn bar_sizing_probe_returns_the_region_size_mask() {
        let mut f = PciFunction::new(Bdf::new(1, 0, 0), 0x1AF4, 0x1050, 0x030000, 0x0040);
        f.set_bar64(0, 0xD000_0000, 0x4000); // 16 KiB
        f.write(BAR0, 4, 0xFFFF_FFFF);
        let probed = f.read(BAR0, 4) as u32;
        // Low 4 bits are type flags; the rest is ~(size-1).
        assert_eq!(probed & !0xF, (!(0x4000u32 - 1)) & !0xF);
        assert_eq!(probed & 0xF, 0b1100); // 64-bit, prefetchable
    }
}
