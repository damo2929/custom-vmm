//! virtio-scsi request handling — §5.2, §5.3.
//!
//! Transport is virtio-scsi throughout; discard is SCSI UNMAP (0x42) only
//! (change-log item 8). All structures are little-endian on the virtio side
//! and big-endian inside SCSI CDBs and parameter data, as SCSI defines.

use crate::engine::StorageEngine;
use libvmm_core::{StorageError, VmmResult};

// -- virtio-scsi response codes ---------------------------------------------

pub const VIRTIO_SCSI_S_OK: u8 = 0;
pub const VIRTIO_SCSI_S_BAD_TARGET: u8 = 3;
pub const VIRTIO_SCSI_S_FAILURE: u8 = 9;

// -- SCSI status ------------------------------------------------------------

pub const SCSI_STATUS_GOOD: u8 = 0x00;
pub const SCSI_STATUS_CHECK_CONDITION: u8 = 0x02;

// -- sense keys and ASC/ASCQ ------------------------------------------------

pub const SENSE_KEY_NOT_READY: u8 = 0x02;
pub const SENSE_KEY_MEDIUM_ERROR: u8 = 0x03;
pub const SENSE_KEY_ILLEGAL_REQUEST: u8 = 0x05;
pub const SENSE_KEY_HARDWARE_ERROR: u8 = 0x04;

pub const ASC_INVALID_COMMAND_OPCODE: (u8, u8) = (0x20, 0x00);
pub const ASC_LBA_OUT_OF_RANGE: (u8, u8) = (0x21, 0x00);
pub const ASC_INVALID_FIELD_IN_CDB: (u8, u8) = (0x24, 0x00);
pub const ASC_LOGICAL_UNIT_NOT_SUPPORTED: (u8, u8) = (0x25, 0x00);
pub const ASC_LOGICAL_UNIT_NOT_READY: (u8, u8) = (0x04, 0x00);
pub const ASC_INTERNAL_TARGET_FAILURE: (u8, u8) = (0x44, 0x00);

// -- CDB opcodes we handle (§5.3) -------------------------------------------

pub const TEST_UNIT_READY: u8 = 0x00;
pub const REQUEST_SENSE: u8 = 0x03;
pub const INQUIRY: u8 = 0x12;
pub const MODE_SENSE_6: u8 = 0x1A;
pub const START_STOP_UNIT: u8 = 0x1B;
pub const READ_CAPACITY_10: u8 = 0x25;
pub const READ_10: u8 = 0x28;
pub const WRITE_10: u8 = 0x2A;
pub const SYNCHRONIZE_CACHE_10: u8 = 0x35;
pub const MODE_SENSE_10: u8 = 0x5A;
pub const UNMAP: u8 = 0x42;
pub const READ_16: u8 = 0x88;
pub const WRITE_16: u8 = 0x8A;
pub const SYNCHRONIZE_CACHE_16: u8 = 0x91;
pub const SERVICE_ACTION_IN_16: u8 = 0x9E;
pub const READ_CAPACITY_16_SA: u8 = 0x10;
pub const REPORT_LUNS: u8 = 0xA0;

/// The `virtio_scsi_req_cmd` header (§5.2), driver-writable.
///
/// ```text
/// struct virtio_scsi_req_cmd {
///    u8 lun[8]; u64 id; u8 task_attr; u8 prio; u8 crn; u8 cdb[32];
/// }
/// ```
#[derive(Debug, Clone)]
pub struct RequestHeader {
    pub lun: [u8; 8],
    pub id: u64,
    pub task_attr: u8,
    pub prio: u8,
    pub crn: u8,
    pub cdb: [u8; 32],
}

/// Fixed size of the request header before any data-out buffers.
pub const REQ_HEADER_LEN: usize = 8 + 8 + 1 + 1 + 1 + 32;
/// Fixed size of the response header before any data-in buffers.
pub const RESP_HEADER_LEN: usize = 4 + 4 + 2 + 1 + 1 + 96;
pub const SENSE_LEN: usize = 96;

impl RequestHeader {
    pub fn parse(bytes: &[u8]) -> VmmResult<Self> {
        if bytes.len() < REQ_HEADER_LEN {
            return Err(libvmm_core::VirtioError::BadDescriptor {
                queue: 0,
                detail: format!(
                    "virtio-scsi request header is {} bytes, need {REQ_HEADER_LEN}",
                    bytes.len()
                ),
            }
            .into());
        }
        let mut lun = [0u8; 8];
        lun.copy_from_slice(&bytes[0..8]);
        let mut cdb = [0u8; 32];
        cdb.copy_from_slice(&bytes[19..51]);
        Ok(RequestHeader {
            lun,
            id: u64::from_le_bytes(bytes[8..16].try_into().unwrap_or_default()),
            task_attr: bytes[16],
            prio: bytes[17],
            crn: bytes[18],
            cdb,
        })
    }

    /// The LUN encoding is `1, target, 0, lun, ...` (§5.2).
    pub const fn target(&self) -> u8 {
        self.lun[1]
    }

    pub const fn logical_unit(&self) -> u16 {
        ((self.lun[2] as u16) << 8) | self.lun[3] as u16
    }

    pub const fn opcode(&self) -> u8 {
        self.cdb[0]
    }
}

/// The `virtio_scsi_resp_cmd` header (§5.2), device-writable.
#[derive(Debug, Clone)]
pub struct ResponseHeader {
    pub sense_len: u32,
    pub resid: u32,
    pub status_qualifier: u16,
    pub status: u8,
    pub response: u8,
    pub sense: [u8; SENSE_LEN],
}

impl Default for ResponseHeader {
    fn default() -> Self {
        ResponseHeader {
            sense_len: 0,
            resid: 0,
            status_qualifier: 0,
            status: SCSI_STATUS_GOOD,
            response: VIRTIO_SCSI_S_OK,
            sense: [0u8; SENSE_LEN],
        }
    }
}

impl ResponseHeader {
    /// A CHECK CONDITION with fixed-format sense data (SPC-4 §4.5.3).
    pub fn check_condition(key: u8, asc_ascq: (u8, u8)) -> Self {
        let mut r = ResponseHeader {
            status: SCSI_STATUS_CHECK_CONDITION,
            ..Default::default()
        };
        r.sense[0] = 0x70; // current error, fixed format
        r.sense[2] = key & 0x0F;
        r.sense[7] = 10; // additional sense length
        r.sense[12] = asc_ascq.0;
        r.sense[13] = asc_ascq.1;
        r.sense_len = 18;
        r
    }

    /// A transport-level failure: the target could not be reached at all.
    pub fn bad_target() -> Self {
        ResponseHeader {
            response: VIRTIO_SCSI_S_BAD_TARGET,
            ..Default::default()
        }
    }

    /// §5.6: what in-flight requests get when a vhost-user back-end is lost.
    pub fn backend_lost() -> Self {
        let mut r = Self::check_condition(SENSE_KEY_NOT_READY, ASC_LOGICAL_UNIT_NOT_READY);
        r.response = VIRTIO_SCSI_S_FAILURE;
        r
    }

    pub fn write_into(&self, out: &mut [u8]) -> usize {
        let n = RESP_HEADER_LEN.min(out.len());
        if n < RESP_HEADER_LEN {
            return 0;
        }
        out[0..4].copy_from_slice(&self.sense_len.to_le_bytes());
        out[4..8].copy_from_slice(&self.resid.to_le_bytes());
        out[8..10].copy_from_slice(&self.status_qualifier.to_le_bytes());
        out[10] = self.status;
        out[11] = self.response;
        out[12..12 + SENSE_LEN].copy_from_slice(&self.sense);
        RESP_HEADER_LEN
    }
}

/// What the emulation layer decided to do with a CDB.
#[derive(Debug)]
pub enum CommandOutcome {
    /// Answer immediately from `data`, with no engine round trip.
    Immediate {
        response: ResponseHeader,
        data: Vec<u8>,
    },
    /// Submit a data transfer to the engine.
    Transfer {
        op: crate::engine::IoOp,
        lba: u64,
        blocks: u32,
    },
    /// SYNCHRONIZE CACHE.
    Flush,
    /// UNMAP — one or more LBA ranges to punch.
    Unmap { ranges: Vec<(u64, u32)> },
}

/// Per-drive identity used in INQUIRY responses (§5.3).
#[derive(Debug, Clone)]
pub struct DriveIdentity {
    /// Vendor is `RUST` for every drive (§5.3).
    pub vendor: [u8; 8],
    pub model: [u8; 16],
    pub revision: [u8; 4],
    /// VPD page 0x83 designator: the drive_id.
    pub drive_id: u32,
}

impl DriveIdentity {
    pub fn new(drive_id: u32, engine: libvmm_config::EngineKind) -> Self {
        let mut vendor = [b' '; 8];
        vendor[..4].copy_from_slice(b"RUST");
        let mut model = [b' '; 16];
        // Model is per-drive: the engine that backs it.
        let name = engine.as_str().as_bytes();
        let n = name.len().min(16);
        model[..n].copy_from_slice(&name[..n]);
        DriveIdentity {
            vendor,
            model,
            revision: *b"0001",
            drive_id,
        }
    }
}

/// Decode a CDB and decide what to do with it (§5.3).
pub fn dispatch(
    req: &RequestHeader,
    engine: &dyn StorageEngine,
    id: &DriveIdentity,
    discard_enabled: bool,
    data_out: &[u8],
) -> CommandOutcome {
    let cdb = &req.cdb;
    match cdb[0] {
        TEST_UNIT_READY => immediate(ResponseHeader::default(), Vec::new()),

        REQUEST_SENSE => {
            // No deferred error is pending, so report NO SENSE.
            let mut sense = vec![0u8; 18];
            sense[0] = 0x70;
            sense[7] = 10;
            immediate(ResponseHeader::default(), sense)
        }

        INQUIRY => inquiry(cdb, id, engine),

        READ_CAPACITY_10 => {
            let last = engine.capacity_blocks().saturating_sub(1);
            let mut d = Vec::with_capacity(8);
            // READ CAPACITY (10) saturates at 0xFFFF_FFFF to tell the
            // initiator to reissue with the 16-byte form.
            d.extend_from_slice(&(last.min(0xFFFF_FFFF) as u32).to_be_bytes());
            d.extend_from_slice(&engine.block_size().to_be_bytes());
            immediate(ResponseHeader::default(), d)
        }

        // READ CAPACITY (16) is a service action of 0x9E (§5.3).
        SERVICE_ACTION_IN_16 if cdb[1] & 0x1F == READ_CAPACITY_16_SA => {
            let mut d = vec![0u8; 32];
            d[0..8].copy_from_slice(&engine.capacity_blocks().saturating_sub(1).to_be_bytes());
            d[8..12].copy_from_slice(&engine.block_size().to_be_bytes());
            // LBPME (bit 7 of byte 14) advertises thin provisioning, so the
            // guest knows UNMAP is meaningful (§5.3).
            if discard_enabled {
                d[14] |= 0x80;
            }
            immediate(ResponseHeader::default(), d)
        }

        READ_10 | WRITE_10 => {
            let lba = u32::from_be_bytes([cdb[2], cdb[3], cdb[4], cdb[5]]) as u64;
            let blocks = u16::from_be_bytes([cdb[7], cdb[8]]) as u32;
            transfer(cdb[0] == READ_10, lba, blocks, engine)
        }

        READ_16 | WRITE_16 => {
            let lba = u64::from_be_bytes([
                cdb[2], cdb[3], cdb[4], cdb[5], cdb[6], cdb[7], cdb[8], cdb[9],
            ]);
            let blocks = u32::from_be_bytes([cdb[10], cdb[11], cdb[12], cdb[13]]);
            transfer(cdb[0] == READ_16, lba, blocks, engine)
        }

        SYNCHRONIZE_CACHE_10 | SYNCHRONIZE_CACHE_16 => CommandOutcome::Flush,

        UNMAP => {
            if !discard_enabled {
                return immediate(
                    ResponseHeader::check_condition(
                        SENSE_KEY_ILLEGAL_REQUEST,
                        ASC_INVALID_COMMAND_OPCODE,
                    ),
                    Vec::new(),
                );
            }
            match parse_unmap(data_out) {
                Some(ranges) => CommandOutcome::Unmap { ranges },
                None => immediate(
                    ResponseHeader::check_condition(
                        SENSE_KEY_ILLEGAL_REQUEST,
                        ASC_INVALID_FIELD_IN_CDB,
                    ),
                    Vec::new(),
                ),
            }
        }

        REPORT_LUNS => {
            // A single LUN 0 behind this target.
            let mut d = vec![0u8; 16];
            d[0..4].copy_from_slice(&8u32.to_be_bytes()); // LUN list length
            immediate(ResponseHeader::default(), d)
        }

        MODE_SENSE_6 | MODE_SENSE_10 => mode_sense(cdb, engine, discard_enabled),

        START_STOP_UNIT => immediate(ResponseHeader::default(), Vec::new()),

        opcode => {
            log::debug!("virtio-scsi: unsupported CDB opcode {opcode:#04x}");
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

fn immediate(response: ResponseHeader, data: Vec<u8>) -> CommandOutcome {
    CommandOutcome::Immediate { response, data }
}

fn transfer(is_read: bool, lba: u64, blocks: u32, engine: &dyn StorageEngine) -> CommandOutcome {
    if let Err(e) = engine.check_range(lba, blocks as u64) {
        log::debug!("virtio-scsi: {e}");
        return immediate(
            ResponseHeader::check_condition(SENSE_KEY_ILLEGAL_REQUEST, ASC_LBA_OUT_OF_RANGE),
            Vec::new(),
        );
    }
    CommandOutcome::Transfer {
        op: if is_read {
            crate::engine::IoOp::Read
        } else {
            crate::engine::IoOp::Write
        },
        lba,
        blocks,
    }
}

/// INQUIRY: standard data, or a VPD page when EVPD is set (§5.3).
fn inquiry(cdb: &[u8; 32], id: &DriveIdentity, engine: &dyn StorageEngine) -> CommandOutcome {
    let evpd = cdb[1] & 0x01 != 0;
    let page = cdb[2];

    if !evpd {
        if page != 0 {
            return immediate(
                ResponseHeader::check_condition(
                    SENSE_KEY_ILLEGAL_REQUEST,
                    ASC_INVALID_FIELD_IN_CDB,
                ),
                Vec::new(),
            );
        }
        let mut d = vec![0u8; 36];
        d[0] = 0x00; // direct-access block device
        d[1] = 0x00; // not removable
        d[2] = 0x06; // SPC-4
        d[3] = 0x02; // response data format 2
        d[4] = 31; // additional length
        d[8..16].copy_from_slice(&id.vendor);
        d[16..32].copy_from_slice(&id.model);
        d[32..36].copy_from_slice(&id.revision);
        return immediate(ResponseHeader::default(), d);
    }

    match page {
        // Supported VPD pages.
        0x00 => {
            let pages = [0x00u8, 0x80, 0x83, 0xB0, 0xB2];
            let mut d = vec![0u8; 4 + pages.len()];
            d[1] = 0x00;
            d[2..4].copy_from_slice(&(pages.len() as u16).to_be_bytes());
            d[4..].copy_from_slice(&pages);
            immediate(ResponseHeader::default(), d)
        }
        // Unit serial number.
        0x80 => {
            let serial = format!("{:08}", id.drive_id);
            let mut d = vec![0u8; 4 + serial.len()];
            d[1] = 0x80;
            d[2..4].copy_from_slice(&(serial.len() as u16).to_be_bytes());
            d[4..].copy_from_slice(serial.as_bytes());
            immediate(ResponseHeader::default(), d)
        }
        // §5.3: "VPD 0x83 = drive_id".
        0x83 => {
            let value = id.drive_id.to_be_bytes();
            let mut d = vec![0u8; 4];
            d[1] = 0x83;
            // One designation descriptor: binary, vendor-specific.
            let mut desc = vec![0u8; 4];
            desc[0] = 0x01; // code set: binary
            desc[1] = 0x00; // designator type: vendor specific
            desc[3] = value.len() as u8;
            desc.extend_from_slice(&value);
            d[2..4].copy_from_slice(&(desc.len() as u16).to_be_bytes());
            d.extend_from_slice(&desc);
            immediate(ResponseHeader::default(), d)
        }
        // Block limits: advertise the UNMAP granularity.
        0xB0 => {
            let mut d = vec![0u8; 64];
            d[1] = 0xB0;
            d[2..4].copy_from_slice(&60u16.to_be_bytes());
            // Maximum UNMAP LBA count and block descriptor count.
            d[20..24].copy_from_slice(&u32::MAX.to_be_bytes());
            d[24..28].copy_from_slice(&0x00FF_FFFFu32.to_be_bytes());
            immediate(ResponseHeader::default(), d)
        }
        // Logical block provisioning.
        0xB2 => {
            let mut d = vec![0u8; 8];
            d[1] = 0xB2;
            d[2..4].copy_from_slice(&4u16.to_be_bytes());
            // LBPU (bit 7): UNMAP is supported (§5.3).
            d[5] = 0x80;
            d[6] = 0x02; // provisioning type: thin
            let _ = engine;
            immediate(ResponseHeader::default(), d)
        }
        _ => immediate(
            ResponseHeader::check_condition(SENSE_KEY_ILLEGAL_REQUEST, ASC_INVALID_FIELD_IN_CDB),
            Vec::new(),
        ),
    }
}

/// MODE SENSE with the caching page, reporting a write-back cache.
fn mode_sense(cdb: &[u8; 32], engine: &dyn StorageEngine, _discard: bool) -> CommandOutcome {
    let ten_byte = cdb[0] == MODE_SENSE_10;
    let page_code = cdb[2] & 0x3F;

    let mut pages = Vec::new();
    if matches!(page_code, 0x08 | 0x3F) {
        // Caching mode page: WCE set, so SYNCHRONIZE CACHE is meaningful.
        let mut p = vec![0u8; 20];
        p[0] = 0x08;
        p[1] = 18;
        p[2] = 0x04; // WCE
        pages.extend_from_slice(&p);
    }

    let block_size = engine.block_size();
    let mut d = Vec::new();
    if ten_byte {
        let len = 6 + pages.len();
        d.extend_from_slice(&(len as u16).to_be_bytes());
        d.extend_from_slice(&[0, 0, 0, 0, 0, 0]);
    } else {
        d.push((3 + pages.len()) as u8);
        d.extend_from_slice(&[0, 0, 0]);
    }
    d.extend_from_slice(&pages);
    let _ = block_size;
    immediate(ResponseHeader::default(), d)
}

/// Parse the UNMAP parameter list from the data-out buffer (SBC-3 §5.28).
///
/// ```text
/// 0..2  parameter list length
/// 2..4  block descriptor data length
/// 8..   16-byte descriptors: {lba u64 BE, blocks u32 BE, reserved u32}
/// ```
fn parse_unmap(data_out: &[u8]) -> Option<Vec<(u64, u32)>> {
    if data_out.len() < 8 {
        return None;
    }
    let descriptor_bytes = u16::from_be_bytes([data_out[2], data_out[3]]) as usize;
    if descriptor_bytes % 16 != 0 || 8 + descriptor_bytes > data_out.len() {
        return None;
    }
    let mut ranges = Vec::with_capacity(descriptor_bytes / 16);
    for chunk in data_out[8..8 + descriptor_bytes].chunks_exact(16) {
        let lba = u64::from_be_bytes(chunk[0..8].try_into().ok()?);
        let blocks = u32::from_be_bytes(chunk[8..12].try_into().ok()?);
        if blocks > 0 {
            ranges.push((lba, blocks));
        }
    }
    Some(ranges)
}

/// Translate an engine error into the response the guest sees.
pub fn response_for_error(e: &libvmm_core::VmmError) -> ResponseHeader {
    match e {
        libvmm_core::VmmError::Storage(StorageError::LbaOutOfRange { .. }) => {
            ResponseHeader::check_condition(SENSE_KEY_ILLEGAL_REQUEST, ASC_LBA_OUT_OF_RANGE)
        }
        libvmm_core::VmmError::Storage(StorageError::UnmapUnsupported { .. }) => {
            ResponseHeader::check_condition(SENSE_KEY_ILLEGAL_REQUEST, ASC_INVALID_COMMAND_OPCODE)
        }
        libvmm_core::VmmError::Storage(StorageError::BackendLost { .. }) => {
            ResponseHeader::backend_lost()
        }
        _ => ResponseHeader::check_condition(SENSE_KEY_MEDIUM_ERROR, ASC_INTERNAL_TARGET_FAILURE),
    }
}
