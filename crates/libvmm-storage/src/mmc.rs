//! §5.3, Revision E — the MMC command set, for CD/DVD/BD-ROM drives.
//!
//! An optical drive is not a block device with different numbers on it. It
//! answers a different command set (MMC-6 rather than SBC-4), reports a
//! different peripheral device type, and a guest binds a different driver to
//! it — `sr` rather than `sd` on Linux, and on UEFI the El Torito path in
//! `PartitionDxe` rather than the MBR/GPT one. So it lives here rather than
//! as a scattering of `if is_optical` inside the block path.
//!
//! What is implemented is what an operating-system installer actually needs
//! to be booted from an ISO, and no more. That set was not guessed: it is
//! the commands Linux's `sr` driver issues during discovery, plus the ones
//! `MdeModulePkg/Universal/Disk/PartitionDxe` issues while looking for an El
//! Torito boot catalogue.
//!
//! | opcode | command | who needs it |
//! |---|---|---|
//! | `0x1E` | PREVENT/ALLOW MEDIUM REMOVAL | `sr` locks the tray while mounted |
//! | `0x23` | READ FORMAT CAPACITIES | `sr` sizes the medium |
//! | `0x25` | READ CAPACITY (10) | everything |
//! | `0x28` | READ (10) | everything |
//! | `0x43` | READ TOC/PMA/ATIP | `sr` and El Torito find the data track |
//! | `0x46` | GET CONFIGURATION | `sr` learns the profile: CD vs DVD vs BD |
//! | `0x4A` | GET EVENT STATUS NOTIFICATION | `sr` polls for a disc change |
//! | `0x51` | READ DISC INFORMATION | `sr` decides the disc is finalised |
//! | `0xA8` | READ (12) | large transfers |
//! | `0xBD` | MECHANISM STATUS | `sr` probes for a changer |
//!
//! Everything that would write is refused with DATA PROTECT rather than
//! passed to the engine. That distinction matters: a guest that gets an I/O
//! error from a write cannot tell a read-only disc from a failing one, and
//! will often retry forever.
//!
//! The ISO is a plain file behind one of the §5.4 engines, opened read-only
//! with 2048-byte blocks. There is no separate ISO parser here and there
//! does not need to be: the guest reads sectors and does its own
//! filesystem work, exactly as it would from a physical drive.

use libvmm_config::DriveMedium;

use crate::engine::StorageEngine;
use crate::scsi::{
    immediate, transfer, CommandOutcome, ResponseHeader, ASC_INVALID_COMMAND_OPCODE,
    ASC_INVALID_FIELD_IN_CDB, ASC_MEDIUM_NOT_PRESENT, ASC_WRITE_PROTECTED, SENSE_KEY_DATA_PROTECT,
    SENSE_KEY_ILLEGAL_REQUEST, SENSE_KEY_NOT_READY,
};

// --- MMC opcodes -----------------------------------------------------------

pub const PREVENT_ALLOW_MEDIUM_REMOVAL: u8 = 0x1E;
pub const READ_FORMAT_CAPACITIES: u8 = 0x23;
pub const READ_TOC_PMA_ATIP: u8 = 0x43;
pub const GET_CONFIGURATION: u8 = 0x46;
pub const GET_EVENT_STATUS_NOTIFICATION: u8 = 0x4A;
pub const READ_DISC_INFORMATION: u8 = 0x51;
pub const READ_12: u8 = 0xA8;
pub const MECHANISM_STATUS: u8 = 0xBD;

/// The logical block size of every optical profile.
pub const OPTICAL_BLOCK_SIZE: u32 = 2048;

/// Commands that would modify the medium. Each is refused with DATA
/// PROTECT; see the module header for why that is not the same as failing
/// the I/O.
const WRITE_LIKE: &[u8] = &[
    crate::scsi::WRITE_10,
    crate::scsi::WRITE_16,
    crate::scsi::UNMAP,
    0x2E, // WRITE AND VERIFY (10)
    0xAA, // WRITE (12)
    0x53, // RESERVE TRACK
    0x5B, // CLOSE TRACK/SESSION
    0xA1, // BLANK
];

/// Is `opcode` one this module answers, rather than the block path?
pub fn claims(opcode: u8) -> bool {
    matches!(
        opcode,
        PREVENT_ALLOW_MEDIUM_REMOVAL
            | READ_FORMAT_CAPACITIES
            | READ_TOC_PMA_ATIP
            | GET_CONFIGURATION
            | GET_EVENT_STATUS_NOTIFICATION
            | READ_DISC_INFORMATION
            | READ_12
            | MECHANISM_STATUS
    ) || WRITE_LIKE.contains(&opcode)
}

/// Answer an MMC command.
///
/// `medium` selects the profile reported by GET CONFIGURATION — the only
/// thing that distinguishes CD from DVD from BD here, because the read path
/// is identical for all three.
pub fn dispatch(
    cdb: &[u8; 32],
    engine: &dyn StorageEngine,
    medium: DriveMedium,
    present: bool,
) -> CommandOutcome {
    // With no medium loaded, every command that touches it must say so with
    // NOT READY / MEDIUM NOT PRESENT. A guest understands that and reports
    // an empty drive; anything else it reports as a broken one.
    if !present && !matches!(cdb[0], GET_CONFIGURATION | MECHANISM_STATUS) {
        return no_medium();
    }

    if WRITE_LIKE.contains(&cdb[0]) {
        return immediate(
            ResponseHeader::check_condition(SENSE_KEY_DATA_PROTECT, ASC_WRITE_PROTECTED),
            Vec::new(),
        );
    }

    match cdb[0] {
        // The tray lock is advisory here — there is no tray — but it must
        // succeed, because `sr` treats a failure as a drive fault.
        PREVENT_ALLOW_MEDIUM_REMOVAL => immediate(ResponseHeader::default(), Vec::new()),

        READ_12 => {
            let lba = u32::from_be_bytes([cdb[2], cdb[3], cdb[4], cdb[5]]) as u64;
            let blocks = u32::from_be_bytes([cdb[6], cdb[7], cdb[8], cdb[9]]);
            transfer(true, lba, blocks, engine)
        }

        READ_FORMAT_CAPACITIES => read_format_capacities(engine),
        READ_TOC_PMA_ATIP => read_toc(cdb, engine),
        GET_CONFIGURATION => get_configuration(medium, present),
        GET_EVENT_STATUS_NOTIFICATION => get_event_status(cdb, present),
        READ_DISC_INFORMATION => read_disc_information(),
        MECHANISM_STATUS => {
            // Eight bytes of "one slot, nothing in motion, no changer".
            immediate(ResponseHeader::default(), vec![0u8; 8])
        }

        opcode => {
            log::debug!("mmc: unsupported CDB opcode {opcode:#04x}");
            immediate(
                ResponseHeader::check_condition(
                    SENSE_KEY_ILLEGAL_REQUEST,
                    ASC_INVALID_COMMAND_OPCODE,
                ),
                Vec::new(),
            )
        }
    }
}

/// NOT READY / MEDIUM NOT PRESENT — an empty drive, not a broken one.
pub fn no_medium() -> CommandOutcome {
    immediate(
        ResponseHeader::check_condition(SENSE_KEY_NOT_READY, ASC_MEDIUM_NOT_PRESENT),
        Vec::new(),
    )
}

/// The last addressable block, in 2048-byte units.
fn last_lba(engine: &dyn StorageEngine) -> u32 {
    let blocks = engine.capacity() / u64::from(OPTICAL_BLOCK_SIZE);
    blocks.saturating_sub(1).min(u64::from(u32::MAX)) as u32
}

/// READ FORMAT CAPACITIES (MMC-6 §6.23).
fn read_format_capacities(engine: &dyn StorageEngine) -> CommandOutcome {
    let blocks = last_lba(engine).saturating_add(1);
    let mut d = vec![0u8; 12];
    // Capacity list header: three reserved bytes then the list length.
    d[3] = 8;
    // Current/maximum capacity descriptor.
    d[4..8].copy_from_slice(&blocks.to_be_bytes());
    // Descriptor type 2 = formatted medium, then the block length as 24 bits.
    d[8] = 0x02;
    d[9..12].copy_from_slice(&OPTICAL_BLOCK_SIZE.to_be_bytes()[1..4]);
    immediate(ResponseHeader::default(), d)
}

/// READ TOC/PMA/ATIP, format 0 (MMC-6 §6.26).
///
/// A data ISO is one track. `sr` and El Torito both want to hear that, and
/// they want the lead-out address, because that is how the size of the
/// medium is confirmed independently of READ CAPACITY.
fn read_toc(cdb: &[u8; 32], engine: &dyn StorageEngine) -> CommandOutcome {
    let format = cdb[2] & 0x0F;
    // MSF bit: addresses as minute/second/frame rather than LBA. Only the
    // audio world uses it and a data disc is asked for in LBA, so refusing
    // is honest — answering in the wrong units silently would not be.
    let msf = cdb[1] & 0x02 != 0;
    if format != 0 || msf {
        return immediate(
            ResponseHeader::check_condition(SENSE_KEY_ILLEGAL_REQUEST, ASC_INVALID_FIELD_IN_CDB),
            Vec::new(),
        );
    }

    // Header, then the one data track, then the lead-out.
    let mut d = Vec::with_capacity(20);
    d.extend_from_slice(&[0, 0]); // data length, patched below
    d.push(1); // first track
    d.push(1); // last track

    // ADR 1 (position), control 0x4: a data track, digital copy prohibited.
    d.extend_from_slice(&[0x00, 0x14, 0x01, 0x00]);
    d.extend_from_slice(&0u32.to_be_bytes()); // track 1 starts at LBA 0

    // Track 0xAA is the lead-out, and its address is the end of the medium.
    d.extend_from_slice(&[0x00, 0x14, 0xAA, 0x00]);
    d.extend_from_slice(&last_lba(engine).saturating_add(1).to_be_bytes());

    let len = (d.len() - 2) as u16;
    d[0..2].copy_from_slice(&len.to_be_bytes());
    immediate(ResponseHeader::default(), d)
}

/// GET CONFIGURATION (MMC-6 §6.6).
///
/// This is where CD, DVD and BD actually differ. The current profile is the
/// one number a guest reads to decide what kind of disc it is holding.
fn get_configuration(medium: DriveMedium, present: bool) -> CommandOutcome {
    let profile = medium.mmc_profile().unwrap_or(0);
    // With no disc loaded the drive reports profile 0 — "no current
    // profile" — while still describing what it is capable of.
    let current = if present { profile } else { 0 };

    let mut features = Vec::new();

    // Profile List (0x0000). One profile, current when a disc is in.
    let mut profile_list = vec![0u8; 4];
    profile_list[0..2].copy_from_slice(&0x0000u16.to_be_bytes());
    profile_list[2] = 0x03; // version 0, persistent, current
    profile_list[3] = 4; // additional length
    profile_list.extend_from_slice(&profile.to_be_bytes());
    profile_list.push(if present { 0x01 } else { 0x00 });
    profile_list.push(0x00);
    features.extend_from_slice(&profile_list);

    // Core (0x0001): the physical interface. 0x08 is "SCSI family", which
    // is what virtio-scsi presents itself as.
    let mut core = vec![0u8; 4];
    core[0..2].copy_from_slice(&0x0001u16.to_be_bytes());
    core[2] = 0x0B; // version 2, persistent, current
    core[3] = 8;
    core.extend_from_slice(&0x0000_0008u32.to_be_bytes());
    core.extend_from_slice(&[0x01, 0x00, 0x00, 0x00]); // DBE
    features.extend_from_slice(&core);

    // Removable Medium (0x0003). Loading mechanism 1 = tray.
    let mut removable = vec![0u8; 4];
    removable[0..2].copy_from_slice(&0x0003u16.to_be_bytes());
    removable[2] = 0x0B;
    removable[3] = 4;
    // Tray loader, can eject, cannot lock out. Bit 1 is Medium Present.
    removable.push(0x20 | 0x08 | if present { 0x02 } else { 0x00 });
    removable.extend_from_slice(&[0, 0, 0]);
    features.extend_from_slice(&removable);

    // Random Readable (0x0010): logical block size and blocking factor.
    let mut random = vec![0u8; 4];
    random[0..2].copy_from_slice(&0x0010u16.to_be_bytes());
    random[2] = 0x03;
    random[3] = 8;
    random.extend_from_slice(&OPTICAL_BLOCK_SIZE.to_be_bytes());
    random.extend_from_slice(&1u16.to_be_bytes()); // blocking
    random.extend_from_slice(&[0x00, 0x00]); // no page-present requirement
    features.extend_from_slice(&random);

    // The profile's own read feature: CD Read, DVD Read or BD Read.
    let read_feature: u16 = match medium {
        DriveMedium::CdRom => 0x001E,
        DriveMedium::DvdRom => 0x001F,
        DriveMedium::BdRom => 0x0040,
        // Unreachable: this module is only entered for optical media.
        _ => 0x0010,
    };
    let mut read = vec![0u8; 4];
    read[0..2].copy_from_slice(&read_feature.to_be_bytes());
    read[2] = 0x03;
    read[3] = 4;
    read.extend_from_slice(&[0, 0, 0, 0]);
    features.extend_from_slice(&read);

    let mut d = Vec::with_capacity(8 + features.len());
    d.extend_from_slice(&((features.len() + 4) as u32).to_be_bytes());
    d.extend_from_slice(&[0, 0]);
    d.extend_from_slice(&current.to_be_bytes());
    d.extend_from_slice(&features);
    immediate(ResponseHeader::default(), d)
}

/// GET EVENT STATUS NOTIFICATION (MMC-6 §6.7).
///
/// `sr` polls this to notice a disc being swapped. The medium here never
/// changes under the guest, so the honest answer is always "no event", but
/// the command must still be answered: a drive that rejects it is treated as
/// one that cannot report media change, and `sr` then falls back to polling
/// TEST UNIT READY far more aggressively.
fn get_event_status(cdb: &[u8; 32], present: bool) -> CommandOutcome {
    const MEDIA_CLASS: u8 = 4;
    let requested = cdb[4];

    // Byte 1 bit 0 is the polled bit; asynchronous notification is not
    // supported, so an asynchronous request is an invalid field.
    if cdb[1] & 0x01 == 0 {
        return immediate(
            ResponseHeader::check_condition(SENSE_KEY_ILLEGAL_REQUEST, ASC_INVALID_FIELD_IN_CDB),
            Vec::new(),
        );
    }

    if requested & (1 << MEDIA_CLASS) == 0 {
        // No class we support was asked for. NEA — no event available —
        // with the supported class list, which is how the initiator learns
        // what to ask for next time.
        let mut d = vec![0u8; 4];
        d[0..2].copy_from_slice(&2u16.to_be_bytes());
        d[2] = 0x80; // NEA
        d[3] = 1 << MEDIA_CLASS;
        return immediate(ResponseHeader::default(), d);
    }

    let mut d = vec![0u8; 8];
    d[0..2].copy_from_slice(&6u16.to_be_bytes());
    d[2] = MEDIA_CLASS;
    d[3] = 1 << MEDIA_CLASS;
    d[4] = 0x00; // event code: no change
    d[5] = if present { 0x02 } else { 0x00 }; // media present
    immediate(ResponseHeader::default(), d)
}

/// READ DISC INFORMATION (MMC-6 §6.22).
///
/// The disc is finalised and has one complete session. A guest that thinks
/// the disc is still open will look for a writable track and refuse to mount
/// it read-only.
fn read_disc_information() -> CommandOutcome {
    let mut d = vec![0u8; 34];
    d[0..2].copy_from_slice(&32u16.to_be_bytes());
    // Byte 2: disc status 2 (complete), last session state 3 (complete),
    // erasable 0.
    d[2] = 0x0E;
    d[3] = 1; // first track
    d[4] = 1; // sessions, LSB
    d[5] = 1; // first track in last session
    d[6] = 1; // last track in last session
    d[8] = 0x00; // disc type: CD-ROM / data
    immediate(ResponseHeader::default(), d)
}

/// MODE SENSE page 0x2A, MM Capabilities and Mechanical Status (MMC-3
/// §C.5). Removed in MMC-6 but still what Linux's `sr` asks for.
pub fn mm_capabilities_page() -> Vec<u8> {
    let mut p = vec![0u8; 28];
    p[0] = 0x2A;
    p[1] = 26; // page length
               // Byte 2: read capability. CD-R and CD-RW read are the bits every
               // drive sets; DVD read is byte 3 bit 3.
    p[2] = 0x03;
    p[3] = 0x08;
    // Byte 4: audio play, composite, etc. — none of it. Bit 0 is "can read
    // Mode 2 Form 1", which a data disc needs.
    p[4] = 0x01;
    // Byte 6: loading mechanism 1 (tray), eject supported, lock supported.
    p[6] = 0x2B;
    // Nominal speeds, in kilobytes per second. 706 is 4x CD.
    p[8..10].copy_from_slice(&706u16.to_be_bytes());
    p[12..14].copy_from_slice(&706u16.to_be_bytes());
    p
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::scsi::CommandOutcome;

    fn data_of(outcome: CommandOutcome) -> Vec<u8> {
        match outcome {
            CommandOutcome::Immediate { response, data } => {
                assert_eq!(response.status, 0, "expected GOOD status");
                data
            }
            other => panic!("expected an immediate response, got {other:?}"),
        }
    }

    fn cdb(bytes: &[u8]) -> [u8; 32] {
        let mut c = [0u8; 32];
        c[..bytes.len()].copy_from_slice(bytes);
        c
    }

    /// A real engine over a real file, because the sizes these commands
    /// report come from the engine and a stub returning constants would
    /// pin nothing.
    fn iso_engine(name: &str, blocks: u64) -> crate::engines::file::FileEngine {
        let mut path = std::env::temp_dir();
        path.push(format!("vmm-mmc-{}-{name}", std::process::id()));
        let _ = std::fs::remove_file(&path);
        crate::engines::file::FileEngine::open(
            &path,
            blocks * u64::from(OPTICAL_BLOCK_SIZE),
            OPTICAL_BLOCK_SIZE,
            "test-iso",
        )
        .expect("open the test ISO")
    }

    /// The one number that tells CD from DVD from BD.
    #[test]
    fn get_configuration_reports_the_profile_for_the_medium() {
        for (medium, expected) in [
            (DriveMedium::CdRom, 0x0008u16),
            (DriveMedium::DvdRom, 0x0010),
            (DriveMedium::BdRom, 0x0040),
        ] {
            let d = data_of(get_configuration(medium, true));
            let current = u16::from_be_bytes([d[6], d[7]]);
            assert_eq!(
                current,
                expected,
                "{} must report profile {expected:#06x}",
                medium.as_str()
            );
        }
    }

    /// An empty drive is a working drive with no disc, and the difference is
    /// visible in exactly one place.
    #[test]
    fn an_empty_drive_reports_no_current_profile_but_still_describes_itself() {
        let d = data_of(get_configuration(DriveMedium::DvdRom, false));
        assert_eq!(
            u16::from_be_bytes([d[6], d[7]]),
            0,
            "with no medium the current profile is 0"
        );
        assert!(
            d.len() > 8,
            "the drive must still list its features; a guest asks what the \
             drive can do before it asks what is in it"
        );
    }

    #[test]
    fn a_write_to_optical_media_is_refused_as_write_protected_not_as_an_io_error() {
        let engine = iso_engine("write", 1024);
        let out = dispatch(
            &cdb(&[crate::scsi::WRITE_10, 0, 0, 0, 0, 0, 0, 0, 1]),
            &engine,
            DriveMedium::DvdRom,
            true,
        );
        match out {
            CommandOutcome::Immediate { response, .. } => {
                assert_eq!(response.status, crate::scsi::SCSI_STATUS_CHECK_CONDITION);
                // Sense key DATA PROTECT, ASC WRITE PROTECTED. A guest that
                // sees a generic I/O error here retries forever.
                assert_eq!(response.sense[2] & 0x0F, SENSE_KEY_DATA_PROTECT);
                assert_eq!(
                    (response.sense[12], response.sense[13]),
                    ASC_WRITE_PROTECTED
                );
            }
            other => panic!("a write must not reach the engine: {other:?}"),
        }
    }

    #[test]
    fn the_table_of_contents_has_one_data_track_and_a_lead_out_at_the_end() {
        // 1024 blocks of 2048 bytes.
        let engine = iso_engine("toc", 1024);
        let d = data_of(read_toc(&cdb(&[READ_TOC_PMA_ATIP]), &engine));

        assert_eq!(d[2], 1, "first track");
        assert_eq!(d[3], 1, "last track");
        // Track 1: a data track (control nibble 4) starting at LBA 0.
        assert_eq!(d[5] >> 4, 0x01, "ADR 1, position information");
        assert_eq!(d[5] & 0x0F, 0x04, "control 4, a data track");
        assert_eq!(d[6], 0x01, "track number");
        assert_eq!(u32::from_be_bytes([d[8], d[9], d[10], d[11]]), 0);
        // Lead-out at the end of the medium.
        assert_eq!(d[14], 0xAA, "lead-out");
        assert_eq!(u32::from_be_bytes([d[16], d[17], d[18], d[19]]), 1024);
    }

    #[test]
    fn every_command_reports_medium_not_present_when_the_tray_is_empty() {
        let engine = iso_engine("empty", 1024);
        for opcode in [
            READ_TOC_PMA_ATIP,
            READ_DISC_INFORMATION,
            READ_FORMAT_CAPACITIES,
        ] {
            match dispatch(&cdb(&[opcode]), &engine, DriveMedium::CdRom, false) {
                CommandOutcome::Immediate { response, .. } => {
                    assert_eq!(response.sense[2] & 0x0F, SENSE_KEY_NOT_READY);
                    assert_eq!(
                        (response.sense[12], response.sense[13]),
                        ASC_MEDIUM_NOT_PRESENT,
                        "{opcode:#04x} must report an empty drive, not a broken one"
                    );
                }
                other => panic!("{opcode:#04x}: {other:?}"),
            }
        }
    }
}
