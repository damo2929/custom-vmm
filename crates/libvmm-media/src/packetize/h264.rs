//! RFC 6184 H.264 packetisation — the transmit side of §7.3.
//!
//! Three packet shapes, chosen per NAL unit:
//!
//! * **Single NAL unit** — a NAL that fits in one packet is sent verbatim,
//!   its own header byte doubling as the RTP payload header.
//! * **STAP-A** — consecutive small NALs are aggregated into one packet.
//!   This exists mainly for SPS+PPS, which are a few dozen bytes each and
//!   precede every IDR; sending them as three separate packets triples the
//!   per-keyframe packet count for no gain.
//! * **FU-A** — a NAL larger than the MTU is fragmented. Only FU-A is used,
//!   never FU-B, because the interleaved transport of §7.3 delivers in
//!   order and the DON field FU-B adds would be dead weight.
//!
//! The marker bit is set on the last packet of an access unit, which is what
//! tells the receiver the frame is complete.

use super::{check_mtu, Packet, RTP_HEADER_LEN};
use crate::depacketize::h264::{NAL_FU_A, NAL_STAP_A};
use crate::rtp::{RtpHeader, CLOCK_RATE_VIDEO, PAYLOAD_TYPE_H264};
use libvmm_core::{MediaError, VmmResult};

/// Largest NAL that may be folded into a STAP-A rather than sent alone.
///
/// Parameter sets are tens of bytes; slices are kilobytes. Aggregating only
/// the small ones keeps the aggregate itself well inside one packet.
const STAP_A_MAX_NAL: usize = 256;

/// `nal_unit_type` values that carry no picture data and always precede a
/// slice, so aggregating them costs nothing in latency.
fn is_parameter_set(nal_type: u8) -> bool {
    // 6 = SEI, 7 = SPS, 8 = PPS.
    matches!(nal_type, 6..=8)
}

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
        check_mtu("H.264 packetiser", mtu)?;
        Ok(Packetizer {
            sequence: 0,
            ssrc,
            mtu,
        })
    }

    /// Next sequence number that will be used, for tests and RTCP.
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
        let ticks = pts.unsigned_abs() * (CLOCK_RATE_VIDEO as u64) / framerate as u64;
        ticks as u32
    }

    /// Packetise one Annex-B access unit.
    pub fn packetize(&mut self, access_unit: &[u8], timestamp: u32) -> VmmResult<Vec<Packet>> {
        let nals = split_annexb(access_unit);
        if nals.is_empty() {
            return Err(MediaError::Packetize {
                detail: "the access unit contained no NAL units".to_string(),
            }
            .into());
        }

        let mut payloads: Vec<(Vec<u8>, bool)> = Vec::new();
        let budget = self.mtu.saturating_sub(RTP_HEADER_LEN);

        let mut index = 0;
        while index < nals.len() {
            let nal = nals[index];
            if nal.is_empty() {
                index += 1;
                continue;
            }

            // Try to aggregate a run of small parameter sets.
            if is_parameter_set(nal[0] & 0x1f) && nal.len() <= STAP_A_MAX_NAL {
                let (aggregate, consumed) = build_stap_a(&nals[index..], budget);
                if consumed > 1 {
                    payloads.push((aggregate, false));
                    index += consumed;
                    continue;
                }
            }

            if nal.len() <= budget {
                payloads.push((nal.to_vec(), false));
            } else {
                for fragment in fragment_fu_a(nal, budget)? {
                    payloads.push((fragment, false));
                }
            }
            index += 1;
        }

        if payloads.is_empty() {
            return Err(MediaError::Packetize {
                detail: "the access unit produced no payloads".to_string(),
            }
            .into());
        }

        // Only the very last packet of the access unit carries the marker.
        if let Some(last) = payloads.last_mut() {
            last.1 = true;
        }

        Ok(payloads
            .into_iter()
            .map(|(payload, marker)| {
                let header = RtpHeader {
                    payload_type: PAYLOAD_TYPE_H264,
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
            })
            .collect())
    }
}

/// Split an Annex-B byte stream into NAL units, dropping the start codes.
pub fn split_annexb(data: &[u8]) -> Vec<&[u8]> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i..].starts_with(&[0, 0, 0, 1]) {
            starts.push((i, 4));
            i += 4;
        } else if data[i..].starts_with(&[0, 0, 1]) {
            starts.push((i, 3));
            i += 3;
        } else {
            i += 1;
        }
    }

    let mut nals = Vec::with_capacity(starts.len());
    for (n, &(offset, code_len)) in starts.iter().enumerate() {
        let begin = offset + code_len;
        let end = starts.get(n + 1).map(|&(o, _)| o).unwrap_or(data.len());
        if begin < end {
            nals.push(&data[begin..end]);
        }
    }
    nals
}

/// Aggregate as many leading small NALs as fit. Returns the payload and how
/// many NALs it consumed; a count of 1 means aggregation was not worthwhile.
fn build_stap_a(nals: &[&[u8]], budget: usize) -> (Vec<u8>, usize) {
    // STAP-A header byte, then per-NAL 2-byte length prefixes.
    let mut payload = vec![0u8; 1];
    let mut consumed = 0usize;
    let mut max_nri = 0u8;

    for nal in nals {
        if nal.is_empty() || nal.len() > STAP_A_MAX_NAL || !is_parameter_set(nal[0] & 0x1f) {
            break;
        }
        if nal.len() > u16::MAX as usize {
            break;
        }
        if payload.len() + 2 + nal.len() > budget {
            break;
        }
        payload.extend_from_slice(&(nal.len() as u16).to_be_bytes());
        payload.extend_from_slice(nal);
        // The aggregate's NRI is the highest of its members (RFC 6184 §5.7.1).
        max_nri = max_nri.max(nal[0] & 0x60);
        consumed += 1;
    }

    // F bit stays 0; NRI from the members; type 24.
    payload[0] = max_nri | NAL_STAP_A;
    (payload, consumed)
}

/// Fragment one oversized NAL into FU-A packets.
fn fragment_fu_a(nal: &[u8], budget: usize) -> VmmResult<Vec<Vec<u8>>> {
    // Two bytes of FU-A overhead: the indicator and the FU header.
    let per_packet = budget.saturating_sub(2);
    if per_packet == 0 {
        return Err(MediaError::Packetize {
            detail: format!("an MTU leaving {budget} payload bytes cannot carry an FU-A header"),
        }
        .into());
    }

    let header = nal[0];
    let indicator = (header & 0xe0) | NAL_FU_A;
    let nal_type = header & 0x1f;
    let body = &nal[1..];

    let mut out = Vec::with_capacity(body.len().div_ceil(per_packet));
    let mut offset = 0;
    while offset < body.len() {
        let end = (offset + per_packet).min(body.len());
        let start_bit = if offset == 0 { 0x80 } else { 0 };
        let end_bit = if end == body.len() { 0x40 } else { 0 };

        let mut packet = Vec::with_capacity(2 + (end - offset));
        packet.push(indicator);
        packet.push(start_bit | end_bit | nal_type);
        packet.extend_from_slice(&body[offset..end]);
        out.push(packet);
        offset = end;
    }

    Ok(out)
}
