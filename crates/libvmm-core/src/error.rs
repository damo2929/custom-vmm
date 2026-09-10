//! Unified error model — §1.6 and Appendix A.
//!
//! Every fallible operation in the suite returns `Result<T, VmmError>`.
//! `VmmError` is a `thiserror` enum whose variants carry a stable numeric
//! code used in logs and in WSS/RTSPS error frames.

use thiserror::Error;

pub type VmmResult<T> = Result<T, VmmError>;

/// Top-level error domain (§1.6). The numeric ranges are Appendix A.
#[derive(Debug, Error)]
pub enum VmmError {
    /// 1xxx — config load / validation.
    #[error("config: {0}")]
    Config(#[from] libvmm_config::ConfigError),
    /// 2xxx — KVM ioctl / vCPU.
    #[error("kvm: {0}")]
    Kvm(#[from] KvmError),
    /// 3xxx — virtqueue / transport.
    #[error("virtio: {0}")]
    Virtio(#[from] VirtioError),
    /// 4xxx — engine / SCSI.
    #[error("storage: {0}")]
    Storage(#[from] StorageError),
    /// 5xxx — capture / encode / RTSP.
    #[error("media: {0}")]
    Media(#[from] MediaError),
    /// 6xxx — WSS / auth.
    #[error("control: {0}")]
    Control(#[from] ControlError),
    /// 7xxx — USB/IP.
    #[error("usbip: {0}")]
    Usbip(#[from] UsbipError),
    /// 8xxx — snapshot / stream.
    #[error("backup: {0}")]
    Backup(#[from] BackupError),
}

impl VmmError {
    /// Stable numeric code carried in logs and error frames (§1.6).
    pub fn code(&self) -> u32 {
        match self {
            VmmError::Config(e) => e.code(),
            VmmError::Kvm(e) => e.code(),
            VmmError::Virtio(e) => e.code(),
            VmmError::Storage(e) => e.code(),
            VmmError::Media(e) => e.code(),
            VmmError::Control(e) => e.code(),
            VmmError::Usbip(e) => e.code(),
            VmmError::Backup(e) => e.code(),
        }
    }

    /// The Appendix A domain name for this error.
    pub fn domain(&self) -> &'static str {
        match self {
            VmmError::Config(_) => "Config",
            VmmError::Kvm(_) => "KVM",
            VmmError::Virtio(_) => "Virtio",
            VmmError::Storage(_) => "Storage",
            VmmError::Media(_) => "Media",
            VmmError::Control(_) => "Control",
            VmmError::Usbip(_) => "USB/IP",
            VmmError::Backup(_) => "Backup",
        }
    }
}

// ---------------------------------------------------------------------------
// 2xxx — KVM
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum KvmError {
    /// 2000 — /dev/kvm could not be opened.
    #[error("cannot open /dev/kvm: {0}")]
    OpenDevice(String),
    /// 2001 — KVM_CREATE_VM failed.
    #[error("KVM_CREATE_VM failed: {0}")]
    CreateVm(String),
    /// 2002 — KVM_CREATE_VCPU failed.
    #[error("KVM_CREATE_VCPU for vcpu {index} failed: {detail}")]
    CreateVcpu { index: u32, detail: String },
    /// 2003 — the host KVM API version is not the one we build against.
    #[error("unsupported KVM API version {got} (expected {expected})")]
    ApiVersion { got: i32, expected: i32 },
    /// 2004 — a required KVM capability is missing.
    #[error("required KVM capability {0} is not available on this host")]
    MissingCapability(&'static str),
    /// 2005 — KVM_SET_USER_MEMORY_REGION failed.
    #[error("KVM_SET_USER_MEMORY_REGION slot {slot} failed: {detail}")]
    SetMemRegion { slot: u32, detail: String },
    /// 2006 — hugepage-backed guest memory could not be mapped.
    #[error("mapping {size_mb} MiB of guest memory failed: {detail}")]
    MemoryMap { size_mb: u64, detail: String },
    /// 2007 — split irqchip could not be enabled; a legacy PIC/PIT would be
    /// required, which §1.4 forbids.
    #[error("KVM_CAP_SPLIT_IRQCHIP could not be enabled: {0}")]
    SplitIrqchip(String),
    /// 2008 — MSI GSI routing setup failed.
    #[error("KVM_SET_GSI_ROUTING failed: {0}")]
    GsiRouting(String),
    /// 2009 — CPUID/MSR/SREGS programming failed.
    #[error("programming vcpu {index} state failed: {detail}")]
    VcpuState { index: u32, detail: String },
    /// 2010 — KVM_RUN returned an error or an exit we cannot service.
    #[error("vcpu {index} run failed: {detail}")]
    VcpuRun { index: u32, detail: String },
    /// 2011 — an eventfd/irqfd registration failed.
    #[error("registering {kind} failed: {detail}")]
    EventFd { kind: &'static str, detail: String },
    /// 2012 — firmware image could not be loaded into its slot.
    #[error("loading firmware {path}: {detail}")]
    FirmwareLoad { path: String, detail: String },
}

impl KvmError {
    pub const fn code(&self) -> u32 {
        match self {
            KvmError::OpenDevice(_) => 2000,
            KvmError::CreateVm(_) => 2001,
            KvmError::CreateVcpu { .. } => 2002,
            KvmError::ApiVersion { .. } => 2003,
            KvmError::MissingCapability(_) => 2004,
            KvmError::SetMemRegion { .. } => 2005,
            KvmError::MemoryMap { .. } => 2006,
            KvmError::SplitIrqchip(_) => 2007,
            KvmError::GsiRouting(_) => 2008,
            KvmError::VcpuState { .. } => 2009,
            KvmError::VcpuRun { .. } => 2010,
            KvmError::EventFd { .. } => 2011,
            KvmError::FirmwareLoad { .. } => 2012,
        }
    }
}

// ---------------------------------------------------------------------------
// 3xxx — virtio
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum VirtioError {
    /// 3001 — the guest cleared FEATURES_OK; the device MUST refuse to run.
    #[error("feature negotiation failed for {device}: {detail}")]
    FeatureMismatch {
        device: &'static str,
        detail: String,
    },
    /// 3002 — the device status handshake was driven out of order.
    #[error("device status transition 0x{from:02x} -> 0x{to:02x} is not permitted")]
    BadStatusTransition { from: u8, to: u8 },
    /// 3005 — a descriptor chain is malformed or points outside guest RAM.
    #[error("bad descriptor in queue {queue}: {detail}")]
    BadDescriptor { queue: u16, detail: String },
    /// 3006 — the driver selected a queue that does not exist.
    #[error("queue {queue} does not exist (device has {count})")]
    NoSuchQueue { queue: u16, count: u16 },
    /// 3007 — the driver programmed an unusable ring size.
    #[error("queue size {size} is not a power of two in 1..={max}")]
    BadQueueSize { size: u16, max: u16 },
    /// 3008 — a queue was enabled before its ring addresses were programmed.
    #[error("queue {queue} enabled with unprogrammed ring addresses")]
    QueueNotConfigured { queue: u16 },
}

impl VirtioError {
    pub const fn code(&self) -> u32 {
        match self {
            VirtioError::FeatureMismatch { .. } => 3001,
            VirtioError::BadStatusTransition { .. } => 3002,
            VirtioError::BadDescriptor { .. } => 3005,
            VirtioError::NoSuchQueue { .. } => 3006,
            VirtioError::BadQueueSize { .. } => 3007,
            VirtioError::QueueNotConfigured { .. } => 3008,
        }
    }
}

// ---------------------------------------------------------------------------
// 4xxx — storage
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum StorageError {
    /// 4001 — a vhost-user back-end closed its socket (§5.6). The VMM MUST
    /// NOT panic: the drive is marked failed and in-flight requests get
    /// CHECK CONDITION.
    #[error("vhost-user back-end for drive {drive_id} was lost: {detail}")]
    BackendLost { drive_id: u32, detail: String },
    /// 4002 — the engine could not be opened at DEVICE_INIT.
    #[error("engine {engine} for {target} failed to open: {detail}")]
    EngineOpen {
        engine: &'static str,
        target: String,
        detail: String,
    },
    /// 4005 — an I/O submission or completion failed.
    #[error("engine I/O error (op {op}, lba {lba}): {detail}")]
    EngineIo {
        op: &'static str,
        lba: u64,
        detail: String,
    },
    /// 4006 — a request addressed an LBA past the end of the image.
    #[error("lba {lba}+{blocks} is out of range (capacity {capacity} blocks)")]
    LbaOutOfRange {
        lba: u64,
        blocks: u64,
        capacity: u64,
    },
    /// 4010 — UNMAP was issued to an engine with no discard support.
    #[error("engine {engine} does not support UNMAP/discard")]
    UnmapUnsupported { engine: &'static str },
    /// 4011 — the guest sent a CDB we do not implement.
    #[error("unsupported SCSI opcode 0x{opcode:02x}")]
    UnsupportedCdb { opcode: u8 },
    /// 4012 — the engine cannot take a snapshot (surfaces as 8001 in backup).
    #[error("engine {engine} cannot snapshot")]
    SnapshotUnsupported { engine: &'static str },
    /// 4013 — a per-queue worker thread could not be started at
    /// `DEVICE_INIT`. Every request queue has its own worker (§5.1), so a
    /// controller short of one has a queue nothing will ever drain — which
    /// hangs the guest rather than failing it. Refusing to build the device
    /// is the only safe answer.
    #[error("{controller}: queue {queue} worker could not be started: {detail}")]
    QueueWorkerSpawn {
        controller: String,
        queue: u16,
        detail: String,
    },
}

impl StorageError {
    pub const fn code(&self) -> u32 {
        match self {
            StorageError::BackendLost { .. } => 4001,
            StorageError::EngineOpen { .. } => 4002,
            StorageError::EngineIo { .. } => 4005,
            StorageError::LbaOutOfRange { .. } => 4006,
            StorageError::UnmapUnsupported { .. } => 4010,
            StorageError::UnsupportedCdb { .. } => 4011,
            StorageError::SnapshotUnsupported { .. } => 4012,
            StorageError::QueueWorkerSpawn { .. } => 4013,
        }
    }
}

// ---------------------------------------------------------------------------
// 5xxx — media
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum MediaError {
    /// 5001 — the hardware encoder could not be initialised.
    #[error("{accelerator} H.264 encoder init failed: {detail}")]
    EncoderInit {
        accelerator: &'static str,
        detail: String,
    },
    /// 5002 — the audio encoder could not be initialised.
    #[error("audio encoder init failed: {0}")]
    AudioEncoderInit(String),
    /// 5003 — an RTSP request was malformed.
    #[error("malformed RTSP request: {0}")]
    BadRequest(String),
    /// 5004 — a method arrived in a state that does not accept it (§7.2).
    #[error("RTSP {method} is not valid in state {state}")]
    BadState { method: String, state: &'static str },
    /// 5005 — Basic Auth missing or invalid; answered 401 before any encoder
    /// resource is allocated (§7.4).
    #[error("RTSP authentication failed: {0}")]
    RtspAuth(String),
    /// 5006 — the requested stream path does not exist.
    #[error("no such RTSP stream: {0}")]
    NoSuchStream(String),
    /// 5007 — TLS setup failed on the RTSPS listener.
    #[error("RTSPS TLS error: {0}")]
    Tls(String),
    /// 5008 — a frame could not be packetised for RTP (§7.3).
    #[error("RTP packetisation failed: {detail}")]
    Packetize { detail: String },
    /// 5009 — a frame could not be encoded (§7.1).
    #[error("{stream} encode failed: {detail}")]
    Encode {
        stream: &'static str,
        detail: String,
    },
    /// 5010 — captured frame geometry did not match the encoder (§7.1).
    #[error("capture geometry mismatch: {detail}")]
    Capture { detail: String },
    /// 5011 — server and client share no codec for a stream (§7.6). The
    /// message names both sets: a client cannot fix a mismatch it cannot see.
    #[error("no common {stream} codec: server offers {offered}, client accepts {wanted}")]
    NoCommonCodec {
        stream: &'static str,
        offered: String,
        wanted: String,
    },
    /// 5012 — the client could not put decoded frames on screen. Distinct
    /// from 5010: that is a frame the encoder cannot accept, this is a
    /// display surface that will not take one.
    #[error("display: {detail}")]
    Display { detail: String },
}

impl MediaError {
    pub const fn code(&self) -> u32 {
        match self {
            MediaError::EncoderInit { .. } => 5001,
            MediaError::AudioEncoderInit(_) => 5002,
            MediaError::BadRequest(_) => 5003,
            MediaError::BadState { .. } => 5004,
            MediaError::RtspAuth(_) => 5005,
            MediaError::NoSuchStream(_) => 5006,
            MediaError::Tls(_) => 5007,
            MediaError::Packetize { .. } => 5008,
            MediaError::Encode { .. } => 5009,
            MediaError::Capture { .. } => 5010,
            MediaError::NoCommonCodec { .. } => 5011,
            MediaError::Display { .. } => 5012,
        }
    }
}

// ---------------------------------------------------------------------------
// 6xxx — control
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum ControlError {
    /// 6000 — the listener could not bind.
    #[error("control listener bind {addr} failed: {detail}")]
    Bind { addr: String, detail: String },
    /// 6005 — a client could not reach the control endpoint.
    #[error("connecting to {addr}: {detail}")]
    Connect { addr: String, detail: String },
    /// 6001 — TLS setup or certificate generation failed.
    #[error("control TLS error: {0}")]
    Tls(String),
    /// 6002 — the WebSocket upgrade handshake was malformed.
    #[error("bad WebSocket upgrade: {0}")]
    BadUpgrade(String),
    /// 6003 — at the client cap; rejected 503 before upgrade (§8.3).
    #[error("client cap of {max} reached")]
    AtClientCap { max: u32 },
    /// 6004 — source IP is locked out; answered 429 with no credential check.
    #[error("source {peer} is locked out for another {remaining_secs}s")]
    LockedOut { peer: String, remaining_secs: u64 },
    /// 6400 — malformed JSON, unknown action, or mismatched `v`.
    #[error("bad control frame: {0}")]
    BadFrame(String),
    /// 6401 — not authenticated (should not occur post-upgrade).
    #[error("not authenticated")]
    NotAuthenticated,
    /// 6422 — input payload out of range (bad keycode / coordinate).
    #[error("input out of range: {0}")]
    InputRange(String),
    /// 6423 — a backup is already running.
    #[error("a backup is already in progress")]
    BackupBusy,
    /// 6500 — internal error executing an action.
    #[error("internal error executing {action}: {detail}")]
    Internal { action: String, detail: String },
}

impl ControlError {
    pub const fn code(&self) -> u32 {
        match self {
            ControlError::Bind { .. } => 6000,
            ControlError::Connect { .. } => 6005,
            ControlError::Tls(_) => 6001,
            ControlError::BadUpgrade(_) => 6002,
            ControlError::AtClientCap { .. } => 6003,
            ControlError::LockedOut { .. } => 6004,
            ControlError::BadFrame(_) => 6400,
            ControlError::NotAuthenticated => 6401,
            ControlError::InputRange(_) => 6422,
            ControlError::BackupBusy => 6423,
            ControlError::Internal { .. } => 6500,
        }
    }
}

// ---------------------------------------------------------------------------
// 7xxx — USB/IP
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum UsbipError {
    /// 7000 — could not connect to the USB/IP server.
    #[error("connecting to USB/IP server {addr}: {detail}")]
    Connect { addr: String, detail: String },
    /// 7001 — the server refused an import (busid not in its allow-set).
    #[error("import of busid {busid} denied by the server")]
    ImportDenied { busid: String },
    /// 7002 — a wire structure was truncated or had a bad version/code.
    #[error("malformed USB/IP {structure}: {detail}")]
    BadWire {
        structure: &'static str,
        detail: String,
    },
    /// 7005 — the peer reset the connection; the device is detached and the
    /// guest gets an xHCI port-disconnect event.
    #[error("USB/IP peer reset: {0}")]
    PeerReset(String),
    /// 7006 — no free virtual xHCI port.
    #[error("no free xHCI port (controller has {ports})")]
    NoFreePort { ports: u8 },
    /// 7007 — TLS error on the :3241 transport.
    #[error("USB/IP TLS error: {0}")]
    Tls(String),
}

impl UsbipError {
    pub const fn code(&self) -> u32 {
        match self {
            UsbipError::Connect { .. } => 7000,
            UsbipError::ImportDenied { .. } => 7001,
            UsbipError::BadWire { .. } => 7002,
            UsbipError::PeerReset(_) => 7005,
            UsbipError::NoFreePort { .. } => 7006,
            UsbipError::Tls(_) => 7007,
        }
    }
}

// ---------------------------------------------------------------------------
// 8xxx — backup
// ---------------------------------------------------------------------------

#[derive(Debug, Error)]
pub enum BackupError {
    /// 8001 — an included drive sits on a non-snapshot engine. PREFLIGHT
    /// aborts rather than produce an inconsistent copy (§10.2).
    #[error("drive {drive_id} uses engine {engine}, which cannot snapshot")]
    EngineNotSnapshotCapable { drive_id: u32, engine: &'static str },
    /// 8002 — taking a snapshot failed.
    #[error("snapshot of {target} failed: {detail}")]
    Snapshot { target: String, detail: String },
    /// 8005 — quiesce timed out. Non-fatal: the backup proceeds
    /// crash-consistent and logs this as a warning (§10.1 step 3).
    #[error("guest quiesce did not complete within {timeout_secs}s; proceeding crash-consistent")]
    QuiesceTimeout { timeout_secs: u64 },
    /// 8010 — writing the .vmbk stream failed.
    #[error("writing backup stream to {path}: {detail}")]
    StreamIo { path: String, detail: String },
    /// 8011 — a backup is already running (mirrors control 6423).
    #[error("a backup is already in progress")]
    AlreadyRunning,
}

impl BackupError {
    pub const fn code(&self) -> u32 {
        match self {
            BackupError::EngineNotSnapshotCapable { .. } => 8001,
            BackupError::Snapshot { .. } => 8002,
            BackupError::QuiesceTimeout { .. } => 8005,
            BackupError::StreamIo { .. } => 8010,
            BackupError::AlreadyRunning => 8011,
        }
    }

    /// 8005 is explicitly a warning, not a failure (§10.1 step 3).
    pub const fn is_warning(&self) -> bool {
        matches!(self, BackupError::QuiesceTimeout { .. })
    }
}
