//! Vorbis RTP depacketisation — RFC 5215.
//!
//! Payload layout:
//!
//! ```text
//! 0                   1                   2                   3
//! | Ident (24 bits)                       | F |VDT| pkts      |
//! | length (16) | vorbis packet data ...                      |
//! ```
//!
//! `VDT` distinguishes raw audio (0) from an in-band configuration packet
//! (1). `F` is the fragment type: 0 = whole packet(s), 1 = first fragment,
//! 2 = continuation, 3 = last fragment.

use super::{bad, RtpPacket, SequenceTracker};
use libvmm_core::VmmResult;

pub const VDT_AUDIO: u8 = 0;
pub const VDT_CONFIG: u8 = 1;
pub const VDT_COMMENT: u8 = 2;

pub const FRAG_WHOLE: u8 = 0;
pub const FRAG_FIRST: u8 = 1;
pub const FRAG_CONTINUATION: u8 = 2;
pub const FRAG_LAST: u8 = 3;

/// One reassembled Vorbis packet.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VorbisPacket {
    pub data: Vec<u8>,
    pub timestamp: u32,
    /// The configuration this packet belongs to, so a decoder can tell when
    /// the stream re-configures mid-session.
    pub ident: u32,
    /// True for an in-band identification/setup header rather than audio.
    pub configuration: bool,
}

#[derive(Debug, Default)]
pub struct Depacketizer {
    sequence: SequenceTracker,
    fragment: Vec<u8>,
    fragment_ident: u32,
    fragment_timestamp: u32,
    in_fragment: bool,
    pub dropped_fragments: u64,
}

impl Depacketizer {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn stats(&self) -> &SequenceTracker {
        &self.sequence
    }

    /// Feed one RTP packet, returning every Vorbis packet it completed.
    ///
    /// A single RTP packet can carry several whole Vorbis packets, so this
    /// returns a vector rather than an option.
    pub fn push(&mut self, packet: &RtpPacket) -> VmmResult<Vec<VorbisPacket>> {
        let in_order = self.sequence.accept(packet.sequence);
        if !in_order && self.in_fragment {
            self.discard_fragment();
        }

        if packet.payload.len() < 4 {
            return Err(bad(format!(
                "Vorbis RTP payload is {} bytes, need at least the 4-byte header",
                packet.payload.len()
            )));
        }

        let p = &packet.payload;
        let ident = u32::from_be_bytes([0, p[0], p[1], p[2]]);
        let fragment_type = (p[3] >> 6) & 0x03;
        let data_type = (p[3] >> 4) & 0x03;
        let packet_count = p[3] & 0x0F;

        let mut out = Vec::new();
        let mut offset = 4;

        match fragment_type {
            FRAG_WHOLE => {
                // `packet_count` complete packets, each length-prefixed.
                for _ in 0..packet_count {
                    if offset + 2 > p.len() {
                        return Err(bad("Vorbis payload ends inside a length prefix".to_string()));
                    }
                    let len = u16::from_be_bytes([p[offset], p[offset + 1]]) as usize;
                    offset += 2;
                    if offset + len > p.len() {
                        return Err(bad(format!(
                            "Vorbis packet declares {len} bytes but only {} remain",
                            p.len() - offset
                        )));
                    }
                    out.push(VorbisPacket {
                        data: p[offset..offset + len].to_vec(),
                        timestamp: packet.timestamp,
                        ident,
                        configuration: data_type != VDT_AUDIO,
                    });
                    offset += len;
                }
            }
            FRAG_FIRST => {
                if self.in_fragment {
                    self.discard_fragment();
                }
                let payload = Self::single_fragment(p, &mut offset)?;
                self.fragment.clear();
                self.fragment.extend_from_slice(payload);
                self.fragment_ident = ident;
                self.fragment_timestamp = packet.timestamp;
                self.in_fragment = true;
            }
            FRAG_CONTINUATION | FRAG_LAST => {
                if !self.in_fragment {
                    // The first fragment was lost; this one is unusable.
                    self.dropped_fragments += 1;
                    return Ok(out);
                }
                let payload = Self::single_fragment(p, &mut offset)?;
                self.fragment.extend_from_slice(payload);
                if fragment_type == FRAG_LAST {
                    out.push(VorbisPacket {
                        data: std::mem::take(&mut self.fragment),
                        timestamp: self.fragment_timestamp,
                        ident: self.fragment_ident,
                        configuration: data_type != VDT_AUDIO,
                    });
                    self.in_fragment = false;
                }
            }
            other => return Err(bad(format!("unknown Vorbis fragment type {other}"))),
        }

        Ok(out)
    }

    /// A fragmented payload carries exactly one length-prefixed chunk.
    fn single_fragment<'a>(p: &'a [u8], offset: &mut usize) -> VmmResult<&'a [u8]> {
        if *offset + 2 > p.len() {
            return Err(bad(
                "Vorbis fragment ends inside its length prefix".to_string()
            ));
        }
        let len = u16::from_be_bytes([p[*offset], p[*offset + 1]]) as usize;
        *offset += 2;
        if *offset + len > p.len() {
            return Err(bad(format!(
                "Vorbis fragment declares {len} bytes but only {} remain",
                p.len() - *offset
            )));
        }
        let slice = &p[*offset..*offset + len];
        *offset += len;
        Ok(slice)
    }

    fn discard_fragment(&mut self) {
        self.fragment.clear();
        self.in_fragment = false;
        self.dropped_fragments += 1;
    }
}

/// Decode the `configuration=` parameter from the SDP `a=fmtp:` line
/// (RFC 5215 §3.2), which carries the base64 identification and setup
/// headers a decoder needs before any audio.
pub fn decode_sdp_configuration(fmtp: &str) -> Option<Vec<u8>> {
    use base64::Engine as _;
    let value = fmtp
        .split(';')
        .map(str::trim)
        .find_map(|p| p.strip_prefix("configuration="))?;
    base64::engine::general_purpose::STANDARD.decode(value).ok()
}

impl super::AudioDepacketizer for Depacketizer {
    fn push(&mut self, packet: &RtpPacket) -> VmmResult<Vec<super::AudioPacket>> {
        Ok(Depacketizer::push(self, packet)?
            .into_iter()
            .map(|p| super::AudioPacket {
                data: p.data,
                timestamp: p.timestamp,
                configuration: p.configuration,
            })
            .collect())
    }
    fn stats(&self) -> &SequenceTracker {
        &self.sequence
    }
    fn dropped(&self) -> u64 {
        self.dropped_fragments
    }
    fn codec(&self) -> vmm_codec_sys::AudioCodec {
        vmm_codec_sys::AudioCodec::Vorbis
    }
}
