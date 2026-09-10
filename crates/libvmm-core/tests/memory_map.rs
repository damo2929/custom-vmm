//! The §1.3 reference memory map, pinned exactly as the spec tabulates it.

use libvmm_config::MachineConfig;
use libvmm_core::memory::*;

fn reference() -> MachineConfig {
    MachineConfig::from_toml_str(include_str!("../../../config/reference-vm.toml")).unwrap()
}

#[test]
fn reference_map_matches_the_spec_1_3_table() {
    let cfg = reference();
    let m = GuestMemoryMap::new(&cfg.memory).unwrap();

    // 0x0000_0000 - 0xBFFF_FFFF  3 GiB  low RAM  slot 0
    assert_eq!(m.low_ram.gpa, 0x0000_0000);
    assert_eq!(m.low_ram.end(), 0xBFFF_FFFF);
    assert_eq!(m.low_ram.size, 3 * GIB);
    assert_eq!(m.low_ram.slot, Some(0));

    // 0x1_0000_0000 - 0x2_3FFF_FFFF  5 GiB  high RAM  slot 1
    let high = m.high_ram.as_ref().unwrap();
    assert_eq!(high.gpa, 0x1_0000_0000);
    assert_eq!(high.end(), 0x2_3FFF_FFFF);
    assert_eq!(high.size, 5 * GIB);
    assert_eq!(high.slot, Some(1));

    // 0xFFC0_0000 - 0xFFFF_FFFF  4 MiB  OVMF code  slot 2, read-only
    assert_eq!(m.ovmf_code.gpa, 0xFFC0_0000);
    assert_eq!(m.ovmf_code.end(), 0xFFFF_FFFF);
    assert_eq!(m.ovmf_code.kind, RegionKind::RomReadOnly);
    assert_eq!(m.ovmf_code.slot, Some(2));

    assert_eq!(m.ram_bytes(), 8 * GIB);
}

#[test]
fn mmio_hole_is_exactly_one_gib() {
    // §1.3 invariant.
    assert_eq!(MMIO_HOLE_START, 0xC000_0000);
    assert_eq!(MMIO_HOLE_END, 0xFFFF_FFFF);
    assert_eq!(MMIO_HOLE_SIZE, GIB);
}

#[test]
fn low_ram_ends_at_or_below_the_hole() {
    let cfg = reference();
    let m = GuestMemoryMap::new(&cfg.memory).unwrap();
    assert!(m.low_ram.end() < MMIO_HOLE_START);
}

#[test]
fn no_region_overlaps_another() {
    let cfg = reference();
    let m = GuestMemoryMap::new(&cfg.memory).unwrap();
    let regions = m.all_regions();
    for w in regions.windows(2) {
        assert!(
            w[0].end() < w[1].gpa,
            "{} [{:#x}..={:#x}] overlaps {} at {:#x}",
            w[0].name,
            w[0].gpa,
            w[0].end(),
            w[1].name,
            w[1].gpa
        );
    }
}

#[test]
fn kvm_slots_are_registered_in_spec_order() {
    let cfg = reference();
    let m = GuestMemoryMap::new(&cfg.memory).unwrap();
    let slots: Vec<u32> = m.kvm_slots().iter().filter_map(|r| r.slot).collect();
    assert_eq!(slots, vec![SLOT_LOW_RAM, SLOT_HIGH_RAM, SLOT_OVMF_CODE]);
}

#[test]
fn ecam_and_bar_window_sit_inside_the_hole() {
    // The whole PCIe window lives inside the hole, leaving RAM untouched.
    const _: () = assert!(ECAM_BASE >= MMIO_HOLE_START);
    const _: () = assert!(ECAM_BASE + ECAM_SIZE - 1 == 0xCFFF_FFFF);
    assert_eq!(PCI_MMIO_BASE, 0xD000_0000);
    assert_eq!(PCI_MMIO_END, 0xFEBF_FFFF);
    assert_eq!(IOAPIC_BASE, 0xFEC0_0000);
    assert_eq!(LAPIC_BASE, 0xFEE0_0000);
}

#[test]
fn reset_vector_lands_in_the_ovmf_region() {
    let cfg = reference();
    let m = GuestMemoryMap::new(&cfg.memory).unwrap();
    assert!(RESET_VECTOR >= m.ovmf_code.gpa && RESET_VECTOR <= m.ovmf_code.end());
}

#[test]
fn mmio_addresses_are_not_reported_as_ram() {
    let cfg = reference();
    let m = GuestMemoryMap::new(&cfg.memory).unwrap();
    assert!(m.is_ram_address(0x1000));
    assert!(m.is_ram_address(0x1_0000_1000));
    assert!(!m.is_ram_address(ECAM_BASE));
    assert!(!m.is_ram_address(IOAPIC_BASE));
    assert_eq!(
        m.mmio_region_for(ECAM_BASE + 0x100).unwrap().name,
        "ECAM / MMCONFIG"
    );
    // The LAPIC is in-kernel, so it is not a userspace MMIO region.
    assert!(m.mmio_region_for(LAPIC_BASE).is_none());
}

#[test]
fn all_ram_below_the_hole_needs_no_high_region() {
    let mut cfg = reference();
    cfg.memory.size_mb = 2048;
    cfg.memory.low_ram_mb = 2048;
    cfg.memory.high_ram_mb = 0;
    let m = GuestMemoryMap::new(&cfg.memory).unwrap();
    assert!(m.high_ram.is_none());
    assert_eq!(m.kvm_slots().len(), 2);
}
