//! H.264 RTP depacketisation — RFC 6184.
//!
//! The encoder is programmed with `packetization-mode=1` (§7.1 SDP), so three
//! payload structures appear: a single NAL unit, STAP-A aggregation (type 24)
//! and FU-A fragmentation (type 28). Output is Annex-B: each NAL prefixed
//! with a 4-byte start code, which is what every decoder and `.h264` file
//! expects.

use super::{bad, RtpPacket, SequenceTracker};
use libvmm_core::VmmResult;

/// Annex-B start code.
pub const START_CODE: [u8; 4] = [0, 0, 0, 1];

/// RFC 6184 payload types carried in the NAL header's type field.
pub const NAL_STAP_A: u8 = 24;
pub const NAL_FU_A: u8 = 28;

/// NAL unit types worth naming.
pub const NAL_IDR: u8 = 5;
pub const NAL_SPS: u8 = 7;
pub const NAL_PPS: u8 = 8;

/// One reassembled access unit: the NALs sharing an RTP timestamp.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AccessUnit {
    /// Annex-B bytes, ready to hand to a decoder or write to a file.
    pub data: Vec<u8>,
    pub timestamp: u32,
    /// True when the unit contains an IDR, SPS or PPS — a decoder can start
    /// here.
    pub keyframe: bool,
}

/// Reassembles H.264 access units from an RTP stream.
#[derive(Debug, Default)]
pub struct Depacketizer {
    sequence: SequenceTracker,
    /// NALs collected for the access unit currently being built.
    current: Vec<u8>,
    current_timestamp: Option<u32>,
    keyframe: bool,
    /// FU-A reassembly buffer.
    fragment: Vec<u8>,
    fragment_header: u8,
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

    /// Feed one RTP packet. Returns a completed access unit when the marker
    /// bit closes one.
    pub fn push(&mut self, packet: &RtpPacket) -> VmmResult<Option<AccessUnit>> {
        let in_order = self.sequence.accept(packet.sequence);
        if !in_order && self.in_fragment {
            // A gap inside a fragmented NAL makes the NAL unrecoverable.
            self.discard_fragment();
        }

        if packet.payload.is_empty() {
            return Err(bad("H.264 RTP packet has an empty payload".to_string()));
        }

        // A timestamp change starts a new access unit even without a marker.
        if self
            .current_timestamp
            .is_some_and(|t| t != packet.timestamp)
            && !self.current.is_empty()
        {
            let finished = self.take_access_unit();
            self.begin(packet.timestamp);
            self.decode_payload(packet)?;
            return Ok(finished);
        }
        if self.current_timestamp.is_none() {
            self.begin(packet.timestamp);
        }

        self.decode_payload(packet)?;

        // The marker bit ends the access unit (RFC 6184 §5.1).
        if packet.marker && !self.current.is_empty() {
            return Ok(self.take_access_unit());
        }
        Ok(None)
    }

    fn decode_payload(&mut self, packet: &RtpPacket) -> VmmResult<()> {
        let header = packet.payload[0];
        match header & 0x1F {
            NAL_STAP_A => self.decode_stap_a(&packet.payload),
            NAL_FU_A => self.decode_fu_a(&packet.payload),
            // 1..=23 are single NAL units carried whole.
            1..=23 => {
                self.emit_nal(&packet.payload);
                Ok(())
            }
            other => Err(bad(format!("unsupported H.264 RTP payload type {other}"))),
        }
    }

    /// STAP-A: `header | (len:u16 nal)*`.
    fn decode_stap_a(&mut self, payload: &[u8]) -> VmmResult<()> {
        let mut offset = 1;
        while offset + 2 <= payload.len() {
            let len = u16::from_be_bytes([payload[offset], payload[offset + 1]]) as usize;
            offset += 2;
            if len == 0 || offset + len > payload.len() {
                return Err(bad(format!(
                    "STAP-A declares a {len}-byte NAL but only {} bytes remain",
                    payload.len().saturating_sub(offset)
                )));
            }
            let nal = payload[offset..offset + len].to_vec();
            self.emit_nal(&nal);
            offset += len;
        }
        if offset != payload.len() {
            return Err(bad(
                "STAP-A has trailing bytes after its last NAL".to_string()
            ));
        }
        Ok(())
    }

    /// FU-A: `indicator | fu_header | fragment`. The start bit opens the NAL,
    /// the end bit closes it, and the real NAL type lives in the FU header.
    fn decode_fu_a(&mut self, payload: &[u8]) -> VmmResult<()> {
        if payload.len() < 3 {
            return Err(bad(
                "FU-A packet is shorter than its two-byte header".to_string()
            ));
        }
        let indicator = payload[0];
        let fu_header = payload[1];
        let start = fu_header & 0x80 != 0;
        let end = fu_header & 0x40 != 0;
        let nal_type = fu_header & 0x1F;

        if start {
            if self.in_fragment {
                // A new fragment began before the last one ended.
                self.discard_fragment();
            }
            // Rebuild the original NAL header: F and NRI from the indicator,
            // type from the FU header.
            self.fragment_header = (indicator & 0xE0) | nal_type;
            self.fragment.clear();
            self.fragment.push(self.fragment_header);
            self.in_fragment = true;
        } else if !self.in_fragment {
            // A continuation with no start: the beginning was lost.
            self.dropped_fragments += 1;
            return Ok(());
        }

        self.fragment.extend_from_slice(&payload[2..]);

        if end {
            let nal = std::mem::take(&mut self.fragment);
            self.in_fragment = false;
            self.emit_nal(&nal);
        }
        Ok(())
    }

    fn emit_nal(&mut self, nal: &[u8]) {
        if nal.is_empty() {
            return;
        }
        if matches!(nal[0] & 0x1F, NAL_IDR | NAL_SPS | NAL_PPS) {
            self.keyframe = true;
        }
        self.current.extend_from_slice(&START_CODE);
        self.current.extend_from_slice(nal);
    }

    fn begin(&mut self, timestamp: u32) {
        self.current_timestamp = Some(timestamp);
        self.keyframe = false;
    }

    fn take_access_unit(&mut self) -> Option<AccessUnit> {
        if self.current.is_empty() {
            return None;
        }
        let unit = AccessUnit {
            data: std::mem::take(&mut self.current),
            timestamp: self.current_timestamp.unwrap_or(0),
            keyframe: self.keyframe,
        };
        self.keyframe = false;
        self.current_timestamp = None;
        Some(unit)
    }

    fn discard_fragment(&mut self) {
        self.fragment.clear();
        self.in_fragment = false;
        self.dropped_fragments += 1;
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
        self.dropped_fragments
    }
    fn codec(&self) -> vmm_codec_sys::VideoCodec {
        vmm_codec_sys::VideoCodec::H264
    }
}
