//! VP9 depacketisation — the inverse of [`crate::packetize::vp9`].

use super::{RtpPacket, SequenceTracker};
use libvmm_core::{MediaError, VmmResult};

const FLAG_PICTURE_ID: u8 = 0x80;
const FLAG_LAYER_INDICES: u8 = 0x20;
const FLAG_FLEXIBLE: u8 = 0x10;
const FLAG_START_OF_FRAME: u8 = 0x08;
const FLAG_END_OF_FRAME: u8 = 0x04;
const FLAG_SCALABILITY: u8 = 0x02;
const PICTURE_ID_EXTENDED: u8 = 0x80;

/// One reassembled VP9 frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Vp9Frame {
    pub data: Vec<u8>,
    pub timestamp: u32,
    pub picture_id: u16,
    /// A frame the decoder can start on.
    pub keyframe: bool,
}

#[derive(Debug, Default)]
pub struct Depacketizer {
    partial: Vec<u8>,
    timestamp: u32,
    picture_id: u16,
    keyframe: bool,
    /// Set once a start-of-frame has been seen; a continuation arriving
    /// before one means the frame began in a lost packet.
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

    /// Feed one RTP packet; returns a frame once it is complete.
    /// Loss counters for this stream.
    pub fn stats(&self) -> &SequenceTracker {
        &self.sequence
    }

    pub fn push(&mut self, packet: &RtpPacket) -> VmmResult<Option<Vp9Frame>> {
        let payload = &packet.payload;
        let flags = *payload
            .first()
            .ok_or_else(|| MediaError::BadRequest("a VP9 packet with no descriptor".to_string()))?;

        let mut at = 1usize;

        if flags & FLAG_PICTURE_ID != 0 {
            let first = *payload.get(at).ok_or_else(truncated)?;
            at += 1;
            if first & PICTURE_ID_EXTENDED != 0 {
                let second = *payload.get(at).ok_or_else(truncated)?;
                at += 1;
                self.picture_id = (u16::from(first & 0x7f) << 8) | u16::from(second);
            } else {
                self.picture_id = u16::from(first & 0x7f);
            }
        }
        if flags & FLAG_LAYER_INDICES != 0 {
            // TID/U/SID/D, then TL0PICIDX when not in flexible mode.
            at += 1;
            if flags & FLAG_FLEXIBLE == 0 {
                at += 1;
            }
        }
        if flags & FLAG_SCALABILITY != 0 {
            // A scalability structure this single-layer stream never sends.
            // Refuse rather than guess at its length and mis-frame the rest.
            return Err(MediaError::BadRequest(
                "a VP9 packet carried a scalability structure, which this \
                 single-layer receiver cannot parse"
                    .to_string(),
            )
            .into());
        }

        if at > payload.len() {
            return Err(truncated().into());
        }
        let body = &payload[at..];

        if flags & FLAG_START_OF_FRAME != 0 {
            if self.started && !self.partial.is_empty() {
                // The previous frame never finished; its tail was lost.
                self.dropped += 1;
            }
            self.partial.clear();
            self.started = true;
            self.timestamp = packet.timestamp;
            // P clear means this frame predicts from nothing: a keyframe.
            self.keyframe = flags & 0x40 == 0;
        } else if !self.started {
            // A continuation with no beginning — the frame started in a
            // packet that was lost, so there is nothing to reassemble onto.
            self.dropped += 1;
            return Ok(None);
        }

        self.partial.extend_from_slice(body);

        if flags & FLAG_END_OF_FRAME == 0 {
            return Ok(None);
        }

        self.started = false;
        Ok(Some(Vp9Frame {
            data: core::mem::take(&mut self.partial),
            timestamp: self.timestamp,
            picture_id: self.picture_id,
            keyframe: self.keyframe,
        }))
    }
}

fn truncated() -> MediaError {
    MediaError::BadRequest("a VP9 packet ended inside its descriptor".to_string())
}

impl super::VideoDepacketizer for Depacketizer {
    fn push(&mut self, packet: &RtpPacket) -> VmmResult<Option<super::CodedUnit>> {
        Ok(Depacketizer::push(self, packet)?.map(|f| super::CodedUnit {
            data: f.data,
            timestamp: f.timestamp,
            keyframe: f.keyframe,
        }))
    }
    fn stats(&self) -> &SequenceTracker {
        &self.sequence
    }
    fn dropped(&self) -> u64 {
        self.dropped
    }
    fn codec(&self) -> vmm_codec_sys::VideoCodec {
        vmm_codec_sys::VideoCodec::Vp9
    }
}
