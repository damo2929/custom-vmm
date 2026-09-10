//! ACPI table generation and linkage — §3.3.
//!
//! Tables are built in host memory and copied into a reserved low-memory
//! staging area; the RSDP is placed at a fixed address below the EBDA so OVMF
//! discovers it with no legacy fw_cfg channel.
//!
//! Two rules from §3.3 are enforced here, not merely documented:
//!
//! * **Checksums MUST verify.** Every generated *and* injected table passes
//!   an 8-bit sum-to-zero check before linkage. An MSDM/SLIC binary that
//!   fails is rejected with `Config(AcpiChecksum)` (1010) and boot aborts.
//! * **Injection is optional.** An absent `msdm_path`/`slic_path` skips that
//!   table with a warning; boot continues.

pub mod builder;
pub mod tables;

pub use builder::{AcpiTableSet, LoadedTable};
pub use tables::{checksum8, verify_checksum, SdtHeader, HEADER_LEN};
