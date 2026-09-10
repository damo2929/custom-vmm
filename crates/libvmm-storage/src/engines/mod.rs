//! The four unified engines — §5.4.
//!
//! | engine               | backing                     | snapshot        | persistent |
//! |----------------------|-----------------------------|-----------------|------------|
//! | `pure_rust_io_uring` | sparse file                 | reflink/FICLONE | yes        |
//! | `rust_nvme`          | host NVMe namespace (VFIO)  | none            | yes        |
//! | `rust_ceph_rbd`      | Ceph RBD image              | RBD snapshot    | yes        |
//! | `rust_hugepage_file` | hugepage mmap               | none            | no         |
//!
//! Implementation status of this first cut is recorded in
//! `IMPLEMENTATION-STATUS.md`: the file-backed and hugepage engines are
//! complete; the NVMe and RBD engines carry their full capability contract
//! (which is what the TPM, firmware and backup rules consult) but return
//! `Storage(EngineOpen)` on open, because a user-space NVMe driver and a
//! native Rust RADOS client are separate bodies of work.

pub mod file;
pub mod hugepage;
pub mod nvme;
pub mod rbd;

use crate::engine::StorageEngine;
use libvmm_config::{EngineBinding, EngineKind};
use libvmm_core::VmmResult;

/// Open the engine an `[firmware.storage]`, `[tpm.storage]` or drive binding
/// names. This is the single construction point for the uniform model (§5.4).
pub fn open(
    binding: &EngineBinding,
    target: &str,
    capacity_hint: u64,
    block_size: u32,
) -> VmmResult<Box<dyn StorageEngine>> {
    match binding.engine {
        EngineKind::PureRustIoUring => {
            let path = binding
                .file_path
                .as_deref()
                .ok_or_else(|| missing(binding.engine, target, "file_path"))?;
            Ok(Box::new(file::FileEngine::open(
                path,
                capacity_hint,
                block_size,
                target,
            )?))
        }
        EngineKind::RustHugepageFile => {
            let path = binding
                .file_path
                .as_deref()
                .ok_or_else(|| missing(binding.engine, target, "file_path"))?;
            let size = binding.shared_mem_size_mb.unwrap_or(0) * 1024 * 1024;
            Ok(Box::new(hugepage::HugepageEngine::open(
                path, size, block_size, target,
            )?))
        }
        EngineKind::RustNvme => Ok(Box::new(nvme::NvmeEngine::open(
            binding, target, block_size,
        )?)),
        EngineKind::RustCephRbd => Ok(Box::new(rbd::RbdEngine::open(binding, target, block_size)?)),
    }
}

fn missing(engine: EngineKind, target: &str, field: &str) -> libvmm_core::VmmError {
    libvmm_core::StorageError::EngineOpen {
        engine: engine.as_str(),
        target: target.to_string(),
        detail: format!("binding is missing `{field}`"),
    }
    .into()
}
