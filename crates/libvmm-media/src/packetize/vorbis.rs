//! RFC 5215 Vorbis packetisation — the audio half of §7.3.
//!
//! Each RTP packet carries a 4-byte Vorbis payload header:
//!
//! ```text
//!  0                   1                   2                   3
//!  0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1 2 3 4 5 6 7 8 9 0 1
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! |                     Ident                     | F |VDT|# pkts.|
//! +-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+-+
//! ```
//!
//! `Ident` names the codebook configuration the receiver was given out of
//! band in the SDP (§7.2), so a receiver that has not seen that
//! configuration knows to discard rather than mis-decode. Every packet in a
//! session carries the same identifier.
//!
//! Packets are then length-prefixed with two bytes each, so several small
//! Vorbis packets can share one RTP packet and a large one can be split
//! across several with the fragment field marking the pieces.

use super::{check_mtu, Packet, RTP_HEADER_LEN};
use crate::rtp::{RtpHeader, PAYLOAD_TYPE_VORBIS};
use libvmm_core::{MediaError, VmmResult};

/// The 4-byte Vorbis payload header.
pub const PAYLOAD_HEADER_LEN: usize = 4;
/// The 2-byte length prefix each packet carries.
pub const LENGTH_PREFIX_LEN: usize = 2;

/// Vorbis Data Type (RFC 5215 §2.3).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DataType {
    /// Raw Vorbis audio.
    Audio = 0,
    /// A Vorbis comment header.
    Comment = 1,
    /// Packed configuration — the identification and setup headers.
    Configuration = 2,
}

/// Fragment field (RFC 5215 §2.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Fragment {
    /// A whole packet, or several.
    None = 0,
    First = 1,
    Middle = 2,
    Last = 3,
}

pub struct Packetizer {
    sequence: u16,
    ssrc: u32,
    /// The 24-bit configuration identifier this session advertises.
    ident: u32,
    mtu: usize,
    sample_rate: u32,
    /// Samples emitted so far, which is the RTP timestamp for Vorbis.
    samples: u64,
}

impl Packetizer {
    pub fn new(ssrc: u32, ident: u32, sample_rate: u32) -> Self {
        Packetizer {
            sequence: 0,
            ssrc,
            ident: ident & 0x00ff_ffff,
            mtu: super::DEFAULT_MTU,
            sample_rate,
            samples: 0,
        }
    }

    pub fn with_mtu(ssrc: u32, ident: u32, sample_rate: u32, mtu: usize) -> VmmResult<Self> {
        check_mtu("Vorbis packetiser", mtu)?;
        let mut p = Packetizer::new(ssrc, ident, sample_rate);
        p.mtu = mtu;
        Ok(p)
    }

    pub fn sequence(&self) -> u16 {
        self.sequence
    }

    pub fn ssrc(&self) -> u32 {
        self.ssrc
    }

    /// The 24-bit configuration identifier, as the SDP must advertise it.
    pub fn ident(&self) -> u32 {
        self.ident
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Advance the media clock by `frames` sample frames.
    ///
    /// Vorbis' RTP clock rate is the sample rate, so the timestamp is simply
    /// the count of sample frames emitted (RFC 5215 §4).
    pub fn advance(&mut self, frames: u64) {
        self.samples = self.samples.wrapping_add(frames);
    }

    pub fn timestamp(&self) -> u32 {
        self.samples as u32
    }

    /// Packetise one Vorbis audio packet at the current timestamp.
    pub fn packetize(&mut self, packet: &[u8]) -> VmmResult<Vec<Packet>> {
        self.packetize_as(packet, DataType::Audio)
    }

    /// Packetise the packed configuration, for receivers fetching it in band.
    pub fn packetize_configuration(&mut self, configuration: &[u8]) -> VmmResult<Vec<Packet>> {
        self.packetize_as(configuration, DataType::Configuration)
    }

    fn packetize_as(&mut self, packet: &[u8], data_type: DataType) -> VmmResult<Vec<Packet>> {
        if packet.is_empty() {
            return Err(MediaError::Packetize {
                detail: "a Vorbis packet of zero bytes cannot be sent".to_string(),
            }
            .into());
        }
        if packet.len() > u16::MAX as usize && data_type != DataType::Audio {
            return Err(MediaError::Packetize {
                detail: format!(
                    "a {} byte configuration exceeds the 16-bit length prefix",
                    packet.len()
                ),
            }
            .into());
        }

        let budget = self
            .mtu
            .saturating_sub(RTP_HEADER_LEN + PAYLOAD_HEADER_LEN + LENGTH_PREFIX_LEN);
        if budget == 0 {
            return Err(MediaError::Packetize {
                detail: format!("an MTU of {} leaves no room for a Vorbis payload", self.mtu),
            }
            .into());
        }

        let timestamp = self.timestamp();

        if packet.len() <= budget {
            // One whole packet in one RTP packet.
            let payload = self.build(Fragment::None, data_type, 1, packet);
            return Ok(vec![self.wrap(payload, timestamp, true)]);
        }

        // Fragmented: the packet count field is zero for every fragment, and
        // the fragment field marks first/middle/last.
        let mut out = Vec::new();
        let mut offset = 0;
        while offset < packet.len() {
            let end = (offset + budget).min(packet.len());
            let fragment = if offset == 0 {
                Fragment::First
            } else if end == packet.len() {
                Fragment::Last
            } else {
                Fragment::Middle
            };
            let payload = self.build(fragment, data_type, 0, &packet[offset..end]);
            let last = end == packet.len();
            out.push(self.wrap(payload, timestamp, last));
            offset = end;
        }
        Ok(out)
    }

    /// Build the Vorbis payload: 4-byte header, then length-prefixed body.
    fn build(
        &self,
        fragment: Fragment,
        data_type: DataType,
        packet_count: u8,
        body: &[u8],
    ) -> Vec<u8> {
        let mut payload = Vec::with_capacity(PAYLOAD_HEADER_LEN + LENGTH_PREFIX_LEN + body.len());
        payload.push((self.ident >> 16) as u8);
        payload.push((self.ident >> 8) as u8);
        payload.push(self.ident as u8);
        // F occupies bits 7-6, VDT bits 5-4, packet count bits 3-0.
        payload.push(((fragment as u8) << 6) | ((data_type as u8) << 4) | (packet_count & 0x0f));
        payload.extend_from_slice(&(body.len() as u16).to_be_bytes());
        payload.extend_from_slice(body);
        payload
    }

    fn wrap(&mut self, payload: Vec<u8>, timestamp: u32, marker: bool) -> Packet {
        let header = RtpHeader {
            payload_type: PAYLOAD_TYPE_VORBIS,
            // RFC 5215 leaves the marker unused for audio; setting it on the
            // final fragment lets a receiver see packet boundaries without
            // reassembling, which costs nothing and helps diagnostics.
            marker,
            sequence: self.sequence,
            timestamp,
            ssrc: self.ssrc,
        };
        self.sequence = self.sequence.wrapping_add(1);
        let mut data = Vec::with_capacity(RTP_HEADER_LEN + payload.len());
        header.write_into(&mut data);
        data.extend_from_slice(&payload);
        Packet { data, marker }
    }
}
