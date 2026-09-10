//! Minimal RFC 6455 frame codec.
//!
//! The control channel carries only UTF-8 JSON text frames (§8.5) plus the
//! ping/pong/close control frames, so a purpose-built codec is smaller and
//! more auditable than a general WebSocket library — and it lets the listener
//! answer the §8.3 pre-upgrade cases (503, 429) itself.

use libvmm_core::{ControlError, VmmResult};

/// Which end of the connection is encoding or decoding.
///
/// RFC 6455 §5.1: a client MUST mask every frame it sends; a server MUST NOT.
/// Carrying the role explicitly means neither end can accidentally accept
/// what the other is required to send.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Client,
    Server,
}

impl Role {
    /// Does a frame *sent* by this role carry a mask?
    pub const fn masks_outbound(self) -> bool {
        matches!(self, Role::Client)
    }

    /// Does a frame *received* by this role carry a mask?
    pub const fn expects_masked_inbound(self) -> bool {
        matches!(self, Role::Server)
    }

    pub const fn peer(self) -> Role {
        match self {
            Role::Client => Role::Server,
            Role::Server => Role::Client,
        }
    }
}

pub const OPCODE_CONTINUATION: u8 = 0x0;
pub const OPCODE_TEXT: u8 = 0x1;
pub const OPCODE_BINARY: u8 = 0x2;
pub const OPCODE_CLOSE: u8 = 0x8;
pub const OPCODE_PING: u8 = 0x9;
pub const OPCODE_PONG: u8 = 0xA;

/// Control frames may not exceed 125 bytes of payload (RFC 6455 §5.5).
pub const MAX_CONTROL_PAYLOAD: usize = 125;

/// Largest text frame we will accept. Control frames are small JSON objects;
/// anything larger is a client bug or an attack.
pub const MAX_FRAME_PAYLOAD: usize = 64 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
    Text(String),
    Binary(Vec<u8>),
    Ping(Vec<u8>),
    Pong(Vec<u8>),
    Close { code: u16, reason: String },
}

/// Encode a frame the server sends. Server frames are never masked.
pub fn encode(frame: &Frame) -> Vec<u8> {
    encode_as(frame, Role::Server)
}

/// Encode a frame the client sends, masked with a fresh random key.
pub fn encode_client(frame: &Frame) -> Vec<u8> {
    encode_as(frame, Role::Client)
}

/// Encode a frame for `role`, masking it if that role must.
pub fn encode_as(frame: &Frame, role: Role) -> Vec<u8> {
    let mask = role.masks_outbound().then(crate::mask_key);
    encode_with_mask(frame, mask)
}

/// Encode with an explicit mask, so the masking itself can be tested against
/// a known key.
pub fn encode_with_mask(frame: &Frame, mask: Option<[u8; 4]>) -> Vec<u8> {
    let (opcode, payload) = match frame {
        Frame::Text(s) => (OPCODE_TEXT, s.as_bytes().to_vec()),
        Frame::Binary(b) => (OPCODE_BINARY, b.clone()),
        Frame::Ping(b) => (OPCODE_PING, b.clone()),
        Frame::Pong(b) => (OPCODE_PONG, b.clone()),
        Frame::Close { code, reason } => {
            let mut p = code.to_be_bytes().to_vec();
            p.extend_from_slice(reason.as_bytes());
            (OPCODE_CLOSE, p)
        }
    };

    let mask_bit = if mask.is_some() { 0x80 } else { 0x00 };
    let mut out = Vec::with_capacity(payload.len() + 14);
    out.push(0x80 | opcode); // FIN set, single-frame message
    match payload.len() {
        n if n < 126 => out.push(mask_bit | n as u8),
        n if n <= u16::MAX as usize => {
            out.push(mask_bit | 126);
            out.extend_from_slice(&(n as u16).to_be_bytes());
        }
        n => {
            out.push(mask_bit | 127);
            out.extend_from_slice(&(n as u64).to_be_bytes());
        }
    }
    match mask {
        Some(key) => {
            out.extend_from_slice(&key);
            for (i, byte) in payload.iter().enumerate() {
                out.push(byte ^ key[i % 4]);
            }
        }
        None => out.extend_from_slice(&payload),
    }
    out
}

/// Result of attempting to decode one frame from a buffer.
#[derive(Debug)]
pub enum Decoded {
    /// A complete frame, and how many bytes it consumed.
    Frame(Frame, usize),
    /// Need more bytes.
    Incomplete,
}

/// Decode a frame arriving at the server, which must be masked.
pub fn decode(buffer: &[u8]) -> VmmResult<Decoded> {
    decode_as(buffer, Role::Server)
}

/// Decode a frame arriving at the client, which must not be masked.
pub fn decode_client(buffer: &[u8]) -> VmmResult<Decoded> {
    decode_as(buffer, Role::Client)
}

/// Decode one frame arriving at `role`.
///
/// RFC 6455 §5.1 is enforced in both directions: a server rejects an unmasked
/// frame, and a client rejects a masked one.
pub fn decode_as(buffer: &[u8], role: Role) -> VmmResult<Decoded> {
    if buffer.len() < 2 {
        return Ok(Decoded::Incomplete);
    }
    let fin = buffer[0] & 0x80 != 0;
    let opcode = buffer[0] & 0x0F;
    let masked = buffer[1] & 0x80 != 0;
    let short_len = (buffer[1] & 0x7F) as usize;

    let (payload_len, mut offset) = match short_len {
        126 => {
            if buffer.len() < 4 {
                return Ok(Decoded::Incomplete);
            }
            (u16::from_be_bytes([buffer[2], buffer[3]]) as usize, 4)
        }
        127 => {
            if buffer.len() < 10 {
                return Ok(Decoded::Incomplete);
            }
            let n = u64::from_be_bytes(match buffer[2..10].try_into() {
                Ok(b) => b,
                Err(_) => return Ok(Decoded::Incomplete),
            });
            (n as usize, 10)
        }
        n => (n, 2),
    };

    if payload_len > MAX_FRAME_PAYLOAD {
        return Err(ControlError::BadFrame(format!(
            "frame payload of {payload_len} bytes exceeds the {MAX_FRAME_PAYLOAD} byte limit"
        ))
        .into());
    }
    let is_control = opcode & 0x08 != 0;
    if is_control {
        if payload_len > MAX_CONTROL_PAYLOAD {
            return Err(
                ControlError::BadFrame("control frame payload exceeds 125 bytes".into()).into(),
            );
        }
        if !fin {
            return Err(
                ControlError::BadFrame("control frames may not be fragmented".into()).into(),
            );
        }
    }
    if masked != role.expects_masked_inbound() {
        return Err(ControlError::BadFrame(format!(
            "frames from a {:?} must {}be masked (RFC 6455 §5.1)",
            role.peer(),
            if role.expects_masked_inbound() {
                ""
            } else {
                "not "
            }
        ))
        .into());
    }

    let mask = if masked {
        let start = offset;
        offset += 4;
        if buffer.len() < offset {
            return Ok(Decoded::Incomplete);
        }
        match buffer[start..start + 4].try_into() {
            Ok(m) => Some::<[u8; 4]>(m),
            Err(_) => return Ok(Decoded::Incomplete),
        }
    } else {
        None
    };

    if buffer.len() < offset + payload_len {
        return Ok(Decoded::Incomplete);
    }

    let payload: Vec<u8> = match mask {
        Some(key) => buffer[offset..offset + payload_len]
            .iter()
            .enumerate()
            .map(|(i, b)| b ^ key[i % 4])
            .collect(),
        None => buffer[offset..offset + payload_len].to_vec(),
    };
    let consumed = offset + payload_len;

    let frame = match opcode {
        OPCODE_TEXT => {
            Frame::Text(String::from_utf8(payload).map_err(|e| {
                ControlError::BadFrame(format!("text frame is not valid UTF-8: {e}"))
            })?)
        }
        OPCODE_BINARY => Frame::Binary(payload),
        OPCODE_PING => Frame::Ping(payload),
        OPCODE_PONG => Frame::Pong(payload),
        OPCODE_CLOSE => {
            let code = if payload.len() >= 2 {
                u16::from_be_bytes([payload[0], payload[1]])
            } else {
                1000
            };
            let reason = if payload.len() > 2 {
                String::from_utf8_lossy(&payload[2..]).into_owned()
            } else {
                String::new()
            };
            Frame::Close { code, reason }
        }
        OPCODE_CONTINUATION => {
            return Err(ControlError::BadFrame(
                "fragmented messages are not supported on the control channel".into(),
            )
            .into())
        }
        other => return Err(ControlError::BadFrame(format!("unknown opcode {other:#x}")).into()),
    };

    Ok(Decoded::Frame(frame, consumed))
}
