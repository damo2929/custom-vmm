//! `rust_ceph_rbd` — a Ceph RBD image over librados/librbd (§5.4).
//!
//! Persistent, and snapshot-capable through a native RBD snapshot rather
//! than a copy (§10.2), which is what makes a backup of a multi-terabyte
//! image finish in constant time.
//!
//! Submissions are asynchronous. Each `submit` issues one `rbd_aio_*` per
//! scatter/gather element and parks the completions; `poll_completions`
//! reaps the finished ones and reports a single [`Completion`] per tag once
//! every element of that request has landed. That matches §5.4's interface
//! and keeps the queue worker from blocking on network round trips.
//!
//! §1.1 originally forbade linking librados, which left this engine
//! unimplementable. That rule was lifted; see HOST-REQUIREMENTS.md §6.

use crate::engine::{
    Completion, CompletionRing, IoOp, IoSlice, SnapshotHandle, SnapshotMethod, StorageEngine,
};
use libvmm_config::{EngineBinding, EngineKind};
use libvmm_core::{StorageError, VmmError, VmmResult};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use vmm_rbd_sys::{Cluster, Image, IoCtx, RbdError};

/// The client entity used when the drive names none. `client.admin` is the
/// conventional default and matches what `rbd` itself assumes.
const DEFAULT_USER: &str = "admin";
/// The conventional cluster name.
const DEFAULT_CLUSTER: &str = "ceph";

/// One submitted scatter/gather element, awaiting its completion.
struct Inflight {
    tag: u64,
    op: IoOp,
    completion: vmm_rbd_sys::Completion,
}

/// Per-tag accounting, so a chained request reports one completion.
struct Pending {
    /// Elements still in flight for this tag.
    outstanding: usize,
    bytes: u32,
    error: Option<i32>,
}

pub struct RbdEngine {
    image: Image,
    /// Retained so the pool context and cluster outlive the image.
    _ioctx: Arc<IoCtx>,
    pub cluster_name: String,
    pub pool_name: String,
    pub image_name: String,
    capacity: u64,
    block_size: u32,
    inflight: Mutex<Vec<Inflight>>,
    pending: Mutex<std::collections::HashMap<u64, Pending>>,
    completed: Mutex<Vec<Completion>>,
}

impl RbdEngine {
    /// Connect to the cluster and open the drive's image.
    pub fn open(binding: &EngineBinding, target: &str, block_size: u32) -> VmmResult<Self> {
        let fail = |detail: String| -> VmmError {
            StorageError::EngineOpen {
                engine: EngineKind::RustCephRbd.as_str(),
                target: target.to_string(),
                detail,
            }
            .into()
        };

        let pool = binding
            .pool_name
            .as_deref()
            .ok_or_else(|| fail("rust_ceph_rbd requires pool_name".to_string()))?;
        let image_name = binding
            .rbd_image
            .as_deref()
            .ok_or_else(|| fail("rust_ceph_rbd requires rbd_image".to_string()))?;
        let cluster_name = binding.cluster_name.as_deref().unwrap_or(DEFAULT_CLUSTER);
        let config = binding.cluster_config.as_deref();

        let cluster = Cluster::connect(config, cluster_name, DEFAULT_USER, None)
            .map_err(|e| fail(format!("{e}. {}", connect_hint(&e, config, cluster_name))))?;

        let ioctx = cluster
            .pool(pool)
            .map_err(|e| fail(format!("opening pool {pool}: {e}")))?;

        let image = Image::open(&ioctx, image_name, None)
            .map_err(|e| fail(format!("opening image {pool}/{image_name}: {e}")))?;

        let capacity = image
            .size()
            .map_err(|e| fail(format!("reading the size of {pool}/{image_name}: {e}")))?;

        if capacity == 0 {
            return Err(fail(format!(
                "{pool}/{image_name} is zero bytes; create it with \
                 `rbd create --size 32G {pool}/{image_name}`"
            )));
        }
        if capacity % block_size as u64 != 0 {
            return Err(fail(format!(
                "{pool}/{image_name} is {capacity} bytes, which is not a whole \
                 number of {block_size}-byte logical blocks"
            )));
        }

        log::info!(
            "rust_ceph_rbd: opened {pool}/{image_name} on cluster {cluster_name} \
             — {capacity} bytes, {block_size}-byte blocks"
        );

        Ok(RbdEngine {
            image,
            _ioctx: ioctx,
            cluster_name: cluster_name.to_string(),
            pool_name: pool.to_string(),
            image_name: image_name.to_string(),
            capacity,
            block_size,
            inflight: Mutex::new(Vec::new()),
            pending: Mutex::new(std::collections::HashMap::new()),
            completed: Mutex::new(Vec::new()),
        })
    }

    /// Record that `count` elements were submitted under one tag.
    fn begin(&self, tag: u64, count: usize) {
        if let Ok(mut pending) = self.pending.lock() {
            let entry = pending.entry(tag).or_insert(Pending {
                outstanding: 0,
                bytes: 0,
                error: None,
            });
            entry.outstanding += count;
        }
    }

    /// Fold one finished element into its tag, publishing the tag's own
    /// completion when the last element lands.
    fn settle(&self, tag: u64, op: IoOp, result: i64) {
        let Ok(mut pending) = self.pending.lock() else {
            return;
        };
        let Some(entry) = pending.get_mut(&tag) else {
            return;
        };

        if result < 0 {
            let errno = (-result).min(i32::MAX as i64) as i32;
            entry.error.get_or_insert(errno);
            log::warn!(
                "rust_ceph_rbd: {} on {}/{} failed: {}",
                op.as_str(),
                self.pool_name,
                self.image_name,
                std::io::Error::from_raw_os_error(errno)
            );
        } else {
            // A write returns 0 on success rather than a byte count, so take
            // the length from the request in that case.
            entry.bytes = entry
                .bytes
                .saturating_add(result.min(u32::MAX as i64) as u32);
        }

        entry.outstanding = entry.outstanding.saturating_sub(1);
        if entry.outstanding > 0 {
            return;
        }

        let finished = match entry.error {
            Some(errno) => Completion::failed(tag, errno),
            None => Completion::ok(tag, entry.bytes),
        };
        pending.remove(&tag);
        drop(pending);

        if let Ok(mut completed) = self.completed.lock() {
            completed.push(finished);
        }
    }

    /// Reap every element librbd has finished with.
    fn reap(&self) {
        let Ok(mut inflight) = self.inflight.lock() else {
            return;
        };
        let mut settled = Vec::new();
        inflight.retain(|entry| {
            if entry.completion.is_complete() {
                settled.push((entry.tag, entry.op, entry.completion.result()));
                false
            } else {
                true
            }
        });
        drop(inflight);

        for (tag, op, result) in settled {
            self.settle(tag, op, result);
        }
    }

    /// Block until everything in flight has landed.
    fn quiesce(&self) -> VmmResult<()> {
        loop {
            let next = {
                let Ok(inflight) = self.inflight.lock() else {
                    return Ok(());
                };
                inflight.is_empty()
            };
            if next {
                return Ok(());
            }
            // Waiting on the head is enough to make progress: reap() then
            // clears every element that finished alongside it.
            {
                let Ok(inflight) = self.inflight.lock() else {
                    return Ok(());
                };
                if let Some(entry) = inflight.first() {
                    entry
                        .completion
                        .wait()
                        .map_err(|e| io_error("flush", 0, e))?;
                }
            }
            self.reap();
        }
    }

    fn byte_offset(&self, lba: u64) -> u64 {
        lba * self.block_size as u64
    }
}

fn io_error(op: &'static str, lba: u64, e: RbdError) -> VmmError {
    StorageError::EngineIo {
        op,
        lba,
        detail: e.to_string(),
    }
    .into()
}

/// Turn a connect failure into something an operator can act on.
fn connect_hint(e: &RbdError, config: Option<&Path>, cluster: &str) -> String {
    if e.is_permission_denied() {
        return "Check that the keyring for this client entity is readable and \
                that the cephx capability allows access to the pool."
            .to_string();
    }
    if e.is_not_found() {
        return match config {
            Some(path) => format!("No such config file: {}.", path.display()),
            None => format!(
                "No ceph.conf found. Set cluster_config on the drive, or install \
                 /etc/ceph/{cluster}.conf."
            ),
        };
    }
    "Check that the monitors in ceph.conf are reachable.".to_string()
}

impl StorageEngine for RbdEngine {
    fn kind(&self) -> EngineKind {
        EngineKind::RustCephRbd
    }

    fn capacity(&self) -> u64 {
        self.capacity
    }

    fn block_size(&self) -> u32 {
        self.block_size
    }

    fn submit(&self, op: IoOp, lba: u64, iov: &[IoSlice], tag: u64) -> VmmResult<()> {
        let total: usize = iov.iter().map(|s| s.len).sum();
        self.check_range(lba, total.div_ceil(self.block_size as usize) as u64)?;

        let elements: Vec<&IoSlice> = iov.iter().filter(|s| s.len > 0).collect();
        if elements.is_empty() {
            // Nothing to do, but the caller is still owed a completion.
            if let Ok(mut completed) = self.completed.lock() {
                completed.push(Completion::ok(tag, 0));
            }
            return Ok(());
        }

        let count = elements.len();
        self.begin(tag, count);

        let mut offset = self.byte_offset(lba);
        let mut submitted = 0usize;
        for slice in elements {
            // SAFETY: the descriptor walker validated that each buffer lies
            // inside guest RAM, which stays mapped for the life of the VM,
            // and the buffer is not touched again until its completion is
            // reaped in poll_completions.
            let result = unsafe {
                match op {
                    IoOp::Read => self.image.read_async(offset, slice.addr, slice.len),
                    IoOp::Write => self.image.write_async(offset, slice.addr, slice.len),
                }
            };

            match result {
                Ok(completion) => {
                    if let Ok(mut inflight) = self.inflight.lock() {
                        inflight.push(Inflight {
                            tag,
                            op,
                            completion,
                        });
                    }
                    offset += slice.len as u64;
                    submitted += 1;
                }
                Err(e) => {
                    // Settle the elements that were never submitted, so the
                    // tag completes instead of leaking an entry that nothing
                    // will ever decrement.
                    for _ in submitted..count {
                        self.settle(tag, op, -(libc::EIO as i64));
                    }
                    return Err(io_error(op.as_str(), lba, e));
                }
            }
        }

        Ok(())
    }

    fn poll_completions(&self, out: &mut CompletionRing) {
        self.reap();
        if let Ok(mut completed) = self.completed.lock() {
            while let Some(c) = completed.first().copied() {
                if !out.push(c) {
                    break;
                }
                completed.remove(0);
            }
        }
    }

    fn discard(&self, lba: u64, len: u64) -> VmmResult<()> {
        // RBD discard is a native operation, not a hole punch: it removes
        // whole backing objects, which is what makes UNMAP actually reclaim
        // cluster capacity (§5.3).
        let completion = self
            .image
            .discard_async(self.byte_offset(lba), len * self.block_size as u64)
            .map_err(|e| io_error("unmap", lba, e))?;
        let result = completion.wait().map_err(|e| io_error("unmap", lba, e))?;
        if result < 0 {
            return Err(io_error(
                "unmap",
                lba,
                RbdError::new("rbd_aio_discard", &self.image_name, result as i32),
            ));
        }
        Ok(())
    }

    fn flush(&self) -> VmmResult<()> {
        // Everything already submitted must land before the flush means
        // anything, so drain first.
        self.quiesce()?;
        self.image.flush().map_err(|e| io_error("flush", 0, e))
    }

    /// §10.2 — a native RBD snapshot, taken in constant time.
    ///
    /// `dst` names where a file-backed engine would put its copy. There is
    /// no file here: the snapshot lives in the cluster, so the destination's
    /// file name is used as the snapshot name and the handle points at the
    /// `pool/image@snapshot` spec the manifest records.
    fn snapshot(&self, dst: &Path) -> VmmResult<SnapshotHandle> {
        let name = dst.file_name().and_then(|n| n.to_str()).ok_or_else(|| {
            io_error(
                "snapshot",
                0,
                RbdError::new("snapshot", dst.display().to_string(), -libc::EINVAL),
            )
        })?;

        // Quiesce so the snapshot is at least as new as the last completion.
        self.flush()?;

        self.image
            .snapshot(name)
            .map_err(|e| io_error("snapshot", 0, e))?;

        log::info!(
            "rust_ceph_rbd: snapshot {}/{}@{name}",
            self.pool_name,
            self.image_name
        );

        Ok(SnapshotHandle {
            path: PathBuf::from(format!("{}/{}@{name}", self.pool_name, self.image_name)),
            method: SnapshotMethod::RbdSnapshot,
            bytes: self.capacity,
        })
    }

    fn is_persistent(&self) -> bool {
        true
    }
}

impl Drop for RbdEngine {
    fn drop(&mut self) {
        // Never leave a completion in flight pointing at guest memory that
        // is about to be unmapped.
        if let Err(e) = self.quiesce() {
            log::warn!("rust_ceph_rbd: draining {} on close: {e}", self.image_name);
        }
    }
}
