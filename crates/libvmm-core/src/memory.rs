//! Guest physical memory map — §1.3.
//!
//! RAM is backed by 1 GiB hugepages around a 1 GiB MMIO hole below 4 GiB.
//! This module computes the map and the KVM slot list; the actual mapping
//! lives in [`crate::kvm`] so the layout can be unit-tested with no /dev/kvm.

use crate::error::{KvmError, VmmResult};
use libvmm_config::Memory as MemoryConfig;

pub const MIB: u64 = 1024 * 1024;
pub const GIB: u64 = 1024 * MIB;

/// The MMIO hole is exactly 0xC000_0000–0xFFFF_FFFF (1 GiB) — §1.3 invariant.
pub const MMIO_HOLE_START: u64 = 0xC000_0000;
pub const MMIO_HOLE_END: u64 = 0xFFFF_FFFF;
pub const MMIO_HOLE_SIZE: u64 = MMIO_HOLE_END - MMIO_HOLE_START + 1;

/// ECAM / PCIe MMCONFIG base (§2.1 as revised by Revision D.5), 256 MiB
/// covering buses 0..=255.
///
/// §2.1 originally put this at the bottom of the MMIO hole, 0xC000_0000.
/// It moved for two reasons that turned out to be the same reason.
///
/// The first is firmware. edk2 fixes `PcdPciExpressBaseAddress` at
/// 0xE000_0000 for every Q35 build, `PlatformInitLib` *programs* that value
/// into the host bridge's `PCIEXBAR` rather than reading ours, and once DXE
/// swaps `BasePciLibCf8` for `DxePciLibI440FxQ35` every configuration access
/// goes there. With ECAM at 0xC000_0000 those reads fell through to nothing,
/// returned zero, and OVMF computed an ACPI timer at port 0x0008 — where it
/// then spun, four and a half million reads in twenty seconds, waiting for a
/// counter that would never advance.
///
/// The second is the host. The reference machine's own firmware reports
/// `PCI: ECAM [mem 0xe0000000-0xefffffff] (base 0xe0000000) for domain 0000
/// [bus 00-ff]` — the same base, the same 256 MiB, the same bus range.
/// Mirroring the live system and satisfying the firmware are the same
/// address.
///
/// The rest of the hole then falls out of edk2's own Q35 map: the 32-bit BAR
/// window runs from the top of low RAM up to ECAM, so it becomes
/// 0xC000_0000..0xDFFF_FFFF — the space ECAM vacated, and half a gigabyte
/// rather than the 236 MiB it had before.
pub const ECAM_BASE: u64 = 0xE000_0000;
pub const ECAM_SIZE: u64 = 256 * MIB;

/// PCIe BAR MMIO window: the bottom of the hole, up to ECAM.
///
/// This is the aperture edk2 derives as `PciExBarBase - Uc32Base`, and with
/// low RAM filling the hole it is exactly this range. Above ECAM,
/// 0xF000_0000..0xFEBF_FFFF, edk2's map has a gap and so does this one; the
/// I/O APIC and LAPIC sit above that.
pub const PCI_MMIO_BASE: u64 = 0xC000_0000;
pub const PCI_MMIO_END: u64 = 0xDFFF_FFFF;
pub const PCI_MMIO_SIZE: u64 = PCI_MMIO_END - PCI_MMIO_BASE + 1;

/// I/O APIC — userspace, because the irqchip is split (§1.4).
pub const IOAPIC_BASE: u64 = 0xFEC0_0000;
pub const IOAPIC_SIZE: u64 = 0x1000;

/// Local APIC — in-kernel.
pub const LAPIC_BASE: u64 = 0xFEE0_0000;
pub const LAPIC_SIZE: u64 = 0x1000;

/// OVMF code region, mapped read-only (§3.1).
pub const OVMF_CODE_BASE: u64 = 0xFFC0_0000;
pub const OVMF_CODE_SIZE: u64 = 4 * MIB;
/// The x86 reset vector, 16 bytes below 4 GiB.
pub const RESET_VECTOR: u64 = 0xFFFF_FFF0;

/// High RAM begins immediately above 4 GiB.
pub const HIGH_RAM_BASE: u64 = 0x1_0000_0000;

/// Low-memory staging area for the generated ACPI tables (§3.3). Sits below
/// the conventional EBDA so OVMF can find the RSDP with no fw_cfg channel.
pub const RSDP_ADDR: u64 = 0x000E_0000;
pub const ACPI_STAGING_BASE: u64 = 0x000F_0000;
pub const ACPI_STAGING_SIZE: u64 = 0x0001_0000;

/// KVM slot numbers, fixed by §1.4's bring-up order.
pub const SLOT_LOW_RAM: u32 = 0;
pub const SLOT_HIGH_RAM: u32 = 1;
pub const SLOT_OVMF_CODE: u32 = 2;
/// The display's framebuffer BAR, registered as guest RAM so the guest can
/// paint into it at memory speed instead of taking an exit per pixel. It is
/// not part of the boot-time map: the slot is (re)registered wherever
/// firmware puts the BAR. See [`crate::display`].
pub const SLOT_FRAMEBUFFER: u32 = 3;

/// What a region is for; drives the KVM flags and who services faults.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RegionKind {
    /// Guest RAM, hugepage-backed, read-write.
    Ram,
    /// Firmware code: `KVM_MEM_READONLY`. A guest write faults to userspace
    /// and is ignored, logged once (§3.1).
    RomReadOnly,
    /// Served by `libvmm-core::pci::config_rw` on `KVM_EXIT_MMIO`.
    UserspaceMmio,
    /// Handled by the in-kernel LAPIC.
    KernelMmio,
}

/// One entry of the guest physical address map.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Region {
    pub name: &'static str,
    pub gpa: u64,
    pub size: u64,
    pub kind: RegionKind,
    /// `Some` only for regions registered with KVM_SET_USER_MEMORY_REGION.
    pub slot: Option<u32>,
}

impl Region {
    /// Inclusive end address.
    pub const fn end(&self) -> u64 {
        self.gpa + self.size - 1
    }

    pub const fn is_ram(&self) -> bool {
        matches!(self.kind, RegionKind::Ram)
    }
}

/// The complete guest physical memory map for one machine.
#[derive(Debug, Clone)]
pub struct GuestMemoryMap {
    pub low_ram: Region,
    /// `None` when the whole of RAM fits below the MMIO hole.
    pub high_ram: Option<Region>,
    pub ovmf_code: Region,
    pub mmio: Vec<Region>,
}

impl GuestMemoryMap {
    /// Build the map from `[memory]`.
    ///
    /// The config layer has already enforced the §1.3 invariants
    /// (1 GiB multiples, `low_ram_mb <= 3072`, split sums to the total); this
    /// re-checks them so the map can never be constructed inconsistently even
    /// if called from a path that skipped validation.
    pub fn new(m: &MemoryConfig) -> VmmResult<Self> {
        let low = m.low_ram_mb * MIB;
        let high = m.high_ram_mb * MIB;

        if (low + high) != m.size_mb * MIB {
            return Err(KvmError::MemoryMap {
                size_mb: m.size_mb,
                detail: format!(
                    "low {} MiB + high {} MiB != total {} MiB",
                    m.low_ram_mb, m.high_ram_mb, m.size_mb
                ),
            }
            .into());
        }
        if low > MMIO_HOLE_START {
            return Err(KvmError::MemoryMap {
                size_mb: m.size_mb,
                detail: format!(
                    "low RAM would end at {:#x}, overlapping the MMIO hole at {:#x}",
                    low, MMIO_HOLE_START
                ),
            }
            .into());
        }
        if low % GIB != 0 || high % GIB != 0 {
            return Err(KvmError::MemoryMap {
                size_mb: m.size_mb,
                detail: "RAM regions must be whole 1 GiB hugepages".to_string(),
            }
            .into());
        }

        let low_ram = Region {
            name: "low RAM",
            gpa: 0,
            size: low,
            kind: RegionKind::Ram,
            slot: Some(SLOT_LOW_RAM),
        };
        let high_ram = (high > 0).then_some(Region {
            name: "high RAM",
            gpa: HIGH_RAM_BASE,
            size: high,
            kind: RegionKind::Ram,
            slot: Some(SLOT_HIGH_RAM),
        });
        let ovmf_code = Region {
            name: "OVMF code",
            gpa: OVMF_CODE_BASE,
            size: OVMF_CODE_SIZE,
            kind: RegionKind::RomReadOnly,
            slot: Some(SLOT_OVMF_CODE),
        };
        let mmio = vec![
            Region {
                name: "ECAM / MMCONFIG",
                gpa: ECAM_BASE,
                size: ECAM_SIZE,
                kind: RegionKind::UserspaceMmio,
                slot: None,
            },
            Region {
                name: "PCIe BAR window",
                gpa: PCI_MMIO_BASE,
                size: PCI_MMIO_SIZE,
                kind: RegionKind::UserspaceMmio,
                slot: None,
            },
            Region {
                name: "I/O APIC",
                gpa: IOAPIC_BASE,
                size: IOAPIC_SIZE,
                kind: RegionKind::UserspaceMmio,
                slot: None,
            },
            Region {
                name: "Local APIC",
                gpa: LAPIC_BASE,
                size: LAPIC_SIZE,
                kind: RegionKind::KernelMmio,
                slot: None,
            },
        ];

        let map = GuestMemoryMap {
            low_ram,
            high_ram,
            ovmf_code,
            mmio,
        };
        map.assert_no_overlap()?;
        Ok(map)
    }

    /// Total guest RAM in bytes.
    pub fn ram_bytes(&self) -> u64 {
        self.low_ram.size + self.high_ram.as_ref().map_or(0, |r| r.size)
    }

    /// The regions registered with `KVM_SET_USER_MEMORY_REGION`, in the order
    /// §1.4 requires (slot 0 low, slot 1 high, slot 2 OVMF read-only).
    pub fn kvm_slots(&self) -> Vec<&Region> {
        let mut v = vec![&self.low_ram];
        if let Some(h) = self.high_ram.as_ref() {
            v.push(h);
        }
        v.push(&self.ovmf_code);
        v
    }

    /// Every region, ordered by guest physical address — the §1.3 table.
    pub fn all_regions(&self) -> Vec<&Region> {
        let mut v: Vec<&Region> = std::iter::once(&self.low_ram)
            .chain(self.mmio.iter())
            .chain(std::iter::once(&self.ovmf_code))
            .chain(self.high_ram.iter())
            .collect();
        v.sort_by_key(|r| r.gpa);
        v
    }

    /// Is this GPA backed by guest RAM?
    pub fn is_ram_address(&self, gpa: u64) -> bool {
        let in_low = gpa < self.low_ram.size;
        let in_high = self
            .high_ram
            .as_ref()
            .is_some_and(|h| gpa >= h.gpa && gpa <= h.end());
        in_low || in_high
    }

    /// Which userspace-MMIO region, if any, owns this GPA.
    pub fn mmio_region_for(&self, gpa: u64) -> Option<&Region> {
        self.mmio
            .iter()
            .find(|r| r.kind == RegionKind::UserspaceMmio && gpa >= r.gpa && gpa <= r.end())
    }

    fn assert_no_overlap(&self) -> VmmResult<()> {
        let regions = self.all_regions();
        for w in regions.windows(2) {
            let (a, b) = (w[0], w[1]);
            if a.end() >= b.gpa {
                return Err(KvmError::MemoryMap {
                    size_mb: self.ram_bytes() / MIB,
                    detail: format!(
                        "{} [{:#x}..={:#x}] overlaps {} [{:#x}..={:#x}]",
                        a.name,
                        a.gpa,
                        a.end(),
                        b.name,
                        b.gpa,
                        b.end()
                    ),
                }
                .into());
            }
        }
        Ok(())
    }

    /// Render the §1.3 table for the boot log.
    pub fn describe(&self) -> String {
        let mut s = String::from("guest physical memory map:\n");
        for r in self.all_regions() {
            let slot = match r.slot {
                Some(n) => format!("slot {n}"),
                None => match r.kind {
                    RegionKind::KernelMmio => "KVM".to_string(),
                    _ => "userspace MMIO".to_string(),
                },
            };
            s.push_str(&format!(
                "  {:#018x} - {:#018x}  {:>8}  {:<18} {}\n",
                r.gpa,
                r.end(),
                human(r.size),
                r.name,
                slot
            ));
        }
        s
    }
}

fn human(bytes: u64) -> String {
    if bytes >= GIB && bytes % GIB == 0 {
        format!("{} GiB", bytes / GIB)
    } else if bytes >= MIB {
        format!("{} MiB", bytes / MIB)
    } else {
        format!("{} KiB", bytes / 1024)
    }
}
