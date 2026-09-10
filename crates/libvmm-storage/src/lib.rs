//! `libvmm-storage` — virtio-scsi, the four unified engines, the vhost-user
//! front-end and the backup preflight (§5, §10).
//!
//! Two invariants from the change log are structural here:
//!
//! * **Queues always equal vcpus** (item 9). [`queue_count`] is the only way
//!   to obtain a queue count, and it takes the vCPU count — there is no
//!   `num_queues` override anywhere in the crate.
//! * **Discard is SCSI UNMAP (0x42) only** (item 8).

// §0.1: datapath code MUST NOT unwrap, expect or panic. Test code may.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod backup;
pub mod engine;
pub mod engines;
pub mod mmc;
pub mod scsi;
pub mod scsi_pci;
pub mod vhost_user;

pub use engine::{
    capabilities, Completion, CompletionRing, EngineCapabilities, IoOp, IoSlice, SnapshotHandle,
    SnapshotMethod, StorageEngine,
};

/// §5.1 — the request-queue count, which always equals the vCPU count.
///
/// Request queue k is served by thread `scsi-q-k`, pinned with `vcpu-k`, so
/// the guest's per-vCPU queue selection gives lockless per-core submission.
/// There is no override: change-log item 9 makes this a hard 1:1 invariant.
pub const fn queue_count(vcpus: u32) -> u16 {
    if vcpus > u16::MAX as u32 {
        u16::MAX
    } else {
        vcpus as u16
    }
}

/// Total virtqueues on the virtio-scsi device: controlq + eventq + N request
/// queues (§5.1).
pub const fn total_queues(vcpus: u32) -> u16 {
    queue_count(vcpus).saturating_add(2)
}

/// Index of request queue `k` in the device's queue array.
pub const CONTROL_QUEUE: u16 = 0;
pub const EVENT_QUEUE: u16 = 1;
pub const fn request_queue_index(k: u16) -> u16 {
    k + 2
}
