//! Cross-section configuration invariants.
//!
//! serde enforces the *shape* of the TOML; this module enforces the
//! *semantics* the specification calls out as MUST. Everything here runs at
//! config-load time so an invalid machine never reaches `MEM_ALLOC` (§1.5).

use crate::error::{ConfigError, ConfigResult};
use crate::*;
use std::collections::HashMap;

/// Low RAM must end at or below the MMIO hole, which starts at 0xC000_0000.
pub const LOW_RAM_CEILING_MB: u64 = 3072;
/// The hard encoder ceiling from change-log item 10.
pub const HARD_BITRATE_CEILING_KBPS: u32 = 2000;
/// The hard WSS client cap from change-log item 11.
pub const HARD_MAX_WSS_CLIENTS: u32 = 2;

pub fn validate(cfg: &MachineConfig) -> ConfigResult<()> {
    validate_vm(&cfg.vm)?;
    validate_compute(&cfg.compute)?;
    validate_memory(&cfg.memory)?;
    validate_encoders(&cfg.display)?;
    validate_listeners(cfg)?;
    validate_tpm(&cfg.tpm)?;
    validate_engine_binding("firmware.storage", &cfg.firmware.storage)?;
    validate_storage(&cfg.storage)?;
    validate_network(&cfg.network)?;
    validate_usb(&cfg.usb)?;
    validate_backup(&cfg.backup)?;
    validate_bus_topology(cfg)?;
    validate_peripheral_slots(&cfg.peripherals)?;
    validate_socket_uniqueness(cfg)?;
    Ok(())
}

/// Conditions that are legal but MUST be logged (§3.3, §4.2, §8.4, §9.2).
pub fn warnings(cfg: &MachineConfig) -> Vec<String> {
    let mut w = Vec::new();

    // §4.2 — volatile EFI NVRAM is permitted, with a warning.
    if !cfg.firmware.storage.engine.is_persistent() {
        w.push(format!(
            "firmware.storage uses the volatile {} engine: EFI variable state \
             is lost on host reboot (§4.2)",
            cfg.firmware.storage.engine.as_str()
        ));
    }

    // §3.3 — injection is optional; a missing path skips the table.
    if cfg.acpi.enabled {
        if cfg.acpi.msdm_path.is_none() {
            w.push("acpi.msdm_path is absent: MSDM table skipped, OA 3.0 activation will not apply (§3.3)".into());
        }
        if cfg.acpi.slic_path.is_none() {
            w.push("acpi.slic_path is absent: SLIC table skipped, OA 2.1 activation will not apply (§3.3)".into());
        }
    }

    // §8.4 — documented known limitation.
    w.push("credentials are stored inline in plaintext TOML and are protected on the wire only by the mandatory TLS 1.3 (KNOWN LIMITATION, §8.4)".into());

    // §9.2 — cert validation off relies on the external cluster TLS authority.
    if cfg.usb.use_tls && !cfg.usb.tls_verify_cert {
        w.push("usb.tls_verify_cert is false: USB/IP peer certificates are not validated; this SHOULD be true on untrusted networks (§9.2)".into());
    }

    // §10.2 — a non-snapshot drive will abort any backup.
    for d in &cfg.storage.drives {
        if !d.engine.is_snapshot_capable() {
            w.push(format!(
                "storage.drives[{}] uses the non-snapshot engine {}: any backup \
                 including it aborts with Backup(EngineNotSnapshotCapable) 8001 (§10.2)",
                d.drive_id,
                d.engine.as_str()
            ));
        }
    }

    w
}

// ---------------------------------------------------------------------------

fn validate_vm(vm: &Vm) -> ConfigResult<()> {
    // §3.2: the SMBIOS Type 1 serial equals vm.name byte-for-byte, so the
    // SMBIOS string limit is a config-level constraint.
    let len = vm.name.len();
    if len == 0 || len > 63 {
        return Err(ConfigError::VmNameLength { len });
    }
    uuid::Uuid::parse_str(&vm.id).map_err(|e| ConfigError::VmIdNotUuid {
        detail: e.to_string(),
    })?;
    Ok(())
}

fn validate_compute(c: &Compute) -> ConfigResult<()> {
    if c.vcpus == 0 {
        return Err(ConfigError::NoVcpus);
    }
    Ok(())
}

fn validate_memory(m: &Memory) -> ConfigResult<()> {
    // §1.3: total RAM MUST be a whole multiple of 1 GiB (hugepage granularity).
    if m.size_mb == 0 || m.size_mb % 1024 != 0 {
        return Err(ConfigError::BadMemoryMultiple { size_mb: m.size_mb });
    }
    if m.low_ram_mb % 1024 != 0 {
        return Err(ConfigError::BadMemoryMultiple {
            size_mb: m.low_ram_mb,
        });
    }
    if m.high_ram_mb % 1024 != 0 {
        return Err(ConfigError::BadMemoryMultiple {
            size_mb: m.high_ram_mb,
        });
    }
    // §1.3 invariant: low RAM ends at or below the 0xC000_0000 hole.
    if m.low_ram_mb > LOW_RAM_CEILING_MB {
        return Err(ConfigError::LowRamOverlapsMmioHole {
            low_ram_mb: m.low_ram_mb,
        });
    }
    if m.low_ram_mb + m.high_ram_mb != m.size_mb {
        return Err(ConfigError::RamSplitMismatch {
            low: m.low_ram_mb,
            high: m.high_ram_mb,
            total: m.size_mb,
        });
    }
    Ok(())
}

fn validate_encoders(d: &Display) -> ConfigResult<()> {
    // Change-log item 10: 2000 kbps is a hard ceiling and VBR stays below it.
    if d.encoder.max_bitrate_kbps > HARD_BITRATE_CEILING_KBPS {
        return Err(ConfigError::BitrateCeilingTooHigh {
            got: d.encoder.max_bitrate_kbps,
        });
    }
    if d.encoder.bitrate_kbps >= d.encoder.max_bitrate_kbps {
        return Err(ConfigError::BitrateAboveCeiling {
            target: d.encoder.bitrate_kbps,
            ceiling: d.encoder.max_bitrate_kbps,
        });
    }
    // §7.1: the capture format is fixed at 48 kHz stereo.
    if d.audio_encoder.sample_rate != 48_000 {
        return Err(ConfigError::AudioFormat {
            field: "sample_rate",
            expected: 48_000,
            got: d.audio_encoder.sample_rate,
        });
    }
    if d.audio_encoder.channels != 2 {
        return Err(ConfigError::AudioFormat {
            field: "channels",
            expected: 2,
            got: d.audio_encoder.channels as u32,
        });
    }
    Ok(())
}

fn validate_listeners(cfg: &MachineConfig) -> ConfigResult<()> {
    // Change-log item 3: TLS 1.3 only, on every listener.
    if cfg.control_wss.tls_min != "1.3" {
        return Err(ConfigError::TlsVersionNotSupported {
            section: "control_wss",
            got: cfg.control_wss.tls_min.clone(),
        });
    }
    if cfg.display.rtsps.tls_min != "1.3" {
        return Err(ConfigError::TlsVersionNotSupported {
            section: "display.rtsps",
            got: cfg.display.rtsps.tls_min.clone(),
        });
    }

    // Change-log item 11: the cap is a hard 2.
    if cfg.control_wss.max_clients == 0 || cfg.control_wss.max_clients > HARD_MAX_WSS_CLIENTS {
        return Err(ConfigError::MaxClientsOutOfRange {
            got: cfg.control_wss.max_clients,
        });
    }

    if cfg.control_wss.auth_required {
        if cfg.control_wss.username.is_empty() {
            return Err(ConfigError::EmptyCredential {
                section: "control_wss",
                field: "username",
            });
        }
        if cfg.control_wss.password.is_empty() {
            return Err(ConfigError::EmptyCredential {
                section: "control_wss",
                field: "password",
            });
        }
    }
    if cfg.display.rtsps.auth_required {
        if cfg.display.rtsps.username.is_empty() {
            return Err(ConfigError::EmptyCredential {
                section: "display.rtsps",
                field: "username",
            });
        }
        if cfg.display.rtsps.password.is_empty() {
            return Err(ConfigError::EmptyCredential {
                section: "display.rtsps",
                field: "password",
            });
        }
    }

    if cfg.control_wss.enabled
        && cfg.display.rtsps.enabled
        && cfg.control_wss.port == cfg.display.rtsps.port
    {
        return Err(ConfigError::PortConflict {
            port: cfg.control_wss.port,
            a: "control_wss",
            b: "display.rtsps",
        });
    }
    Ok(())
}

fn validate_tpm(tpm: &Tpm) -> ConfigResult<()> {
    if !tpm.enabled {
        return Ok(());
    }
    // §6.2: losing TPM state breaks BitLocker unlock and attestation, so a
    // volatile engine is refused outright — unlike EFI NVRAM (§4.2).
    if !tpm.storage.engine.is_persistent() {
        return Err(ConfigError::TpmVolatileEngine {
            engine: tpm.storage.engine.as_str(),
        });
    }
    validate_engine_binding("tpm.storage", &tpm.storage)
}

/// Check that an engine binding carries exactly the fields its engine needs.
///
/// This is the single routine used for drives, `[firmware.storage]` and
/// `[tpm.storage]` — the uniform storage model of §5.4.
pub fn validate_engine_binding(section: &str, b: &EngineBinding) -> ConfigResult<()> {
    let engine = b.engine;
    let name = engine.as_str();

    let require_file = matches!(
        engine,
        EngineKind::PureRustIoUring | EngineKind::RustHugepageFile
    );
    let require_nvme = matches!(engine, EngineKind::RustNvme);
    let require_rbd = matches!(engine, EngineKind::RustCephRbd);

    if require_file && b.file_path.is_none() {
        return Err(ConfigError::EngineMissingField {
            section: section.into(),
            engine: name,
            field: "file_path",
        });
    }
    if !require_file && b.file_path.is_some() {
        return Err(ConfigError::EngineUnexpectedField {
            section: section.into(),
            engine: name,
            field: "file_path",
        });
    }

    if require_nvme {
        if b.pci_bdf.is_none() {
            return Err(ConfigError::EngineMissingField {
                section: section.into(),
                engine: name,
                field: "pci_bdf",
            });
        }
        if b.nsid.is_none() {
            return Err(ConfigError::EngineMissingField {
                section: section.into(),
                engine: name,
                field: "nsid",
            });
        }
    } else {
        if b.pci_bdf.is_some() {
            return Err(ConfigError::EngineUnexpectedField {
                section: section.into(),
                engine: name,
                field: "pci_bdf",
            });
        }
        if b.nsid.is_some() {
            return Err(ConfigError::EngineUnexpectedField {
                section: section.into(),
                engine: name,
                field: "nsid",
            });
        }
    }

    if require_rbd {
        for (present, field) in [
            (b.cluster_config.is_some(), "cluster_config"),
            (b.cluster_name.is_some(), "cluster_name"),
            (b.pool_name.is_some(), "pool_name"),
            (b.rbd_image.is_some(), "rbd_image"),
        ] {
            if !present {
                return Err(ConfigError::EngineMissingField {
                    section: section.into(),
                    engine: name,
                    field,
                });
            }
        }
    } else {
        for (present, field) in [
            (b.cluster_config.is_some(), "cluster_config"),
            (b.cluster_name.is_some(), "cluster_name"),
            (b.pool_name.is_some(), "pool_name"),
            (b.rbd_image.is_some(), "rbd_image"),
        ] {
            if present {
                return Err(ConfigError::EngineUnexpectedField {
                    section: section.into(),
                    engine: name,
                    field,
                });
            }
        }
    }

    match engine {
        EngineKind::RustHugepageFile if b.shared_mem_size_mb.is_none() => {
            return Err(ConfigError::EngineMissingField {
                section: section.into(),
                engine: name,
                field: "shared_mem_size_mb",
            });
        }
        EngineKind::RustHugepageFile => {}
        _ if b.shared_mem_size_mb.is_some() => {
            return Err(ConfigError::EngineUnexpectedField {
                section: section.into(),
                engine: name,
                field: "shared_mem_size_mb",
            });
        }
        _ => {}
    }

    Ok(())
}

fn validate_storage(s: &Storage) -> ConfigResult<()> {
    let mut seen_ids: HashMap<u32, ()> = HashMap::new();
    let mut bootable = 0usize;

    for d in &s.drives {
        if seen_ids.insert(d.drive_id, ()).is_some() {
            return Err(ConfigError::DuplicateDriveId {
                drive_id: d.drive_id,
            });
        }
        if d.bootable {
            bootable += 1;
        }
        validate_engine_binding(&format!("storage.drives[{}]", d.drive_id), &d.binding())?;
    }

    if !s.drives.is_empty() && bootable != 1 {
        return Err(ConfigError::BootableDriveCount { count: bootable });
    }
    Ok(())
}

fn validate_network(n: &Network) -> ConfigResult<()> {
    for c in &n.cards {
        if !is_valid_mac(&c.mac_address) {
            return Err(ConfigError::BadMacAddress {
                card_id: c.card_id,
                mac: c.mac_address.clone(),
            });
        }
    }
    Ok(())
}

fn is_valid_mac(mac: &str) -> bool {
    let parts: Vec<&str> = mac.split(':').collect();
    parts.len() == 6
        && parts
            .iter()
            .all(|p| p.len() == 2 && p.chars().all(|c| c.is_ascii_hexdigit()))
}

fn validate_usb(u: &Usb) -> ConfigResult<()> {
    let addr = u
        .client_usbip_server
        .parse::<std::net::SocketAddr>()
        .map_err(|_| ConfigError::BadUsbipEndpoint {
            addr: u.client_usbip_server.clone(),
        })?;

    // §9: cleartext is :3240, TLS 1.3 is :3241.
    let expected = if u.use_tls { 3241 } else { 3240 };
    if addr.port() != expected {
        return Err(ConfigError::UsbipPortTlsMismatch {
            port: addr.port(),
            use_tls: u.use_tls,
        });
    }
    Ok(())
}

fn validate_backup(b: &Backup) -> ConfigResult<()> {
    if !(1..=19).contains(&b.zstd_level) {
        return Err(ConfigError::ZstdLevelOutOfRange { got: b.zstd_level });
    }
    Ok(())
}

fn validate_bus_topology(cfg: &MachineConfig) -> ConfigResult<()> {
    let buses: [(&'static str, u8); 5] = [
        ("display", cfg.display.bus),
        ("peripherals", cfg.peripherals.bus),
        ("storage", cfg.storage.bus),
        ("network", cfg.network.bus),
        ("usb", cfg.usb.bus),
    ];
    for i in 0..buses.len() {
        for j in (i + 1)..buses.len() {
            if buses[i].1 == buses[j].1 {
                return Err(ConfigError::BusConflict {
                    bus: buses[i].1,
                    a: buses[i].0,
                    b: buses[j].0,
                });
            }
        }
    }
    Ok(())
}

fn validate_peripheral_slots(p: &Peripherals) -> ConfigResult<()> {
    let slots: [(&'static str, u8); 5] = [
        ("keyboard_slot", p.keyboard_slot),
        ("tablet_slot", p.tablet_slot),
        ("rng_slot", p.rng_slot),
        ("serial_slot", p.serial_slot),
        ("tpm_slot", p.tpm_slot),
    ];
    for i in 0..slots.len() {
        for j in (i + 1)..slots.len() {
            if slots[i].1 == slots[j].1 {
                return Err(ConfigError::PeripheralSlotConflict {
                    slot: slots[i].1,
                    a: slots[i].0,
                    b: slots[j].0,
                });
            }
        }
    }
    Ok(())
}

fn validate_socket_uniqueness(cfg: &MachineConfig) -> ConfigResult<()> {
    let mut seen: HashMap<&std::path::Path, ()> = HashMap::new();
    for d in &cfg.storage.drives {
        if seen.insert(d.socket_path.as_path(), ()).is_some() {
            return Err(ConfigError::DuplicateSocketPath {
                path: d.socket_path.clone(),
            });
        }
    }
    for c in &cfg.network.cards {
        if seen.insert(c.socket_path.as_path(), ()).is_some() {
            return Err(ConfigError::DuplicateSocketPath {
                path: c.socket_path.clone(),
            });
        }
    }
    Ok(())
}
