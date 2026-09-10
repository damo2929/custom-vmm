//! Locate librados and librbd for the `rust_ceph_rbd` engine (§5.4).
//!
//! Ceph ships no pkg-config files, so unlike the codecs these are resolved
//! by header probe plus a direct `-l` — which is also why the two sys crates
//! are separate: a missing Ceph should not fail a build that only needs the
//! codecs, and vice versa.

use std::path::PathBuf;
use vmm_sysdeps::{bindgen_builder, Sysroot};

fn main() {
    println!("cargo:rerun-if-changed=build.rs");
    println!("cargo:rerun-if-changed=wrappers");

    let sysroot = Sysroot::discover();
    let out = PathBuf::from(std::env::var("OUT_DIR").expect("OUT_DIR is set by cargo"));

    let mut includes = Vec::new();
    for (lib, header) in [("rados", "rados/librados.h"), ("rbd", "rbd/librbd.h")] {
        match sysroot.link_unpackaged(lib, header) {
            Ok(dir) => includes.push(dir),
            Err(e) => panic!("{e}"),
        }
    }
    includes.sort();
    includes.dedup();

    let bindings = bindgen_builder(&includes)
        .header("wrappers/ceph.h")
        .allowlist_function("rados_.*")
        .allowlist_function("rbd_.*")
        .allowlist_type("rados_.*")
        .allowlist_type("rbd_.*")
        .allowlist_var("RADOS_.*")
        .allowlist_var("RBD_.*")
        .allowlist_var("LIBRADOS_.*")
        .allowlist_var("LIBRBD_.*")
        .generate()
        .unwrap_or_else(|e| panic!("generating Ceph bindings: {e}"));

    bindings
        .write_to_file(out.join("ceph.rs"))
        .unwrap_or_else(|e| panic!("writing ceph.rs: {e}"));
}
