//! `rust_nvme` — host NVMe namespace over VFIO, user-space SQ/CQ (§5.4).
//!
//! The capability contract is complete and authoritative: persistent, and
//! **not** snapshot-capable, so a drive on this engine aborts a backup with
//! `Backup(EngineNotSnapshotCapable)` 8001 (§10.2) — that rule is enforced by
//! the preflight in `libvmm-storage::backup`, which reads these methods.
//!
//! The datapath itself (VFIO binding, admin/IO queue-pair setup, doorbell
//! writes, polled completion) is not implemented in this first cut; `open`
//! returns `Storage(EngineOpen)` 4002 so a machine configured for it fails
//! loudly at DEVICE_INIT rather than silently running on a stub.

use crate::engine::{CompletionRing, IoOp, IoSlice, SnapshotHandle, StorageEngine};
use libvmm_config::{EngineBinding, EngineKind};
use libvmm_core::{StorageError, VmmResult};
use std::path::Path;

pub struct NvmeEngine {
    pub pci_bdf: String,
    pub nsid: u32,
    capacity: u64,
    block_size: u32,
}

impl NvmeEngine {
    pub fn open(binding: &EngineBinding, target: &str, block_size: u32) -> VmmResult<Self> {
        let bdf = binding.pci_bdf.clone().unwrap_or_default();
        let nsid = binding.nsid.unwrap_or(0);
        Err(StorageError::EngineOpen {
            engine: EngineKind::RustNvme.as_str(),
            target: target.to_string(),
            detail: format!(
                "the user-space NVMe driver is not implemented in this build \
                 (would bind {bdf} nsid {nsid} via VFIO with {block_size}-byte blocks)"
            ),
        }
        .into())
    }
}

impl StorageEngine for NvmeEngine {
    fn kind(&self) -> EngineKind {
        EngineKind::RustNvme
    }
    fn capacity(&self) -> u64 {
        self.capacity
    }
    fn block_size(&self) -> u32 {
        self.block_size
    }
    fn submit(&self, op: IoOp, lba: u64, _iov: &[IoSlice], _tag: u64) -> VmmResult<()> {
        Err(StorageError::EngineIo {
            op: op.as_str(),
            lba,
            detail: "NVMe datapath not implemented".into(),
        }
        .into())
    }
    fn poll_completions(&self, _out: &mut CompletionRing) {}
    fn discard(&self, _lba: u64, _len: u64) -> VmmResult<()> {
        // NVMe DEALLOCATE would map here; not implemented in this build.
        Err(StorageError::UnmapUnsupported {
            engine: EngineKind::RustNvme.as_str(),
        }
        .into())
    }
    fn flush(&self) -> VmmResult<()> {
        Err(StorageError::EngineIo {
            op: "flush",
            lba: 0,
            detail: "NVMe datapath not implemented".into(),
        }
        .into())
    }
    /// §10.2 — a raw namespace has no snapshot. Always `Err`.
    fn snapshot(&self, _dst: &Path) -> VmmResult<SnapshotHandle> {
        Err(self.snapshot_unsupported())
    }
    fn is_persistent(&self) -> bool {
        true
    }
}
