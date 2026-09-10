//! AV1 depacketisation — the inverse of [`crate::packetize::av1`].
//!
//! Only the `W = 1` shape the packetiser emits is reassembled: one OBU
//! element per packet, fragmented across as many packets as the temporal
//! unit needs. A packet carrying `W = 0` (every element length-prefixed) or
//! `W = 2..3` is rejected rather than mis-parsed, because guessing at
//! element boundaries would hand the decoder a plausible-looking but corrupt
//! bitstream.

use super::{RtpPacket, SequenceTracker};
use libvmm_core::{MediaError, VmmResult};

const FLAG_CONTINUES_PREVIOUS: u8 = 0x80; // Z
const FLAG_CONTINUES_NEXT: u8 = 0x40; // Y
const FLAG_NEW_SEQUENCE: u8 = 0x08; // N
const W_MASK: u8 = 0x30;
const W_SHIFT: u8 = 4;

/// One reassembled AV1 temporal unit.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Av1TemporalUnit {
    pub data: Vec<u8>,
    pub timestamp: u32,
    /// Starts a new coded video sequence, so a decoder can begin here.
    pub keyframe: bool,
}

#[derive(Debug, Default)]
pub struct Depacketizer {
    partial: Vec<u8>,
    timestamp: u32,
    keyframe: bool,
    started: bool,
    pub dropped: u64,
    /// Loss and reordering, the same accounting the H.264 and Vorbis
    /// depacketisers keep. Without it a session on this codec reports zero
    /// loss however much it drops.
    sequence: SequenceTracker,
}

impl Depacketizer {
    pub fn new() -> Self {
        Depacketizer::default()
    }

    /// Feed one RTP packet; returns a temporal unit once it is complete.
    /// Loss counters for this stream.
    pub fn stats(&self) -> &SequenceTracker {
        &self.sequence
    }

    pub fn push(&mut self, packet: &RtpPacket) -> VmmResult<Option<Av1TemporalUnit>> {
        let aggregation = *packet.payload.first().ok_or_else(|| {
            MediaError::BadRequest("an AV1 packet with no aggregation header".to_string())
        })?;

        let elements = (aggregation & W_MASK) >> W_SHIFT;
        if elements != 1 {
            return Err(MediaError::BadRequest(format!(
                "an AV1 packet declared W={elements}; this receiver reassembles only the \
                 single-element form its packetiser emits"
            ))
            .into());
        }

        let body = &packet.payload[1..];
        let continues_previous = aggregation & FLAG_CONTINUES_PREVIOUS != 0;
        let continues_next = aggregation & FLAG_CONTINUES_NEXT != 0;

        if continues_previous {
            if !self.started {
                // The unit began in a packet that was lost.
                self.dropped += 1;
                return Ok(None);
            }
        } else {
            if self.started && !self.partial.is_empty() {
                // The previous unit never finished.
                self.dropped += 1;
            }
            self.partial.clear();
            self.started = true;
            self.timestamp = packet.timestamp;
            self.keyframe = aggregation & FLAG_NEW_SEQUENCE != 0;
        }

        self.partial.extend_from_slice(body);

        if continues_next {
            return Ok(None);
        }

        self.started = false;
        Ok(Some(Av1TemporalUnit {
            data: core::mem::take(&mut self.partial),
            timestamp: self.timestamp,
            keyframe: self.keyframe,
        }))
    }
}

impl super::VideoDepacketizer for Depacketizer {
    fn push(&mut self, packet: &RtpPacket) -> VmmResult<Option<super::CodedUnit>> {
        Ok(Depacketizer::push(self, packet)?.map(|u| super::CodedUnit {
            data: u.data,
            timestamp: u.timestamp,
            keyframe: u.keyframe,
        }))
    }
    fn stats(&self) -> &SequenceTracker {
        &self.sequence
    }
    fn dropped(&self) -> u64 {
        self.dropped
    }
    fn codec(&self) -> vmm_codec_sys::VideoCodec {
        vmm_codec_sys::VideoCodec::Av1
    }
}
