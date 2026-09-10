//! §3.2 SMBIOS and §3.3 ACPI: verbatim serial, checksums, optional injection.

use libvmm_config::MachineConfig;
use libvmm_core::acpi::{self, tables, AcpiTableSet};
use libvmm_core::memory::GuestMemoryMap;
use libvmm_core::smbios::SmbiosType1;
use std::path::Path;

fn reference() -> MachineConfig {
    MachineConfig::from_toml_str(include_str!("../../../config/reference-vm.toml")).unwrap()
}

/// Build a well-formed table with the given signature and a valid checksum.
fn synthetic_table(signature: &[u8; 4], extra: usize) -> Vec<u8> {
    let mut t = Vec::new();
    tables::SdtHeader::new(signature, 1, b"TESTTBL_").write_into(&mut t);
    t.extend_from_slice(&vec![0xAB; extra]);
    tables::finalize(&mut t);
    t
}

fn build_with(
    cfg: &MachineConfig,
    files: Vec<(&str, Vec<u8>)>,
) -> Result<AcpiTableSet, libvmm_core::VmmError> {
    let map = GuestMemoryMap::new(&cfg.memory).unwrap();
    let reader = move |p: &Path| -> std::io::Result<Vec<u8>> {
        let name = p.file_name().unwrap().to_string_lossy().to_string();
        files
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, b)| b.clone())
            .ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, name))
    };
    acpi::builder::build(cfg, &map, &reader)
}

// -- §3.2 --------------------------------------------------------------------

#[test]
fn smbios_serial_is_vm_name_byte_for_byte() {
    let cfg = reference();
    let t1 = SmbiosType1::from_config(&cfg.vm);
    assert_eq!(t1.serial_number, cfg.vm.name);

    // And it survives serialisation into the string table unchanged — no
    // hash, no prefix, no UUID substitution.
    let bytes = t1.to_bytes(0x0100);
    assert_eq!(
        SmbiosType1::serial_from_bytes(&bytes).as_deref(),
        Some(cfg.vm.name.as_str())
    );
}

#[test]
fn smbios_uuid_equals_vm_id() {
    let cfg = reference();
    let t1 = SmbiosType1::from_config(&cfg.vm);
    let expected = uuid::Uuid::parse_str(&cfg.vm.id).unwrap();
    assert_eq!(t1.uuid, *expected.as_bytes());
}

// -- §3.3 checksums ----------------------------------------------------------

#[test]
fn every_generated_table_sums_to_zero() {
    let cfg = reference();
    let set = build_with(
        &cfg,
        vec![
            ("msdm.bin", synthetic_table(b"MSDM", 32)),
            ("slic.bin", synthetic_table(b"SLIC", 300)),
        ],
    )
    .unwrap();
    assert!(set.all_checksums_valid());
    for t in &set.tables {
        assert!(
            tables::verify_checksum(&t.bytes),
            "{} does not sum to zero",
            t.signature
        );
    }
}

#[test]
fn the_expected_table_set_is_present() {
    let cfg = reference();
    let set = build_with(
        &cfg,
        vec![
            ("msdm.bin", synthetic_table(b"MSDM", 32)),
            ("slic.bin", synthetic_table(b"SLIC", 300)),
        ],
    )
    .unwrap();
    for sig in [
        "XSDT", "MCFG", "APIC", "SRAT", "SLIT", "DSDT", "FACP", "TPM2", "MSDM", "SLIC",
    ] {
        assert!(
            set.find(sig).is_some(),
            "{sig} is missing from the table set"
        );
    }
}

#[test]
fn rsdp_points_at_the_xsdt() {
    let cfg = reference();
    let set = build_with(
        &cfg,
        vec![
            ("msdm.bin", synthetic_table(b"MSDM", 8)),
            ("slic.bin", synthetic_table(b"SLIC", 8)),
        ],
    )
    .unwrap();
    let xsdt_addr = u64::from_le_bytes(set.rsdp[24..32].try_into().unwrap());
    assert_eq!(xsdt_addr, set.find("XSDT").unwrap().gpa);
    assert_eq!(&set.rsdp[0..8], b"RSD PTR ");
    assert_eq!(
        set.rsdp[15], 2,
        "RSDP must be revision 2 so the XSDT is used"
    );
}

#[test]
fn injected_table_with_a_bad_checksum_aborts_boot_with_1010() {
    let cfg = reference();
    let mut bad = synthetic_table(b"MSDM", 32);
    // Corrupt a byte after the checksum so the 8-bit sum no longer zeroes.
    let last = bad.len() - 1;
    bad[last] = bad[last].wrapping_add(1);

    let err = build_with(
        &cfg,
        vec![("msdm.bin", bad), ("slic.bin", synthetic_table(b"SLIC", 8))],
    )
    .expect_err("a bad injected checksum MUST abort boot");
    assert_eq!(err.code(), 1010, "{err}");
}

#[test]
fn injected_table_with_the_wrong_signature_is_rejected() {
    let cfg = reference();
    let err = build_with(
        &cfg,
        vec![
            ("msdm.bin", synthetic_table(b"SSDT", 32)),
            ("slic.bin", synthetic_table(b"SLIC", 8)),
        ],
    )
    .expect_err("signature mismatch MUST abort boot");
    assert_eq!(err.code(), 1010);
}

#[test]
fn absent_injection_path_skips_the_table_with_a_warning() {
    // §3.3: injection is optional; boot continues.
    let mut cfg = reference();
    cfg.acpi.msdm_path = None;
    cfg.acpi.slic_path = None;
    let set = build_with(&cfg, vec![]).unwrap();
    assert!(set.find("MSDM").is_none());
    assert!(set.find("SLIC").is_none());
    assert_eq!(set.warnings.len(), 2);
    assert!(set.warnings.iter().all(|w| w.contains("skipped")));
    assert!(set.all_checksums_valid());
}

#[test]
fn madt_has_one_lapic_per_vcpu_and_no_pcat_compat() {
    let mut cfg = reference();
    cfg.compute.vcpus = 4;
    let set = build_with(
        &cfg,
        vec![
            ("msdm.bin", synthetic_table(b"MSDM", 8)),
            ("slic.bin", synthetic_table(b"SLIC", 8)),
        ],
    )
    .unwrap();
    let madt = &set.find("APIC").unwrap().bytes;

    // Flags immediately after the 36-byte header and the 4-byte LAPIC address.
    let flags = u32::from_le_bytes(madt[40..44].try_into().unwrap());
    assert_eq!(
        flags & 1,
        0,
        "PCAT_COMPAT must be clear: there is no 8259 (§1.4)"
    );

    // Walk the interrupt-controller structures.
    let mut lapics = 0;
    let mut ioapics = 0;
    let mut off = 44;
    while off + 2 <= madt.len() {
        let (kind, len) = (madt[off], madt[off + 1] as usize);
        if len == 0 {
            break;
        }
        match kind {
            0 => lapics += 1,
            1 => ioapics += 1,
            _ => {}
        }
        off += len;
    }
    assert_eq!(lapics, 4);
    assert_eq!(ioapics, 1);
}

#[test]
fn fadt_carries_a_reset_register() {
    let cfg = reference();
    let set = build_with(
        &cfg,
        vec![
            ("msdm.bin", synthetic_table(b"MSDM", 8)),
            ("slic.bin", synthetic_table(b"SLIC", 8)),
        ],
    )
    .unwrap();
    let fadt = &set.find("FACP").unwrap().bytes;
    // RESET_REG is a 12-byte GAS at offset 116; RESET_VALUE follows at 128.
    let reset_addr = u64::from_le_bytes(fadt[120..128].try_into().unwrap());
    assert_eq!(reset_addr, acpi::builder::RESET_REG_ADDR);
    assert_eq!(fadt[128], acpi::builder::RESET_VALUE);
}

/// Revision D.10: the FADT must describe the machine that exists, not a
/// PC-compatible one. Every field checked here is a register something in
/// `crate::cloudhv` actually decodes, and the flags say which half of the
/// table to believe.
#[test]
fn the_fadt_describes_the_hardware_reduced_platform_it_runs_on() {
    use libvmm_core::cloudhv;

    let cfg = reference();
    let set = build_with(
        &cfg,
        vec![
            ("msdm.bin", synthetic_table(b"MSDM", 8)),
            ("slic.bin", synthetic_table(b"SLIC", 8)),
        ],
    )
    .unwrap();
    let fadt = &set.find("FACP").unwrap().bytes;
    assert_eq!(fadt.len(), 276, "a revision 6 FADT");

    let flags = u32::from_le_bytes(fadt[112..116].try_into().unwrap());
    assert_ne!(flags & (1 << 20), 0, "HW_REDUCED_ACPI");
    assert_ne!(flags & (1 << 10), 0, "RESET_REG_SUP");
    assert_ne!(
        flags & (1 << 8),
        0,
        "TMR_VAL_EXT — and CloudHvPm's timer is the 32-bit one"
    );

    // With HW_REDUCED_ACPI set there is no FACS, and pointing at one is an
    // error rather than a courtesy.
    assert_eq!(u32::from_le_bytes(fadt[36..40].try_into().unwrap()), 0);
    assert_eq!(u64::from_le_bytes(fadt[132..140].try_into().unwrap()), 0);

    // A GAS is space(1) width(1) offset(1) size(1) address(8).
    let gas = |at: usize| -> (u8, u8, u64) {
        (
            fadt[at],
            fadt[at + 1],
            u64::from_le_bytes(fadt[at + 4..at + 12].try_into().unwrap()),
        )
    };
    const SYSTEM_IO: u8 = 1;

    assert_eq!(
        gas(244),
        (SYSTEM_IO, 8, u64::from(cloudhv::ACPI_SHUTDOWN_IO_ADDRESS)),
        "SLEEP_CONTROL_REG"
    );
    assert_eq!(
        gas(256),
        (
            SYSTEM_IO,
            8,
            u64::from(cloudhv::ACPI_SLEEP_STATUS_IO_ADDRESS)
        ),
        "SLEEP_STATUS_REG"
    );
    assert_eq!(
        gas(208),
        (SYSTEM_IO, 32, u64::from(cloudhv::ACPI_TIMER_IO_ADDRESS)),
        "X_PM_TMR_BLK"
    );

    // The PM1 blocks a hardware-reduced platform is not supposed to have.
    // Windows' hvloader refuses a HW-reduced FADT whose PM1a GAS is zero,
    // so they are declared, and `CloudHvPm` decodes them.
    assert_eq!(
        gas(148),
        (SYSTEM_IO, 32, u64::from(cloudhv::PM1A_EVT_IO_ADDRESS)),
        "X_PM1a_EVT_BLK"
    );
    assert_eq!(
        gas(172),
        (SYSTEM_IO, 16, u64::from(cloudhv::PM1A_CNT_IO_ADDRESS)),
        "X_PM1a_CNT_BLK"
    );
    for port in [
        cloudhv::PM1A_EVT_IO_ADDRESS,
        cloudhv::PM1A_CNT_IO_ADDRESS,
        cloudhv::ACPI_TIMER_IO_ADDRESS,
        cloudhv::ACPI_SHUTDOWN_IO_ADDRESS,
        cloudhv::ACPI_SLEEP_STATUS_IO_ADDRESS,
        cloudhv::RESET_IO_ADDRESS,
    ] {
        assert!(
            cloudhv::CloudHvPm::claims(port),
            "the FADT names {port:#06x}, so something must answer it"
        );
    }
}

#[test]
fn tpm2_table_is_omitted_when_the_tpm_is_disabled() {
    let mut cfg = reference();
    cfg.tpm.enabled = false;
    let set = build_with(
        &cfg,
        vec![
            ("msdm.bin", synthetic_table(b"MSDM", 8)),
            ("slic.bin", synthetic_table(b"SLIC", 8)),
        ],
    )
    .unwrap();
    assert!(set.find("TPM2").is_none());
}

/// The FADT must point at the DSDT, and the DSDT must not be in the XSDT.
///
/// Both halves of this were wrong, and neither was caught by a checksum: a
/// table set can verify perfectly and still be unusable. What found it was
/// a Linux guest, which reported `Could not acquire table length at
/// 0000000000000000` and then oopsed in `acpi_tb_load_namespace`
/// dereferencing the null descriptor.
#[test]
fn the_fadt_points_at_the_dsdt_and_the_xsdt_does_not() {
    const FADT_DSDT: usize = 40;
    const FADT_X_DSDT: usize = 140;

    let cfg = reference();
    let map = libvmm_core::memory::GuestMemoryMap::new(&cfg.memory).expect("memory map");
    let set = libvmm_core::acpi::builder::build(&cfg, &map, &|p| std::fs::read(p))
        .expect("build the table set");

    let dsdt = set.find("DSDT").expect("a DSDT must be generated");
    let fadt = set.find("FACP").expect("a FADT must be generated");

    let narrow = u32::from_le_bytes(
        fadt.bytes[FADT_DSDT..FADT_DSDT + 4]
            .try_into()
            .expect("4 bytes"),
    );
    let wide = u64::from_le_bytes(
        fadt.bytes[FADT_X_DSDT..FADT_X_DSDT + 8]
            .try_into()
            .expect("8 bytes"),
    );
    assert_ne!(narrow, 0, "FADT.DSDT must not be null");
    assert_eq!(u64::from(narrow), dsdt.gpa, "FADT.DSDT must be the DSDT");
    assert_eq!(wide, dsdt.gpa, "FADT.X_DSDT must be the DSDT");

    // Patching the address invalidated the checksum, so it had to be redone.
    assert!(
        set.all_checksums_valid(),
        "the FADT must still sum to zero after being linked to the DSDT"
    );

    // ACPI reaches the DSDT only through the FADT. A firmware that also
    // lists it in the XSDT makes ACPICA load the namespace twice.
    let xsdt = set.find("XSDT").expect("an XSDT must be generated");
    let entries: Vec<u64> = xsdt.bytes[36..]
        .chunks_exact(8)
        .map(|c| u64::from_le_bytes(c.try_into().expect("8 bytes")))
        .collect();
    assert!(
        !entries.contains(&dsdt.gpa),
        "the DSDT must not have an XSDT entry"
    );
    assert!(
        entries.contains(&fadt.gpa),
        "the FADT must have an XSDT entry"
    );
}
