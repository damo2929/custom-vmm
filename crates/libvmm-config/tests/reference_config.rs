//! The §11 reference machine must load, validate, and expose exactly the
//! values the specification states. Every negative test below pins one
//! documented MUST to its Appendix A error code.

use libvmm_config::{EngineKind, MachineConfig, RateControl};

const REFERENCE: &str = include_str!("../../../config/reference-vm.toml");

fn reference() -> MachineConfig {
    MachineConfig::from_toml_str(REFERENCE).expect("§11 reference config must load")
}

/// Mutate one line of the reference TOML and expect a specific error code.
fn expect_code(patched: &str, code: u32) {
    match MachineConfig::from_toml_str(patched) {
        Ok(_) => panic!("expected config error {code}, config was accepted"),
        Err(e) => assert_eq!(e.code(), code, "wrong error code: {e}"),
    }
}

fn patch(from: &str, to: &str) -> String {
    assert!(REFERENCE.contains(from), "patch anchor not present: {from}");
    REFERENCE.replacen(from, to, 1)
}

#[test]
fn reference_config_loads() {
    let c = reference();
    assert_eq!(c.vm.name, "production-windows-workload-01");
    assert_eq!(c.compute.vcpus, 2);
    assert_eq!(c.memory.size_mb, 8192);
    assert_eq!(c.memory.low_ram_mb, 3072);
    assert_eq!(c.memory.high_ram_mb, 5120);
    assert_eq!(c.firmware.code_start_addr, 0xFFC0_0000);
    assert_eq!(c.storage.drives.len(), 5);
    assert_eq!(c.network.cards.len(), 1);
}

/// Revision E: every drive the guest sees is either a solid-state disk or an
/// optical drive, and the reference machine has both.
#[test]
fn every_block_drive_is_a_solid_state_disk_that_supports_trim() {
    use libvmm_config::DriveMedium;
    let c = reference();

    let block: Vec<_> = c
        .storage
        .drives
        .iter()
        .filter(|d| !d.medium.is_optical())
        .collect();
    assert_eq!(block.len(), 4);
    for d in &block {
        assert_eq!(
            d.medium,
            DriveMedium::Ssd,
            "drive {} is not optical, so it is an SSD; there is no other \
             non-optical medium",
            d.drive_id
        );
        assert_eq!(d.medium.rotation_rate(), Some(libvmm_config::NON_ROTATING));
        assert!(d.medium.supports_discard(), "an SSD supports TRIM");
    }
    // And discard cannot be switched off underneath them.
    assert!(c.storage.discard_unmap);

    let optical: Vec<_> = c
        .storage
        .drives
        .iter()
        .filter(|d| d.medium.is_optical())
        .collect();
    assert_eq!(optical.len(), 1, "the reference machine has one DVD-ROM");
    let dvd = optical[0];
    assert_eq!(dvd.medium, DriveMedium::DvdRom);
    assert_eq!(dvd.medium.block_size(), 2048);
    assert!(dvd.medium.is_read_only() && dvd.medium.is_removable());
    assert_eq!(dvd.medium.mmc_profile(), Some(0x0010));
    assert!(
        dvd.file_path.is_some(),
        "an optical drive is an ISO file; error 1044 refuses one without"
    );
}

#[test]
fn encoder_ceiling_is_two_thousand_and_target_stays_below() {
    let c = reference();
    assert_eq!(c.display.encoder.rate_control, RateControl::Vbr);
    assert_eq!(c.display.encoder.max_bitrate_kbps, 2000);
    assert!(c.display.encoder.bitrate_kbps < c.display.encoder.max_bitrate_kbps);
}

#[test]
fn engine_capability_matrix_matches_spec_5_4_and_10_2() {
    // engine, persistent, snapshot-capable
    let table = [
        (EngineKind::PureRustIoUring, true, true),
        (EngineKind::RustNvme, true, false),
        (EngineKind::RustCephRbd, true, true),
        (EngineKind::RustHugepageFile, false, false),
    ];
    for (engine, persistent, snap) in table {
        assert_eq!(engine.is_persistent(), persistent, "{}", engine.as_str());
        assert_eq!(engine.is_snapshot_capable(), snap, "{}", engine.as_str());
    }
}

// -- §0.1 deny_unknown_fields ------------------------------------------------

#[test]
fn unknown_key_aborts_with_1001() {
    expect_code(&patch("[compute]\n", "[compute]\nnuma_nodes = 2\n"), 1001);
}

#[test]
fn num_queues_override_is_rejected_as_unknown_key() {
    // Change-log item 9: queues always equal vcpus, there is no override, so
    // `num_queues` must not merely be ignored — it must abort boot.
    expect_code(
        &patch("[storage]\nbus = 3", "[storage]\nbus = 3\nnum_queues = 8"),
        1001,
    );
}

// -- §1.3 memory invariants --------------------------------------------------

#[test]
fn non_gib_multiple_total_ram_aborts_with_1002() {
    let p = patch("size_mb = 8192", "size_mb = 8000").replacen(
        "high_ram_mb = 5120",
        "high_ram_mb = 4928",
        1,
    );
    expect_code(&p, 1002);
}

#[test]
fn low_ram_above_the_mmio_hole_aborts_with_1003() {
    let p = patch("low_ram_mb = 3072", "low_ram_mb = 4096").replacen(
        "high_ram_mb = 5120",
        "high_ram_mb = 4096",
        1,
    );
    expect_code(&p, 1003);
}

#[test]
fn ram_split_that_does_not_sum_aborts_with_1004() {
    expect_code(&patch("high_ram_mb = 5120", "high_ram_mb = 4096"), 1004);
}

// -- §3.2 SMBIOS serial ------------------------------------------------------

#[test]
fn oversized_vm_name_aborts_with_1005() {
    let long = "x".repeat(64);
    expect_code(
        &patch(
            "name = \"production-windows-workload-01\"",
            &format!("name = \"{long}\""),
        ),
        1005,
    );
}

#[test]
fn empty_vm_name_aborts_with_1005() {
    expect_code(
        &patch("name = \"production-windows-workload-01\"", "name = \"\""),
        1005,
    );
}

#[test]
fn non_uuid_vm_id_aborts_with_1006() {
    expect_code(
        &patch(
            "id = \"4b827e8a-86a0-410a-9d32-26db1a0397ce\"",
            "id = \"not-a-uuid\"",
        ),
        1006,
    );
}

// -- §6.2 TPM must never lose state -----------------------------------------

#[test]
fn volatile_tpm_engine_aborts_with_1020() {
    let p = patch(
        "[tpm.storage]\nengine = \"pure_rust_io_uring\"",
        "[tpm.storage]\nengine = \"rust_hugepage_file\"\nshared_mem_size_mb = 16",
    );
    expect_code(&p, 1020);
}

// -- §4.2 EFI NVRAM is volatile-permitted, with a warning --------------------

#[test]
fn volatile_efi_nvram_is_permitted_but_warns() {
    let p = patch(
        "[firmware.storage]\nengine = \"pure_rust_io_uring\"",
        "[firmware.storage]\nengine = \"rust_hugepage_file\"\nshared_mem_size_mb = 16",
    );
    let cfg = MachineConfig::from_toml_str(&p).expect("volatile EFI NVRAM is permitted (§4.2)");
    assert!(
        cfg.warnings()
            .iter()
            .any(|w| w.contains("firmware.storage") && w.contains("volatile")),
        "a volatile EFI NVRAM engine MUST log a warning: {:?}",
        cfg.warnings()
    );
}

// -- §5.4 engine field bindings ---------------------------------------------

#[test]
fn engine_missing_required_field_aborts_with_1030() {
    let p = patch(
        "engine = \"rust_nvme\" # NOT snapshot-capable - excluded from backup\npci_bdf = \"0000:04:00.0\" # host NVMe device (backing), not a guest bus\nnsid = 1\n",
        "engine = \"rust_nvme\"\npci_bdf = \"0000:04:00.0\"\n",
    );
    expect_code(&p, 1030);
}

#[test]
fn engine_with_foreign_field_aborts_with_1031() {
    let p = patch(
        "engine = \"pure_rust_io_uring\" # snapshot-capable (reflink on CoW FS)\nfile_path = \"/var/lib/vmm/disks/boot_os.raw\"",
        "engine = \"pure_rust_io_uring\"\nfile_path = \"/var/lib/vmm/disks/boot_os.raw\"\npool_name = \"vms\"",
    );
    expect_code(&p, 1031);
}

// -- §7.1 / change-log item 10 ----------------------------------------------

#[test]
fn bitrate_target_at_or_above_ceiling_aborts_with_1050() {
    expect_code(&patch("bitrate_kbps = 1800", "bitrate_kbps = 2000"), 1050);
}

#[test]
fn raising_the_hard_ceiling_aborts_with_1051() {
    expect_code(
        &patch("max_bitrate_kbps = 2000", "max_bitrate_kbps = 4000"),
        1051,
    );
}

// -- change-log items 3 and 11 ----------------------------------------------

#[test]
fn tls_downgrade_aborts_with_1060() {
    expect_code(&patch("tls_min = \"1.3\" # TLS 1.3 only; cert self-signed at boot\nauth_required = true\nusername = \"admin\" # PLAINTEXT (known limitation)\npassword = \"hypervisor@01\"\nmax_clients", "tls_min = \"1.2\"\nauth_required = true\nusername = \"admin\"\npassword = \"hypervisor@01\"\nmax_clients"), 1060);
}

#[test]
fn raising_the_client_cap_aborts_with_1061() {
    expect_code(&patch("max_clients = 2", "max_clients = 4"), 1061);
}

// -- §9 USB/IP transport -----------------------------------------------------

#[test]
fn usbip_port_contradicting_use_tls_aborts_with_1082() {
    expect_code(
        &patch(
            "client_usbip_server = \"[2001:db8::200]:3241\"",
            "client_usbip_server = \"[2001:db8::200]:3240\"",
        ),
        1082,
    );
}

// -- topology ----------------------------------------------------------------

#[test]
fn duplicate_pcie_bus_aborts_with_1070() {
    expect_code(&patch("[storage]\nbus = 3", "[storage]\nbus = 2"), 1070);
}

#[test]
fn duplicate_peripheral_slot_aborts_with_1071() {
    expect_code(&patch("tablet_slot = 1", "tablet_slot = 0"), 1071);
}

#[test]
fn duplicate_drive_id_aborts_with_1040() {
    expect_code(
        &patch(
            "drive_id = 1\nbootable = false",
            "drive_id = 0\nbootable = false",
        ),
        1040,
    );
}

#[test]
fn duplicate_socket_path_aborts_with_1041() {
    expect_code(
        &patch(
            "socket_path = \"/var/run/vmm-vhost-scsi1.sock\"",
            "socket_path = \"/var/run/vmm-vhost-scsi0.sock\"",
        ),
        1041,
    );
}

// -- warnings ----------------------------------------------------------------

#[test]
fn reference_config_warns_about_known_limitations() {
    let w = reference().warnings();
    assert!(
        w.iter().any(|s| s.contains("KNOWN LIMITATION")),
        "plaintext credential warning (§8.4) missing"
    );
    assert!(
        w.iter().any(|s| s.contains("tls_verify_cert")),
        "USB/IP cert-validation warning (§9.2) missing"
    );
    // Two non-snapshot drives (rust_nvme, rust_hugepage_file) in the reference set.
    assert_eq!(w.iter().filter(|s| s.contains("8001")).count(), 2);
}
