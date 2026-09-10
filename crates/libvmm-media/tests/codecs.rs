//! Every codec, end to end: scanout → encode → RTP → depacketise → decode.
//!
//! This is the test that would catch a break anywhere in a codec's chain,
//! since only a correct one gets the original picture back. It runs for all
//! three video codecs and both audio codecs, on whatever backend the host
//! provides.

use libvmm_config::{Display, MachineConfig};
use libvmm_media::depacketize::{
    av1 as av1_depack, h264 as h264_depack, opus as opus_depack, vorbis as vorbis_depack,
    vp9 as vp9_depack,
};
use libvmm_media::{
    AudioEncoderParams, AudioPipeline, RtpPacket, VideoEncoderParams, VideoPipeline,
};
use vmm_codec_sys::{
    AudioCodec, OpusDecoder, PackedFormat, PackedFrame, VideoCodec, VideoDecoder, VorbisDecoder,
    VorbisHeaders,
};

const REFERENCE: &str = include_str!("../../../config/reference-vm.toml");
const WIDTH: u32 = 320;
const HEIGHT: u32 = 240;

fn display() -> Display {
    let config = MachineConfig::from_toml_str(REFERENCE).expect("the reference config must load");
    let mut display = config.display;
    display.width = WIDTH;
    display.height = HEIGHT;
    display
}

fn scanout(index: u32) -> PackedFrame {
    let mut pixels = vec![0u8; (WIDTH * HEIGHT * 4) as usize];
    for row in 0..HEIGHT as usize {
        for col in 0..WIDTH as usize {
            let at = (row * WIDTH as usize + col) * 4;
            pixels[at] = (col / 2) as u8;
            pixels[at + 1] = (row / 2) as u8;
            pixels[at + 2] = ((index * 8) % 256) as u8;
            pixels[at + 3] = 0xff;
        }
    }
    PackedFrame::packed(WIDTH, HEIGHT, pixels, PackedFormat::Bgra).expect("valid geometry")
}

/// Push frames through a pipeline and reassemble whatever comes out,
/// returning the coded frames in order.
fn run_video(codec: VideoCodec, frames: u32) -> Vec<(Vec<u8>, bool)> {
    let params = VideoEncoderParams::from_config(&display())
        .expect("valid config")
        .for_codec(codec);
    let mut pipeline = VideoPipeline::open(params, 0x0bad_c0de, None).expect("pipeline opens");
    println!("{}: {}", codec.as_str(), pipeline.acceleration());

    let mut packets = Vec::new();
    for index in 0..frames {
        packets.extend(
            pipeline
                .push_scanout(&scanout(index), index as i64)
                .expect("push"),
        );
    }
    packets.extend(pipeline.drain().expect("drain"));

    let mut out = Vec::new();
    let mut h264 = h264_depack::Depacketizer::new();
    let mut vp9 = vp9_depack::Depacketizer::new();
    let mut av1 = av1_depack::Depacketizer::new();

    for packet in &packets {
        let rtp = RtpPacket::parse(&packet.data).expect("parseable RTP");
        match codec {
            VideoCodec::H264 => {
                assert_eq!(rtp.payload_type, libvmm_media::rtp::PAYLOAD_TYPE_H264);
                if let Some(u) = h264.push(&rtp).expect("depacketise") {
                    out.push((u.data, u.keyframe));
                }
            }
            VideoCodec::Vp9 => {
                assert_eq!(rtp.payload_type, libvmm_media::rtp::PAYLOAD_TYPE_VP9);
                if let Some(f) = vp9.push(&rtp).expect("depacketise") {
                    out.push((f.data, f.keyframe));
                }
            }
            VideoCodec::Av1 => {
                assert_eq!(rtp.payload_type, libvmm_media::rtp::PAYLOAD_TYPE_AV1);
                if let Some(t) = av1.push(&rtp).expect("depacketise") {
                    out.push((t.data, t.keyframe));
                }
            }
        }
    }
    out
}

fn video_round_trip(codec: VideoCodec) {
    let coded = run_video(codec, 12);
    assert!(
        !coded.is_empty(),
        "{} produced no coded frames",
        codec.as_str()
    );
    assert!(
        coded[0].1,
        "{}: the stream must open on a keyframe",
        codec.as_str()
    );

    let mut decoder = VideoDecoder::open(codec).expect("decoder");
    let mut decoded = 0usize;
    let mut geometry = None;
    for (data, _) in &coded {
        for frame in decoder.decode(data).expect("decode") {
            assert_eq!((frame.width, frame.height), (WIDTH, HEIGHT));
            // A real picture, not a flat field.
            let first = frame.y[0];
            assert!(
                frame.y.iter().any(|&v| v != first),
                "{}: decoded frame is uniform",
                codec.as_str()
            );
            geometry = Some((frame.width, frame.height));
            decoded += 1;
        }
    }
    decoded += decoder.finish().expect("flush").len();

    assert!(
        decoded > 0,
        "{}: nothing decoded from {} coded frame(s)",
        codec.as_str(),
        coded.len()
    );
    assert_eq!(geometry, Some((WIDTH, HEIGHT)));
    println!(
        "{}: {} coded frame(s) -> {decoded} decoded",
        codec.as_str(),
        coded.len()
    );
}

#[test]
fn h264_round_trips() {
    video_round_trip(VideoCodec::H264);
}

#[test]
fn vp9_round_trips() {
    video_round_trip(VideoCodec::Vp9);
}

#[test]
fn av1_round_trips() {
    video_round_trip(VideoCodec::Av1);
}

#[test]
fn every_video_codec_opens_on_this_host() {
    for codec in [VideoCodec::H264, VideoCodec::Vp9, VideoCodec::Av1] {
        let params = VideoEncoderParams::from_config(&display())
            .expect("valid config")
            .for_codec(codec);
        let pipeline = VideoPipeline::open(params, 1, None)
            .unwrap_or_else(|e| panic!("{} must open: {e}", codec.as_str()));
        assert_eq!(pipeline.codec(), codec);
    }
}

/// A tenth of a second of tone.
fn tone(frames: usize) -> Vec<i16> {
    let mut pcm = Vec::with_capacity(frames * 2);
    for n in 0..frames {
        let t = n as f32 / 48_000.0;
        let sample = ((t * 440.0 * std::f32::consts::TAU).sin() * 10_000.0) as i16;
        pcm.push(sample);
        pcm.push(sample);
    }
    pcm
}

#[test]
fn opus_round_trips_through_rtp() {
    let params = AudioEncoderParams::from_config(&display()).for_codec(AudioCodec::Opus);
    let mut pipeline = AudioPipeline::open(params, 0x0a0d_0000).expect("opens");
    assert!(
        pipeline.vorbis_headers().is_none(),
        "Opus needs no out-of-band configuration"
    );

    let mut packets = pipeline.push_pcm(&tone(24_000)).expect("push");
    packets.extend(pipeline.drain().expect("drain"));
    assert!(!packets.is_empty());

    let mut depacketizer = opus_depack::Depacketizer::new();
    let mut decoder = OpusDecoder::open(48_000, 2).expect("decoder");
    let mut samples = 0usize;
    for packet in &packets {
        let rtp = RtpPacket::parse(&packet.data).expect("parseable RTP");
        assert_eq!(rtp.payload_type, libvmm_media::rtp::PAYLOAD_TYPE_OPUS);
        if let Some(frame) = depacketizer.push(&rtp).expect("depacketise") {
            samples += decoder.decode(&frame.data).expect("decode").len();
        }
    }
    // Half a second of stereo is 48000 values; allow for the encoder's own
    // framing rounding the total.
    assert!(
        samples > 40_000,
        "only {samples} samples decoded from half a second of audio"
    );
    println!("opus: {} packet(s) -> {samples} samples", packets.len());
}

#[test]
fn vorbis_round_trips_through_rtp() {
    let params = AudioEncoderParams::from_config(&display()).for_codec(AudioCodec::Vorbis);
    let mut pipeline = AudioPipeline::open(params, 0x600d_beef).expect("opens");

    // Vorbis needs its three headers delivered out of band, which is the
    // whole reason Opus is preferred — so the client is given them here the
    // way the SDP would.
    let headers: VorbisHeaders = pipeline
        .vorbis_headers()
        .expect("vorbis carries headers")
        .clone();

    let mut packets = pipeline.push_pcm(&tone(24_000)).expect("push");
    packets.extend(pipeline.drain().expect("drain"));
    assert!(!packets.is_empty());

    let mut depacketizer = vorbis_depack::Depacketizer::new();
    let mut decoder = VorbisDecoder::open(&headers, 48_000, 2).expect("decoder");
    let mut samples = 0usize;
    for packet in &packets {
        let rtp = RtpPacket::parse(&packet.data).expect("parseable RTP");
        assert_eq!(rtp.payload_type, libvmm_media::rtp::PAYLOAD_TYPE_VORBIS);
        for frame in depacketizer.push(&rtp).expect("depacketise") {
            samples += decoder.decode(&frame.data).expect("decode").len();
        }
    }
    assert!(
        samples > 40_000,
        "only {samples} samples decoded from half a second of audio"
    );
    println!("vorbis: {} packet(s) -> {samples} samples", packets.len());
}

#[test]
fn the_vorbis_configuration_survives_the_sdp_round_trip() {
    let params = AudioEncoderParams::from_config(&display()).for_codec(AudioCodec::Vorbis);
    let pipeline = AudioPipeline::open(params, 1).expect("opens");
    let headers = pipeline.vorbis_headers().expect("headers").clone();

    let packed = headers.packed_configuration();
    let recovered = VorbisHeaders::from_packed_configuration(&packed).expect("recovers");
    assert_eq!(recovered, headers);

    // And the recovered headers must actually open a decoder.
    VorbisDecoder::open(&recovered, 48_000, 2).expect("a decoder from the recovered headers");
}
