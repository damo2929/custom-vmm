//! §5 SCSI command handling, the §5.4/§10.2 engine capability matrix, the
//! §5.1 queue invariant and the §5.6 crash-isolation contract.

use libvmm_config::{EngineKind, MachineConfig};
use libvmm_storage::backup;
use libvmm_storage::engine::*;
use libvmm_storage::engines::file::FileEngine;
use libvmm_storage::engines::hugepage::HugepageEngine;
use libvmm_storage::scsi::{self, *};
use libvmm_storage::vhost_user::*;
use libvmm_storage::{queue_count, total_queues};
use std::path::PathBuf;

fn reference() -> MachineConfig {
    MachineConfig::from_toml_str(include_str!("../../../config/reference-vm.toml")).unwrap()
}

fn tmp(name: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("vmm-test-{}-{}", std::process::id(), name));
    p
}

const BS: u32 = 512;
const CAP: u64 = 16 * 1024 * 1024;

fn file_engine(name: &str) -> FileEngine {
    let p = tmp(name);
    let _ = std::fs::remove_file(&p);
    FileEngine::open(&p, CAP, BS, "test-drive").unwrap()
}

fn cdb(bytes: &[u8]) -> RequestHeader {
    let mut raw = vec![0u8; REQ_HEADER_LEN];
    raw[0] = 1; // LUN: 1, target, 0, lun
    raw[19..19 + bytes.len()].copy_from_slice(bytes);
    RequestHeader::parse(&raw).unwrap()
}

fn immediate_data(o: CommandOutcome) -> (ResponseHeader, Vec<u8>) {
    match o {
        CommandOutcome::Immediate { response, data } => (response, data),
        other => panic!("expected an immediate response, got {other:?}"),
    }
}

// -- §5.1 queue invariant ----------------------------------------------------

#[test]
fn queues_always_equal_vcpus() {
    // Change-log item 9: hard 1:1, no override.
    for vcpus in [1u32, 2, 4, 8, 64] {
        assert_eq!(queue_count(vcpus) as u32, vcpus);
        // controlq + eventq + N request queues.
        assert_eq!(total_queues(vcpus) as u32, vcpus + 2);
    }
    let cfg = reference();
    assert_eq!(queue_count(cfg.compute.vcpus) as u32, cfg.compute.vcpus);
}

// -- §5.4 / §10.2 capability matrix -----------------------------------------

#[test]
fn capability_matrix_matches_the_spec_tables() {
    let expected = [
        (
            EngineKind::PureRustIoUring,
            Some(SnapshotMethod::Reflink),
            true,
        ),
        (EngineKind::RustNvme, None, true),
        (
            EngineKind::RustCephRbd,
            Some(SnapshotMethod::RbdSnapshot),
            true,
        ),
        (EngineKind::RustHugepageFile, None, false),
    ];
    for (kind, snapshot, persistent) in expected {
        let c = capabilities(kind);
        assert_eq!(c.snapshot, snapshot, "{}", kind.as_str());
        assert_eq!(c.persistent, persistent, "{}", kind.as_str());
        assert_eq!(c.snapshot.is_some(), kind.is_snapshot_capable());
        assert_eq!(c.persistent, kind.is_persistent());
    }
}

// -- §5.3 SCSI commands ------------------------------------------------------

#[test]
fn inquiry_reports_vendor_rust() {
    let e = file_engine("inquiry");
    let id = DriveIdentity::new(0, EngineKind::PureRustIoUring);
    let (r, d) = immediate_data(scsi::dispatch(
        &cdb(&[INQUIRY, 0, 0, 0, 36, 0]),
        &e,
        &id,
        true,
        &[],
    ));
    assert_eq!(r.status, SCSI_STATUS_GOOD);
    assert_eq!(&d[8..12], b"RUST", "§5.3: vendor is RUST");
    assert_eq!(d[0], 0x00, "direct-access block device");
}

#[test]
fn vpd_page_83_carries_the_drive_id() {
    let e = file_engine("vpd83");
    let id = DriveIdentity::new(7, EngineKind::PureRustIoUring);
    // EVPD=1, page 0x83.
    let (r, d) = immediate_data(scsi::dispatch(
        &cdb(&[INQUIRY, 0x01, 0x83, 0, 64, 0]),
        &e,
        &id,
        true,
        &[],
    ));
    assert_eq!(r.status, SCSI_STATUS_GOOD);
    assert_eq!(d[1], 0x83);
    // Designator payload is the big-endian drive_id.
    let designator = &d[8..12];
    assert_eq!(u32::from_be_bytes(designator.try_into().unwrap()), 7);
}

#[test]
fn read_capacity_16_reports_size_and_block_size() {
    let e = file_engine("readcap");
    let id = DriveIdentity::new(0, EngineKind::PureRustIoUring);
    let mut c = [0u8; 16];
    c[0] = SERVICE_ACTION_IN_16;
    c[1] = READ_CAPACITY_16_SA;
    let (_, d) = immediate_data(scsi::dispatch(&cdb(&c), &e, &id, true, &[]));

    let last_lba = u64::from_be_bytes(d[0..8].try_into().unwrap());
    assert_eq!(last_lba, CAP / BS as u64 - 1);
    assert_eq!(u32::from_be_bytes(d[8..12].try_into().unwrap()), BS);
    assert_ne!(
        d[14] & 0x80,
        0,
        "LBPME must be set for thin provisioning (§5.3)"
    );
}

#[test]
fn read_and_write_10_decode_lba_and_block_count() {
    let e = file_engine("rw10");
    let id = DriveIdentity::new(0, EngineKind::PureRustIoUring);

    // WRITE(10) lba=0x1234, blocks=8
    let c = [WRITE_10, 0, 0x00, 0x00, 0x12, 0x34, 0, 0x00, 0x08, 0];
    match scsi::dispatch(&cdb(&c), &e, &id, true, &[]) {
        CommandOutcome::Transfer { op, lba, blocks } => {
            assert_eq!(op, IoOp::Write);
            assert_eq!(lba, 0x1234);
            assert_eq!(blocks, 8);
        }
        other => panic!("expected a transfer, got {other:?}"),
    }

    let c = [READ_10, 0, 0, 0, 0, 0x10, 0, 0, 0x04, 0];
    match scsi::dispatch(&cdb(&c), &e, &id, true, &[]) {
        CommandOutcome::Transfer { op, lba, blocks } => {
            assert_eq!(op, IoOp::Read);
            assert_eq!(lba, 0x10);
            assert_eq!(blocks, 4);
        }
        other => panic!("expected a transfer, got {other:?}"),
    }
}

#[test]
fn a_read_past_the_end_is_check_condition_lba_out_of_range() {
    let e = file_engine("oob");
    let id = DriveIdentity::new(0, EngineKind::PureRustIoUring);
    let mut c = [0u8; 16];
    c[0] = READ_16;
    c[2..10].copy_from_slice(&(CAP / BS as u64).to_be_bytes()); // one past the end
    c[10..14].copy_from_slice(&8u32.to_be_bytes());

    let (r, _) = immediate_data(scsi::dispatch(&cdb(&c), &e, &id, true, &[]));
    assert_eq!(r.status, SCSI_STATUS_CHECK_CONDITION);
    assert_eq!(r.sense[2] & 0x0F, SENSE_KEY_ILLEGAL_REQUEST);
    assert_eq!((r.sense[12], r.sense[13]), ASC_LBA_OUT_OF_RANGE);
}

#[test]
fn unmap_parses_the_descriptor_list() {
    let e = file_engine("unmap");
    let id = DriveIdentity::new(0, EngineKind::PureRustIoUring);

    // One 16-byte descriptor: lba 64, 32 blocks.
    let mut data_out = vec![0u8; 24];
    data_out[2..4].copy_from_slice(&16u16.to_be_bytes());
    data_out[8..16].copy_from_slice(&64u64.to_be_bytes());
    data_out[16..20].copy_from_slice(&32u32.to_be_bytes());

    match scsi::dispatch(
        &cdb(&[UNMAP, 0, 0, 0, 0, 0, 0, 0, 24, 0]),
        &e,
        &id,
        true,
        &data_out,
    ) {
        CommandOutcome::Unmap { ranges } => assert_eq!(ranges, vec![(64u64, 32u32)]),
        other => panic!("expected an unmap, got {other:?}"),
    }
}

#[test]
fn unmap_is_refused_when_discard_is_disabled() {
    let e = file_engine("nounmap");
    let id = DriveIdentity::new(0, EngineKind::PureRustIoUring);
    let (r, _) = immediate_data(scsi::dispatch(
        &cdb(&[UNMAP, 0, 0, 0, 0, 0, 0, 0, 8, 0]),
        &e,
        &id,
        false,
        &[],
    ));
    assert_eq!(r.status, SCSI_STATUS_CHECK_CONDITION);
    assert_eq!((r.sense[12], r.sense[13]), ASC_INVALID_COMMAND_OPCODE);
}

#[test]
fn synchronize_cache_maps_to_a_flush() {
    let e = file_engine("sync");
    let id = DriveIdentity::new(0, EngineKind::PureRustIoUring);
    for opcode in [SYNCHRONIZE_CACHE_10, SYNCHRONIZE_CACHE_16] {
        assert!(matches!(
            scsi::dispatch(
                &cdb(&[opcode, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
                &e,
                &id,
                true,
                &[]
            ),
            CommandOutcome::Flush
        ));
    }
}

#[test]
fn an_unknown_opcode_is_check_condition_invalid_opcode() {
    let e = file_engine("badop");
    let id = DriveIdentity::new(0, EngineKind::PureRustIoUring);
    let (r, _) = immediate_data(scsi::dispatch(
        &cdb(&[0xFE, 0, 0, 0, 0, 0]),
        &e,
        &id,
        true,
        &[],
    ));
    assert_eq!(r.status, SCSI_STATUS_CHECK_CONDITION);
    assert_eq!((r.sense[12], r.sense[13]), ASC_INVALID_COMMAND_OPCODE);
}

#[test]
fn response_header_serialises_to_the_spec_5_2_layout() {
    let r = ResponseHeader::check_condition(SENSE_KEY_MEDIUM_ERROR, ASC_INTERNAL_TARGET_FAILURE);
    let mut out = vec![0u8; RESP_HEADER_LEN];
    assert_eq!(r.write_into(&mut out), RESP_HEADER_LEN);
    assert_eq!(u32::from_le_bytes(out[0..4].try_into().unwrap()), 18); // sense_len
    assert_eq!(out[10], SCSI_STATUS_CHECK_CONDITION); // status
    assert_eq!(out[11], VIRTIO_SCSI_S_OK); // response: transport is fine
    assert_eq!(out[12], 0x70); // fixed-format sense
}

// -- engine datapath ---------------------------------------------------------

#[test]
fn file_engine_round_trips_data_and_reports_completions() {
    let e = file_engine("roundtrip");
    let mut written = vec![0xA5u8; 4096];
    written[0] = 0x5A;
    let mut read_back = vec![0u8; 4096];

    // SAFETY: both buffers are live for the duration of the submissions.
    let out_iov = [unsafe { IoSlice::new(written.as_mut_ptr(), written.len()) }];
    let in_iov = [unsafe { IoSlice::new(read_back.as_mut_ptr(), read_back.len()) }];

    e.submit(IoOp::Write, 8, &out_iov, 0xAA).unwrap();
    e.flush().unwrap();
    e.submit(IoOp::Read, 8, &in_iov, 0xBB).unwrap();

    let mut ring = CompletionRing::with_capacity(16);
    e.poll_completions(&mut ring);
    let completions: Vec<Completion> = ring.drain().collect();
    assert_eq!(completions.len(), 2);
    assert!(completions.iter().all(|c| c.is_ok() && c.bytes == 4096));
    assert_eq!(read_back, written);
}

#[test]
fn file_engine_discard_punches_a_hole() {
    let e = file_engine("discard");
    let mut data = vec![0xFFu8; 8192];
    // SAFETY: `data` outlives the submission.
    let iov = [unsafe { IoSlice::new(data.as_mut_ptr(), data.len()) }];
    e.submit(IoOp::Write, 0, &iov, 1).unwrap();
    e.flush().unwrap();

    e.discard(0, 8192 / BS as u64).unwrap();

    let mut read_back = vec![0xFFu8; 8192];
    // SAFETY: `read_back` outlives the submission.
    let iov = [unsafe { IoSlice::new(read_back.as_mut_ptr(), read_back.len()) }];
    e.submit(IoOp::Read, 0, &iov, 2).unwrap();
    assert!(
        read_back.iter().all(|b| *b == 0),
        "a punched hole reads as zeroes"
    );
}

#[test]
fn file_engine_snapshots_and_the_copy_matches() {
    let e = file_engine("snap-src");
    let mut data = vec![0x42u8; 4096];
    // SAFETY: `data` outlives the submission.
    let iov = [unsafe { IoSlice::new(data.as_mut_ptr(), data.len()) }];
    e.submit(IoOp::Write, 0, &iov, 1).unwrap();

    let dst = tmp("snap-dst");
    let _ = std::fs::remove_file(&dst);
    let handle = e.snapshot(&dst).unwrap();
    assert_eq!(handle.bytes, CAP);
    assert!(matches!(
        handle.method,
        SnapshotMethod::Reflink | SnapshotMethod::FullCopy
    ));

    let copied = std::fs::read(&dst).unwrap();
    assert_eq!(&copied[0..4096], &data[..]);
    let _ = std::fs::remove_file(&dst);
}

#[test]
fn hugepage_engine_is_volatile_and_cannot_snapshot() {
    // 2 MiB is small enough not to need a reserved 1 GiB page.
    let p = tmp("hugepage");
    let _ = std::fs::remove_file(&p);
    let e = match HugepageEngine::open(&p, 2 * 1024 * 1024, BS, "scratch") {
        Ok(e) => e,
        // A host with no hugepages reserved cannot run this; the capability
        // assertions below are covered by the matrix test regardless.
        Err(_) => return,
    };
    assert!(!e.is_persistent(), "§4.2/§6.2 hinge on this being false");
    let err = e.snapshot(&tmp("hugepage-snap")).unwrap_err();
    assert_eq!(err.code(), 4012);
    let _ = std::fs::remove_file(&p);
}

// -- §10 backup preflight ----------------------------------------------------

#[test]
fn preflight_aborts_with_8001_on_a_non_snapshot_drive() {
    // The reference machine has rust_nvme and rust_hugepage_file drives.
    let cfg = reference();
    let err = backup::preflight(&cfg).expect_err("preflight must abort (§10.2)");
    assert_eq!(err.code(), 8001);
    assert!(matches!(
        err,
        libvmm_core::BackupError::EngineNotSnapshotCapable { .. }
    ));
}

#[test]
fn preflight_passes_when_every_drive_can_snapshot() {
    let mut cfg = reference();
    cfg.storage
        .drives
        .retain(|d| d.engine.is_snapshot_capable());
    let drives = backup::preflight(&cfg).expect("all-snapshot machine must pass preflight");
    // Two writable drives. The reference machine's DVD-ROM also sits on a
    // snapshot-capable engine and so survives the filter above, but a
    // backup excludes optical media: the ISO is read-only external media,
    // not machine state, and a Windows installer image is several
    // gigabytes of it in every backup.
    assert_eq!(drives.len(), 2);
    assert!(
        drives.iter().all(|d| !d.medium.is_optical()),
        "a backup must not include an optical drive"
    );
}

#[test]
fn quiesce_timeout_is_a_warning_and_yields_a_crash_consistent_backup() {
    // §10.1 step 3: timeout or no agent -> proceed crash-consistent, log a
    // warning, skip the thaw.
    let outcome = backup::QuiesceOutcome::CrashConsistent;
    assert!(!outcome.quiesced());
    assert!(!outcome.needs_thaw());
    let warn = libvmm_core::BackupError::QuiesceTimeout { timeout_secs: 10 };
    assert_eq!(warn.code(), 8005);
    assert!(warn.is_warning(), "8005 is a warning, not a failure");

    assert!(backup::QuiesceOutcome::Frozen.needs_thaw());
}

#[test]
fn manifest_serialises_the_spec_10_3_shape() {
    let m = backup::Manifest {
        created_utc: "2026-09-09T12:00:00Z".into(),
        vm_name: "production-windows-workload-01".into(),
        vm_id: "4b827e8a-86a0-410a-9d32-26db1a0397ce".into(),
        quiesced: false,
        zstd_level: 3,
        members: vec![backup::Member {
            path: "storage/drive_0_boot.raw".into(),
            bytes: 10_500_000_000,
            sha256: "abc123".into(),
        }],
    };
    let json = m.to_json();
    assert!(json.contains(r#""created_utc":"2026-09-09T12:00:00Z""#));
    assert!(json.contains(r#""quiesced":false"#));
    assert!(json.contains(r#""zstd_level":3"#));
    assert!(json.contains(r#""path":"storage/drive_0_boot.raw""#));
}

#[test]
fn drive_member_paths_follow_the_spec_10_3_naming() {
    let cfg = reference();
    let boot = cfg.storage.drives.iter().find(|d| d.bootable).unwrap();
    assert_eq!(
        backup::member_path_for_drive(boot),
        "storage/drive_0_boot.raw"
    );
    let data = cfg.storage.drives.iter().find(|d| d.drive_id == 1).unwrap();
    assert_eq!(
        backup::member_path_for_drive(data),
        "storage/drive_1_data.raw"
    );
}

// -- §5.6 vhost-user crash isolation ----------------------------------------

#[test]
fn a_lost_backend_fails_the_drive_without_panicking() {
    let mut c = BackendConnection::new(2, PathBuf::from("/var/run/vmm-vhost-scsi2.sock"), 2);
    c.state = BackendState::Ready;
    assert!(c.is_usable());

    let e = c.mark_lost("peer closed the socket");
    assert_eq!(e.code(), 4001, "must surface Storage(BackendLost)");
    assert!(!c.is_usable());
    // New submissions are refused rather than attempted.
    assert_eq!(c.check_usable().unwrap_err().code(), 4001);

    // In-flight requests get CHECK CONDITION, not a crash.
    let r = ResponseHeader::backend_lost();
    assert_eq!(r.status, SCSI_STATUS_CHECK_CONDITION);
    assert_eq!(r.response, VIRTIO_SCSI_S_FAILURE);
}

#[test]
fn a_backend_cannot_reach_memory_outside_its_mem_table() {
    let mut c = BackendConnection::new(0, PathBuf::from("/tmp/s.sock"), 2);
    c.set_mem_table(vec![MemoryRegion {
        guest_phys_addr: 0,
        memory_size: 3 * 1024 * 1024 * 1024,
        userspace_addr: 0x7F00_0000_0000,
        mmap_offset: 0,
    }]);
    assert!(c.grants(0x1000, 4096));
    // High RAM was never granted.
    assert!(!c.grants(0x1_0000_0000, 4096));
    // A range straddling the end of the granted region is refused.
    assert!(!c.grants(3 * 1024 * 1024 * 1024 - 2048, 4096));
}

#[test]
fn the_vhost_user_handshake_starts_with_set_owner() {
    // §5.6 lists SET_OWNER first, before features and the mem table.
    assert_eq!(HANDSHAKE_ORDER[0], FrontEndRequest::SetOwner);
    assert_eq!(HANDSHAKE_ORDER[1], FrontEndRequest::SetFeatures);
    assert_eq!(HANDSHAKE_ORDER[2], FrontEndRequest::SetMemTable);
}
