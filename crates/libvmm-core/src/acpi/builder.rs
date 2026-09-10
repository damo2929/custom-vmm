//! Builds the §3.3 table set: RSDP, XSDT, MCFG, MADT, SRAT, SLIT, DSDT,
//! FADT, TPM2, plus verbatim MSDM/SLIC injection.

use super::tables::{
    checksum8, declared_length, finalize, signature_of, verify_checksum, SdtHeader, HEADER_LEN,
};
use crate::error::VmmResult;
use crate::memory::{
    GuestMemoryMap, ACPI_STAGING_BASE, ACPI_STAGING_SIZE, ECAM_BASE, HIGH_RAM_BASE, IOAPIC_BASE,
    LAPIC_BASE, RSDP_ADDR,
};
use libvmm_config::{ConfigError, MachineConfig};
use std::path::Path;

/// The guest-visible reset register (FADT `RESET_REG`). A WSS `reboot` action
/// pulses this (§1.5, §8.5).
pub const RESET_REG_ADDR: u64 = 0x0000_0CF9;
pub const RESET_VALUE: u8 = 0x06;

/// Where the ACPI PM1a control block lives, used for the S5 soft-off write.
pub const PM1A_CNT_ADDR: u64 = 0x0000_0600;
/// TPM 2.0 control area, advertised by the TPM2 table (§6).
pub const TPM2_CONTROL_AREA: u64 = 0xFED4_0040;

/// One table placed in the staging area.
#[derive(Debug, Clone)]
pub struct LoadedTable {
    pub signature: String,
    pub gpa: u64,
    pub bytes: Vec<u8>,
    /// True when the bytes came verbatim from disk (MSDM/SLIC).
    pub injected: bool,
}

/// The complete linked table set, ready to copy into guest memory.
#[derive(Debug, Clone)]
pub struct AcpiTableSet {
    /// The RSDP, placed at [`RSDP_ADDR`].
    pub rsdp: Vec<u8>,
    pub rsdp_gpa: u64,
    /// XSDT plus every table it points at, in staging-area order.
    pub tables: Vec<LoadedTable>,
    /// Conditions that did not abort boot but MUST be logged (§3.3).
    pub warnings: Vec<String>,
}

impl AcpiTableSet {
    /// Total bytes occupied in the staging area.
    pub fn staging_bytes(&self) -> u64 {
        self.tables.iter().map(|t| t.bytes.len() as u64).sum()
    }

    /// Every table, including the RSDP, must sum to zero (§3.3).
    pub fn all_checksums_valid(&self) -> bool {
        // The RSDP has two checksums: the first 20 bytes, and all 36.
        let rsdp_ok = verify_checksum(&self.rsdp[..20]) && verify_checksum(&self.rsdp);
        rsdp_ok && self.tables.iter().all(|t| verify_checksum(&t.bytes))
    }

    pub fn find(&self, signature: &str) -> Option<&LoadedTable> {
        self.tables.iter().find(|t| t.signature == signature)
    }
}

/// Build the table set for a machine.
///
/// `read_file` is injected so the ACPI builder is testable without touching
/// the filesystem; production callers pass [`std::fs::read`].
pub fn build(
    cfg: &MachineConfig,
    map: &GuestMemoryMap,
    read_file: &dyn Fn(&Path) -> std::io::Result<Vec<u8>>,
) -> VmmResult<AcpiTableSet> {
    let mut warnings = Vec::new();
    let vcpus = cfg.compute.vcpus;

    // Generated tables, in the order §3.3 lists them.
    let mut bodies: Vec<Vec<u8>> = vec![
        build_mcfg(),
        build_madt(vcpus),
        build_srat(map),
        build_slit(),
        build_dsdt(),
        build_fadt(),
    ];
    if cfg.tpm.enabled {
        bodies.push(build_tpm2());
    }

    // Verbatim injection (§3.3): optional, but a bad checksum aborts boot.
    if cfg.acpi.enabled {
        for (path, label) in [
            (cfg.acpi.msdm_path.as_ref(), "MSDM"),
            (cfg.acpi.slic_path.as_ref(), "SLIC"),
        ] {
            match path {
                None => warnings.push(format!(
                    "acpi: no {} path configured, table skipped; activation will not apply (§3.3)",
                    label.to_lowercase()
                )),
                Some(p) => match read_file(p) {
                    Ok(bytes) => {
                        validate_injected(&bytes, label)?;
                        bodies.push(bytes);
                    }
                    Err(e) => {
                        // §3.3 makes injection optional: an unreadable table
                        // is skipped with a warning and boot continues —
                        // activation simply will not apply. Only a *corrupt*
                        // table is the hard 1010 failure, below.
                        warnings.push(format!(
                            "acpi: cannot read {} ({e}); {} table skipped, activation will not apply (§3.3)",
                            p.display(),
                            label
                        ));
                    }
                },
            }
        }
    }

    // Lay the tables out in the staging area and record their addresses so
    // the XSDT can point at them.
    // The XSDT holds a pointer to every table except the DSDT, which is
    // reached through the FADT instead.
    let xsdt_entries = bodies.iter().filter(|b| signature_of(b) != "DSDT").count();
    let xsdt_len = HEADER_LEN + 8 * xsdt_entries;
    let mut cursor = ACPI_STAGING_BASE + xsdt_len as u64;
    let mut tables: Vec<LoadedTable> = Vec::with_capacity(bodies.len() + 1);
    let mut pointers: Vec<u64> = Vec::with_capacity(bodies.len());

    for bytes in bodies {
        let signature = signature_of(&bytes);
        let injected = matches!(signature.as_str(), "MSDM" | "SLIC");
        let gpa = cursor;
        cursor += bytes.len() as u64;
        // The DSDT is deliberately *not* given an XSDT entry. ACPI reaches
        // it only through the FADT's DSDT/X_DSDT fields, and a firmware
        // that lists it in both places makes ACPICA load it twice.
        if signature != "DSDT" {
            pointers.push(gpa);
        }
        tables.push(LoadedTable {
            signature,
            gpa,
            bytes,
            injected,
        });
    }

    // Point the FADT at the DSDT now that both have addresses.
    //
    // Nothing else does this. Left at zero, ACPICA installs a table
    // descriptor whose address is 0, reports "Could not acquire table
    // length at 0000000000000000", and then oopses in
    // `acpi_tb_load_namespace` dereferencing it — which is exactly how
    // this was found, in a guest, rather than by any test here.
    link_fadt_to_dsdt(&mut tables)?;

    let xsdt = build_xsdt(&pointers);
    debug_assert_eq!(xsdt.len(), xsdt_len);
    tables.insert(
        0,
        LoadedTable {
            signature: "XSDT".to_string(),
            gpa: ACPI_STAGING_BASE,
            bytes: xsdt,
            injected: false,
        },
    );

    let used = cursor - ACPI_STAGING_BASE;
    if used > ACPI_STAGING_SIZE {
        return Err(ConfigError::AcpiChecksum {
            table: "staging".to_string(),
            detail: format!("table set is {used} bytes, staging area is {ACPI_STAGING_SIZE}"),
        }
        .into());
    }

    let rsdp = build_rsdp(ACPI_STAGING_BASE);
    let set = AcpiTableSet {
        rsdp,
        rsdp_gpa: RSDP_ADDR,
        tables,
        warnings,
    };

    // Belt and braces: refuse to hand back a set that does not verify.
    if !set.all_checksums_valid() {
        return Err(ConfigError::AcpiChecksum {
            table: "generated set".to_string(),
            detail: "a generated table does not sum to zero".to_string(),
        }
        .into());
    }
    Ok(set)
}

/// Write the DSDT's address into the FADT and re-checksum it.
///
/// FADT offsets, from ACPI 6.x table 5.9: `DSDT` is a 32-bit address at 40,
/// `X_DSDT` a 64-bit one at 140. Both are written — a 64-bit-capable OS
/// prefers `X_DSDT`, but firmware is expected to fill in both when the
/// address fits in 32 bits, and ours always does because the staging area
/// is in low memory.
fn link_fadt_to_dsdt(tables: &mut [LoadedTable]) -> VmmResult<()> {
    const FADT_DSDT: usize = 40;
    const FADT_X_DSDT: usize = 140;

    let dsdt_gpa = tables
        .iter()
        .find(|t| t.signature == "DSDT")
        .map(|t| t.gpa)
        .ok_or_else(|| ConfigError::AcpiChecksum {
            table: "FACP".to_string(),
            detail: "no DSDT was generated for the FADT to point at".to_string(),
        })?;

    let fadt = tables
        .iter_mut()
        .find(|t| t.signature == "FACP")
        .ok_or_else(|| ConfigError::AcpiChecksum {
            table: "FACP".to_string(),
            detail: "no FADT was generated".to_string(),
        })?;

    if fadt.bytes.len() < FADT_X_DSDT + 8 {
        return Err(ConfigError::AcpiChecksum {
            table: "FACP".to_string(),
            detail: format!(
                "FADT is {} bytes, too short to hold X_DSDT at {FADT_X_DSDT}",
                fadt.bytes.len()
            ),
        }
        .into());
    }

    let narrow = u32::try_from(dsdt_gpa).map_err(|_| ConfigError::AcpiChecksum {
        table: "FACP".to_string(),
        detail: format!("DSDT at {dsdt_gpa:#x} does not fit the 32-bit DSDT field"),
    })?;
    fadt.bytes[FADT_DSDT..FADT_DSDT + 4].copy_from_slice(&narrow.to_le_bytes());
    fadt.bytes[FADT_X_DSDT..FADT_X_DSDT + 8].copy_from_slice(&dsdt_gpa.to_le_bytes());

    // The checksum covers the whole table, so it has to be redone.
    fadt.bytes[9] = 0;
    let sum = fadt.bytes.iter().fold(0u8, |a, b| a.wrapping_add(*b));
    fadt.bytes[9] = (!sum).wrapping_add(1);
    Ok(())
}

/// §3.3: an injected binary that fails its checksum is rejected with 1010.
fn validate_injected(bytes: &[u8], label: &str) -> VmmResult<()> {
    if bytes.len() < HEADER_LEN {
        return Err(ConfigError::AcpiChecksum {
            table: label.to_string(),
            detail: format!("{} bytes is shorter than an ACPI header", bytes.len()),
        }
        .into());
    }
    let sig = signature_of(bytes);
    if sig != label {
        return Err(ConfigError::AcpiChecksum {
            table: label.to_string(),
            detail: format!("signature is \"{sig}\", expected \"{label}\""),
        }
        .into());
    }
    if let Some(declared) = declared_length(bytes) {
        if declared as usize != bytes.len() {
            return Err(ConfigError::AcpiChecksum {
                table: label.to_string(),
                detail: format!("header declares {declared} bytes, file is {}", bytes.len()),
            }
            .into());
        }
    }
    if !verify_checksum(bytes) {
        return Err(ConfigError::AcpiChecksum {
            table: label.to_string(),
            detail: "8-bit sum is non-zero".to_string(),
        }
        .into());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Individual tables
// ---------------------------------------------------------------------------

/// RSDP v2: 8-bit checksum over the first 20 bytes, extended checksum over
/// all 36. Points at the XSDT — there is no RSDT (no 32-bit legacy path).
fn build_rsdp(xsdt_gpa: u64) -> Vec<u8> {
    let mut r = Vec::with_capacity(36);
    r.extend_from_slice(b"RSD PTR ");
    r.push(0); // checksum, patched below
    r.extend_from_slice(b"RUSTVM");
    r.push(2); // revision 2 => XSDT present
    r.extend_from_slice(&0u32.to_le_bytes()); // RsdtAddress: unused
    r.extend_from_slice(&36u32.to_le_bytes()); // Length
    r.extend_from_slice(&xsdt_gpa.to_le_bytes());
    r.push(0); // extended checksum, patched below
    r.extend_from_slice(&[0u8; 3]); // reserved
    debug_assert_eq!(r.len(), 36);

    r[8] = checksum8(&r[..20]);
    r[32] = 0;
    r[32] = checksum8(&r);
    r
}

/// XSDT — 64-bit pointers to every other table.
fn build_xsdt(pointers: &[u64]) -> Vec<u8> {
    let mut t = Vec::new();
    SdtHeader::new(b"XSDT", 1, b"RVMMXSDT").write_into(&mut t);
    for p in pointers {
        t.extend_from_slice(&p.to_le_bytes());
    }
    finalize(&mut t);
    t
}

/// MCFG — ECAM base 0xC000_0000, buses 0..=255 (§2.1).
fn build_mcfg() -> Vec<u8> {
    let mut t = Vec::new();
    SdtHeader::new(b"MCFG", 1, b"RVMMMCFG").write_into(&mut t);
    t.extend_from_slice(&[0u8; 8]); // reserved
    t.extend_from_slice(&ECAM_BASE.to_le_bytes());
    t.extend_from_slice(&0u16.to_le_bytes()); // PCI segment group
    t.push(0); // start bus
    t.push(255); // end bus
    t.extend_from_slice(&[0u8; 4]); // reserved
    finalize(&mut t);
    t
}

/// MADT — one Local APIC per vCPU plus the userspace I/O APIC.
///
/// The PCAT_COMPAT flag is deliberately clear: there is no 8259 to mask,
/// because `KVM_CAP_SPLIT_IRQCHIP` never instantiates one (§1.4).
fn build_madt(vcpus: u32) -> Vec<u8> {
    const MADT_LAPIC: u8 = 0;
    const MADT_IOAPIC: u8 = 1;

    let mut t = Vec::new();
    SdtHeader::new(b"APIC", 5, b"RVMMAPIC").write_into(&mut t);
    t.extend_from_slice(&(LAPIC_BASE as u32).to_le_bytes());
    t.extend_from_slice(&0u32.to_le_bytes()); // flags: PCAT_COMPAT clear

    for cpu in 0..vcpus {
        t.push(MADT_LAPIC);
        t.push(8); // length
        t.push(cpu as u8); // ACPI processor UID
        t.push(cpu as u8); // APIC ID
        t.extend_from_slice(&1u32.to_le_bytes()); // enabled
    }

    t.push(MADT_IOAPIC);
    t.push(12); // length
    t.push(0); // I/O APIC ID
    t.push(0); // reserved
    t.extend_from_slice(&(IOAPIC_BASE as u32).to_le_bytes());
    t.extend_from_slice(&0u32.to_le_bytes()); // GSI base

    finalize(&mut t);
    t
}

/// SRAT — a single NUMA node covering both RAM ranges.
fn build_srat(map: &GuestMemoryMap) -> Vec<u8> {
    const SRAT_MEMORY: u8 = 1;

    let mut t = Vec::new();
    SdtHeader::new(b"SRAT", 3, b"RVMMSRAT").write_into(&mut t);
    t.extend_from_slice(&1u32.to_le_bytes()); // reserved, must be 1
    t.extend_from_slice(&[0u8; 8]); // reserved

    let mut affinity = |base: u64, len: u64| {
        t.push(SRAT_MEMORY);
        t.push(40); // length
        t.extend_from_slice(&0u32.to_le_bytes()); // proximity domain 0
        t.extend_from_slice(&[0u8; 2]); // reserved
        t.extend_from_slice(&base.to_le_bytes());
        t.extend_from_slice(&len.to_le_bytes());
        t.extend_from_slice(&[0u8; 4]); // reserved
        t.extend_from_slice(&1u32.to_le_bytes()); // enabled
        t.extend_from_slice(&[0u8; 8]); // reserved
    };
    affinity(0, map.low_ram.size);
    if let Some(h) = map.high_ram.as_ref() {
        affinity(HIGH_RAM_BASE, h.size);
    }

    finalize(&mut t);
    t
}

/// SLIT — one node, identity distance (10 to itself).
fn build_slit() -> Vec<u8> {
    let mut t = Vec::new();
    SdtHeader::new(b"SLIT", 1, b"RVMMSLIT").write_into(&mut t);
    t.extend_from_slice(&1u64.to_le_bytes()); // locality count
    t.push(10); // distance[0][0]
    finalize(&mut t);
    t
}

/// DSDT — PCIe root with `_OSC`, a power button, and the reset method.
///
/// The AML here is a hand-assembled minimum: a `_SB.PCI0` device with the
/// PNP0A08 (PCIe) HID, and a PNP0C0C power button so the guest raises an SCI
/// on the WSS `powerdown` action (§8.5).
fn build_dsdt() -> Vec<u8> {
    let mut t = Vec::new();
    SdtHeader::new(b"DSDT", 2, b"RVMMDSDT").write_into(&mut t);
    t.extend_from_slice(&aml::pcie_root_and_power_button());
    finalize(&mut t);
    t
}

/// FADT (revision 6) — the hardware-reduced description of the CloudHv
/// platform (Revision D.10).
///
/// # Why hardware-reduced
///
/// The machine has no chipset, and its power management is two byte-wide
/// registers at `0x0600`/`0x0601` plus a free-running timer at `0x0608`.
/// That is ACPI 5.0's hardware-reduced profile exactly, and the FADT has to
/// say so — the flag is not a preference, it tells the operating system
/// which half of this table to believe.
///
/// The previous version of this function claimed the opposite and supplied
/// none of what that implies: `HW_REDUCED_ACPI` clear but `FIRMWARE_CTRL`
/// zero, which is illegal because a FACS is mandatory then; `PM_TMR_BLK`
/// zero although the timer exists; `PM1a_EVT_BLK` at `0x05FC`, which
/// nothing decodes; and `PM1a_CNT_BLK` at `0x0600`, which on this platform
/// is `SLEEP_CONTROL_REG` and means something else entirely. Linux
/// tolerated all of it.
///
/// # Why the PM1 blocks exist anyway
///
/// A hardware-reduced platform has no PM1 event or control block. This one
/// declares both, at ports [`cloudhv::PM1A_EVT_IO_ADDRESS`] and
/// [`cloudhv::PM1A_CNT_IO_ADDRESS`], for the reason Cloud Hypervisor gives
/// in `vmm/src/acpi.rs`:
///
/// > Windows' nested-Hyper-V hvloader rejects a HW-reduced FADT whose PM1a
/// > GAS is zero; point the blocks at unused ACPI I/O ports (conforming
/// > guests ignore them).
///
/// [`cloudhv::PM1A_EVT_IO_ADDRESS`]: crate::cloudhv::PM1A_EVT_IO_ADDRESS
/// [`cloudhv::PM1A_CNT_IO_ADDRESS`]: crate::cloudhv::PM1A_CNT_IO_ADDRESS
fn build_fadt() -> Vec<u8> {
    use crate::cloudhv;

    /// bit 1 = 8042 absent, bit 2 = no VGA, bit 5 = no CMOS RTC.
    ///
    /// The RTC bit is set and the machine does have a CMOS at `0x70`. That
    /// is not a contradiction: on a hardware-reduced platform ACPI has no
    /// RTC, and a guest reads the wall clock through UEFI's `GetTime`,
    /// which is the firmware reading that same CMOS. What the bit forbids
    /// is the guest going behind the firmware's back.
    const IAPC_LEGACY_FREE: u16 = (1 << 1) | (1 << 2) | (1 << 5);
    /// `TMR_VAL_EXT` (bit 8): the PM timer counts 32 bits, not 24.
    /// `RESET_REG_SUP` (bit 10): `RESET_REG` below is real.
    /// `HW_REDUCED_ACPI` (bit 20): believe the sleep registers, not the
    /// PM1 control block.
    const FADT_FLAGS: u32 = (1 << 8) | (1 << 10) | (1 << 20);

    let mut t = Vec::new();
    SdtHeader::new(b"FACP", 6, b"RVMMFACP").write_into(&mut t);
    // FIRMWARE_CTRL and X_FIRMWARE_CTRL stay zero. ACPI 6.5 §5.2.9: with
    // HW_REDUCED_ACPI set there is no FACS, and pointing at one is an
    // error rather than a courtesy.
    t.extend_from_slice(&0u32.to_le_bytes()); // FIRMWARE_CTRL (32-bit)
    t.extend_from_slice(&0u32.to_le_bytes()); // DSDT (32-bit), filled in by link_fadt_to_dsdt
    t.push(0); // reserved
    t.push(0); // preferred PM profile: unspecified
    t.extend_from_slice(&9u16.to_le_bytes()); // SCI_INT
    t.extend_from_slice(&0u32.to_le_bytes()); // SMI_CMD: none, no SMM
    t.push(0); // ACPI_ENABLE
    t.push(0); // ACPI_DISABLE
    t.push(0); // S4BIOS_REQ
    t.push(0); // PSTATE_CNT
    t.extend_from_slice(&u32::from(cloudhv::PM1A_EVT_IO_ADDRESS).to_le_bytes()); // PM1a_EVT_BLK
    t.extend_from_slice(&0u32.to_le_bytes()); // PM1b_EVT_BLK
    t.extend_from_slice(&u32::from(cloudhv::PM1A_CNT_IO_ADDRESS).to_le_bytes()); // PM1a_CNT_BLK
    t.extend_from_slice(&0u32.to_le_bytes()); // PM1b_CNT_BLK
    t.extend_from_slice(&0u32.to_le_bytes()); // PM2_CNT_BLK
    t.extend_from_slice(&u32::from(cloudhv::ACPI_TIMER_IO_ADDRESS).to_le_bytes()); // PM_TMR_BLK
    t.extend_from_slice(&0u32.to_le_bytes()); // GPE0_BLK
    t.extend_from_slice(&0u32.to_le_bytes()); // GPE1_BLK
    t.push(4); // PM1_EVT_LEN
    t.push(2); // PM1_CNT_LEN
    t.push(0); // PM2_CNT_LEN
    t.push(4); // PM_TMR_LEN
    t.push(0); // GPE0_BLK_LEN
    t.push(0); // GPE1_BLK_LEN
    t.push(0); // GPE1_BASE
    t.push(0); // CST_CNT
    t.extend_from_slice(&0u16.to_le_bytes()); // P_LVL2_LAT
    t.extend_from_slice(&0u16.to_le_bytes()); // P_LVL3_LAT
    t.extend_from_slice(&0u16.to_le_bytes()); // FLUSH_SIZE
    t.extend_from_slice(&0u16.to_le_bytes()); // FLUSH_STRIDE
    t.push(0); // DUTY_OFFSET
    t.push(0); // DUTY_WIDTH
    t.push(0); // DAY_ALRM
    t.push(0); // MON_ALRM
    t.push(0); // CENTURY
    t.extend_from_slice(&IAPC_LEGACY_FREE.to_le_bytes()); // IAPC_BOOT_ARCH
    t.push(0); // reserved
    t.extend_from_slice(&FADT_FLAGS.to_le_bytes()); // flags

    // RESET_REG — a GAS in system I/O space, decoded by `CloudHvPm`.
    t.extend_from_slice(&gas_io(RESET_REG_ADDR, 8));
    t.push(RESET_VALUE); // RESET_VALUE
    t.extend_from_slice(&0u16.to_le_bytes()); // ARM_BOOT_ARCH
    t.push(3); // FADT minor version — ACPI 6.3

    t.extend_from_slice(&0u64.to_le_bytes()); // X_FIRMWARE_CTRL
    t.extend_from_slice(&0u64.to_le_bytes()); // X_DSDT, filled in by link_fadt_to_dsdt
    t.extend_from_slice(&gas_io(u64::from(cloudhv::PM1A_EVT_IO_ADDRESS), 32)); // X_PM1a_EVT_BLK
    t.extend_from_slice(&[0u8; 12]); // X_PM1b_EVT_BLK
    t.extend_from_slice(&gas_io(u64::from(cloudhv::PM1A_CNT_IO_ADDRESS), 16)); // X_PM1a_CNT_BLK
    t.extend_from_slice(&[0u8; 12]); // X_PM1b_CNT_BLK
    t.extend_from_slice(&[0u8; 12]); // X_PM2_CNT_BLK
    t.extend_from_slice(&gas_io(u64::from(cloudhv::ACPI_TIMER_IO_ADDRESS), 32)); // X_PM_TMR_BLK
    t.extend_from_slice(&[0u8; 12]); // X_GPE0_BLK
    t.extend_from_slice(&[0u8; 12]); // X_GPE1_BLK
                                     // The two registers that *are* the power management on this machine.
    t.extend_from_slice(&gas_io(u64::from(cloudhv::ACPI_SHUTDOWN_IO_ADDRESS), 8)); // SLEEP_CONTROL_REG
    t.extend_from_slice(&gas_io(u64::from(cloudhv::ACPI_SLEEP_STATUS_IO_ADDRESS), 8)); // SLEEP_STATUS_REG
    t.extend_from_slice(b"RUSTVMM "); // Hypervisor Vendor Identity

    finalize(&mut t);
    t
}

/// TPM2 — advertises the virtio-tpm control area to the guest (§6).
fn build_tpm2() -> Vec<u8> {
    let mut t = Vec::new();
    SdtHeader::new(b"TPM2", 4, b"RVMMTPM2").write_into(&mut t);
    t.extend_from_slice(&0u16.to_le_bytes()); // platform class: client
    t.extend_from_slice(&0u16.to_le_bytes()); // reserved
    t.extend_from_slice(&TPM2_CONTROL_AREA.to_le_bytes());
    t.extend_from_slice(&7u32.to_le_bytes()); // start method: CRB
    finalize(&mut t);
    t
}

/// A Generic Address Structure in system-I/O space.
fn gas_io(address: u64, bit_width: u8) -> [u8; 12] {
    let mut g = [0u8; 12];
    g[0] = 1; // address space: system I/O
    g[1] = bit_width;
    g[2] = 0; // bit offset
    g[3] = match bit_width {
        8 => 1,
        16 => 2,
        32 => 3,
        _ => 0,
    };
    g[4..12].copy_from_slice(&address.to_le_bytes());
    g
}

/// Minimal hand-assembled AML for the DSDT body.
mod aml {
    const OP_SCOPE: u8 = 0x10;
    const OP_DEVICE_EXT: [u8; 2] = [0x5B, 0x82];
    const OP_NAME: u8 = 0x08;
    const OP_STRING: u8 = 0x0D;
    const OP_DWORD: u8 = 0x0C;

    /// `_SB` containing `PCI0` (PNP0A08) and `PWRB` (PNP0C0C).
    pub fn pcie_root_and_power_button() -> Vec<u8> {
        let mut sb = Vec::new();
        sb.extend_from_slice(b"_SB_");
        sb.extend_from_slice(&device("PCI0", &pci0_body()));
        sb.extend_from_slice(&device("PWRB", &pwrb_body()));
        let mut out = vec![OP_SCOPE];
        out.extend_from_slice(&pkg_length(sb.len()));
        out.extend_from_slice(&sb);
        out
    }

    fn pci0_body() -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&name_eisaid("_HID", 0x0A08_D041)); // PNP0A08, PCIe
        b.extend_from_slice(&name_eisaid("_CID", 0x030A_D041)); // PNP0A03, PCI
        b.extend_from_slice(&name_dword("_UID", 0));
        b.extend_from_slice(&name_dword("_BBN", 0));
        b.extend_from_slice(&name_string("_STR", "PCIe Root Bridge"));
        b
    }

    fn pwrb_body() -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&name_eisaid("_HID", 0x0C0C_D041)); // PNP0C0C
        b.extend_from_slice(&name_dword("_UID", 0));
        b
    }

    fn device(name: &str, body: &[u8]) -> Vec<u8> {
        let mut inner = Vec::with_capacity(4 + body.len());
        inner.extend_from_slice(name.as_bytes());
        inner.extend_from_slice(body);
        let mut out = Vec::new();
        out.extend_from_slice(&OP_DEVICE_EXT);
        out.extend_from_slice(&pkg_length(inner.len()));
        out.extend_from_slice(&inner);
        out
    }

    fn name_dword(name: &str, value: u32) -> Vec<u8> {
        let mut v = vec![OP_NAME];
        v.extend_from_slice(name.as_bytes());
        v.push(OP_DWORD);
        v.extend_from_slice(&value.to_le_bytes());
        v
    }

    fn name_eisaid(name: &str, eisaid: u32) -> Vec<u8> {
        name_dword(name, eisaid)
    }

    fn name_string(name: &str, s: &str) -> Vec<u8> {
        let mut v = vec![OP_NAME];
        v.extend_from_slice(name.as_bytes());
        v.push(OP_STRING);
        v.extend_from_slice(s.as_bytes());
        v.push(0);
        v
    }

    /// AML PkgLength, which encodes its own byte count in the top two bits of
    /// the lead byte. `len` is the payload; the result covers payload+prefix.
    fn pkg_length(payload: usize) -> Vec<u8> {
        for prefix in 1..=4usize {
            let total = payload + prefix;
            let encoded = encode_pkg_length(total);
            if encoded.len() == prefix {
                return encoded;
            }
        }
        encode_pkg_length(payload + 4)
    }

    fn encode_pkg_length(total: usize) -> Vec<u8> {
        if total < 0x40 {
            vec![total as u8]
        } else if total < 0x1000 {
            vec![0x40 | (total & 0x0F) as u8, (total >> 4) as u8]
        } else if total < 0x10_0000 {
            vec![
                0x80 | (total & 0x0F) as u8,
                (total >> 4) as u8,
                (total >> 12) as u8,
            ]
        } else {
            vec![
                0xC0 | (total & 0x0F) as u8,
                (total >> 4) as u8,
                (total >> 12) as u8,
                (total >> 20) as u8,
            ]
        }
    }
}
