//! `rust_hugepage_file` — volatile hugepage-mapped scratch.
//!
//! Submission is a `memcpy` over the mapped region (§5.4). The engine is
//! **volatile**: it is rejected outright for TPM state (§6.2) and permitted
//! for EFI NVRAM only with a warning (§4.2), and it can never take part in a
//! backup (§10.2, error 8001).

use crate::engine::{Completion, CompletionRing, IoOp, IoSlice, SnapshotHandle, StorageEngine};
use libvmm_config::EngineKind;
use libvmm_core::{StorageError, VmmResult};
use std::path::Path;
use std::sync::Mutex;

pub struct HugepageEngine {
    ptr: *mut u8,
    len: usize,
    block_size: u32,
    completions: Mutex<Vec<Completion>>,
}

// The mapping is owned by this engine and shared with the queue workers.
unsafe impl Send for HugepageEngine {}
unsafe impl Sync for HugepageEngine {}

impl HugepageEngine {
    /// Map `size` bytes of hugepage-backed scratch.
    ///
    /// `path` is honoured when it names a hugetlbfs file, which is how §11's
    /// `/dev/hugepages/fast_scratch.mem` is meant to work; otherwise an
    /// anonymous `MAP_HUGETLB` region is used.
    pub fn open(path: &Path, size: u64, block_size: u32, target: &str) -> VmmResult<Self> {
        let err = |detail: String| -> libvmm_core::VmmError {
            StorageError::EngineOpen {
                engine: EngineKind::RustHugepageFile.as_str(),
                target: target.to_string(),
                detail,
            }
            .into()
        };
        if size == 0 {
            return Err(err(
                "shared_mem_size_mb must be greater than zero".to_string()
            ));
        }
        if block_size == 0 || !block_size.is_power_of_two() {
            return Err(err(format!(
                "block size {block_size} is not a power of two"
            )));
        }

        // Prefer a hugetlbfs-backed file when the parent directory exists, so
        // the region is visible in /dev/hugepages as the reference config
        // expects.
        let file = path.parent().filter(|p| p.exists()).and_then(|_| {
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(path)
                .ok()
        });

        let (fd, flags) = match &file {
            Some(f) => {
                use std::os::unix::io::AsRawFd;
                (f.as_raw_fd(), libc::MAP_SHARED)
            }
            None => (
                -1,
                libc::MAP_PRIVATE | libc::MAP_ANONYMOUS | libc::MAP_HUGETLB,
            ),
        };

        // SAFETY: a fresh mapping with no fixed address; the result is
        // checked against MAP_FAILED before any dereference.
        let ptr = unsafe {
            libc::mmap(
                std::ptr::null_mut(),
                size as usize,
                libc::PROT_READ | libc::PROT_WRITE,
                flags,
                fd,
                0,
            )
        };
        if ptr == libc::MAP_FAILED {
            return Err(err(format!(
                "mmap of {size} bytes failed: {} (are hugepages reserved?)",
                std::io::Error::last_os_error()
            )));
        }

        Ok(HugepageEngine {
            ptr: ptr as *mut u8,
            len: size as usize,
            block_size,
            completions: Mutex::new(Vec::new()),
        })
    }

    fn record(&self, c: Completion) {
        if let Ok(mut q) = self.completions.lock() {
            q.push(c);
        }
    }
}

impl Drop for HugepageEngine {
    fn drop(&mut self) {
        // SAFETY: ptr/len are exactly what mmap returned.
        unsafe {
            libc::munmap(self.ptr as *mut libc::c_void, self.len);
        }
    }
}

impl StorageEngine for HugepageEngine {
    fn kind(&self) -> EngineKind {
        EngineKind::RustHugepageFile
    }

    fn capacity(&self) -> u64 {
        self.len as u64
    }

    fn block_size(&self) -> u32 {
        self.block_size
    }

    fn submit(&self, op: IoOp, lba: u64, iov: &[IoSlice], tag: u64) -> VmmResult<()> {
        let mut offset = (lba * self.block_size as u64) as usize;
        let total: usize = iov.iter().map(|s| s.len).sum();
        self.check_range(lba, total.div_ceil(self.block_size as usize) as u64)?;

        let mut transferred = 0u32;
        for slice in iov {
            if slice.len == 0 {
                continue;
            }
            if offset + slice.len > self.len {
                self.record(Completion::failed(tag, libc::EIO));
                return Err(StorageError::EngineIo {
                    op: op.as_str(),
                    lba,
                    detail: "transfer runs past the end of the mapped region".to_string(),
                }
                .into());
            }
            // SAFETY: bounds checked above; the guest buffer was validated by
            // the descriptor walker and the region is mapped RW.
            unsafe {
                let region = self.ptr.add(offset);
                match op {
                    IoOp::Read => std::ptr::copy_nonoverlapping(region, slice.addr, slice.len),
                    IoOp::Write => std::ptr::copy_nonoverlapping(slice.addr, region, slice.len),
                }
            }
            offset += slice.len;
            transferred += slice.len as u32;
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
        // Scratch memory: discard means zero.
        let offset = (lba * self.block_size as u64) as usize;
        let length = (len * self.block_size as u64) as usize;
        if offset + length > self.len {
            return Err(StorageError::LbaOutOfRange {
                lba,
                blocks: len,
                capacity: self.capacity_blocks(),
            }
            .into());
        }
        // SAFETY: bounds checked above.
        unsafe {
            std::ptr::write_bytes(self.ptr.add(offset), 0, length);
        }
        Ok(())
    }

    fn flush(&self) -> VmmResult<()> {
        // Nothing to flush: the region is volatile by definition.
        Ok(())
    }

    /// §10.2 — volatile scratch cannot be snapshotted. Surfaces as 8001.
    fn snapshot(&self, _dst: &Path) -> VmmResult<SnapshotHandle> {
        Err(self.snapshot_unsupported())
    }

    /// §4.2 / §6.2 hinge on this returning false.
    fn is_persistent(&self) -> bool {
        false
    }
}
