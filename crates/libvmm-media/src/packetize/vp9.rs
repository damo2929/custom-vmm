//! VP9 RTP packetisation, per the VP9 payload draft
//! (draft-ietf-payload-vp9, the format WebRTC deploys).
//!
//! Every packet carries a payload descriptor whose first byte is a set of
//! flags:
//!
//! ```text
//!  0 1 2 3 4 5 6 7
//! +-+-+-+-+-+-+-+-+
//! |I|P|L|F|B|E|V|Z|
//! +-+-+-+-+-+-+-+-+
//! ```
//!
//! `I` a picture ID follows · `P` inter-picture predicted · `L` layer
//! indices follow · `F` flexible mode · `B` start of frame · `E` end of
//! frame · `V` a scalability structure follows · `Z` not a reference for
//! upper layers.
//!
//! This stream is single-layer and non-flexible, so `L`, `F`, `V` and `Z`
//! are always zero and the descriptor is three bytes: the flags plus a
//! 15-bit extended picture ID. Scalability would only earn its complexity
//! with multiple clients at different rates, which §8.3 caps at two and
//! §7.1 gives one encoder.

use super::{check_mtu, Packet, RTP_HEADER_LEN};
use crate::rtp::{RtpHeader, CLOCK_RATE_VIDEO, PAYLOAD_TYPE_VP9};
use libvmm_core::{MediaError, VmmResult};

/// Flags in the first descriptor byte.
const FLAG_PICTURE_ID: u8 = 0x80;
const FLAG_INTER_PICTURE: u8 = 0x40;
const FLAG_START_OF_FRAME: u8 = 0x08;
const FLAG_END_OF_FRAME: u8 = 0x04;

/// Set in the first picture-ID byte to mark it as 15-bit rather than 7-bit.
const PICTURE_ID_EXTENDED: u8 = 0x80;

/// Descriptor length: flags plus a 15-bit picture ID.
pub const DESCRIPTOR_LEN: usize = 3;

pub struct Packetizer {
    sequence: u16,
    ssrc: u32,
    mtu: usize,
    /// Wraps at 2^15, as the extended picture ID field is 15 bits.
    picture_id: u16,
}

impl Packetizer {
    pub fn new(ssrc: u32) -> Self {
        Packetizer {
            sequence: 0,
            ssrc,
            mtu: super::DEFAULT_MTU,
            picture_id: 0,
        }
    }

    pub fn with_mtu(ssrc: u32, mtu: usize) -> VmmResult<Self> {
        check_mtu("VP9 packetiser", mtu)?;
        let mut p = Packetizer::new(ssrc);
        p.mtu = mtu;
        Ok(p)
    }

    pub fn sequence(&self) -> u16 {
        self.sequence
    }

    pub fn ssrc(&self) -> u32 {
        self.ssrc
    }

    /// The 90 kHz RTP timestamp for a frame at `pts` in `framerate` units.
    pub fn timestamp(pts: i64, framerate: u32) -> u32 {
        if framerate == 0 {
            return 0;
        }
        (pts.unsigned_abs() * u64::from(CLOCK_RATE_VIDEO) / u64::from(framerate)) as u32
    }

    /// Packetise one coded VP9 frame.
    pub fn packetize(
        &mut self,
        frame: &[u8],
        timestamp: u32,
        keyframe: bool,
    ) -> VmmResult<Vec<Packet>> {
        if frame.is_empty() {
            return Err(MediaError::Packetize {
                detail: "a VP9 frame of zero bytes cannot be sent".to_string(),
            }
            .into());
        }

        let budget = self.mtu.saturating_sub(RTP_HEADER_LEN + DESCRIPTOR_LEN);
        if budget == 0 {
            return Err(MediaError::Packetize {
                detail: format!("an MTU of {} leaves no room for a VP9 payload", self.mtu),
            }
            .into());
        }

        let picture_id = self.picture_id & 0x7fff;
        self.picture_id = self.picture_id.wrapping_add(1) & 0x7fff;

        let mut packets = Vec::with_capacity(frame.len().div_ceil(budget));
        let mut offset = 0;
        while offset < frame.len() {
            let end = (offset + budget).min(frame.len());
            let first = offset == 0;
            let last = end == frame.len();

            let mut flags = FLAG_PICTURE_ID;
            // A keyframe is not predicted from anything, so P stays clear.
            if !keyframe {
                flags |= FLAG_INTER_PICTURE;
            }
            if first {
                flags |= FLAG_START_OF_FRAME;
            }
            if last {
                flags |= FLAG_END_OF_FRAME;
            }

            let mut payload = Vec::with_capacity(DESCRIPTOR_LEN + (end - offset));
            payload.push(flags);
            payload.push(PICTURE_ID_EXTENDED | (picture_id >> 8) as u8);
            payload.push(picture_id as u8);
            payload.extend_from_slice(&frame[offset..end]);

            let header = RtpHeader {
                payload_type: PAYLOAD_TYPE_VP9,
                // The marker marks the last packet of a frame.
                marker: last,
                sequence: self.sequence,
                timestamp,
                ssrc: self.ssrc,
            };
            self.sequence = self.sequence.wrapping_add(1);

            let mut data = Vec::with_capacity(RTP_HEADER_LEN + payload.len());
            header.write_into(&mut data);
            data.extend_from_slice(&payload);
            packets.push(Packet { data, marker: last });

            offset = end;
        }

        Ok(packets)
    }
}
