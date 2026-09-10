//! The unified storage engine interface — §5.4.
//!
//! All four engines implement one trait; firmware (§4.1), TPM (§6.2) and
//! every drive consume it identically. This is the single uniform storage
//! model of the functional spec.

use libvmm_config::EngineKind;
use libvmm_core::{StorageError, VmmResult};
use std::path::{Path, PathBuf};

/// An I/O operation submitted to an engine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum IoOp {
    Read,
    Write,
}

impl IoOp {
    pub const fn as_str(self) -> &'static str {
        match self {
            IoOp::Read => "read",
            IoOp::Write => "write",
        }
    }
}

/// A scatter/gather element. Borrowed from guest memory so the io_uring path
/// can register it as a fixed buffer and DMA into it with no copy (§5.5).
#[derive(Debug, Clone, Copy)]
pub struct IoSlice {
    pub addr: *mut u8,
    pub len: usize,
}

// The pointers refer to guest hugepages owned by the VM and shared with the
// queue-worker threads by design.
unsafe impl Send for IoSlice {}
unsafe impl Sync for IoSlice {}

impl IoSlice {
    /// # Safety
    /// `addr` must point at `len` valid bytes that outlive the submission.
    pub const unsafe fn new(addr: *mut u8, len: usize) -> Self {
        IoSlice { addr, len }
    }

    /// # Safety
    /// The caller must ensure no other reference to the region is live.
    pub unsafe fn as_slice(&self) -> &[u8] {
        std::slice::from_raw_parts(self.addr, self.len)
    }

    /// # Safety
    /// The caller must ensure no other reference to the region is live.
    #[allow(clippy::mut_from_ref)]
    pub unsafe fn as_mut_slice(&self) -> &mut [u8] {
        std::slice::from_raw_parts_mut(self.addr, self.len)
    }
}

/// A finished submission, matched to its request by `tag`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Completion {
    pub tag: u64,
    /// Bytes transferred, or the error if `result` is negative.
    pub bytes: u32,
    /// `None` on success; on failure the errno the engine reported.
    pub error: Option<i32>,
}

impl Completion {
    pub const fn ok(tag: u64, bytes: u32) -> Self {
        Completion {
            tag,
            bytes,
            error: None,
        }
    }
    pub const fn failed(tag: u64, errno: i32) -> Self {
        Completion {
            tag,
            bytes: 0,
            error: Some(errno),
        }
    }
    pub const fn is_ok(&self) -> bool {
        self.error.is_none()
    }
}

/// A fixed-capacity completion ring. The datapath drains it without
/// allocating (§0.1).
pub struct CompletionRing {
    entries: Vec<Completion>,
    capacity: usize,
}

impl CompletionRing {
    pub fn with_capacity(capacity: usize) -> Self {
        CompletionRing {
            entries: Vec::with_capacity(capacity),
            capacity,
        }
    }

    /// Record a completion. Returns false if the ring is full, which the
    /// worker treats as backpressure rather than an error.
    pub fn push(&mut self, c: Completion) -> bool {
        if self.entries.len() >= self.capacity {
            return false;
        }
        self.entries.push(c);
        true
    }

    pub fn drain(&mut self) -> std::vec::Drain<'_, Completion> {
        self.entries.drain(..)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// A snapshot taken for a backup (§10).
#[derive(Debug, Clone)]
pub struct SnapshotHandle {
    /// Where the snapshot can be read from for streaming.
    pub path: PathBuf,
    /// How the snapshot was taken, for the manifest and the log.
    pub method: SnapshotMethod,
    pub bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotMethod {
    /// `FICLONE` reflink on a CoW filesystem (XFS/Btrfs).
    Reflink,
    /// Native RBD snapshot.
    RbdSnapshot,
    /// Byte-for-byte copy. Only used where a CoW clone is unavailable but the
    /// engine is still declared snapshot-capable.
    FullCopy,
}

/// §5.4 — the one interface every engine implements.
pub trait StorageEngine: Send + Sync {
    /// Which of the four engines this is.
    fn kind(&self) -> EngineKind;

    /// Capacity in bytes.
    fn capacity(&self) -> u64;

    /// Logical block size: 512 or 4096 (§5.3 READ CAPACITY).
    fn block_size(&self) -> u32;

    /// Submit an operation. Completions surface through
    /// [`poll_completions`](StorageEngine::poll_completions).
    fn submit(&self, op: IoOp, lba: u64, iov: &[IoSlice], tag: u64) -> VmmResult<()>;

    /// Drain finished operations into `out`.
    fn poll_completions(&self, out: &mut CompletionRing);

    /// SCSI UNMAP (§5.3): punch holes, or the engine-native discard.
    fn discard(&self, lba: u64, len: u64) -> VmmResult<()>;

    /// SYNCHRONIZE CACHE (§5.3).
    fn flush(&self) -> VmmResult<()>;

    /// Take a snapshot for a backup. `Err` when the engine cannot (§10.2).
    fn snapshot(&self, dst: &Path) -> VmmResult<SnapshotHandle>;

    /// Does state survive host reboot? TPM requires `true` (§6.2).
    fn is_persistent(&self) -> bool;

    /// Capacity in logical blocks.
    fn capacity_blocks(&self) -> u64 {
        self.capacity()
            .checked_div(self.block_size() as u64)
            .unwrap_or(0)
    }

    /// Reject a request that runs past the end of the image before it reaches
    /// the backing store.
    fn check_range(&self, lba: u64, blocks: u64) -> VmmResult<()> {
        let capacity = self.capacity_blocks();
        if lba.saturating_add(blocks) > capacity {
            return Err(StorageError::LbaOutOfRange {
                lba,
                blocks,
                capacity,
            }
            .into());
        }
        Ok(())
    }

    /// Shared implementation of the §10.2 "can this engine snapshot?" rule.
    fn snapshot_unsupported(&self) -> libvmm_core::VmmError {
        StorageError::SnapshotUnsupported {
            engine: self.kind().as_str(),
        }
        .into()
    }
}

/// The §5.4 / §10.2 capability matrix, as data.
///
/// Keeping it in one place means the backup preflight (§10.1 step 2), the
/// TPM volatile-engine check (§6.2) and the EFI NVRAM warning (§4.2) all
/// consult the same source of truth.
#[derive(Debug, Clone, Copy)]
pub struct EngineCapabilities {
    pub kind: EngineKind,
    pub backing: &'static str,
    pub submit_path: &'static str,
    pub snapshot: Option<SnapshotMethod>,
    pub persistent: bool,
}

pub const CAPABILITIES: [EngineCapabilities; 4] = [
    EngineCapabilities {
        kind: EngineKind::PureRustIoUring,
        backing: "sparse file",
        submit_path: "io_uring SQPOLL, fixed buffers",
        snapshot: Some(SnapshotMethod::Reflink),
        persistent: true,
    },
    EngineCapabilities {
        kind: EngineKind::RustNvme,
        backing: "host NVMe namespace (VFIO)",
        submit_path: "user-space NVMe SQ/CQ, polled",
        snapshot: None,
        persistent: true,
    },
    EngineCapabilities {
        kind: EngineKind::RustCephRbd,
        backing: "Ceph RBD image",
        submit_path: "native Rust RADOS async ops",
        snapshot: Some(SnapshotMethod::RbdSnapshot),
        persistent: true,
    },
    EngineCapabilities {
        kind: EngineKind::RustHugepageFile,
        backing: "hugepage mmap",
        submit_path: "memcpy over mapped region",
        snapshot: None,
        persistent: false,
    },
];

pub fn capabilities(kind: EngineKind) -> EngineCapabilities {
    // The array is exhaustive over EngineKind; the fallback keeps this
    // total without a panic on the boot path.
    CAPABILITIES
        .iter()
        .find(|c| c.kind == kind)
        .copied()
        .unwrap_or(CAPABILITIES[0])
}
