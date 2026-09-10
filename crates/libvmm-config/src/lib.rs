//! Declarative machine configuration — Detailed Design Spec §11.
//!
//! The TOML is deserialised with serde using `deny_unknown_fields`
//! throughout (§0.1 "Configuration"): an unknown key MUST abort boot with
//! `Config(UnknownKey)` (error 1001).
//!
//! Every default stated in §11 is expressed here as a `#[serde(default = ...)]`
//! so that the schema is self-documenting and a partial TOML resolves to the
//! same machine the reference config describes.

#![forbid(unsafe_code)]

pub mod error;
pub mod validate;

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

pub use error::{ConfigError, ConfigResult};

/// Root of the declarative machine configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MachineConfig {
    pub vm: Vm,
    pub compute: Compute,
    pub memory: Memory,
    pub firmware: Firmware,
    pub display: Display,
    pub control_wss: ControlWss,
    pub peripherals: Peripherals,
    pub tpm: Tpm,
    pub acpi: Acpi,
    pub storage: Storage,
    pub network: Network,
    pub usb: Usb,
    #[serde(default)]
    pub backup: Backup,
}

impl MachineConfig {
    /// Parse and fully validate a configuration from TOML text.
    ///
    /// Parsing failures (including unknown keys, per `deny_unknown_fields`)
    /// and invariant violations both abort with a `ConfigError`; there is no
    /// "warn and continue" path for the schema itself.
    pub fn from_toml_str(text: &str) -> ConfigResult<Self> {
        let cfg: MachineConfig = toml::from_str(text).map_err(ConfigError::from_toml)?;
        cfg.validate()?;
        Ok(cfg)
    }

    /// Read, parse and validate a configuration file.
    pub fn load(path: &Path) -> ConfigResult<Self> {
        let text = std::fs::read_to_string(path).map_err(|e| ConfigError::Io {
            path: path.to_path_buf(),
            detail: e.to_string(),
        })?;
        Self::from_toml_str(&text)
    }

    /// Run every cross-section invariant from the specification.
    pub fn validate(&self) -> ConfigResult<()> {
        validate::validate(self)
    }

    /// Warnings that do not abort boot but MUST be logged (§4.2, §3.3).
    pub fn warnings(&self) -> Vec<String> {
        validate::warnings(self)
    }
}

// ---------------------------------------------------------------------------
// [vm]
// ---------------------------------------------------------------------------

/// `[vm]` — identity surfaced verbatim in SMBIOS Type 1 (§3.2).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Vm {
    /// SMBIOS Type 1 `serial_number`, byte-for-byte, 1..=63 bytes (§3.2).
    pub name: String,
    /// SMBIOS Type 1 UUID.
    pub id: String,
}

// ---------------------------------------------------------------------------
// [compute]
// ---------------------------------------------------------------------------

/// `[compute]` — vCPU count also fixes the storage queue count (§5.1, 1:1).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Compute {
    pub vcpus: u32,
    #[serde(default = "t")]
    pub cpu_passthrough: bool,
    #[serde(default = "t")]
    pub hypervisor_bit: bool,
    #[serde(default = "t")]
    pub kvm_ptp_clock: bool,
}

// ---------------------------------------------------------------------------
// [memory]
// ---------------------------------------------------------------------------

/// `[memory]` — 1GiB hugepage backed, split around the MMIO hole (§1.3).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Memory {
    /// Total guest RAM. MUST be a whole multiple of 1024 (1GiB granularity).
    pub size_mb: u64,
    #[serde(default = "t")]
    pub hugepages_1gb: bool,
    #[serde(default = "t")]
    pub dynamic_allocation: bool,
    /// Low RAM below the MMIO hole. MUST be <= 3072 (§1.3 invariant).
    pub low_ram_mb: u64,
    /// High RAM at/above 0x1_0000_0000.
    pub high_ram_mb: u64,
}

// ---------------------------------------------------------------------------
// [firmware]
// ---------------------------------------------------------------------------

/// `[firmware]` — OVMF code + variable store (§3.1, §4).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Firmware {
    pub code_path: PathBuf,
    pub vars_path: PathBuf,
    #[serde(default = "default_code_start_addr")]
    pub code_start_addr: u64,
    /// EFI NVRAM binding to the unified storage engine model (§4.1).
    pub storage: EngineBinding,
}

// ---------------------------------------------------------------------------
// Unified storage engine model (§5.4) — shared by drives, firmware and TPM.
// ---------------------------------------------------------------------------

/// The four unified engines (§5.4). Names dropped the `dpdk_` prefix per
/// change-log item 1: these are pure-Rust reimplementations, not C libraries.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EngineKind {
    /// Sparse file over io_uring SQPOLL with fixed buffers. Persistent,
    /// snapshot-capable via reflink/FICLONE on a CoW filesystem.
    PureRustIoUring,
    /// Host NVMe namespace via VFIO, user-space SQ/CQ. Persistent, no snapshot.
    RustNvme,
    /// Ceph RBD image over native Rust RADOS. Persistent, RBD snapshot.
    RustCephRbd,
    /// Hugepage mmap scratch. Volatile, no snapshot.
    RustHugepageFile,
}

impl EngineKind {
    /// §5.4 / §10.2 capability matrix — persistence.
    pub const fn is_persistent(self) -> bool {
        !matches!(self, EngineKind::RustHugepageFile)
    }

    /// §10.2 capability matrix — snapshot support. A non-snapshot engine in
    /// an included drive aborts the backup with 8001.
    pub const fn is_snapshot_capable(self) -> bool {
        matches!(self, EngineKind::PureRustIoUring | EngineKind::RustCephRbd)
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            EngineKind::PureRustIoUring => "pure_rust_io_uring",
            EngineKind::RustNvme => "rust_nvme",
            EngineKind::RustCephRbd => "rust_ceph_rbd",
            EngineKind::RustHugepageFile => "rust_hugepage_file",
        }
    }
}

/// An engine binding used by `[firmware.storage]` and `[tpm.storage]`.
/// Drives use [`Drive`], which carries the same engine-specific fields plus
/// the virtio-scsi attributes.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EngineBinding {
    pub engine: EngineKind,
    /// `pure_rust_io_uring` / `rust_hugepage_file` backing file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_path: Option<PathBuf>,
    /// `rust_nvme` host device.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pci_bdf: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nsid: Option<u32>,
    /// `rust_ceph_rbd` cluster coordinates.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cluster_config: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cluster_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pool_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rbd_image: Option<String>,
    /// `rust_hugepage_file` mapping size.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shared_mem_size_mb: Option<u64>,
}

// ---------------------------------------------------------------------------
// [display]
// ---------------------------------------------------------------------------

/// `[display]` — virtio-gpu scanout and virtio-snd capture source (§7.1).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Display {
    pub bus: u8,
    #[serde(default = "t")]
    pub enabled: bool,
    #[serde(default = "default_width")]
    pub width: u32,
    #[serde(default = "default_height")]
    pub height: u32,
    #[serde(default = "default_color_depth")]
    pub color_depth_bits: u32,
    #[serde(default = "default_framerate_cap")]
    pub framerate_cap: u32,
    #[serde(default = "t")]
    pub virtio_sound_enabled: bool,
    pub encoder: VideoEncoder,
    pub audio_encoder: AudioEncoder,
    pub rtsps: Rtsps,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RateControl {
    /// Constrained VBR (§7.1): output stays below `max_bitrate_kbps`.
    Vbr,
    Cbr,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum HardwareAccelerator {
    Vaapi,
    Nvenc,
}

/// `[display.encoder]` — H.264, constrained VBR (§7.1).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VideoEncoder {
    #[serde(default = "default_rate_control")]
    pub rate_control: RateControl,
    /// Target average, 1800 kbps in the reference config.
    #[serde(default = "default_bitrate_kbps")]
    pub bitrate_kbps: u32,
    /// Hard ceiling (change-log item 10). VBR MUST always stay below it.
    #[serde(default = "default_max_bitrate_kbps")]
    pub max_bitrate_kbps: u32,
    #[serde(default = "default_hwaccel")]
    pub hardware_accelerator: HardwareAccelerator,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AudioCodec {
    OggVorbis,
}

/// `[display.audio_encoder]` — Vorbis 128 kbps, 48 kHz stereo (§7.1).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AudioEncoder {
    #[serde(default = "default_audio_codec")]
    pub codec: AudioCodec,
    #[serde(default = "default_audio_bitrate")]
    pub bitrate_kbps: u32,
    #[serde(default = "default_sample_rate")]
    pub sample_rate: u32,
    #[serde(default = "default_channels")]
    pub channels: u8,
}

/// `[display.rtsps]` — TLS 1.3-only RTSP listener (§7.4).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Rtsps {
    #[serde(default = "t")]
    pub enabled: bool,
    #[serde(default = "default_stream_path")]
    pub stream_path: String,
    #[serde(default = "default_rtsp_port")]
    pub port: u16,
    #[serde(default = "default_ipv6_bind")]
    pub ipv6_bind: String,
    /// TLS 1.3 only, no fallback (change-log item 3).
    #[serde(default = "default_tls_min")]
    pub tls_min: String,
    #[serde(default = "t")]
    pub auth_required: bool,
    /// KNOWN LIMITATION (§8.4): plaintext in TOML.
    pub username: String,
    pub password: String,
}

// ---------------------------------------------------------------------------
// [control_wss]
// ---------------------------------------------------------------------------

/// `[control_wss]` — unified control channel (§8).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ControlWss {
    #[serde(default = "t")]
    pub enabled: bool,
    #[serde(default = "default_wss_port")]
    pub port: u16,
    #[serde(default = "default_ipv6_bind")]
    pub ipv6_bind: String,
    #[serde(default = "default_tls_min")]
    pub tls_min: String,
    #[serde(default = "t")]
    pub auth_required: bool,
    /// KNOWN LIMITATION (§8.4): plaintext in TOML.
    pub username: String,
    pub password: String,
    /// Hard cap; a 3rd client is rejected at handshake (change-log item 11).
    #[serde(default = "default_max_clients")]
    pub max_clients: u32,
    /// Lockout threshold (change-log item 5).
    #[serde(default = "default_max_auth_attempts")]
    pub max_auth_attempts: u32,
    /// Configurable lockout window in seconds.
    #[serde(default = "default_lockout_secs")]
    pub lockout_duration_secs: u64,
}

// ---------------------------------------------------------------------------
// [peripherals]
// ---------------------------------------------------------------------------

/// `[peripherals]` — slot assignment on the peripheral bus (§11).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Peripherals {
    pub bus: u8,
    #[serde(default)]
    pub keyboard_slot: u8,
    #[serde(default = "one_u8")]
    pub tablet_slot: u8,
    #[serde(default = "two_u8")]
    pub rng_slot: u8,
    #[serde(default = "three_u8")]
    pub serial_slot: u8,
    #[serde(default = "four_u8")]
    pub tpm_slot: u8,
}

// ---------------------------------------------------------------------------
// [tpm]
// ---------------------------------------------------------------------------

/// `[tpm]` — virtio-tpm (§6). State MUST never be lost.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Tpm {
    #[serde(default = "t")]
    pub enabled: bool,
    #[serde(default = "default_tpm_version")]
    pub version: String,
    /// Persistent engines only; volatile is rejected at boot (§6.2).
    pub storage: EngineBinding,
}

// ---------------------------------------------------------------------------
// [acpi]
// ---------------------------------------------------------------------------

/// `[acpi]` — generated table set plus verbatim MSDM/SLIC injection (§3.3).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Acpi {
    #[serde(default = "t")]
    pub enabled: bool,
    /// Optional: if absent the table is skipped with a warning (§3.3).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub msdm_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub slic_path: Option<PathBuf>,
}

// ---------------------------------------------------------------------------
// [storage]
// ---------------------------------------------------------------------------

/// `[storage]` — virtio-scsi throughout (§5).
///
/// `num_queues` is deliberately absent: queues always equal vcpus (change-log
/// item 9). Supplying it is an unknown key and aborts boot.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Storage {
    pub bus: u8,
    #[serde(default = "t")]
    pub thread_per_vcpu: bool,
    #[serde(default = "t")]
    pub sqpoll_thread: bool,
    /// Discard is SCSI UNMAP (0x42) only (change-log item 8).
    #[serde(default = "t")]
    pub discard_unmap: bool,
    #[serde(default = "default_max_discard_sectors")]
    pub max_discard_sectors: u64,
    #[serde(default)]
    pub drives: Vec<Drive>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DriveTransport {
    /// virtio-scsi is authoritative (change-log item 8).
    VirtioScsi,
}

/// What kind of drive the guest sees (§5.3, Revision E).
///
/// This is the *presentation*, not the backing store: every medium here is a
/// file behind one of the four §5.4 engines. What it changes is what SCSI
/// says about itself — the peripheral device type, whether the medium is
/// removable, the logical block size, and the rotation rate — and that is
/// what decides whether a guest treats a drive as an SSD, a spinning disk,
/// or something it can boot an installer from.
///
/// virtio-scsi carries all of them on one HBA, which is the reason §5 chose
/// it: SCSI has always addressed mixed device types on one bus, so an
/// optical drive and an SSD need no second controller between them.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DriveMedium {
    /// Solid-state disk. Direct-access, writable, fixed, 512-byte blocks,
    /// and **rotation rate 1** in VPD page 0xB1 — which is the entire
    /// mechanism by which a guest knows it is not a spinning disk. Linux
    /// publishes it as `/sys/block/sdX/queue/rotational = 0` and its I/O
    /// schedulers, readahead and discard behaviour all key off that.
    ///
    /// This is the only non-optical medium there is, deliberately. Every
    /// drive that is not an optical drive is presented as a solid-state
    /// disk that supports TRIM, because every drive here *is* one: the
    /// backing store is a file or a namespace that punches holes, with no
    /// seek latency to model and no reason to ask a guest to schedule for
    /// one. There is no rotating-disk variant to select.
    #[default]
    Ssd,
    /// CD-ROM. MMC peripheral type, removable, read-only, 2048-byte blocks.
    #[serde(rename = "cdrom")]
    CdRom,
    /// DVD-ROM. Identical to [`DriveMedium::CdRom`] except for the profile
    /// reported by GET CONFIGURATION and the capacities that implies.
    #[serde(rename = "dvdrom")]
    DvdRom,
    /// BD-ROM (Blu-ray).
    #[serde(rename = "bdrom")]
    BdRom,
}

/// VPD page 0xB1's value for a solid-state medium (SBC-4 §B.2 table B.2).
pub const NON_ROTATING: u16 = 1;

impl DriveMedium {
    /// SCSI peripheral device type, INQUIRY byte 0 bits 4:0 (SPC-4 §6.4.2).
    ///
    /// `0x00` is direct-access; `0x05` is CD/DVD, which selects the MMC
    /// command set. A guest reads this byte before anything else and it
    /// decides which driver binds: on Linux, `sd` or `sr`.
    pub const fn peripheral_device_type(self) -> u8 {
        match self {
            DriveMedium::Ssd => 0x00,
            DriveMedium::CdRom | DriveMedium::DvdRom | DriveMedium::BdRom => 0x05,
        }
    }

    /// Is this the MMC command set rather than the block command set?
    pub const fn is_optical(self) -> bool {
        self.peripheral_device_type() == 0x05
    }

    /// INQUIRY byte 1 bit 7, RMB. Optical media can be ejected.
    pub const fn is_removable(self) -> bool {
        self.is_optical()
    }

    /// Optical media here are ROM: a write must be refused by the drive, not
    /// by the file permissions, so the guest gets a sense key it understands
    /// rather than an I/O error it does not.
    pub const fn is_read_only(self) -> bool {
        self.is_optical()
    }

    /// Logical block size. 2048 for every optical profile — that is not a
    /// choice, it is the sector size of the media.
    pub const fn block_size(self) -> u32 {
        if self.is_optical() {
            2048
        } else {
            512
        }
    }

    /// VPD page 0xB1 MEDIUM ROTATION RATE (SBC-4 §B.2).
    ///
    /// `1` means "non-rotating", and it is the only value this machine ever
    /// reports for a block device. `None` means the page is not offered at
    /// all, which is correct for optical media: the field describes a block
    /// device's platters, and an MMC drive's spindle speed is not that.
    pub const fn rotation_rate(self) -> Option<u16> {
        match self {
            DriveMedium::Ssd => Some(NON_ROTATING),
            _ => None,
        }
    }

    /// Does this medium support TRIM / SCSI UNMAP?
    ///
    /// Every block device here does, unconditionally. Optical media are ROM
    /// and have nothing to discard.
    pub const fn supports_discard(self) -> bool {
        !self.is_optical()
    }

    /// The MMC feature profile reported by GET CONFIGURATION (MMC-6 §5.3.1).
    pub const fn mmc_profile(self) -> Option<u16> {
        match self {
            DriveMedium::CdRom => Some(0x0008),
            DriveMedium::DvdRom => Some(0x0010),
            DriveMedium::BdRom => Some(0x0040),
            _ => None,
        }
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            DriveMedium::Ssd => "ssd",
            DriveMedium::CdRom => "cdrom",
            DriveMedium::DvdRom => "dvdrom",
            DriveMedium::BdRom => "bdrom",
        }
    }
}

/// `[[storage.drives]]` — one guest disk bound to a unified engine.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Drive {
    pub drive_id: u32,
    #[serde(default)]
    pub bootable: bool,
    #[serde(rename = "type")]
    pub transport: DriveTransport,
    /// What the guest sees. Defaults to [`DriveMedium::Ssd`], so an existing
    /// configuration that names no medium keeps describing a solid-state
    /// disk — which is what every drive in this tree was before the field
    /// existed.
    #[serde(default)]
    pub medium: DriveMedium,
    pub engine: EngineKind,
    /// vhost-user front-end socket for this drive's back-end (§5.6).
    pub socket_path: PathBuf,

    // Engine-specific fields; validated against `engine` in `validate.rs`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pci_bdf: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub nsid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cluster_config: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cluster_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pool_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rbd_image: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub shared_mem_size_mb: Option<u64>,
}

impl Drive {
    /// Project the engine-specific fields into the shared binding shape so
    /// drives, firmware and TPM can be validated by one routine (§5.4).
    pub fn binding(&self) -> EngineBinding {
        EngineBinding {
            engine: self.engine,
            file_path: self.file_path.clone(),
            pci_bdf: self.pci_bdf.clone(),
            nsid: self.nsid,
            cluster_config: self.cluster_config.clone(),
            cluster_name: self.cluster_name.clone(),
            pool_name: self.pool_name.clone(),
            rbd_image: self.rbd_image.clone(),
            shared_mem_size_mb: self.shared_mem_size_mb,
        }
    }
}

// ---------------------------------------------------------------------------
// [network]
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NetEngine {
    /// Pure-Rust AF_XDP datapath (change-log item 1).
    RustAfXdp,
}

/// `[network]` — virtio-net over AF_XDP (§11).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Network {
    pub bus: u8,
    pub engine: NetEngine,
    #[serde(default = "default_queue_pairs")]
    pub num_queue_pairs: u32,
    #[serde(default)]
    pub ipv6_only: bool,
    #[serde(default = "default_ipv6_bind")]
    pub bind_address: String,
    #[serde(default)]
    pub cards: Vec<NetworkCard>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NetworkCard {
    pub card_id: u32,
    pub mac_address: String,
    pub socket_path: PathBuf,
}

// ---------------------------------------------------------------------------
// [usb]
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum UsbControllerKind {
    Xhci,
}

/// `[usb]` — xHCI controller bridging to a remote USB/IP server (§9).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Usb {
    pub bus: u8,
    #[serde(rename = "type", default = "default_usb_kind")]
    pub controller: UsbControllerKind,
    #[serde(default = "default_usb_ports")]
    pub ports: u8,
    pub client_usbip_server: String,
    /// `true` selects :3241 with TLS 1.3 (§9.2).
    #[serde(default = "t")]
    pub use_tls: bool,
    /// SHOULD be true on untrusted networks (§9.2).
    #[serde(default)]
    pub tls_verify_cert: bool,
}

// ---------------------------------------------------------------------------
// [backup]
// ---------------------------------------------------------------------------

/// `[backup]` — low-priority live backup worker (§10).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Backup {
    #[serde(default = "default_zstd_level")]
    pub zstd_level: i32,
    #[serde(default = "default_nice")]
    pub nice_priority: i32,
    #[serde(default = "t")]
    pub ioprio_idle: bool,
    /// Best-effort; on timeout the backup proceeds crash-consistent (item 7).
    #[serde(default = "default_quiesce_timeout")]
    pub quiesce_timeout_secs: u64,
}

impl Default for Backup {
    fn default() -> Self {
        Backup {
            zstd_level: default_zstd_level(),
            nice_priority: default_nice(),
            ioprio_idle: true,
            quiesce_timeout_secs: default_quiesce_timeout(),
        }
    }
}

// ---------------------------------------------------------------------------
// Defaults (every value is the one stated in §11).
// ---------------------------------------------------------------------------

fn t() -> bool {
    true
}
fn one_u8() -> u8 {
    1
}
fn two_u8() -> u8 {
    2
}
fn three_u8() -> u8 {
    3
}
fn four_u8() -> u8 {
    4
}
fn default_code_start_addr() -> u64 {
    0xFFC0_0000
}
fn default_width() -> u32 {
    1920
}
fn default_height() -> u32 {
    1080
}
fn default_color_depth() -> u32 {
    32
}
fn default_framerate_cap() -> u32 {
    30
}
fn default_rate_control() -> RateControl {
    RateControl::Vbr
}
fn default_bitrate_kbps() -> u32 {
    1800
}
fn default_max_bitrate_kbps() -> u32 {
    2000
}
fn default_hwaccel() -> HardwareAccelerator {
    HardwareAccelerator::Vaapi
}
fn default_audio_codec() -> AudioCodec {
    AudioCodec::OggVorbis
}
fn default_audio_bitrate() -> u32 {
    128
}
fn default_sample_rate() -> u32 {
    48_000
}
fn default_channels() -> u8 {
    2
}
fn default_stream_path() -> String {
    "/live".to_string()
}
fn default_rtsp_port() -> u16 {
    8554
}
fn default_wss_port() -> u16 {
    8080
}
fn default_ipv6_bind() -> String {
    "[::]".to_string()
}
fn default_tls_min() -> String {
    "1.3".to_string()
}
fn default_max_clients() -> u32 {
    2
}
fn default_max_auth_attempts() -> u32 {
    10
}
fn default_lockout_secs() -> u64 {
    300
}
fn default_tpm_version() -> String {
    "2.0".to_string()
}
fn default_max_discard_sectors() -> u64 {
    2_097_152
}
fn default_queue_pairs() -> u32 {
    2
}
fn default_usb_kind() -> UsbControllerKind {
    UsbControllerKind::Xhci
}
fn default_usb_ports() -> u8 {
    8
}
fn default_zstd_level() -> i32 {
    3
}
fn default_nice() -> i32 {
    19
}
fn default_quiesce_timeout() -> u64 {
    10
}
