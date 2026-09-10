//! RTP depacketisation — the client half of §7.1/§7.3.
//!
//! The hypervisor packetises H.264 per RFC 6184 and Vorbis per RFC 5215 and
//! interleaves both over the RTSPS connection (§7.3). These reassemble the
//! elementary streams a decoder consumes.
//!
//! Reassembly is where a media client is most easily fooled, so both
//! depacketisers reject truncated payloads, detect sequence discontinuities,
//! and drop an incomplete fragment rather than emitting a corrupt access
//! unit.

pub mod av1;
pub mod h264;
pub mod opus;
pub mod vorbis;
pub mod vp9;

use libvmm_core::{MediaError, VmmResult};
use vmm_codec_sys::{AudioCodec, VideoCodec};

/// One coded video unit, whatever codec produced it.
///
/// Reassembly output is codec-agnostic by nature: a decoder needs the bytes,
/// when they are for, and whether it may start here. Carrying AV1 in a type
/// called `AccessUnit` — which is what this replaced — made every signature
/// in the client claim H.264.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CodedUnit {
    /// Bytes ready for a decoder, or to write to a file.
    pub data: Vec<u8>,
    /// The RTP timestamp the unit was carried on.
    pub timestamp: u32,
    /// True when a decoder can begin here: an IDR/SPS/PPS for H.264, a
    /// keyframe for VP9, the start of a coded video sequence for AV1.
    pub keyframe: bool,
}

/// One coded audio packet.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct AudioPacket {
    pub data: Vec<u8>,
    pub timestamp: u32,
    /// True when this carries codec configuration rather than audio.
    ///
    /// Only Vorbis produces these — RFC 5215 allows the identification and
    /// setup headers in band. Opus never does (§7.1.3), which is one of the
    /// reasons Revision B prefers it. A consumer must not hand a
    /// configuration packet to a decoder as if it were sound.
    pub configuration: bool,
}

/// The contract every video depacketiser meets.
///
/// A trait rather than five inherent `push` methods because the client
/// chooses one at runtime from the negotiated SDP, and because the loss
/// accounting below was missing from three of the five until it was noticed
/// that packet-loss reporting was dead on exactly the codecs negotiation
/// selects.
pub trait VideoDepacketizer: Send {
    /// Feed one RTP packet; returns a unit when one completes.
    fn push(&mut self, packet: &RtpPacket) -> VmmResult<Option<CodedUnit>>;
    /// Sequence accounting, for loss reporting.
    fn stats(&self) -> &SequenceTracker;
    /// Fragments discarded because reassembly could not complete them.
    fn dropped(&self) -> u64;
    fn codec(&self) -> VideoCodec;
}

/// The contract every audio depacketiser meets.
pub trait AudioDepacketizer: Send {
    /// Feed one RTP packet; returns any packets it completed. Vorbis can
    /// yield several from one RTP packet, Opus exactly one.
    fn push(&mut self, packet: &RtpPacket) -> VmmResult<Vec<AudioPacket>>;
    fn stats(&self) -> &SequenceTracker;
    fn dropped(&self) -> u64;
    fn codec(&self) -> AudioCodec;
}

/// The depacketiser for a negotiated video codec (§7.6).
pub fn video_for(codec: VideoCodec) -> Box<dyn VideoDepacketizer> {
    match codec {
        VideoCodec::H264 => Box::new(h264::Depacketizer::new()),
        VideoCodec::Vp9 => Box::new(vp9::Depacketizer::default()),
        VideoCodec::Av1 => Box::new(av1::Depacketizer::default()),
    }
}

/// The depacketiser for a negotiated audio codec (§7.6).
pub fn audio_for(codec: AudioCodec) -> Box<dyn AudioDepacketizer> {
    match codec {
        AudioCodec::Vorbis => Box::new(vorbis::Depacketizer::new()),
        AudioCodec::Opus => Box::new(opus::Depacketizer::default()),
    }
}

/// A parsed RTP packet: the 12-byte header plus its payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RtpPacket {
    pub payload_type: u8,
    pub marker: bool,
    pub sequence: u16,
    pub timestamp: u32,
    pub ssrc: u32,
    pub payload: Vec<u8>,
}

impl RtpPacket {
    pub const HEADER_LEN: usize = 12;

    /// Parse an RTP packet, honouring the CSRC count and the extension and
    /// padding flags (RFC 3550 §5.1).
    pub fn parse(bytes: &[u8]) -> VmmResult<Self> {
        if bytes.len() < Self::HEADER_LEN {
            return Err(bad(format!(
                "RTP packet is {} bytes, need at least 12",
                bytes.len()
            )));
        }
        let version = bytes[0] >> 6;
        if version != 2 {
            return Err(bad(format!("RTP version {version}, expected 2")));
        }
        let has_padding = bytes[0] & 0x20 != 0;
        let has_extension = bytes[0] & 0x10 != 0;
        let csrc_count = (bytes[0] & 0x0F) as usize;

        let mut offset = Self::HEADER_LEN + csrc_count * 4;
        if bytes.len() < offset {
            return Err(bad("RTP packet is shorter than its CSRC list".to_string()));
        }

        if has_extension {
            if bytes.len() < offset + 4 {
                return Err(bad(
                    "RTP packet is shorter than its extension header".to_string()
                ));
            }
            let words = u16::from_be_bytes([bytes[offset + 2], bytes[offset + 3]]) as usize;
            offset += 4 + words * 4;
            if bytes.len() < offset {
                return Err(bad(
                    "RTP extension runs past the end of the packet".to_string()
                ));
            }
        }

        let mut end = bytes.len();
        if has_padding {
            let pad = *bytes.last().unwrap_or(&0) as usize;
            if pad == 0 || pad > end - offset {
                return Err(bad(format!(
                    "RTP padding of {pad} bytes does not fit the packet"
                )));
            }
            end -= pad;
        }

        Ok(RtpPacket {
            payload_type: bytes[1] & 0x7F,
            marker: bytes[1] & 0x80 != 0,
            sequence: u16::from_be_bytes([bytes[2], bytes[3]]),
            timestamp: u32::from_be_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]),
            ssrc: u32::from_be_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]),
            payload: bytes[offset..end].to_vec(),
        })
    }
}

pub(crate) fn bad(detail: String) -> libvmm_core::VmmError {
    MediaError::BadRequest(detail).into()
}

/// Tracks RTP sequence numbers so a depacketiser can tell a lost packet from
/// an in-order one, and count what it dropped.
#[derive(Debug, Default)]
pub struct SequenceTracker {
    expected: Option<u16>,
    pub received: u64,
    pub lost: u64,
    pub reordered: u64,
}

impl SequenceTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a packet. Returns false when it is out of order or follows a
    /// gap, which means any partially reassembled unit must be discarded.
    pub fn accept(&mut self, sequence: u16) -> bool {
        self.received += 1;
        let Some(expected) = self.expected else {
            self.expected = Some(sequence.wrapping_add(1));
            return true;
        };
        self.expected = Some(sequence.wrapping_add(1));

        if sequence == expected {
            return true;
        }
        // A 16-bit sequence wraps, so compare as a signed difference.
        let gap = sequence.wrapping_sub(expected) as i16;
        if gap > 0 {
            self.lost += gap as u64;
        } else {
            self.reordered += 1;
        }
        false
    }
}
