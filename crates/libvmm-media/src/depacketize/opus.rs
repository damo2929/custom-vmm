//! Opus depacketisation — the inverse of [`crate::packetize::opus`].
//!
//! There is almost nothing to do: RFC 7587 puts the Opus packet in the RTP
//! payload unwrapped, so depacketisation is a copy. The type exists so the
//! client's demux can treat all codecs the same way, and so packet loss is
//! counted somewhere.

use super::{RtpPacket, SequenceTracker};
use libvmm_core::{MediaError, VmmResult};

/// One Opus packet, ready for the decoder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpusFrame {
    pub data: Vec<u8>,
    /// 48 kHz sample count, per RFC 7587 §4.1.
    pub timestamp: u32,
    /// Set on the first packet of a talkspurt.
    pub start_of_talkspurt: bool,
}

#[derive(Debug, Default)]
pub struct Depacketizer {
    pub packets: u64,
    /// Loss and reordering, the same accounting the H.264 and Vorbis
    /// depacketisers keep. Without it a session on this codec reports zero
    /// loss however much it drops.
    sequence: SequenceTracker,
}

impl Depacketizer {
    pub fn new() -> Self {
        Depacketizer::default()
    }

    /// Loss counters for this stream.
    pub fn stats(&self) -> &SequenceTracker {
        &self.sequence
    }

    pub fn push(&mut self, packet: &RtpPacket) -> VmmResult<Option<OpusFrame>> {
        if packet.payload.is_empty() {
            return Err(MediaError::BadRequest(
                "an Opus RTP packet with an empty payload".to_string(),
            )
            .into());
        }
        self.packets += 1;
        Ok(Some(OpusFrame {
            data: packet.payload.to_vec(),
            timestamp: packet.timestamp,
            start_of_talkspurt: packet.marker,
        }))
    }
}

impl super::AudioDepacketizer for Depacketizer {
    fn push(&mut self, packet: &RtpPacket) -> VmmResult<Vec<super::AudioPacket>> {
        Ok(Depacketizer::push(self, packet)?
            .into_iter()
            .map(|f| super::AudioPacket {
                data: f.data,
                timestamp: f.timestamp,
                // RFC 7587: the payload is the Opus packet. There is no
                // in-band configuration to distinguish.
                configuration: false,
            })
            .collect())
    }
    fn stats(&self) -> &SequenceTracker {
        &self.sequence
    }
    fn dropped(&self) -> u64 {
        // Opus is never fragmented: one RTP packet is one frame, so there is
        // nothing partial that could be discarded.
        0
    }
    fn codec(&self) -> vmm_codec_sys::AudioCodec {
        vmm_codec_sys::AudioCodec::Opus
    }
}
