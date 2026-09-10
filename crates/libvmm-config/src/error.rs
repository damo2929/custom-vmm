//! Config-domain errors — Appendix A range 1000–1999.
//!
//! Each variant carries a stable numeric code that appears in logs and in
//! WSS/RTSPS error frames (§1.6).

use std::path::PathBuf;
use thiserror::Error;

pub type ConfigResult<T> = Result<T, ConfigError>;

#[derive(Debug, Error)]
pub enum ConfigError {
    // -- load / schema ------------------------------------------------------
    /// 1008 — the configuration file could not be read.
    #[error("config file {path}: {detail}")]
    Io { path: PathBuf, detail: String },

    /// 1001 — `deny_unknown_fields` rejected a key. Boot MUST abort.
    #[error("unknown configuration key: {detail}")]
    UnknownKey { detail: String },

    /// 1000 — TOML syntax or type error.
    #[error("malformed configuration: {detail}")]
    Malformed { detail: String },

    // -- memory -------------------------------------------------------------
    /// 1002 — total RAM is not a whole multiple of 1 GiB (§1.3).
    #[error("memory.size_mb = {size_mb} must be a whole multiple of 1024 (1GiB hugepages)")]
    BadMemoryMultiple { size_mb: u64 },

    /// 1003 — low RAM would overlap the MMIO hole (§1.3 invariant).
    #[error("memory.low_ram_mb = {low_ram_mb} exceeds the 3072 MiB ceiling below the MMIO hole")]
    LowRamOverlapsMmioHole { low_ram_mb: u64 },

    /// 1004 — low + high does not add up to the declared total.
    #[error("memory.low_ram_mb ({low}) + memory.high_ram_mb ({high}) != memory.size_mb ({total})")]
    RamSplitMismatch { low: u64, high: u64, total: u64 },

    // -- vm identity --------------------------------------------------------
    /// 1005 — SMBIOS Type 1 serial constraint (§3.2).
    #[error("vm.name must be 1..=63 bytes (SMBIOS Type 1 serial), got {len}")]
    VmNameLength { len: usize },

    /// 1006 — `vm.id` is not a UUID.
    #[error("vm.id is not a valid UUID: {detail}")]
    VmIdNotUuid { detail: String },

    // -- compute ------------------------------------------------------------
    /// 1007 — at least one vCPU is required; it also fixes the queue count.
    #[error("compute.vcpus must be >= 1 (it also fixes the storage queue count 1:1)")]
    NoVcpus,

    // -- acpi ---------------------------------------------------------------
    /// 1010 — a generated or injected table failed its 8-bit sum-to-zero check.
    #[error("ACPI table {table} failed its checksum check ({detail})")]
    AcpiChecksum { table: String, detail: String },

    // -- tpm ----------------------------------------------------------------
    /// 1020 — TPM state must never be lost, so a volatile engine is refused.
    #[error("tpm.storage.engine = {engine} is volatile; TPM state MUST never be lost (§6.2)")]
    TpmVolatileEngine { engine: &'static str },

    // -- engine bindings ----------------------------------------------------
    /// 1030 — an engine is missing a field it requires.
    #[error("{section}: engine {engine} requires `{field}`")]
    EngineMissingField {
        section: String,
        engine: &'static str,
        field: &'static str,
    },

    /// 1031 — an engine was given a field belonging to a different engine.
    #[error("{section}: `{field}` is not valid for engine {engine}")]
    EngineUnexpectedField {
        section: String,
        engine: &'static str,
        field: &'static str,
    },

    // -- storage ------------------------------------------------------------
    /// 1040 — duplicate `drive_id`.
    #[error("storage.drives: duplicate drive_id {drive_id}")]
    DuplicateDriveId { drive_id: u32 },

    /// 1041 — duplicate vhost-user socket path across drives/cards.
    #[error("duplicate socket_path {path} (each back-end needs its own socket)")]
    DuplicateSocketPath { path: PathBuf },

    /// 1042 — more than one bootable drive, or none.
    #[error("storage.drives: expected exactly one bootable drive, found {count}")]
    BootableDriveCount { count: usize },

    /// 1043 — an optical drive was bound to an engine that cannot present a
    /// read-only ISO image.
    #[error(
        "storage.drives[{drive_id}]: medium {medium} is an ISO image and needs a file-backed \
         engine; {engine} addresses a device, not a file"
    )]
    OpticalEngineNotFileBacked {
        drive_id: u32,
        medium: &'static str,
        engine: &'static str,
    },

    /// 1044 — an optical drive has no ISO to present.
    #[error(
        "storage.drives[{drive_id}]: medium {medium} needs file_path pointing at an ISO image"
    )]
    OpticalMissingImage { drive_id: u32, medium: &'static str },

    /// 1045 — discard was turned off on a machine that has block drives.
    #[error(
        "storage.discard_unmap = false, but {count} drive(s) are presented to the guest as \
         solid-state disks, which advertise UNMAP. A drive cannot claim to be an SSD and \
         refuse TRIM."
    )]
    DiscardRequiredForBlockDrives { count: usize },

    // -- display / encoder --------------------------------------------------
    /// 1050 — VBR target must stay strictly below the hard ceiling (item 10).
    #[error("display.encoder.bitrate_kbps ({target}) must be < max_bitrate_kbps ({ceiling})")]
    BitrateAboveCeiling { target: u32, ceiling: u32 },

    /// 1051 — the 2000 kbps ceiling is a hard cap, not a suggestion.
    #[error("display.encoder.max_bitrate_kbps ({got}) exceeds the hard 2000 kbps ceiling (§7.1)")]
    BitrateCeilingTooHigh { got: u32 },

    /// 1052 — audio capture format is fixed at 48 kHz S16LE stereo (§7.1).
    #[error("display.audio_encoder: {field} must be {expected}, got {got}")]
    AudioFormat {
        field: &'static str,
        expected: u32,
        got: u32,
    },

    // -- listeners ----------------------------------------------------------
    /// 1060 — TLS 1.3 only, no fallback (change-log item 3).
    #[error("{section}.tls_min = \"{got}\": TLS 1.3 only, no fallback is permitted")]
    TlsVersionNotSupported { section: &'static str, got: String },

    /// 1061 — the WSS client cap is a hard 2 (change-log item 11).
    #[error("control_wss.max_clients = {got}: the hard cap is 2")]
    MaxClientsOutOfRange { got: u32 },

    /// 1062 — two listeners cannot share a TCP port.
    #[error("port {port} is bound by both {a} and {b}")]
    PortConflict {
        port: u16,
        a: &'static str,
        b: &'static str,
    },

    /// 1063 — auth is required but credentials are empty.
    #[error("{section}: auth_required is true but {field} is empty")]
    EmptyCredential {
        section: &'static str,
        field: &'static str,
    },

    // -- pci topology -------------------------------------------------------
    /// 1070 — two subsystems claimed the same PCIe bus.
    #[error("PCIe bus {bus} is claimed by both {a} and {b}")]
    BusConflict {
        bus: u8,
        a: &'static str,
        b: &'static str,
    },

    /// 1071 — two peripherals claimed the same slot on the peripheral bus.
    #[error("peripherals: slot {slot} is claimed by both {a} and {b}")]
    PeripheralSlotConflict {
        slot: u8,
        a: &'static str,
        b: &'static str,
    },

    // -- network / usb ------------------------------------------------------
    /// 1080 — malformed MAC address.
    #[error("network.cards[{card_id}].mac_address \"{mac}\" is not a valid MAC")]
    BadMacAddress { card_id: u32, mac: String },

    /// 1081 — the USB/IP server endpoint is not a socket address.
    #[error("usb.client_usbip_server \"{addr}\" is not a valid socket address")]
    BadUsbipEndpoint { addr: String },

    /// 1082 — `use_tls` selects :3241, cleartext is :3240 (§9).
    #[error("usb.client_usbip_server port {port} contradicts use_tls = {use_tls} (TLS=3241, cleartext=3240)")]
    UsbipPortTlsMismatch { port: u16, use_tls: bool },

    // -- backup -------------------------------------------------------------
    /// 1090 — zstd level out of range.
    #[error("backup.zstd_level {got} is outside 1..=19")]
    ZstdLevelOutOfRange { got: i32 },
}

impl ConfigError {
    /// Stable numeric code (Appendix A, 1xxx range).
    pub const fn code(&self) -> u32 {
        match self {
            ConfigError::Malformed { .. } => 1000,
            ConfigError::UnknownKey { .. } => 1001,
            ConfigError::BadMemoryMultiple { .. } => 1002,
            ConfigError::LowRamOverlapsMmioHole { .. } => 1003,
            ConfigError::RamSplitMismatch { .. } => 1004,
            ConfigError::VmNameLength { .. } => 1005,
            ConfigError::VmIdNotUuid { .. } => 1006,
            ConfigError::NoVcpus => 1007,
            ConfigError::Io { .. } => 1008,
            ConfigError::AcpiChecksum { .. } => 1010,
            ConfigError::TpmVolatileEngine { .. } => 1020,
            ConfigError::EngineMissingField { .. } => 1030,
            ConfigError::EngineUnexpectedField { .. } => 1031,
            ConfigError::DuplicateDriveId { .. } => 1040,
            ConfigError::DuplicateSocketPath { .. } => 1041,
            ConfigError::BootableDriveCount { .. } => 1042,
            ConfigError::OpticalEngineNotFileBacked { .. } => 1043,
            ConfigError::OpticalMissingImage { .. } => 1044,
            ConfigError::DiscardRequiredForBlockDrives { .. } => 1045,
            ConfigError::BitrateAboveCeiling { .. } => 1050,
            ConfigError::BitrateCeilingTooHigh { .. } => 1051,
            ConfigError::AudioFormat { .. } => 1052,
            ConfigError::TlsVersionNotSupported { .. } => 1060,
            ConfigError::MaxClientsOutOfRange { .. } => 1061,
            ConfigError::PortConflict { .. } => 1062,
            ConfigError::EmptyCredential { .. } => 1063,
            ConfigError::BusConflict { .. } => 1070,
            ConfigError::PeripheralSlotConflict { .. } => 1071,
            ConfigError::BadMacAddress { .. } => 1080,
            ConfigError::BadUsbipEndpoint { .. } => 1081,
            ConfigError::UsbipPortTlsMismatch { .. } => 1082,
            ConfigError::ZstdLevelOutOfRange { .. } => 1090,
        }
    }

    /// Classify a `toml` deserialisation failure. `deny_unknown_fields`
    /// surfaces as a distinguishable "unknown field" message, which maps to
    /// 1001 rather than the generic 1000.
    pub fn from_toml(e: toml::de::Error) -> Self {
        let msg = e.to_string();
        if msg.contains("unknown field") || msg.contains("unknown variant") {
            ConfigError::UnknownKey { detail: msg }
        } else {
            ConfigError::Malformed { detail: msg }
        }
    }
}
