//! Low-priority zstd live backup — §10.
//!
//! Sequence (§10.1):
//! 1. spawn backup-worker: `nice(19)`, `IOPRIO_CLASS_IDLE`
//! 2. PREFLIGHT: every included drive's engine must support snapshot, else 8001
//! 3. QUIESCE, best-effort <= 10s, via `guest-fsfreeze-freeze` over virtio-serial
//! 4. SNAPSHOT each drive + firmware + tpm image
//! 5. THAW (only if step 3 froze)
//! 6. STREAM: tar the snapshots + acpi bins + toml -> zstd -> PATH.vmbk
//! 7. progress frames over WSS; final complete frame with sha256

use crate::engine::StorageEngine;
use libvmm_config::{Drive, MachineConfig};
use libvmm_core::{BackupError, VmmResult};

/// The phases a backup passes through, reported in progress frames.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Phase {
    Preflight,
    Quiesce,
    Snapshot,
    Thaw,
    Stream,
    Cleanup,
}

impl Phase {
    pub const fn as_str(self) -> &'static str {
        match self {
            Phase::Preflight => "preflight",
            Phase::Quiesce => "quiesce",
            Phase::Snapshot => "snapshot",
            Phase::Thaw => "thaw",
            Phase::Stream => "stream",
            Phase::Cleanup => "cleanup",
        }
    }
}

/// Outcome of the §10.1 step-3 quiesce attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QuiesceOutcome {
    /// The guest agent froze the filesystems; the backup is consistent and
    /// step 5 must thaw.
    Frozen,
    /// No agent, or the 10s timeout elapsed. The backup proceeds
    /// crash-consistent with a warning (8005) and step 5 is skipped.
    CrashConsistent,
}

impl QuiesceOutcome {
    pub const fn quiesced(self) -> bool {
        matches!(self, QuiesceOutcome::Frozen)
    }
    /// §10.1 step 5 runs "only if step 3 froze".
    pub const fn needs_thaw(self) -> bool {
        self.quiesced()
    }
}

/// §10.1 step 2 — PREFLIGHT.
///
/// Every included drive must sit on a snapshot-capable engine. If any does
/// not, the backup aborts with `Backup(EngineNotSnapshotCapable)` (8001)
/// rather than produce an inconsistent copy (§10.2).
///
/// Returns the drives that will be captured.
pub fn preflight(cfg: &MachineConfig) -> Result<Vec<&Drive>, BackupError> {
    for d in &cfg.storage.drives {
        if !d.engine.is_snapshot_capable() {
            return Err(BackupError::EngineNotSnapshotCapable {
                drive_id: d.drive_id,
                engine: d.engine.as_str(),
            });
        }
    }
    // Firmware and TPM images are captured too (§10.3), so their engines must
    // also be able to snapshot.
    if !cfg.firmware.storage.engine.is_snapshot_capable() {
        return Err(BackupError::EngineNotSnapshotCapable {
            drive_id: u32::MAX,
            engine: cfg.firmware.storage.engine.as_str(),
        });
    }
    if cfg.tpm.enabled && !cfg.tpm.storage.engine.is_snapshot_capable() {
        return Err(BackupError::EngineNotSnapshotCapable {
            drive_id: u32::MAX,
            engine: cfg.tpm.storage.engine.as_str(),
        });
    }
    Ok(cfg.storage.drives.iter().collect())
}

/// Which engines in a machine cannot snapshot, for the pre-emptive warning
/// the config layer logs at boot (§10.2).
pub fn non_snapshot_drives(cfg: &MachineConfig) -> Vec<&Drive> {
    cfg.storage
        .drives
        .iter()
        .filter(|d| !d.engine.is_snapshot_capable())
        .collect()
}

/// One member of the `.vmbk` archive (§10.3 manifest).
#[derive(Debug, Clone)]
pub struct Member {
    pub path: String,
    pub bytes: u64,
    pub sha256: String,
}

/// The `.vmbk` manifest (§10.3).
#[derive(Debug, Clone)]
pub struct Manifest {
    pub created_utc: String,
    pub vm_name: String,
    pub vm_id: String,
    /// False when step 3 timed out or found no agent.
    pub quiesced: bool,
    pub zstd_level: i32,
    pub members: Vec<Member>,
}

impl Manifest {
    /// Serialise as the JSON §10.3 specifies.
    pub fn to_json(&self) -> String {
        let members: Vec<String> = self
            .members
            .iter()
            .map(|m| {
                format!(
                    r#"{{"path":{},"bytes":{},"sha256":"{}"}}"#,
                    json_string(&m.path),
                    m.bytes,
                    m.sha256
                )
            })
            .collect();
        format!(
            r#"{{"created_utc":"{}","vm":{{"name":{},"id":"{}"}},"quiesced":{},"zstd_level":{},"members":[{}]}}"#,
            self.created_utc,
            json_string(&self.vm_name),
            self.vm_id,
            self.quiesced,
            self.zstd_level,
            members.join(",")
        )
    }
}

fn json_string(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The archive path each captured item takes inside the `.vmbk` (§10.3).
pub fn member_path_for_drive(d: &Drive) -> String {
    let role = if d.bootable { "boot" } else { "data" };
    format!("storage/drive_{}_{}.raw", d.drive_id, role)
}

pub const MEMBER_FIRMWARE: &str = "firmware/efi_nvram.bin";
pub const MEMBER_TPM: &str = "tpm/tpm_state.bin";
pub const MEMBER_CONFIG: &str = "vm_config.toml";
pub const MEMBER_MSDM: &str = "acpi/msdm.bin";
pub const MEMBER_SLIC: &str = "acpi/slic.bin";
pub const MEMBER_MANIFEST: &str = "manifest.json";

/// Apply the §10.1 step-1 scheduling policy to the calling thread.
///
/// The backup must not compete with the guest, so the worker runs at
/// `nice(19)` in the idle I/O class.
pub fn lower_priority(nice_value: i32, ioprio_idle: bool) -> VmmResult<()> {
    // SAFETY: setpriority on the calling thread with an in-range value.
    let rc = unsafe { libc::setpriority(libc::PRIO_PROCESS, 0, nice_value) };
    if rc != 0 {
        log::warn!(
            "backup: could not set nice({nice_value}): {}",
            std::io::Error::last_os_error()
        );
    }

    if ioprio_idle {
        const IOPRIO_WHO_PROCESS: libc::c_int = 1;
        const IOPRIO_CLASS_IDLE: libc::c_int = 3;
        const IOPRIO_CLASS_SHIFT: libc::c_int = 13;
        let value = IOPRIO_CLASS_IDLE << IOPRIO_CLASS_SHIFT;
        // SAFETY: ioprio_set has no memory arguments.
        let rc = unsafe { libc::syscall(libc::SYS_ioprio_set, IOPRIO_WHO_PROCESS, 0, value) };
        if rc != 0 {
            log::warn!(
                "backup: could not set IOPRIO_CLASS_IDLE: {}",
                std::io::Error::last_os_error()
            );
        }
    }
    Ok(())
}

/// Snapshot one engine into `dst`, mapping any engine refusal to 8001.
pub fn snapshot_or_8001(
    engine: &dyn StorageEngine,
    dst: &std::path::Path,
    drive_id: u32,
) -> Result<crate::engine::SnapshotHandle, BackupError> {
    engine.snapshot(dst).map_err(|e| match e {
        libvmm_core::VmmError::Storage(libvmm_core::StorageError::SnapshotUnsupported {
            engine,
        }) => BackupError::EngineNotSnapshotCapable { drive_id, engine },
        other => BackupError::Snapshot {
            target: dst.display().to_string(),
            detail: other.to_string(),
        },
    })
}

// ---------------------------------------------------------------------------
// §10.1 step 6 / §10.3 — the .vmbk stream
// ---------------------------------------------------------------------------

use sha2::{Digest, Sha256};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

/// How often a progress frame is emitted, as a fraction of the total.
const PROGRESS_STEP_PERCENT: u64 = 2;

/// One thing to put in the archive.
#[derive(Debug, Clone)]
pub struct Item {
    /// Path inside the archive (§10.3 layout).
    pub archive_path: String,
    /// Where to read it from — a snapshot, not the live image.
    pub source: PathBuf,
}

/// Progress callback: `(bytes_done, bytes_total)`.
pub type ProgressFn<'a> = dyn Fn(u64, u64) + 'a;

/// Result of a completed stream.
#[derive(Debug)]
pub struct StreamResult {
    pub path: PathBuf,
    /// SHA-256 of the finished `.vmbk`, reported in the `complete` frame.
    pub sha256: String,
    pub bytes_written: u64,
    pub bytes_read: u64,
    pub manifest: Manifest,
}

impl StreamResult {
    /// Compression ratio, for the log.
    pub fn ratio(&self) -> f64 {
        if self.bytes_written == 0 {
            0.0
        } else {
            self.bytes_read as f64 / self.bytes_written as f64
        }
    }
}

/// Write the `.vmbk`: tar the members, zstd the tar, hash the result (§10.3).
///
/// ```text
/// vm-snapshot.vmbk (zstd tarball)
///  |-- manifest.json
///  |-- vm_config.toml
///  |-- firmware/efi_nvram.bin
///  |-- tpm/tpm_state.bin
///  |-- acpi/{msdm.bin, slic.bin}
///  `-- storage/{drive_0_boot.raw, ...}
/// ```
///
/// The manifest is written **last** inside the tar, because it carries each
/// member's SHA-256 and those are only known once the member has been read.
pub fn write_vmbk(
    destination: &Path,
    items: &[Item],
    mut manifest: Manifest,
    zstd_level: i32,
    progress: &ProgressFn<'_>,
) -> Result<StreamResult, BackupError> {
    let io_err = |detail: String| BackupError::StreamIo {
        path: destination.display().to_string(),
        detail,
    };

    // Total bytes to read, so progress is meaningful rather than a spinner.
    let total: u64 = items
        .iter()
        .map(|i| std::fs::metadata(&i.source).map(|m| m.len()).unwrap_or(0))
        .sum();

    let file = std::fs::File::create(destination).map_err(|e| io_err(e.to_string()))?;
    // Hash the compressed stream as it is written, so the `complete` frame's
    // sha256 needs no second pass over the finished file.
    let hashing = HashingWriter::new(file);
    let mut encoder = zstd::stream::write::Encoder::new(hashing, zstd_level)
        .map_err(|e| io_err(format!("initialising zstd level {zstd_level}: {e}")))?;
    // A backup that silently restores corrupt data is worse than one that
    // fails, so put a checksum in the frame itself: the decoder then rejects
    // damage even where it happens to fall in compressed padding, which the
    // per-member SHA-256 alone would not catch.
    encoder
        .include_checksum(true)
        .map_err(|e| io_err(format!("enabling the zstd frame checksum: {e}")))?;
    let mut archive = tar::Builder::new(encoder);

    let mut read_so_far = 0u64;
    let mut last_reported = 0u64;
    manifest.members.clear();

    for item in items {
        let mut source = std::fs::File::open(&item.source)
            .map_err(|e| io_err(format!("{}: {e}", item.source.display())))?;
        let length = source
            .metadata()
            .map_err(|e| io_err(format!("{}: {e}", item.source.display())))?
            .len();

        // Stream through a hasher into the tar: the member is never held in
        // memory, which matters for a multi-terabyte drive image.
        let mut header = tar::Header::new_gnu();
        header.set_size(length);
        header.set_mode(0o600);
        header.set_mtime(manifest_epoch());
        header.set_cksum();

        let mut hasher = Sha256::new();
        let mut counting = CountingReader {
            inner: &mut source,
            hasher: &mut hasher,
            read: 0,
        };

        archive
            .append_data(&mut header, &item.archive_path, &mut counting)
            .map_err(|e| io_err(format!("appending {}: {e}", item.archive_path)))?;

        let member_bytes = counting.read;
        read_so_far += member_bytes;
        manifest.members.push(Member {
            path: item.archive_path.clone(),
            bytes: member_bytes,
            sha256: hex(&hasher.finalize()),
        });

        // §10.1 step 7: progress frames over WSS.
        let percent = (read_so_far * 100).checked_div(total).unwrap_or(100);
        if percent >= last_reported + PROGRESS_STEP_PERCENT || read_so_far == total {
            progress(read_so_far, total);
            last_reported = percent;
        }
    }

    // The manifest goes in last: it names every member's hash.
    let manifest_json = manifest.to_json();
    let mut header = tar::Header::new_gnu();
    header.set_size(manifest_json.len() as u64);
    header.set_mode(0o600);
    header.set_mtime(manifest_epoch());
    header.set_cksum();
    archive
        .append_data(&mut header, MEMBER_MANIFEST, manifest_json.as_bytes())
        .map_err(|e| io_err(format!("appending the manifest: {e}")))?;

    let encoder = archive
        .into_inner()
        .map_err(|e| io_err(format!("finishing the tar: {e}")))?;
    let hashing = encoder
        .finish()
        .map_err(|e| io_err(format!("finishing the zstd stream: {e}")))?;
    let (mut file, digest, written) = hashing.finish();
    file.flush().map_err(|e| io_err(e.to_string()))?;
    // The backup must survive a host crash immediately after it reports done.
    file.sync_all().map_err(|e| io_err(format!("fsync: {e}")))?;

    progress(total, total);

    Ok(StreamResult {
        path: destination.to_path_buf(),
        sha256: hex(&digest),
        bytes_written: written,
        bytes_read: read_so_far,
        manifest,
    })
}

/// Read the manifest back out of a `.vmbk`, to verify one.
pub fn read_manifest(archive: &Path) -> Result<String, BackupError> {
    let io_err = |detail: String| BackupError::StreamIo {
        path: archive.display().to_string(),
        detail,
    };
    let file = std::fs::File::open(archive).map_err(|e| io_err(e.to_string()))?;
    let decoder = zstd::stream::read::Decoder::new(file).map_err(|e| io_err(e.to_string()))?;
    let mut tar = tar::Archive::new(decoder);

    for entry in tar.entries().map_err(|e| io_err(e.to_string()))? {
        let mut entry = entry.map_err(|e| io_err(e.to_string()))?;
        let is_manifest = entry
            .path()
            .map(|p| p.to_string_lossy() == MEMBER_MANIFEST)
            .unwrap_or(false);
        if is_manifest {
            let mut text = String::new();
            entry
                .read_to_string(&mut text)
                .map_err(|e| io_err(e.to_string()))?;
            return Ok(text);
        }
    }
    Err(io_err("no manifest.json in the archive".to_string()))
}

/// Verify every member's SHA-256 against the manifest.
pub fn verify_vmbk(archive: &Path) -> Result<Vec<(String, bool)>, BackupError> {
    let io_err = |detail: String| BackupError::StreamIo {
        path: archive.display().to_string(),
        detail,
    };
    let manifest = read_manifest(archive)?;

    let file = std::fs::File::open(archive).map_err(|e| io_err(e.to_string()))?;
    let decoder = zstd::stream::read::Decoder::new(file).map_err(|e| io_err(e.to_string()))?;
    let mut tar = tar::Archive::new(decoder);

    let mut results = Vec::new();
    for entry in tar.entries().map_err(|e| io_err(e.to_string()))? {
        let mut entry = entry.map_err(|e| io_err(e.to_string()))?;
        let name = entry
            .path()
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or_default();
        if name == MEMBER_MANIFEST {
            continue;
        }

        let mut hasher = Sha256::new();
        let mut buffer = [0u8; 64 * 1024];
        loop {
            match entry.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => hasher.update(&buffer[..n]),
                Err(e) => return Err(io_err(format!("reading {name}: {e}"))),
            }
        }
        let actual = hex(&hasher.finalize());
        // The manifest is small JSON; a substring check is enough to confirm
        // the pair appears together.
        let expected = format!(r#""path":"{name}""#);
        let matches = manifest
            .find(&expected)
            .map(|at| manifest[at..].contains(&actual))
            .unwrap_or(false);
        results.push((name, matches));
    }
    Ok(results)
}

fn manifest_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

/// Wraps a writer, hashing and counting everything that passes through.
struct HashingWriter<W: Write> {
    inner: W,
    hasher: Sha256,
    written: u64,
}

impl<W: Write> HashingWriter<W> {
    fn new(inner: W) -> Self {
        HashingWriter {
            inner,
            hasher: Sha256::new(),
            written: 0,
        }
    }
    fn finish(self) -> (W, Vec<u8>, u64) {
        (self.inner, self.hasher.finalize().to_vec(), self.written)
    }
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        self.written += n as u64;
        Ok(n)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.inner.flush()
    }
}

/// Wraps a reader, hashing and counting on the way past.
struct CountingReader<'a, R: Read> {
    inner: &'a mut R,
    hasher: &'a mut Sha256,
    read: u64,
}

impl<R: Read> Read for CountingReader<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        let n = self.inner.read(buf)?;
        self.hasher.update(&buf[..n]);
        self.read += n as u64;
        Ok(n)
    }
}
