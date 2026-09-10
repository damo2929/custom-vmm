//! Interleaved RTP transport framing — §7.3.
//!
//! ```text
//! $ <channel:u8> <length:u16 BE> <RTP or RTCP packet>
//!  channel 0 = video RTP (H.264, RFC 6184)  1 = video RTCP
//!  channel 2 = audio RTP (Vorbis, RFC 5215) 3 = audio RTCP
//! ```
//!
//! The length prefix is big-endian (§0.1).

use libvmm_core::{MediaError, VmmResult};

pub const MAGIC: u8 = b'$';

pub const CHANNEL_VIDEO_RTP: u8 = 0;
pub const CHANNEL_VIDEO_RTCP: u8 = 1;
pub const CHANNEL_AUDIO_RTP: u8 = 2;
pub const CHANNEL_AUDIO_RTCP: u8 = 3;

/// RTP payload types. Both streams are dynamic, per RFC 6184 / RFC 5215.
pub const PAYLOAD_TYPE_H264: u8 = 96;
pub const PAYLOAD_TYPE_VORBIS: u8 = 97;
/// VP9, per the AOM RTP payload specification.
pub const PAYLOAD_TYPE_VP9: u8 = 98;
/// AV1, per the AOM RTP payload specification.
pub const PAYLOAD_TYPE_AV1: u8 = 99;
/// Opus, per RFC 7587.
pub const PAYLOAD_TYPE_OPUS: u8 = 100;

/// H.264 sample clock (RFC 6184).
pub const CLOCK_RATE_VIDEO: u32 = 90_000;

/// Wrap a packet in the interleaved framing.
pub fn frame(channel: u8, packet: &[u8]) -> VmmResult<Vec<u8>> {
    if packet.len() > u16::MAX as usize {
        return Err(MediaError::BadRequest(format!(
            "interleaved packet of {} bytes exceeds the 65535-byte length prefix",
            packet.len()
        ))
        .into());
    }
    let mut out = Vec::with_capacity(4 + packet.len());
    out.push(MAGIC);
    out.push(channel);
    out.extend_from_slice(&(packet.len() as u16).to_be_bytes());
    out.extend_from_slice(packet);
    Ok(out)
}

/// One decoded interleaved frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Interleaved {
    pub channel: u8,
    pub payload: Vec<u8>,
    /// Bytes consumed from the input.
    pub consumed: usize,
}

/// Decode one interleaved frame, or `None` if more bytes are needed.
pub fn parse(buffer: &[u8]) -> VmmResult<Option<Interleaved>> {
    if buffer.len() < 4 {
        return Ok(None);
    }
    if buffer[0] != MAGIC {
        return Err(MediaError::BadRequest(format!(
            "interleaved frame must start with '$', got {:#04x}",
            buffer[0]
        ))
        .into());
    }
    let channel = buffer[1];
    let length = u16::from_be_bytes([buffer[2], buffer[3]]) as usize;
    if buffer.len() < 4 + length {
        return Ok(None);
    }
    Ok(Some(Interleaved {
        channel,
        payload: buffer[4..4 + length].to_vec(),
        consumed: 4 + length,
    }))
}

/// A 12-byte RTP header (RFC 3550).
#[derive(Debug, Clone, Copy)]
pub struct RtpHeader {
    pub payload_type: u8,
    pub marker: bool,
    pub sequence: u16,
    pub timestamp: u32,
    pub ssrc: u32,
}

impl RtpHeader {
    pub fn write_into(&self, out: &mut Vec<u8>) {
        out.push(0x80); // version 2, no padding, no extension, no CSRC
        out.push((self.payload_type & 0x7F) | if self.marker { 0x80 } else { 0 });
        out.extend_from_slice(&self.sequence.to_be_bytes());
        out.extend_from_slice(&self.timestamp.to_be_bytes());
        out.extend_from_slice(&self.ssrc.to_be_bytes());
    }
}

/// Which interleaved channel a stream's RTP and RTCP use.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ChannelPair {
    pub rtp: u8,
    pub rtcp: u8,
}

pub const VIDEO_CHANNELS: ChannelPair = ChannelPair {
    rtp: CHANNEL_VIDEO_RTP,
    rtcp: CHANNEL_VIDEO_RTCP,
};
pub const AUDIO_CHANNELS: ChannelPair = ChannelPair {
    rtp: CHANNEL_AUDIO_RTP,
    rtcp: CHANNEL_AUDIO_RTCP,
};
