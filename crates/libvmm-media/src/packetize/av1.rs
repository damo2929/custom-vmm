//! AV1 RTP packetisation, per the AOM "RTP Payload Format For AV1".
//!
//! Every packet begins with a one-byte aggregation header:
//!
//! ```text
//!  0 1 2 3 4 5 6 7
//! +-+-+-+-+-+-+-+-+
//! |Z|Y| W |N|-|-|-|
//! +-+-+-+-+-+-+-+-+
//! ```
//!
//! `Z` the first OBU element continues an OBU fragmented from the previous
//! packet · `Y` the last element continues in the next packet · `W` how many
//! OBU elements the packet holds · `N` this packet starts a new coded video
//! sequence.
//!
//! This packetiser always uses `W = 1`: one OBU element per packet, filling
//! the MTU. With `W` non-zero the final element carries no length prefix, so
//! a single-element packet is pure payload after the header — which makes
//! fragmenting a large temporal unit both simple and free of overhead. The
//! alternative, `W = 0` with every element length-prefixed, buys OBU-level
//! aggregation that is only worth having when many tiny OBUs share a packet;
//! at 1080p the encoder emits few, large OBUs and there is nothing to
//! aggregate.

use super::{check_mtu, Packet, RTP_HEADER_LEN};
use crate::rtp::{RtpHeader, CLOCK_RATE_VIDEO, PAYLOAD_TYPE_AV1};
use libvmm_core::{MediaError, VmmResult};

const FLAG_CONTINUES_PREVIOUS: u8 = 0x80; // Z
const FLAG_CONTINUES_NEXT: u8 = 0x40; // Y
const FLAG_NEW_SEQUENCE: u8 = 0x08; // N
/// `W = 1`, in bits 5-4.
const W_ONE_ELEMENT: u8 = 0x10;

/// The aggregation header is one byte.
pub const AGGREGATION_HEADER_LEN: usize = 1;

pub struct Packetizer {
    sequence: u16,
    ssrc: u32,
    mtu: usize,
}

impl Packetizer {
    pub fn new(ssrc: u32) -> Self {
        Packetizer {
            sequence: 0,
            ssrc,
            mtu: super::DEFAULT_MTU,
        }
    }

    pub fn with_mtu(ssrc: u32, mtu: usize) -> VmmResult<Self> {
        check_mtu("AV1 packetiser", mtu)?;
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

    /// Packetise one AV1 temporal unit.
    ///
    /// `keyframe` sets `N` on the first packet: it tells a receiver that a
    /// new coded video sequence starts here, which is what lets a client
    /// joining mid-stream know it can begin decoding.
    pub fn packetize(
        &mut self,
        temporal_unit: &[u8],
        timestamp: u32,
        keyframe: bool,
    ) -> VmmResult<Vec<Packet>> {
        if temporal_unit.is_empty() {
            return Err(MediaError::Packetize {
                detail: "an AV1 temporal unit of zero bytes cannot be sent".to_string(),
            }
            .into());
        }

        let budget = self
            .mtu
            .saturating_sub(RTP_HEADER_LEN + AGGREGATION_HEADER_LEN);
        if budget == 0 {
            return Err(MediaError::Packetize {
                detail: format!("an MTU of {} leaves no room for an AV1 payload", self.mtu),
            }
            .into());
        }

        let mut packets = Vec::with_capacity(temporal_unit.len().div_ceil(budget));
        let mut offset = 0;
        while offset < temporal_unit.len() {
            let end = (offset + budget).min(temporal_unit.len());
            let first = offset == 0;
            let last = end == temporal_unit.len();

            let mut aggregation = W_ONE_ELEMENT;
            if !first {
                aggregation |= FLAG_CONTINUES_PREVIOUS;
            }
            if !last {
                aggregation |= FLAG_CONTINUES_NEXT;
            }
            if first && keyframe {
                aggregation |= FLAG_NEW_SEQUENCE;
            }

            let header = RtpHeader {
                payload_type: PAYLOAD_TYPE_AV1,
                marker: last,
                sequence: self.sequence,
                timestamp,
                ssrc: self.ssrc,
            };
            self.sequence = self.sequence.wrapping_add(1);

            let mut data =
                Vec::with_capacity(RTP_HEADER_LEN + AGGREGATION_HEADER_LEN + (end - offset));
            header.write_into(&mut data);
            data.push(aggregation);
            data.extend_from_slice(&temporal_unit[offset..end]);
            packets.push(Packet { data, marker: last });

            offset = end;
        }

        Ok(packets)
    }
}
