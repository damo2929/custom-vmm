//! RFC 6184 and RFC 5215 packetisation, paired against the depacketisers.
//!
//! The strongest check available is the round trip: whatever the transmit
//! side emits, the receive side must reassemble byte for byte. That pins
//! both halves together, so neither can drift.

use libvmm_media::depacketize::{h264 as h264_depack, vorbis as vorbis_depack};
use libvmm_media::packetize::{h264 as h264_pack, vorbis as vorbis_pack, Packet, RTP_HEADER_LEN};
use libvmm_media::RtpPacket;

const SSRC: u32 = 0xdead_beef;

/// Build an Annex-B access unit from (nal_type, length) pairs.
fn access_unit(nals: &[(u8, usize)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (index, (nal_type, length)) in nals.iter().enumerate() {
        out.extend_from_slice(&[0, 0, 0, 1]);
        // F=0, NRI=3 for parameter sets and IDR, 2 otherwise.
        let nri = if matches!(nal_type, 5 | 7 | 8) {
            0x60
        } else {
            0x40
        };
        out.push(nri | nal_type);
        for i in 0..*length {
            out.push(((index * 31 + i * 7) % 251) as u8);
        }
    }
    out
}

/// Strip the RTP header and hand the payload to the depacketiser.
fn to_rtp(packet: &Packet) -> RtpPacket {
    RtpPacket::parse(&packet.data).expect("the packetiser must emit a parseable RTP packet")
}

#[test]
fn a_small_access_unit_becomes_one_packet_per_nal() {
    let mut packetizer = h264_pack::Packetizer::new(SSRC);
    let unit = access_unit(&[(5, 200)]);
    let packets = packetizer.packetize(&unit, 0).expect("packetising");

    assert_eq!(packets.len(), 1);
    assert!(packets[0].marker, "the last packet must carry the marker");
    // 12-byte RTP header, then the NAL verbatim including its header byte.
    assert_eq!(packets[0].data.len(), RTP_HEADER_LEN + 201);
}

#[test]
fn parameter_sets_are_aggregated_into_one_stap_a() {
    let mut packetizer = h264_pack::Packetizer::new(SSRC);
    // SPS, PPS, then an IDR slice — the shape of every keyframe.
    let unit = access_unit(&[(7, 20), (8, 6), (5, 300)]);
    let packets = packetizer.packetize(&unit, 0).expect("packetising");

    assert_eq!(
        packets.len(),
        2,
        "SPS and PPS should share a STAP-A, leaving the slice on its own"
    );

    // First packet is a STAP-A (type 24).
    let first = to_rtp(&packets[0]);
    assert_eq!(first.payload[0] & 0x1f, 24);
    assert!(!packets[0].marker);
    assert!(packets[1].marker);
}

#[test]
fn an_oversized_nal_is_fragmented_and_reassembles() {
    let mut packetizer =
        h264_pack::Packetizer::with_mtu(SSRC, 300).expect("300 is above the minimum");
    let unit = access_unit(&[(5, 2000)]);
    let packets = packetizer.packetize(&unit, 0).expect("packetising");

    assert!(
        packets.len() > 1,
        "a 2000-byte NAL must fragment at MTU 300"
    );

    // First fragment has S set, last has E set, middles have neither.
    let first = to_rtp(&packets[0]);
    assert_eq!(first.payload[0] & 0x1f, 28, "FU-A indicator");
    assert_eq!(first.payload[1] & 0x80, 0x80, "S bit on the first fragment");
    let last = to_rtp(&packets[packets.len() - 1]);
    assert_eq!(last.payload[1] & 0x40, 0x40, "E bit on the last fragment");
    assert!(packets[packets.len() - 1].marker);

    // And the depacketiser puts it back together.
    let mut depacketizer = h264_depack::Depacketizer::new();
    let mut reassembled = None;
    for packet in &packets {
        if let Some(unit) = depacketizer.push(&to_rtp(packet)).expect("depacketising") {
            reassembled = Some(unit);
        }
    }
    let reassembled = reassembled.expect("the access unit must reassemble");
    assert_eq!(reassembled.data, unit);
    assert!(reassembled.keyframe);
}

#[test]
fn a_full_keyframe_round_trips_through_both_halves() {
    let mut packetizer =
        h264_pack::Packetizer::with_mtu(SSRC, 512).expect("512 is above the minimum");
    let unit = access_unit(&[(7, 25), (8, 8), (5, 5000)]);
    let packets = packetizer.packetize(&unit, 3000).expect("packetising");

    let mut depacketizer = h264_depack::Depacketizer::new();
    let mut reassembled = None;
    for packet in &packets {
        if let Some(u) = depacketizer.push(&to_rtp(packet)).expect("depacketising") {
            reassembled = Some(u);
        }
    }

    let reassembled = reassembled.expect("the access unit must reassemble");
    assert_eq!(
        reassembled.data, unit,
        "the reassembled access unit must be byte-identical"
    );
    assert!(reassembled.keyframe);
}

#[test]
fn sequence_numbers_advance_by_one_per_packet() {
    let mut packetizer =
        h264_pack::Packetizer::with_mtu(SSRC, 300).expect("300 is above the minimum");
    let packets = packetizer
        .packetize(&access_unit(&[(1, 3000)]), 0)
        .expect("packetising");

    let sequences: Vec<u16> = packets.iter().map(|p| to_rtp(p).sequence).collect();
    for pair in sequences.windows(2) {
        assert_eq!(
            pair[1],
            pair[0].wrapping_add(1),
            "sequence numbers must be contiguous: {sequences:?}"
        );
    }
    assert_eq!(packetizer.sequence(), packets.len() as u16);
}

#[test]
fn every_packet_carries_the_same_timestamp_and_ssrc() {
    let mut packetizer =
        h264_pack::Packetizer::with_mtu(SSRC, 300).expect("300 is above the minimum");
    let packets = packetizer
        .packetize(&access_unit(&[(5, 4000)]), 90_000)
        .expect("packetising");

    for packet in &packets {
        let rtp = to_rtp(packet);
        assert_eq!(rtp.timestamp, 90_000);
        assert_eq!(rtp.ssrc, SSRC);
        assert_eq!(rtp.payload_type, libvmm_media::rtp::PAYLOAD_TYPE_H264);
    }
}

#[test]
fn the_video_timestamp_advances_at_ninety_kilohertz() {
    // One second of video at 30 fps must advance the clock by exactly 90000.
    assert_eq!(h264_pack::Packetizer::timestamp(0, 30), 0);
    assert_eq!(h264_pack::Packetizer::timestamp(30, 30), 90_000);
    assert_eq!(h264_pack::Packetizer::timestamp(15, 30), 45_000);
    assert_eq!(h264_pack::Packetizer::timestamp(60, 60), 90_000);
}

#[test]
fn an_empty_access_unit_is_refused() {
    let mut packetizer = h264_pack::Packetizer::new(SSRC);
    assert!(packetizer.packetize(&[], 0).is_err());
    // Start codes with nothing between them are equally unusable.
    assert!(packetizer.packetize(&[0, 0, 0, 1], 0).is_err());
}

#[test]
fn an_mtu_below_the_minimum_is_refused() {
    assert!(h264_pack::Packetizer::with_mtu(SSRC, 16).is_err());
    assert!(vorbis_pack::Packetizer::with_mtu(SSRC, 1, 48_000, 16).is_err());
}

// ---------------------------------------------------------------------------
// Vorbis, RFC 5215
// ---------------------------------------------------------------------------

const IDENT: u32 = 0x00ab_cdef;

#[test]
fn a_small_vorbis_packet_becomes_one_rtp_packet() {
    let mut packetizer = vorbis_pack::Packetizer::new(SSRC, IDENT, 48_000);
    let payload = vec![0x5au8; 200];
    let packets = packetizer.packetize(&payload).expect("packetising");

    assert_eq!(packets.len(), 1);
    let rtp = to_rtp(&packets[0]);
    assert_eq!(rtp.payload_type, libvmm_media::rtp::PAYLOAD_TYPE_VORBIS);

    // Ident in the top 24 bits, then F=0, VDT=0, one packet.
    assert_eq!(rtp.payload[0], 0xab);
    assert_eq!(rtp.payload[1], 0xcd);
    assert_eq!(rtp.payload[2], 0xef);
    assert_eq!(rtp.payload[3], 0x01, "F=0, VDT=0, packet count 1");
    // Then the 2-byte length prefix.
    assert_eq!(u16::from_be_bytes([rtp.payload[4], rtp.payload[5]]), 200);
    assert_eq!(&rtp.payload[6..], &payload[..]);
}

#[test]
fn a_large_vorbis_packet_fragments_with_the_right_markers() {
    let mut packetizer =
        vorbis_pack::Packetizer::with_mtu(SSRC, IDENT, 48_000, 128).expect("above the minimum");
    let payload = vec![0x33u8; 1000];
    let packets = packetizer.packetize(&payload).expect("packetising");

    assert!(packets.len() > 2, "1000 bytes at MTU 128 must fragment");

    let fragment_type = |p: &Packet| (to_rtp(p).payload[3] >> 6) & 0x03;
    let packet_count = |p: &Packet| to_rtp(p).payload[3] & 0x0f;

    assert_eq!(fragment_type(&packets[0]), 1, "first fragment");
    assert_eq!(fragment_type(&packets[1]), 2, "middle fragment");
    assert_eq!(
        fragment_type(&packets[packets.len() - 1]),
        3,
        "last fragment"
    );
    for packet in &packets {
        assert_eq!(
            packet_count(packet),
            0,
            "a fragmented packet must report a count of zero"
        );
    }

    // Reassembling the bodies must give the original back.
    let mut body = Vec::new();
    for packet in &packets {
        let rtp = to_rtp(packet);
        let length = u16::from_be_bytes([rtp.payload[4], rtp.payload[5]]) as usize;
        body.extend_from_slice(&rtp.payload[6..6 + length]);
    }
    assert_eq!(body, payload);
}

#[test]
fn a_vorbis_packet_round_trips_through_the_depacketizer() {
    let mut packetizer = vorbis_pack::Packetizer::new(SSRC, IDENT, 48_000);
    let payload = vec![0x77u8; 300];
    let packets = packetizer.packetize(&payload).expect("packetising");

    let mut depacketizer = vorbis_depack::Depacketizer::new();
    let mut recovered = Vec::new();
    for packet in &packets {
        recovered.extend(
            depacketizer
                .push(&to_rtp(packet))
                .expect("depacketising")
                .into_iter()
                .map(|p| p.data),
        );
    }
    assert_eq!(recovered, vec![payload]);
}

#[test]
fn the_audio_timestamp_counts_sample_frames() {
    let mut packetizer = vorbis_pack::Packetizer::new(SSRC, IDENT, 48_000);
    assert_eq!(packetizer.timestamp(), 0);

    let packets = packetizer.packetize(&[1u8; 32]).expect("packetising");
    assert_eq!(
        to_rtp(&packets[0]).timestamp,
        0,
        "the first packet starts at zero"
    );

    // A Vorbis long block at 48 kHz is 1024 frames.
    packetizer.advance(1024);
    let packets = packetizer.packetize(&[2u8; 32]).expect("packetising");
    assert_eq!(to_rtp(&packets[0]).timestamp, 1024);
}

#[test]
fn the_configuration_carries_the_configuration_data_type() {
    let mut packetizer = vorbis_pack::Packetizer::new(SSRC, IDENT, 48_000);
    let packets = packetizer
        .packetize_configuration(&[9u8; 100])
        .expect("packetising");
    let rtp = to_rtp(&packets[0]);
    // VDT occupies bits 5-4; 2 is packed configuration.
    assert_eq!((rtp.payload[3] >> 4) & 0x03, 2);
}

#[test]
fn an_empty_vorbis_packet_is_refused() {
    let mut packetizer = vorbis_pack::Packetizer::new(SSRC, IDENT, 48_000);
    assert!(packetizer.packetize(&[]).is_err());
}

#[test]
fn the_ident_is_masked_to_twenty_four_bits() {
    let packetizer = vorbis_pack::Packetizer::new(SSRC, 0xffff_ffff, 48_000);
    assert_eq!(packetizer.ident(), 0x00ff_ffff);
}
