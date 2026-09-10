//! RFC 7587 Opus packetisation.
//!
//! The simplest payload format here by a wide margin: one Opus packet per
//! RTP packet, carried verbatim. There is no payload header, no fragment
//! field and no aggregation — an Opus packet is self-delimiting and always
//! fits an MTU at any sane bitrate, so none of that machinery is needed.
//!
//! Two details that are easy to get wrong:
//!
//! * **The clock is always 48 kHz**, whatever rate the source was captured
//!   at (RFC 7587 §4.1). Opus resamples internally and the RTP timestamp
//!   counts 48 kHz samples regardless.
//! * **The marker bit means the start of a talkspurt**, not the end of a
//!   frame (§4.2). It is set on the first packet after a silence gap, which
//!   for this stream means the first packet of the session, since DTX is
//!   off and audio is continuous.

use super::{check_mtu, Packet, RTP_HEADER_LEN};
use crate::rtp::RtpHeader;
use libvmm_core::{MediaError, VmmResult};
use vmm_codec_sys::OpusPacket;

/// RFC 7587 §4.1: the RTP clock rate is 48 kHz for every Opus stream.
pub const CLOCK_RATE: u32 = 48_000;

/// Opus' dynamic payload type in this tree's assignment.
pub const PAYLOAD_TYPE: u8 = crate::rtp::PAYLOAD_TYPE_OPUS;

pub struct Packetizer {
    sequence: u16,
    ssrc: u32,
    /// The capture rate, kept only to check the caller is at 48 kHz.
    sample_rate: u32,
    mtu: usize,
    /// Cleared after the first packet; see the marker-bit note above.
    start_of_talkspurt: bool,
}

impl Packetizer {
    pub fn new(ssrc: u32, sample_rate: u32) -> Self {
        Packetizer {
            sequence: 0,
            ssrc,
            sample_rate,
            mtu: super::DEFAULT_MTU,
            start_of_talkspurt: true,
        }
    }

    pub fn with_mtu(ssrc: u32, sample_rate: u32, mtu: usize) -> VmmResult<Self> {
        check_mtu("Opus packetiser", mtu)?;
        let mut p = Packetizer::new(ssrc, sample_rate);
        p.mtu = mtu;
        Ok(p)
    }

    pub fn sequence(&self) -> u16 {
        self.sequence
    }

    pub fn ssrc(&self) -> u32 {
        self.ssrc
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Packetise one Opus packet.
    pub fn packetize(&mut self, packet: &OpusPacket) -> VmmResult<Vec<Packet>> {
        if packet.data.is_empty() {
            return Err(MediaError::Packetize {
                detail: "an Opus packet of zero bytes cannot be sent".to_string(),
            }
            .into());
        }

        // An Opus packet at any bitrate this tree configures is a few hundred
        // bytes. If one somehow exceeds the MTU there is nothing to be done:
        // RFC 7587 defines no fragmentation, so say so rather than emit
        // something a receiver would silently mis-parse.
        let budget = self.mtu.saturating_sub(RTP_HEADER_LEN);
        if packet.data.len() > budget {
            return Err(MediaError::Packetize {
                detail: format!(
                    "an Opus packet of {} bytes exceeds the {budget}-byte payload budget, \
                     and RFC 7587 defines no fragmentation. Lower the audio bitrate or \
                     shorten the frame duration.",
                    packet.data.len()
                ),
            }
            .into());
        }

        let header = RtpHeader {
            payload_type: PAYLOAD_TYPE,
            marker: self.start_of_talkspurt,
            sequence: self.sequence,
            // The encoder's pts already counts 48 kHz samples, which is
            // exactly the RTP timestamp RFC 7587 asks for.
            timestamp: packet.pts as u32,
            ssrc: self.ssrc,
        };
        self.start_of_talkspurt = false;
        self.sequence = self.sequence.wrapping_add(1);

        let mut data = Vec::with_capacity(RTP_HEADER_LEN + packet.data.len());
        header.write_into(&mut data);
        data.extend_from_slice(&packet.data);

        Ok(vec![Packet {
            data,
            marker: header.marker,
        }])
    }

    /// Mark the next packet as beginning a new talkspurt.
    ///
    /// Only meaningful if the capture path ever stops sending; this stream
    /// runs DTX off and is continuous, so it is here for completeness rather
    /// than because the pipeline calls it.
    pub fn mark_talkspurt(&mut self) {
        self.start_of_talkspurt = true;
    }
}
