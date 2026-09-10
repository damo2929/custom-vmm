//! USB/IP wire structures — §9.1.
//!
//! **All USB/IP structures are big-endian** (network order), unlike the
//! little-endian virtio side (§0.1). Every encoder and decoder here uses
//! `to_be_bytes`/`from_be_bytes` for that reason.

use libvmm_core::{UsbipError, VmmResult};

/// The protocol version carried in `op_common`.
pub const USBIP_VERSION: u16 = 0x0111;

// -- op_common codes (§9.1) --------------------------------------------------

pub const OP_REQ_DEVLIST: u16 = 0x8005;
pub const OP_REP_DEVLIST: u16 = 0x0005;
pub const OP_REQ_IMPORT: u16 = 0x8003;
pub const OP_REP_IMPORT: u16 = 0x0003;

pub const ST_OK: u32 = 0x0000_0000;
pub const ST_NA: u32 = 0x0000_0001;

// -- usbip_header_basic commands (§9.1) --------------------------------------

pub const USBIP_CMD_SUBMIT: u32 = 1;
pub const USBIP_CMD_UNLINK: u32 = 2;
pub const USBIP_RET_SUBMIT: u32 = 3;
pub const USBIP_RET_UNLINK: u32 = 4;

pub const DIRECTION_OUT: u32 = 0;
pub const DIRECTION_IN: u32 = 1;

/// A busid is a fixed 32-byte NUL-padded field on the wire.
pub const BUSID_LEN: usize = 32;
/// The device path field in a devlist entry.
pub const PATH_LEN: usize = 256;

/// `struct op_common { u16 version; u16 code; u32 status; }`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OpCommon {
    pub version: u16,
    pub code: u16,
    pub status: u32,
}

impl OpCommon {
    pub const LEN: usize = 8;

    pub const fn new(code: u16, status: u32) -> Self {
        OpCommon {
            version: USBIP_VERSION,
            code,
            status,
        }
    }

    pub fn encode(&self) -> [u8; Self::LEN] {
        let mut b = [0u8; Self::LEN];
        b[0..2].copy_from_slice(&self.version.to_be_bytes());
        b[2..4].copy_from_slice(&self.code.to_be_bytes());
        b[4..8].copy_from_slice(&self.status.to_be_bytes());
        b
    }

    pub fn decode(bytes: &[u8]) -> VmmResult<Self> {
        if bytes.len() < Self::LEN {
            return Err(short("op_common", bytes.len(), Self::LEN));
        }
        Ok(OpCommon {
            version: u16::from_be_bytes([bytes[0], bytes[1]]),
            code: u16::from_be_bytes([bytes[2], bytes[3]]),
            status: u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
        })
    }

    /// Reject a peer speaking a different protocol version.
    pub fn check_version(&self) -> VmmResult<()> {
        if self.version != USBIP_VERSION {
            return Err(UsbipError::BadWire {
                structure: "op_common",
                detail: format!(
                    "peer version {:#06x}, expected {USBIP_VERSION:#06x}",
                    self.version
                ),
            }
            .into());
        }
        Ok(())
    }
}

/// `struct usbip_header_basic { u32 command; u32 seqnum; u32 devid;
///                              u32 direction; u32 ep; }`
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HeaderBasic {
    pub command: u32,
    pub seqnum: u32,
    pub devid: u32,
    pub direction: u32,
    pub ep: u32,
}

impl HeaderBasic {
    pub const LEN: usize = 20;

    pub fn encode(&self) -> [u8; Self::LEN] {
        let mut b = [0u8; Self::LEN];
        b[0..4].copy_from_slice(&self.command.to_be_bytes());
        b[4..8].copy_from_slice(&self.seqnum.to_be_bytes());
        b[8..12].copy_from_slice(&self.devid.to_be_bytes());
        b[12..16].copy_from_slice(&self.direction.to_be_bytes());
        b[16..20].copy_from_slice(&self.ep.to_be_bytes());
        b
    }

    pub fn decode(bytes: &[u8]) -> VmmResult<Self> {
        if bytes.len() < Self::LEN {
            return Err(short("usbip_header_basic", bytes.len(), Self::LEN));
        }
        let word =
            |i: usize| u32::from_be_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]);
        Ok(HeaderBasic {
            command: word(0),
            seqnum: word(4),
            devid: word(8),
            direction: word(12),
            ep: word(16),
        })
    }

    pub const fn is_in(&self) -> bool {
        self.direction == DIRECTION_IN
    }
}

/// `struct usbip_cmd_submit { header; u32 transfer_flags;
///    i32 transfer_buffer_length; i32 start_frame; i32 number_of_packets;
///    i32 interval; u8 setup[8]; /* +data */ }`
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CmdSubmit {
    pub header: HeaderBasic,
    pub transfer_flags: u32,
    pub transfer_buffer_length: i32,
    pub start_frame: i32,
    pub number_of_packets: i32,
    pub interval: i32,
    pub setup: [u8; 8],
}

impl CmdSubmit {
    pub const LEN: usize = HeaderBasic::LEN + 4 + 4 + 4 + 4 + 4 + 8;

    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(Self::LEN);
        b.extend_from_slice(&self.header.encode());
        b.extend_from_slice(&self.transfer_flags.to_be_bytes());
        b.extend_from_slice(&self.transfer_buffer_length.to_be_bytes());
        b.extend_from_slice(&self.start_frame.to_be_bytes());
        b.extend_from_slice(&self.number_of_packets.to_be_bytes());
        b.extend_from_slice(&self.interval.to_be_bytes());
        b.extend_from_slice(&self.setup);
        b
    }

    pub fn decode(bytes: &[u8]) -> VmmResult<Self> {
        if bytes.len() < Self::LEN {
            return Err(short("usbip_cmd_submit", bytes.len(), Self::LEN));
        }
        let header = HeaderBasic::decode(bytes)?;
        let at = HeaderBasic::LEN;
        let u32_at =
            |i: usize| u32::from_be_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]);
        let i32_at =
            |i: usize| i32::from_be_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]);
        let mut setup = [0u8; 8];
        setup.copy_from_slice(&bytes[at + 20..at + 28]);
        Ok(CmdSubmit {
            header,
            transfer_flags: u32_at(at),
            transfer_buffer_length: i32_at(at + 4),
            start_frame: i32_at(at + 8),
            number_of_packets: i32_at(at + 12),
            interval: i32_at(at + 16),
            setup,
        })
    }

    /// Bytes of payload that follow the header on the wire: OUT transfers
    /// carry their data, IN transfers do not.
    pub fn payload_len(&self) -> usize {
        if self.header.is_in() {
            0
        } else {
            self.transfer_buffer_length.max(0) as usize
        }
    }
}

/// `USBIP_RET_SUBMIT` — the completion that drives the xHCI event ring.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RetSubmit {
    pub header: HeaderBasic,
    pub status: i32,
    pub actual_length: i32,
    pub start_frame: i32,
    pub number_of_packets: i32,
    pub error_count: i32,
}

impl RetSubmit {
    pub const LEN: usize = HeaderBasic::LEN + 4 * 5 + 8;

    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(Self::LEN);
        b.extend_from_slice(&self.header.encode());
        b.extend_from_slice(&self.status.to_be_bytes());
        b.extend_from_slice(&self.actual_length.to_be_bytes());
        b.extend_from_slice(&self.start_frame.to_be_bytes());
        b.extend_from_slice(&self.number_of_packets.to_be_bytes());
        b.extend_from_slice(&self.error_count.to_be_bytes());
        b.extend_from_slice(&[0u8; 8]); // padding to match the C layout
        b
    }

    pub fn decode(bytes: &[u8]) -> VmmResult<Self> {
        if bytes.len() < Self::LEN {
            return Err(short("usbip_ret_submit", bytes.len(), Self::LEN));
        }
        let header = HeaderBasic::decode(bytes)?;
        let at = HeaderBasic::LEN;
        let i32_at =
            |i: usize| i32::from_be_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]);
        Ok(RetSubmit {
            header,
            status: i32_at(at),
            actual_length: i32_at(at + 4),
            start_frame: i32_at(at + 8),
            number_of_packets: i32_at(at + 12),
            error_count: i32_at(at + 16),
        })
    }
}

/// `USBIP_CMD_UNLINK` — guest-initiated cancellation (§9.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CmdUnlink {
    pub header: HeaderBasic,
    /// The seqnum of the submission being cancelled.
    pub unlink_seqnum: u32,
}

impl CmdUnlink {
    pub const LEN: usize = HeaderBasic::LEN + 4 + 24;

    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(Self::LEN);
        b.extend_from_slice(&self.header.encode());
        b.extend_from_slice(&self.unlink_seqnum.to_be_bytes());
        b.extend_from_slice(&[0u8; 24]);
        b
    }

    pub fn decode(bytes: &[u8]) -> VmmResult<Self> {
        if bytes.len() < Self::LEN {
            return Err(short("usbip_cmd_unlink", bytes.len(), Self::LEN));
        }
        let header = HeaderBasic::decode(bytes)?;
        let at = HeaderBasic::LEN;
        Ok(CmdUnlink {
            header,
            unlink_seqnum: u32::from_be_bytes([
                bytes[at],
                bytes[at + 1],
                bytes[at + 2],
                bytes[at + 3],
            ]),
        })
    }
}

/// One device in an `OP_REP_DEVLIST` reply, or the body of `OP_REP_IMPORT`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    pub path: String,
    pub busid: String,
    pub busnum: u32,
    pub devnum: u32,
    pub speed: u32,
    pub id_vendor: u16,
    pub id_product: u16,
    pub bcd_device: u16,
    pub device_class: u8,
    pub device_subclass: u8,
    pub device_protocol: u8,
    pub configuration_value: u8,
    pub num_configurations: u8,
    pub num_interfaces: u8,
}

impl DeviceInfo {
    pub const LEN: usize = PATH_LEN + BUSID_LEN + 4 * 3 + 2 * 3 + 6;

    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::with_capacity(Self::LEN);
        b.extend_from_slice(&fixed(&self.path, PATH_LEN));
        b.extend_from_slice(&fixed(&self.busid, BUSID_LEN));
        b.extend_from_slice(&self.busnum.to_be_bytes());
        b.extend_from_slice(&self.devnum.to_be_bytes());
        b.extend_from_slice(&self.speed.to_be_bytes());
        b.extend_from_slice(&self.id_vendor.to_be_bytes());
        b.extend_from_slice(&self.id_product.to_be_bytes());
        b.extend_from_slice(&self.bcd_device.to_be_bytes());
        b.push(self.device_class);
        b.push(self.device_subclass);
        b.push(self.device_protocol);
        b.push(self.configuration_value);
        b.push(self.num_configurations);
        b.push(self.num_interfaces);
        b
    }

    pub fn decode(bytes: &[u8]) -> VmmResult<Self> {
        if bytes.len() < Self::LEN {
            return Err(short("usbip_usb_device", bytes.len(), Self::LEN));
        }
        let at = PATH_LEN + BUSID_LEN;
        let u32_at =
            |i: usize| u32::from_be_bytes([bytes[i], bytes[i + 1], bytes[i + 2], bytes[i + 3]]);
        let u16_at = |i: usize| u16::from_be_bytes([bytes[i], bytes[i + 1]]);
        Ok(DeviceInfo {
            path: unfixed(&bytes[0..PATH_LEN]),
            busid: unfixed(&bytes[PATH_LEN..PATH_LEN + BUSID_LEN]),
            busnum: u32_at(at),
            devnum: u32_at(at + 4),
            speed: u32_at(at + 8),
            id_vendor: u16_at(at + 12),
            id_product: u16_at(at + 14),
            bcd_device: u16_at(at + 16),
            device_class: bytes[at + 18],
            device_subclass: bytes[at + 19],
            device_protocol: bytes[at + 20],
            configuration_value: bytes[at + 21],
            num_configurations: bytes[at + 22],
            num_interfaces: bytes[at + 23],
        })
    }
}

fn fixed(s: &str, len: usize) -> Vec<u8> {
    let mut b = vec![0u8; len];
    let src = s.as_bytes();
    let n = src.len().min(len - 1);
    b[..n].copy_from_slice(&src[..n]);
    b
}

fn unfixed(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|b| *b == 0).unwrap_or(bytes.len());
    String::from_utf8_lossy(&bytes[..end]).into_owned()
}

fn short(structure: &'static str, got: usize, need: usize) -> libvmm_core::VmmError {
    UsbipError::BadWire {
        structure,
        detail: format!("{got} bytes received, need {need}"),
    }
    .into()
}
