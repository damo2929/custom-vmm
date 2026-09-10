//! vhost-user front-end — §5.6.
//!
//! The VMM is the front-end. Each drive's engine back-end connects on its
//! `socket_path`, and the front-end sends `SET_OWNER`, `SET_FEATURES`,
//! `SET_MEM_TABLE` (guest hugepages), `SET_VRING_NUM/ADDR/BASE`,
//! `SET_VRING_KICK` (ioeventfd) and `SET_VRING_CALL` (irqfd).
//!
//! Crash isolation is the load-bearing property here: a back-end crash closes
//! the socket, and the front-end marks the drive failed, returns
//! CHECK CONDITION to in-flight requests, and surfaces `Storage(BackendLost)`.
//! **It MUST NOT panic the VMM.**

use libvmm_core::{StorageError, VmmResult};
use std::path::PathBuf;

/// vhost-user protocol messages the front-end sends (§5.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum FrontEndRequest {
    GetFeatures = 1,
    SetFeatures = 2,
    SetOwner = 3,
    SetMemTable = 5,
    SetVringNum = 8,
    SetVringAddr = 9,
    SetVringBase = 10,
    SetVringKick = 12,
    SetVringCall = 13,
    SetVringEnable = 18,
}

/// The order §5.6 lists, which is also the order the front-end sends them.
pub const HANDSHAKE_ORDER: [FrontEndRequest; 4] = [
    FrontEndRequest::SetOwner,
    FrontEndRequest::SetFeatures,
    FrontEndRequest::SetMemTable,
    FrontEndRequest::SetVringNum,
];

/// One memory region granted to the back-end.
///
/// The back-end never sees guest memory it was not granted in the mem table
/// (§5.6), so this list is the whole of its view.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemoryRegion {
    pub guest_phys_addr: u64,
    pub memory_size: u64,
    pub userspace_addr: u64,
    pub mmap_offset: u64,
}

/// Health of one drive's back-end.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendState {
    Connecting,
    Ready,
    /// The socket closed. In-flight requests get CHECK CONDITION and the
    /// drive stays failed until the VM is reset.
    Failed,
}

/// Front-end state for one drive's back-end connection.
pub struct BackendConnection {
    pub drive_id: u32,
    pub socket_path: PathBuf,
    pub state: BackendState,
    /// Regions granted in `SET_MEM_TABLE`.
    pub mem_table: Vec<MemoryRegion>,
    pub queues: u16,
}

impl BackendConnection {
    pub fn new(drive_id: u32, socket_path: PathBuf, queues: u16) -> Self {
        BackendConnection {
            drive_id,
            socket_path,
            state: BackendState::Connecting,
            mem_table: Vec::new(),
            queues,
        }
    }

    /// Grant exactly the guest RAM regions the back-end may map.
    pub fn set_mem_table(&mut self, regions: Vec<MemoryRegion>) {
        self.mem_table = regions;
    }

    /// Is `[gpa, gpa+len)` inside a granted region?
    ///
    /// Used to assert the §5.6 isolation property: a back-end must not be
    /// able to reach memory outside its mem table.
    pub fn grants(&self, gpa: u64, len: u64) -> bool {
        self.mem_table.iter().any(|r| {
            gpa >= r.guest_phys_addr && gpa.saturating_add(len) <= r.guest_phys_addr + r.memory_size
        })
    }

    /// Record that the back-end's socket closed.
    ///
    /// Returns the error to surface on the control thread. The caller
    /// completes in-flight requests with
    /// [`crate::scsi::ResponseHeader::backend_lost`] — nothing here panics.
    pub fn mark_lost(&mut self, detail: impl Into<String>) -> libvmm_core::VmmError {
        self.state = BackendState::Failed;
        StorageError::BackendLost {
            drive_id: self.drive_id,
            detail: detail.into(),
        }
        .into()
    }

    pub fn is_usable(&self) -> bool {
        self.state == BackendState::Ready
    }

    /// Refuse new submissions to a failed back-end.
    pub fn check_usable(&self) -> VmmResult<()> {
        if self.state == BackendState::Failed {
            return Err(StorageError::BackendLost {
                drive_id: self.drive_id,
                detail: "back-end socket is closed".to_string(),
            }
            .into());
        }
        Ok(())
    }
}
