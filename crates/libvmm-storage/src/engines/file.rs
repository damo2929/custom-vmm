//! `pure_rust_io_uring` — sparse file backing.
//!
//! §5.5 specifies an io_uring SQPOLL worker with registered fixed buffers for
//! zero-copy DMA. This first implementation uses the same submit/poll shape
//! (submissions are queued, completions drained through a ring) over
//! positional `pread`/`pwrite`, so the SCSI layer, the vhost-user front-end
//! and the backup path are all exercised against a real backing store while
//! the io_uring submission path is swapped in underneath.
//!
//! Discard is `FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE` and snapshot is
//! `FICLONE` on a CoW filesystem, both exactly as §5.3 and §10.2 specify.

use crate::engine::{
    Completion, CompletionRing, IoOp, IoSlice, SnapshotHandle, SnapshotMethod, StorageEngine,
};
use libvmm_config::EngineKind;
use libvmm_core::{StorageError, VmmResult};
use std::fs::{File, OpenOptions};
use std::os::unix::fs::{FileExt, OpenOptionsExt};
use std::os::unix::io::AsRawFd;
use std::path::{Path, PathBuf};
use std::sync::Mutex;

/// `FICLONE` — reflink a whole file on a CoW filesystem.
const FICLONE: libc::c_ulong = 0x4009_9409;

pub struct FileEngine {
    file: File,
    path: PathBuf,
    capacity: u64,
    block_size: u32,
    /// Completions waiting to be drained by the queue worker.
    completions: Mutex<Vec<Completion>>,
}

impl FileEngine {
    /// Open (creating if absent) the sparse backing file.
    ///
    /// `capacity_hint` sizes a newly created image; an existing file keeps
    /// its current length so a drive is never silently truncated.
    pub fn open(path: &Path, capacity_hint: u64, block_size: u32, target: &str) -> VmmResult<Self> {
        let err = |detail: String| -> libvmm_core::VmmError {
            StorageError::EngineOpen {
                engine: EngineKind::PureRustIoUring.as_str(),
                target: target.to_string(),
                detail,
            }
            .into()
        };

        let existed = path.exists();
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .custom_flags(libc::O_CLOEXEC)
            .open(path)
            .map_err(|e| err(format!("{}: {e}", path.display())))?;

        if !existed && capacity_hint > 0 {
            // Sparse: set the length without writing blocks.
            file.set_len(capacity_hint)
                .map_err(|e| err(format!("set_len: {e}")))?;
        }

        let capacity = file
            .metadata()
            .map_err(|e| err(format!("stat: {e}")))?
            .len();
        if capacity == 0 {
            // A zero-length image would present a zero-block LUN, which the
            // guest cannot boot from and which would silently swallow every
            // write. §11 has no per-drive size field, so the image must be
            // provisioned before the machine starts.
            return Err(err(format!(
                "{} is empty; provision the image before starting the machine \
                 (e.g. `truncate -s 32G {}`)",
                path.display(),
                path.display()
            )));
        }
        if block_size == 0 || !block_size.is_power_of_two() {
            return Err(err(format!(
                "block size {block_size} is not a power of two"
            )));
        }

        Ok(FileEngine {
            file,
            path: path.to_path_buf(),
            capacity,
            block_size,
            completions: Mutex::new(Vec::new()),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    fn record(&self, c: Completion) {
        if let Ok(mut q) = self.completions.lock() {
            q.push(c);
        }
    }
}

impl StorageEngine for FileEngine {
    fn kind(&self) -> EngineKind {
        EngineKind::PureRustIoUring
    }

    fn capacity(&self) -> u64 {
        self.capacity
    }

    fn block_size(&self) -> u32 {
        self.block_size
    }

    fn submit(&self, op: IoOp, lba: u64, iov: &[IoSlice], tag: u64) -> VmmResult<()> {
        let mut offset = lba * self.block_size as u64;
        let total: usize = iov.iter().map(|s| s.len).sum();
        self.check_range(lba, total.div_ceil(self.block_size as usize) as u64)?;

        let mut transferred = 0u32;
        for slice in iov {
            if slice.len == 0 {
                continue;
            }
            // SAFETY: the descriptor walker validated that each buffer lies
            // inside guest RAM and it stays mapped for the life of the VM.
            let result = unsafe {
                match op {
                    IoOp::Read => self.file.read_at(slice.as_mut_slice(), offset),
                    IoOp::Write => self.file.write_at(slice.as_slice(), offset),
                }
            };
            match result {
                Ok(n) => {
                    transferred += n as u32;
                    offset += n as u64;
                    if n < slice.len {
                        // Short read past EOF: the rest of a read is zeroes,
                        // which a sparse image implies anyway.
                        break;
                    }
                }
                Err(e) => {
                    let errno = e.raw_os_error().unwrap_or(libc::EIO);
                    self.record(Completion::failed(tag, errno));
                    return Err(StorageError::EngineIo {
                        op: op.as_str(),
                        lba,
                        detail: e.to_string(),
                    }
                    .into());
                }
            }
        }
        self.record(Completion::ok(tag, transferred));
        Ok(())
    }

    fn poll_completions(&self, out: &mut CompletionRing) {
        if let Ok(mut q) = self.completions.lock() {
            while let Some(c) = q.first().copied() {
                if !out.push(c) {
                    break;
                }
                q.remove(0);
            }
        }
    }

    fn discard(&self, lba: u64, len: u64) -> VmmResult<()> {
        let offset = (lba * self.block_size as u64) as libc::off_t;
        let length = (len * self.block_size as u64) as libc::off_t;
        // SAFETY: a plain fallocate on an owned fd with in-range arguments.
        let rc = unsafe {
            libc::fallocate(
                self.file.as_raw_fd(),
                libc::FALLOC_FL_PUNCH_HOLE | libc::FALLOC_FL_KEEP_SIZE,
                offset,
                length,
            )
        };
        if rc != 0 {
            let e = std::io::Error::last_os_error();
            // A filesystem with no hole-punching support is a configuration
            // problem, not a medium error: report it as UnmapUnsupported.
            if e.raw_os_error() == Some(libc::EOPNOTSUPP) {
                return Err(StorageError::UnmapUnsupported {
                    engine: EngineKind::PureRustIoUring.as_str(),
                }
                .into());
            }
            return Err(StorageError::EngineIo {
                op: "unmap",
                lba,
                detail: e.to_string(),
            }
            .into());
        }
        Ok(())
    }

    fn flush(&self) -> VmmResult<()> {
        self.file.sync_data().map_err(|e| {
            StorageError::EngineIo {
                op: "flush",
                lba: 0,
                detail: e.to_string(),
            }
            .into()
        })
    }

    /// §10.2 — reflink / `FICLONE` on a CoW filesystem (XFS/Btrfs).
    fn snapshot(&self, dst: &Path) -> VmmResult<SnapshotHandle> {
        let fail = |detail: String| -> libvmm_core::VmmError {
            StorageError::EngineIo {
                op: "snapshot",
                lba: 0,
                detail,
            }
            .into()
        };

        // Flush first so the clone is at least as new as the last completion.
        self.flush()?;

        let out = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .custom_flags(libc::O_CLOEXEC)
            .open(dst)
            .map_err(|e| fail(format!("{}: {e}", dst.display())))?;

        // SAFETY: FICLONE takes the source fd by value; both fds are owned
        // and open for the duration of the call.
        let rc = unsafe { libc::ioctl(out.as_raw_fd(), FICLONE, self.file.as_raw_fd()) };
        if rc == 0 {
            return Ok(SnapshotHandle {
                path: dst.to_path_buf(),
                method: SnapshotMethod::Reflink,
                bytes: self.capacity,
            });
        }

        // Not a CoW filesystem. The engine is still declared snapshot-capable
        // (§10.2), so fall back to a full copy rather than failing the backup,
        // and say so in the manifest via the method field.
        let e = std::io::Error::last_os_error();
        log::warn!(
            "reflink of {} failed ({e}); falling back to a full copy — put drive images on XFS or Btrfs for instant snapshots",
            self.path.display()
        );
        drop(out);
        std::fs::copy(&self.path, dst).map_err(|e| fail(format!("full copy: {e}")))?;
        Ok(SnapshotHandle {
            path: dst.to_path_buf(),
            method: SnapshotMethod::FullCopy,
            bytes: self.capacity,
        })
    }

    fn is_persistent(&self) -> bool {
        true
    }
}
