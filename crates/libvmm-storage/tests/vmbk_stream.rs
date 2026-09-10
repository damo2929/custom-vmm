//! §10.3 — the `.vmbk` archive: layout, manifest, checksums and round trip.

use libvmm_storage::backup::{self, Item, Manifest};
use std::io::Write;
use std::path::PathBuf;

fn scratch(name: &str) -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("vmbk-test-{}-{name}", std::process::id()));
    std::fs::create_dir_all(&p).unwrap();
    p
}

/// Write a file of `len` bytes with a recognisable, compressible pattern.
fn make_source(dir: &std::path::Path, name: &str, len: usize, fill: u8) -> PathBuf {
    let path = dir.join(name);
    let mut f = std::fs::File::create(&path).unwrap();
    f.write_all(&vec![fill; len]).unwrap();
    path
}

fn manifest(quiesced: bool, level: i32) -> Manifest {
    Manifest {
        created_utc: "2026-09-09T12:00:00Z".into(),
        vm_name: "production-windows-workload-01".into(),
        vm_id: "4b827e8a-86a0-410a-9d32-26db1a0397ce".into(),
        quiesced,
        zstd_level: level,
        members: Vec::new(),
    }
}

#[test]
fn a_vmbk_has_the_spec_10_3_layout_and_verifies() {
    let dir = scratch("layout");
    let items = vec![
        Item {
            archive_path: backup::MEMBER_CONFIG.into(),
            source: make_source(&dir, "vm_config.toml", 512, b'#'),
        },
        Item {
            archive_path: backup::MEMBER_FIRMWARE.into(),
            source: make_source(&dir, "efi_nvram.bin", 4096, 0xEF),
        },
        Item {
            archive_path: backup::MEMBER_TPM.into(),
            source: make_source(&dir, "tpm_state.bin", 2048, 0x7B),
        },
        Item {
            archive_path: "storage/drive_0_boot.raw".into(),
            source: make_source(&dir, "boot.raw", 1 << 20, 0xA5),
        },
    ];

    let out = dir.join("vm-snapshot.vmbk");
    let progress_seen = std::cell::RefCell::new(Vec::new());
    let result = backup::write_vmbk(&out, &items, manifest(true, 3), 3, &|done, total| {
        progress_seen.borrow_mut().push((done, total));
    })
    .expect("the stream should succeed");

    // The archive exists and is genuinely compressed: the sources are highly
    // repetitive, so zstd should shrink them dramatically.
    assert!(out.exists());
    assert_eq!(result.bytes_read, 512 + 4096 + 2048 + (1 << 20));
    assert!(
        result.bytes_written < result.bytes_read / 10,
        "ratio was {:.1}x",
        result.ratio()
    );
    assert_eq!(result.sha256.len(), 64, "a hex SHA-256 is 64 characters");

    // §10.1 step 7: progress was reported, and it ends at 100%.
    let seen = progress_seen.borrow();
    assert!(!seen.is_empty(), "progress frames must be emitted");
    let (last_done, last_total) = *seen.last().unwrap();
    assert_eq!(last_done, last_total);

    // Every member's recorded hash matches what is in the archive.
    let verified = backup::verify_vmbk(&out).expect("verify");
    assert_eq!(verified.len(), items.len());
    for (name, ok) in &verified {
        assert!(ok, "{name} failed its manifest checksum");
    }

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_manifest_records_every_member_with_its_size_and_hash() {
    let dir = scratch("manifest");
    let items = vec![Item {
        archive_path: "storage/drive_0_boot.raw".into(),
        source: make_source(&dir, "boot.raw", 8192, 0x11),
    }];

    let out = dir.join("m.vmbk");
    let result = backup::write_vmbk(&out, &items, manifest(false, 3), 3, &|_, _| {}).unwrap();

    assert_eq!(result.manifest.members.len(), 1);
    let member = &result.manifest.members[0];
    assert_eq!(member.path, "storage/drive_0_boot.raw");
    assert_eq!(member.bytes, 8192);
    assert_eq!(member.sha256.len(), 64);

    // And it is readable back out of the archive, in the §10.3 shape.
    let json = backup::read_manifest(&out).expect("read the manifest");
    assert!(json.contains(r#""created_utc":"2026-09-09T12:00:00Z""#));
    assert!(
        json.contains(r#""quiesced":false"#),
        "a crash-consistent backup must say so"
    );
    assert!(json.contains(r#""zstd_level":3"#));
    assert!(json.contains(&member.sha256));

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn corruption_is_caught_by_the_member_checksums() {
    let dir = scratch("corrupt");
    let items = vec![Item {
        archive_path: "storage/drive_0_boot.raw".into(),
        source: make_source(&dir, "boot.raw", 65536, 0x22),
    }];
    let out = dir.join("c.vmbk");
    backup::write_vmbk(&out, &items, manifest(true, 1), 1, &|_, _| {}).unwrap();

    // Damage the middle of the compressed stream. The zstd frame checksum
    // should reject it outright; if a flip happens to survive decompression,
    // the per-member SHA-256 catches it. Either is a detection — a silent
    // pass is not.
    let mut bytes = std::fs::read(&out).unwrap();
    let start = bytes.len() / 3;
    let end = (bytes.len() * 2 / 3).max(start + 1);
    for b in &mut bytes[start..end] {
        *b ^= 0xFF;
    }
    std::fs::write(&out, &bytes).unwrap();

    match backup::verify_vmbk(&out) {
        Ok(results) => assert!(
            results.iter().any(|(_, ok)| !ok),
            "corruption must be detected, got {results:?}"
        ),
        Err(e) => assert_eq!(e.code(), 8010, "a broken frame is a stream error"),
    }

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn the_zstd_level_from_the_config_is_honoured() {
    let dir = scratch("levels");
    let source = make_source(&dir, "data.raw", 1 << 20, 0x33);

    let mut sizes = Vec::new();
    for level in [1, 19] {
        let items = vec![Item {
            archive_path: "storage/d.raw".into(),
            source: source.clone(),
        }];
        let out = dir.join(format!("l{level}.vmbk"));
        let r = backup::write_vmbk(&out, &items, manifest(true, level), level, &|_, _| {}).unwrap();
        sizes.push(r.bytes_written);
    }
    // Level 19 must not be larger than level 1 on the same input.
    assert!(
        sizes[1] <= sizes[0],
        "level 19 produced {} vs level 1 {}",
        sizes[1],
        sizes[0]
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn an_unwritable_destination_is_reported_as_8010() {
    let dir = scratch("unwritable");
    let items = vec![Item {
        archive_path: "storage/d.raw".into(),
        source: make_source(&dir, "d.raw", 128, 0x44),
    }];
    let e = backup::write_vmbk(
        std::path::Path::new("/proc/definitely/not/writable.vmbk"),
        &items,
        manifest(true, 3),
        3,
        &|_, _| {},
    )
    .expect_err("an unwritable destination must fail");
    assert_eq!(e.code(), 8010);

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_missing_source_is_reported_rather_than_silently_skipped() {
    let dir = scratch("missing");
    let items = vec![Item {
        archive_path: "storage/gone.raw".into(),
        source: dir.join("does-not-exist.raw"),
    }];
    let out = dir.join("x.vmbk");
    let e = backup::write_vmbk(&out, &items, manifest(true, 3), 3, &|_, _| {})
        .expect_err("a missing member must fail the backup");
    assert_eq!(e.code(), 8010);

    let _ = std::fs::remove_dir_all(&dir);
}
