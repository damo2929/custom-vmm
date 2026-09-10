//! `rust_ceph_rbd` (§5.4) — the paths that do not need a live cluster.
//!
//! Connecting is covered by the ignored test at the bottom, which needs a
//! reachable Ceph. Everything above it is what an operator hits when the
//! drive is misconfigured, and those must fail clearly rather than at the
//! first I/O.

use libvmm_config::{EngineBinding, EngineKind};
use libvmm_storage::engines;
use std::path::PathBuf;

fn binding(pool: Option<&str>, image: Option<&str>, config: Option<&str>) -> EngineBinding {
    EngineBinding {
        engine: EngineKind::RustCephRbd,
        file_path: None,
        pci_bdf: None,
        nsid: None,
        cluster_config: config.map(PathBuf::from),
        cluster_name: Some("ceph".to_string()),
        pool_name: pool.map(str::to_string),
        rbd_image: image.map(str::to_string),
        shared_mem_size_mb: None,
    }
}

/// Everything here fails at open, so this is the shared shape of the check.
fn open_error(binding: &EngineBinding) -> String {
    match engines::open(binding, "drive rbd-test", 0, 512) {
        Ok(_) => panic!("expected the open to fail without a cluster"),
        Err(e) => {
            assert_eq!(e.code(), 4002, "engine open failures are 4002: {e}");
            e.to_string()
        }
    }
}

#[test]
fn a_missing_pool_name_is_named_in_the_error() {
    let error = open_error(&binding(None, Some("vol01"), None));
    assert!(
        error.contains("pool_name"),
        "the error must name the missing field: {error}"
    );
}

#[test]
fn a_missing_image_name_is_named_in_the_error() {
    let error = open_error(&binding(Some("rbd"), None, None));
    assert!(
        error.contains("rbd_image"),
        "the error must name the missing field: {error}"
    );
}

#[test]
fn an_unreadable_cluster_config_says_which_file() {
    let missing = "/nonexistent/ceph.conf";
    let error = open_error(&binding(Some("rbd"), Some("vol01"), Some(missing)));
    assert!(
        error.contains(missing),
        "the error must name the config it could not read: {error}"
    );
}

#[test]
fn a_failure_to_reach_the_cluster_explains_itself() {
    // With no ceph.conf and no cluster, this fails somewhere in connect. The
    // requirement is that the message is actionable, whichever step failed.
    let error = open_error(&binding(Some("rbd"), Some("vol01"), None));
    let actionable = ["ceph.conf", "monitors", "keyring", "cluster_config"];
    assert!(
        actionable.iter().any(|hint| error.contains(hint)),
        "the error gives an operator nothing to act on: {error}"
    );
}

#[test]
fn the_engine_is_declared_persistent_and_snapshot_capable() {
    // §5.4's capability matrix is what the backup preflight consults, so it
    // must be right whether or not a cluster is reachable.
    assert!(EngineKind::RustCephRbd.is_persistent());
    assert!(EngineKind::RustCephRbd.is_snapshot_capable());
}

/// Requires a reachable Ceph cluster and an image named by the environment:
///
/// ```sh
/// VMM_TEST_RBD_POOL=rbd VMM_TEST_RBD_IMAGE=vmm-test \
///     cargo test -p libvmm-storage --test rbd_engine -- --ignored
/// ```
#[test]
#[ignore = "needs a reachable Ceph cluster"]
fn a_real_image_opens_reads_and_snapshots() {
    use libvmm_storage::engine::{CompletionRing, IoOp, IoSlice};

    let pool = std::env::var("VMM_TEST_RBD_POOL").expect("VMM_TEST_RBD_POOL");
    let image = std::env::var("VMM_TEST_RBD_IMAGE").expect("VMM_TEST_RBD_IMAGE");
    let config = std::env::var("VMM_TEST_RBD_CONF").ok();

    let engine = engines::open(
        &binding(Some(&pool), Some(&image), config.as_deref()),
        "drive rbd-test",
        0,
        512,
    )
    .expect("the image must open");

    assert_eq!(engine.kind(), EngineKind::RustCephRbd);
    assert!(engine.is_persistent());
    assert!(engine.capacity() > 0);
    assert_eq!(engine.capacity() % 512, 0);

    // Write a block, read it back.
    let pattern: Vec<u8> = (0..512).map(|i| (i % 251) as u8).collect();
    let mut readback = vec![0u8; 512];

    // SAFETY: both buffers outlive the submissions, which are drained below.
    let write = unsafe { IoSlice::new(pattern.as_ptr() as *mut u8, pattern.len()) };
    engine.submit(IoOp::Write, 0, &[write], 1).expect("write");

    let mut ring = CompletionRing::with_capacity(16);
    for _ in 0..10_000 {
        engine.poll_completions(&mut ring);
        if !ring.is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    let completions: Vec<_> = ring.drain().collect();
    assert_eq!(completions.len(), 1);
    assert!(completions[0].is_ok(), "write failed: {:?}", completions[0]);

    engine.flush().expect("flush");

    // SAFETY: as above.
    let read = unsafe { IoSlice::new(readback.as_mut_ptr(), readback.len()) };
    engine.submit(IoOp::Read, 0, &[read], 2).expect("read");
    for _ in 0..10_000 {
        engine.poll_completions(&mut ring);
        if !ring.is_empty() {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(1));
    }
    let completions: Vec<_> = ring.drain().collect();
    assert_eq!(completions.len(), 1);
    assert!(completions[0].is_ok());
    assert_eq!(readback, pattern, "the block did not read back");

    // §10.2: a native RBD snapshot, not a copy.
    let name = format!("vmm-test-{}", std::process::id());
    let handle = engine
        .snapshot(std::path::Path::new(&name))
        .expect("snapshot");
    assert_eq!(
        handle.method,
        libvmm_storage::engine::SnapshotMethod::RbdSnapshot
    );
    assert!(handle.path.to_string_lossy().contains(&format!("@{name}")));
}
