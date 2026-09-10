//! RTP depacketisation: the client half of §7.1/§7.3.

use libvmm_media::depacketize::{h264, vorbis, RtpPacket, SequenceTracker};
use libvmm_media::rtp::{PAYLOAD_TYPE_H264, PAYLOAD_TYPE_VORBIS};

/// Build an RTP packet around `payload`.
fn rtp(pt: u8, seq: u16, timestamp: u32, marker: bool, payload: &[u8]) -> Vec<u8> {
    let mut p = vec![0x80, (pt & 0x7F) | if marker { 0x80 } else { 0 }];
    p.extend_from_slice(&seq.to_be_bytes());
    p.extend_from_slice(&timestamp.to_be_bytes());
    p.extend_from_slice(&0xCAFE_BABEu32.to_be_bytes());
    p.extend_from_slice(payload);
    p
}

fn parse(bytes: &[u8]) -> RtpPacket {
    RtpPacket::parse(bytes).unwrap()
}

// -- RTP header --------------------------------------------------------------

#[test]
fn an_rtp_header_parses_with_its_fields() {
    let p = parse(&rtp(PAYLOAD_TYPE_H264, 42, 9000, true, b"payload"));
    assert_eq!(p.payload_type, PAYLOAD_TYPE_H264);
    assert_eq!(p.sequence, 42);
    assert_eq!(p.timestamp, 9000);
    assert!(p.marker);
    assert_eq!(p.ssrc, 0xCAFE_BABE);
    assert_eq!(p.payload, b"payload");
}

#[test]
fn csrc_and_extension_headers_are_skipped() {
    // Two CSRCs plus a one-word extension before the payload.
    let mut bytes = vec![0x92, PAYLOAD_TYPE_H264]; // version 2, extension, CC=2
    bytes.extend_from_slice(&1u16.to_be_bytes());
    bytes.extend_from_slice(&0u32.to_be_bytes());
    bytes.extend_from_slice(&0u32.to_be_bytes());
    bytes.extend_from_slice(&[0u8; 8]); // two CSRCs
    bytes.extend_from_slice(&[0xBE, 0xDE, 0x00, 0x01]); // extension, 1 word
    bytes.extend_from_slice(&[0u8; 4]);
    bytes.extend_from_slice(b"real");

    assert_eq!(parse(&bytes).payload, b"real");
}

#[test]
fn padding_is_stripped() {
    let mut bytes = rtp(PAYLOAD_TYPE_H264, 1, 0, false, b"data");
    bytes[0] |= 0x20; // padding flag
    bytes.extend_from_slice(&[0, 0, 3]); // three padding bytes, count last
    assert_eq!(parse(&bytes).payload, b"data");
}

#[test]
fn a_truncated_or_wrong_version_packet_is_rejected() {
    assert!(RtpPacket::parse(&[0x80, 0x60]).is_err());
    let mut bad = rtp(PAYLOAD_TYPE_H264, 1, 0, false, b"x");
    bad[0] = 0x40; // version 1
    assert!(RtpPacket::parse(&bad).is_err());
}

#[test]
fn absurd_padding_is_rejected_rather_than_underflowing() {
    let mut bytes = rtp(PAYLOAD_TYPE_H264, 1, 0, false, b"ab");
    bytes[0] |= 0x20;
    let last = bytes.len() - 1;
    bytes[last] = 200; // more padding than the packet holds
    assert!(RtpPacket::parse(&bytes).is_err());
}

// -- sequence tracking -------------------------------------------------------

#[test]
fn the_sequence_tracker_counts_loss_and_reordering() {
    let mut t = SequenceTracker::new();
    assert!(t.accept(10));
    assert!(t.accept(11));
    // 12 and 13 are lost.
    assert!(!t.accept(14));
    assert_eq!(t.lost, 2);
    // An old packet arrives late.
    assert!(!t.accept(12));
    assert_eq!(t.reordered, 1);
}

#[test]
fn the_sequence_tracker_handles_the_16_bit_wrap() {
    let mut t = SequenceTracker::new();
    assert!(t.accept(65534));
    assert!(t.accept(65535));
    assert!(
        t.accept(0),
        "wrapping to 0 is in order, not a 65535-packet gap"
    );
    assert_eq!(t.lost, 0);
}

// -- H.264, RFC 6184 ---------------------------------------------------------

#[test]
fn a_single_nal_becomes_one_annex_b_access_unit() {
    let mut d = h264::Depacketizer::new();
    // NAL type 1 (non-IDR slice).
    let nal = [0x41u8, 0xAA, 0xBB, 0xCC];
    let unit = d
        .push(&parse(&rtp(PAYLOAD_TYPE_H264, 1, 9000, true, &nal)))
        .unwrap()
        .unwrap();

    assert_eq!(&unit.data[..4], &h264::START_CODE);
    assert_eq!(&unit.data[4..], &nal);
    assert_eq!(unit.timestamp, 9000);
    assert!(!unit.keyframe);
}

#[test]
fn stap_a_unpacks_every_aggregated_nal() {
    let mut d = h264::Depacketizer::new();
    // STAP-A carrying an SPS and a PPS.
    let sps = [0x67u8, 0x42, 0x00, 0x1E];
    let pps = [0x68u8, 0xCE, 0x3C, 0x80];
    let mut payload = vec![0x78]; // NAL type 24, STAP-A
    payload.extend_from_slice(&(sps.len() as u16).to_be_bytes());
    payload.extend_from_slice(&sps);
    payload.extend_from_slice(&(pps.len() as u16).to_be_bytes());
    payload.extend_from_slice(&pps);

    let unit = d
        .push(&parse(&rtp(PAYLOAD_TYPE_H264, 1, 9000, true, &payload)))
        .unwrap()
        .unwrap();
    assert!(unit.keyframe, "SPS/PPS make the unit a decoder start point");

    // Two start codes, one per NAL.
    let starts = unit
        .data
        .windows(4)
        .filter(|w| *w == h264::START_CODE)
        .count();
    assert_eq!(starts, 2);
    assert!(unit.data.windows(4).any(|w| w == sps));
    assert!(unit.data.windows(4).any(|w| w == pps));
}

#[test]
fn fu_a_reassembles_a_fragmented_nal() {
    let mut d = h264::Depacketizer::new();
    // An IDR slice (type 5) split across three FU-A packets.
    // Indicator keeps F/NRI; FU header carries start/end and the real type.
    let start = [0x7Cu8, 0x85, b'a', b'b']; // S=1, type 5
    let middle = [0x7Cu8, 0x05, b'c', b'd']; // continuation
    let end = [0x7Cu8, 0x45, b'e', b'f']; // E=1

    assert!(d
        .push(&parse(&rtp(PAYLOAD_TYPE_H264, 1, 9000, false, &start)))
        .unwrap()
        .is_none());
    assert!(d
        .push(&parse(&rtp(PAYLOAD_TYPE_H264, 2, 9000, false, &middle)))
        .unwrap()
        .is_none());
    let unit = d
        .push(&parse(&rtp(PAYLOAD_TYPE_H264, 3, 9000, true, &end)))
        .unwrap()
        .unwrap();

    // Reconstructed NAL header: F/NRI from the indicator, type 5 from the FU.
    assert_eq!(&unit.data[..4], &h264::START_CODE);
    assert_eq!(
        unit.data[4], 0x65,
        "NRI from the indicator, type 5 from the FU header"
    );
    assert_eq!(&unit.data[5..], b"abcdef");
    assert!(unit.keyframe, "an IDR is a keyframe");
}

#[test]
fn a_lost_fragment_discards_the_nal_instead_of_emitting_a_corrupt_one() {
    let mut d = h264::Depacketizer::new();
    let start = [0x7Cu8, 0x85, b'a', b'b'];
    let end = [0x7Cu8, 0x45, b'e', b'f'];

    d.push(&parse(&rtp(PAYLOAD_TYPE_H264, 1, 9000, false, &start)))
        .unwrap();
    // Sequence 2 is lost; 3 arrives.
    let unit = d
        .push(&parse(&rtp(PAYLOAD_TYPE_H264, 3, 9000, true, &end)))
        .unwrap();

    assert!(
        d.dropped_fragments > 0,
        "the gap must discard the partial NAL"
    );
    // Nothing usable was produced from the broken fragment.
    assert!(unit.is_none() || unit.is_some_and(|u| !u.data.windows(6).any(|w| w == b"abcdef")));
}

#[test]
fn a_continuation_without_a_start_is_dropped() {
    let mut d = h264::Depacketizer::new();
    let orphan = [0x7Cu8, 0x05, b'x', b'y'];
    assert!(d
        .push(&parse(&rtp(PAYLOAD_TYPE_H264, 1, 9000, false, &orphan)))
        .unwrap()
        .is_none());
    assert_eq!(d.dropped_fragments, 1);
}

#[test]
fn a_timestamp_change_closes_the_previous_access_unit() {
    let mut d = h264::Depacketizer::new();
    // First unit, no marker.
    assert!(d
        .push(&parse(&rtp(
            PAYLOAD_TYPE_H264,
            1,
            9000,
            false,
            &[0x41, 0xAA]
        )))
        .unwrap()
        .is_none());
    // A new timestamp begins the next unit and flushes the first.
    let flushed = d
        .push(&parse(&rtp(
            PAYLOAD_TYPE_H264,
            2,
            12000,
            false,
            &[0x41, 0xBB],
        )))
        .unwrap();
    assert!(flushed.is_some());
    assert_eq!(flushed.unwrap().timestamp, 9000);
}

#[test]
fn a_lying_stap_a_length_is_rejected() {
    let mut d = h264::Depacketizer::new();
    // Declares a 200-byte NAL in a 6-byte payload.
    let payload = [0x78u8, 0x00, 0xC8, 0x41, 0xAA, 0xBB];
    assert!(d
        .push(&parse(&rtp(PAYLOAD_TYPE_H264, 1, 9000, true, &payload)))
        .is_err());
}

// -- Vorbis, RFC 5215 --------------------------------------------------------

/// Build a Vorbis RTP payload.
fn vorbis_payload(ident: u32, fragment: u8, data_type: u8, packets: &[&[u8]]) -> Vec<u8> {
    let mut p = vec![
        ((ident >> 16) & 0xFF) as u8,
        ((ident >> 8) & 0xFF) as u8,
        (ident & 0xFF) as u8,
        (fragment << 6) | (data_type << 4) | (packets.len() as u8 & 0x0F),
    ];
    for packet in packets {
        p.extend_from_slice(&(packet.len() as u16).to_be_bytes());
        p.extend_from_slice(packet);
    }
    p
}

#[test]
fn a_whole_vorbis_packet_is_extracted() {
    let mut d = vorbis::Depacketizer::new();
    let payload = vorbis_payload(
        0x123456,
        vorbis::FRAG_WHOLE,
        vorbis::VDT_AUDIO,
        &[b"audio-data"],
    );
    let out = d
        .push(&parse(&rtp(PAYLOAD_TYPE_VORBIS, 1, 48000, false, &payload)))
        .unwrap();

    assert_eq!(out.len(), 1);
    assert_eq!(out[0].data, b"audio-data");
    assert_eq!(out[0].ident, 0x123456);
    assert!(!out[0].configuration);
}

#[test]
fn several_packets_in_one_rtp_packet_all_come_out() {
    let mut d = vorbis::Depacketizer::new();
    let payload = vorbis_payload(
        1,
        vorbis::FRAG_WHOLE,
        vorbis::VDT_AUDIO,
        &[b"one", b"two", b"three"],
    );
    let out = d
        .push(&parse(&rtp(PAYLOAD_TYPE_VORBIS, 1, 48000, false, &payload)))
        .unwrap();

    assert_eq!(out.len(), 3);
    assert_eq!(out[0].data, b"one");
    assert_eq!(out[2].data, b"three");
}

#[test]
fn a_fragmented_vorbis_packet_reassembles() {
    let mut d = vorbis::Depacketizer::new();
    let first = vorbis_payload(7, vorbis::FRAG_FIRST, vorbis::VDT_AUDIO, &[b"aaa"]);
    let cont = vorbis_payload(7, vorbis::FRAG_CONTINUATION, vorbis::VDT_AUDIO, &[b"bbb"]);
    let last = vorbis_payload(7, vorbis::FRAG_LAST, vorbis::VDT_AUDIO, &[b"ccc"]);

    assert!(d
        .push(&parse(&rtp(PAYLOAD_TYPE_VORBIS, 1, 48000, false, &first)))
        .unwrap()
        .is_empty());
    assert!(d
        .push(&parse(&rtp(PAYLOAD_TYPE_VORBIS, 2, 48000, false, &cont)))
        .unwrap()
        .is_empty());
    let out = d
        .push(&parse(&rtp(PAYLOAD_TYPE_VORBIS, 3, 48000, false, &last)))
        .unwrap();

    assert_eq!(out.len(), 1);
    assert_eq!(out[0].data, b"aaabbbccc");
    assert_eq!(out[0].ident, 7);
}

#[test]
fn an_in_band_configuration_packet_is_flagged() {
    let mut d = vorbis::Depacketizer::new();
    let payload = vorbis_payload(
        1,
        vorbis::FRAG_WHOLE,
        vorbis::VDT_CONFIG,
        &[b"setup-header"],
    );
    let out = d
        .push(&parse(&rtp(PAYLOAD_TYPE_VORBIS, 1, 0, false, &payload)))
        .unwrap();
    assert!(
        out[0].configuration,
        "a decoder must not treat this as audio"
    );
}

#[test]
fn a_vorbis_continuation_without_a_first_fragment_is_dropped() {
    let mut d = vorbis::Depacketizer::new();
    let cont = vorbis_payload(
        1,
        vorbis::FRAG_CONTINUATION,
        vorbis::VDT_AUDIO,
        &[b"orphan"],
    );
    assert!(d
        .push(&parse(&rtp(PAYLOAD_TYPE_VORBIS, 1, 0, false, &cont)))
        .unwrap()
        .is_empty());
    assert_eq!(d.dropped_fragments, 1);
}

#[test]
fn a_lying_vorbis_length_is_rejected() {
    let mut d = vorbis::Depacketizer::new();
    // Declares 500 bytes in a payload that holds 4.
    let payload = vec![0, 0, 1, 0x01, 0x01, 0xF4, b'a', b'b'];
    assert!(d
        .push(&parse(&rtp(PAYLOAD_TYPE_VORBIS, 1, 0, false, &payload)))
        .is_err());
}

#[test]
fn a_short_vorbis_payload_is_rejected() {
    let mut d = vorbis::Depacketizer::new();
    assert!(d
        .push(&parse(&rtp(PAYLOAD_TYPE_VORBIS, 1, 0, false, &[0, 0])))
        .is_err());
}

#[test]
fn the_sdp_configuration_parameter_decodes() {
    use base64::Engine as _;
    let raw = b"vorbis-setup-headers";
    let encoded = base64::engine::general_purpose::STANDARD.encode(raw);
    let fmtp = format!("97 delivery-method=inline; configuration={encoded}");
    assert_eq!(
        vorbis::decode_sdp_configuration(&fmtp).as_deref(),
        Some(&raw[..])
    );
    assert!(vorbis::decode_sdp_configuration("97 delivery-method=inline").is_none());
}
