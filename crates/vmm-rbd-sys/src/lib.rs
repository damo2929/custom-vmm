//! librados/librbd bindings for the `rust_ceph_rbd` engine (§5.4).
//!
//! §1.1's no-C rule was lifted; this crate is the entire Ceph surface. The
//! rest of the tree sees safe types only, and the ownership nesting librados
//! requires — image inside pool context inside cluster — is enforced by
//! reference counting rather than left to the caller.

pub mod raw;

mod cluster;
mod error;
mod image;

pub use cluster::{Cluster, IoCtx};
pub use error::{RbdError, Result};
pub use image::{Completion, Image};
