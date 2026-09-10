//! The §7.1 pipeline end to end: scanout in, RTP out, picture back.
//!
//! This is the test that would catch a break anywhere along the chain —
//! colour conversion, encode, packetisation, depacketisation, decode — since
//! only a correct pipeline gets the original image back.

use libvmm_config::{Display, MachineConfig};
use libvmm_media::depacketize::h264 as h264_depack;
use libvmm_media::RtpPacket;
use libvmm_media::{AudioEncoderParams, AudioPipeline, VideoEncoderParams, VideoPipeline};
use vmm_codec_sys::{PackedFormat, PackedFrame, VideoCodec, VideoDecoder};

/// The §11 reference machine, so the pipeline is exercised against exactly
/// the configuration the specification describes rather than a hand-built
/// one that could drift from it.
const REFERENCE: &str = include_str!("../../../config/reference-vm.toml");

const WIDTH: u32 = 320;
const HEIGHT: u32 = 240;

/// The reference display, scaled down so the tests stay quick. Everything
/// that matters to the pipeline — codec, rate control, the 2000 kbps
/// ceiling, the audio format — comes from the reference config unchanged.
fn display() -> Display {
    let config = MachineConfig::from_toml_str(REFERENCE).expect("the reference config must load");
    let mut display = config.display;
    display.width = WIDTH;
    display.height = HEIGHT;
    display
}

/// A BGRA scanout carrying a recognisable pattern.
fn scanout(index: u32) -> PackedFrame {
    let mut pixels = vec![0u8; (WIDTH * HEIGHT * 4) as usize];
    for row in 0..HEIGHT as usize {
        for col in 0..WIDTH as usize {
            let at = (row * WIDTH as usize + col) * 4;
            pixels[at] = (col / 2) as u8; // B
            pixels[at + 1] = (row / 2) as u8; // G
            pixels[at + 2] = ((index * 8) % 256) as u8; // R
            pixels[at + 3] = 0xff;
        }
    }
    PackedFrame::packed(WIDTH, HEIGHT, pixels, PackedFormat::Bgra).expect("valid geometry")
}

#[test]
fn a_scanout_travels_all_the_way_to_a_decoded_picture() {
    let display = display();
    let params = VideoEncoderParams::from_config(&display).expect("valid encoder config");
    let mut pipeline = VideoPipeline::open(params, 0x1234_5678, None).expect("pipeline opens");

    // Whatever the host offers, the pipeline must have produced an encoder.
    println!("encoder backend: {}", pipeline.acceleration());

    // A hardware encoder holds the first frame in its pipeline, so push
    // until something comes out rather than assuming the software path's
    // zero latency.
    let mut packets = Vec::new();
    for index in 0..8 {
        packets = pipeline
            .push_scanout(&scanout(index), index as i64)
            .expect("push");
        if !packets.is_empty() {
            break;
        }
    }
    assert!(
        !packets.is_empty(),
        "no packets after 8 frames on {}",
        pipeline.acceleration()
    );
    assert!(
        packets.last().is_some_and(|p| p.marker),
        "the last packet of an access unit carries the marker"
    );

    // Depacketise, then decode.
    let mut depacketizer = h264_depack::Depacketizer::new();
    let mut decoder = VideoDecoder::open(VideoCodec::H264).expect("decoder");
    let mut decoded = Vec::new();

    for packet in &packets {
        let rtp = RtpPacket::parse(&packet.data).expect("parseable RTP");
        if let Some(unit) = depacketizer.push(&rtp).expect("depacketising") {
            assert!(unit.keyframe, "the first access unit must be an IDR");
            decoded.extend(decoder.decode(&unit.data).expect("decoding"));
        }
    }

    assert_eq!(decoded.len(), 1, "one frame in, one frame out");
    let frame = &decoded[0];
    assert_eq!((frame.width, frame.height), (WIDTH, HEIGHT));

    // The luma plane must carry the diagonal gradient the scanout had, not a
    // flat field — which is what any break in the chain would produce.
    let first = frame.y[0];
    assert!(
        frame.y.iter().any(|&v| v != first),
        "the decoded luma plane is uniform"
    );
}

#[test]
fn a_sequence_of_frames_keeps_the_rtp_stream_contiguous() {
    let display = display();
    let params = VideoEncoderParams::from_config(&display).expect("valid config");
    let mut pipeline = VideoPipeline::open(params, 0x1234_5678, None).expect("pipeline opens");

    let mut sequences = Vec::new();
    let mut timestamps = Vec::new();
    for index in 0..15 {
        for packet in pipeline
            .push_scanout(&scanout(index), index as i64)
            .expect("push")
        {
            let rtp = RtpPacket::parse(&packet.data).expect("parseable RTP");
            sequences.push(rtp.sequence);
            timestamps.push(rtp.timestamp);
        }
    }
    for packet in pipeline.drain().expect("drain") {
        let rtp = RtpPacket::parse(&packet.data).expect("parseable RTP");
        sequences.push(rtp.sequence);
        timestamps.push(rtp.timestamp);
    }

    assert!(!sequences.is_empty());
    for pair in sequences.windows(2) {
        assert_eq!(pair[1], pair[0].wrapping_add(1), "a gap in {sequences:?}");
    }
    // Timestamps must be non-decreasing and advance by 3000 ticks per frame
    // at 30 fps on a 90 kHz clock.
    for pair in timestamps.windows(2) {
        assert!(pair[1] >= pair[0], "the media clock went backwards");
    }
    assert!(
        timestamps.iter().any(|&t| t >= 3000),
        "the media clock never advanced: {timestamps:?}"
    );
}

#[test]
fn the_pipeline_refuses_a_scanout_of_the_wrong_size() {
    let display = display();
    let params = VideoEncoderParams::from_config(&display).expect("valid config");
    let mut pipeline = VideoPipeline::open(params, 1, None).expect("pipeline opens");

    let wrong =
        PackedFrame::packed(64, 64, vec![0u8; 64 * 64 * 4], PackedFormat::Bgra).expect("geometry");
    let error = pipeline
        .push_scanout(&wrong, 0)
        .expect_err("a mismatched scanout must be refused");
    assert_eq!(error.code(), 5010, "capture geometry mismatch is 5010");
}

#[test]
fn a_configuration_above_the_hard_ceiling_fails_encoder_init() {
    let mut display = display();
    display.encoder.max_bitrate_kbps = 4000;
    let error =
        VideoEncoderParams::from_config(&display).expect_err("§7.1 caps max_bitrate_kbps at 2000");
    assert_eq!(error.code(), 5001);
}

#[test]
fn the_audio_pipeline_produces_rtp_and_a_stable_ident() {
    let display = display();
    let params = AudioEncoderParams::from_config(&display);
    let mut pipeline = AudioPipeline::open(params, 0xaabb_ccdd).expect("audio pipeline opens");

    // Opus is the default, and needs no out-of-band configuration at all —
    // which is the point of preferring it.
    assert_eq!(pipeline.codec(), vmm_codec_sys::AudioCodec::Opus);
    assert!(pipeline.ident().is_none());
    assert!(pipeline.vorbis_headers().is_none());

    // A tenth of a second of tone.
    let frames = 4800usize;
    let mut pcm = Vec::with_capacity(frames * 2);
    for n in 0..frames {
        let t = n as f32 / 48_000.0;
        let sample = ((t * 440.0 * std::f32::consts::TAU).sin() * 10_000.0) as i16;
        pcm.push(sample);
        pcm.push(sample);
    }

    let mut packets = pipeline.push_pcm(&pcm).expect("push");
    packets.extend(pipeline.drain().expect("drain"));
    assert!(!packets.is_empty(), "audio produced no RTP packets");

    for packet in &packets {
        let rtp = RtpPacket::parse(&packet.data).expect("parseable RTP");
        assert_eq!(rtp.payload_type, libvmm_media::rtp::PAYLOAD_TYPE_OPUS);
        // RFC 7587: the payload is the Opus packet, with no header of its
        // own, so the first byte is the Opus TOC rather than a descriptor.
        assert!(!rtp.payload.is_empty());
    }

    let (samples_in, packets_out, bytes_out) = pipeline.stats();
    assert_eq!(samples_in, frames as u64);
    assert!(packets_out > 0);
    assert!(bytes_out > 0);
}

#[test]
fn the_same_vorbis_configuration_yields_the_same_ident() {
    // Vorbis' RFC 5215 ident must identify the codebooks, so two encoders
    // opened the same way have to agree — otherwise a reconnecting client
    // would think the configuration changed.
    let display = display();
    let params =
        AudioEncoderParams::from_config(&display).for_codec(vmm_codec_sys::AudioCodec::Vorbis);
    let a = AudioPipeline::open(params, 1).expect("opens");
    let b = AudioPipeline::open(params, 2).expect("opens");
    assert_eq!(a.codec(), vmm_codec_sys::AudioCodec::Vorbis);
    assert!(a.ident().is_some());
    assert_eq!(
        a.ident(),
        b.ident(),
        "identical codebooks must produce an identical ident"
    );
    let headers = a.vorbis_headers().expect("vorbis carries headers");
    assert_eq!(headers.identification[0], 1);
    assert_eq!(headers.comment[0], 3);
    assert_eq!(headers.setup[0], 5);
}

#[test]
fn the_audio_pipeline_refuses_a_partial_frame() {
    let display = display();
    let params = AudioEncoderParams::from_config(&display);
    let mut pipeline = AudioPipeline::open(params, 1).expect("opens");
    let error = pipeline
        .push_pcm(&[0i16; 3])
        .expect_err("an odd sample count is not whole stereo frames");
    assert_eq!(error.code(), 5010);
}

#[test]
fn video_stats_account_for_every_frame() {
    let display = display();
    let params = VideoEncoderParams::from_config(&display).expect("valid config");
    let mut pipeline = VideoPipeline::open(params, 1, None).expect("opens");

    for index in 0..10 {
        pipeline
            .push_scanout(&scanout(index), index as i64)
            .expect("push");
    }
    pipeline.drain().expect("drain");

    let (frames_in, frames_out, bytes_out, breaches) = pipeline.stats();
    assert_eq!(frames_in, 10);
    assert_eq!(frames_out, 10, "zerolatency must not hold frames back");
    assert!(bytes_out > 0);
    assert_eq!(
        breaches, 0,
        "the VBV must keep every frame under the §7.1 ceiling"
    );
}

// ---------------------------------------------------------------------------
// §7.1: a keyframe at least every two seconds
// ---------------------------------------------------------------------------

/// Decode the access units a run produced and report which were keyframes.
fn keyframe_positions(packets: &[libvmm_media::Packet]) -> Vec<usize> {
    let mut depacketizer = h264_depack::Depacketizer::new();
    let mut positions = Vec::new();
    let mut index = 0;
    for packet in packets {
        let rtp = RtpPacket::parse(&packet.data).expect("parseable RTP");
        if let Some(unit) = depacketizer.push(&rtp).expect("depacketising") {
            if unit.keyframe {
                positions.push(index);
            }
            index += 1;
        }
    }
    positions
}

#[test]
fn the_gop_delivers_a_keyframe_every_two_seconds_at_full_rate() {
    let display = display();
    let params = VideoEncoderParams::from_config(&display).expect("valid config");

    // §7.1 sizes the GOP at two seconds of frames.
    assert_eq!(
        params.gop_length,
        params.framerate * 2,
        "the GOP must be two seconds of frames"
    );

    let mut pipeline = VideoPipeline::open(params, 1, None).expect("opens");

    // Four seconds of frames arriving exactly on schedule, so the clock
    // never has to intervene.
    let start = std::time::Instant::now();
    let mut packets = Vec::new();
    let frames = params.framerate * 4;
    for index in 0..frames {
        let at = start
            + std::time::Duration::from_secs_f64(f64::from(index) / f64::from(params.framerate));
        packets.extend(
            pipeline
                .push_scanout_at(&scanout(index), index as i64, at)
                .expect("push"),
        );
    }
    packets.extend(pipeline.drain().expect("drain"));

    let keyframes = keyframe_positions(&packets);
    assert!(
        keyframes.len() >= 2,
        "four seconds must contain at least two keyframes, saw {keyframes:?}"
    );
    assert_eq!(keyframes[0], 0, "the stream must open on a keyframe");

    // No gap between keyframes may exceed two seconds of frames.
    let limit = (params.framerate * 2) as usize;
    for pair in keyframes.windows(2) {
        assert!(
            pair[1] - pair[0] <= limit,
            "{} frames between keyframes exceeds the {limit}-frame limit: {keyframes:?}",
            pair[1] - pair[0]
        );
    }
    assert_eq!(
        pipeline.forced_keyframes(),
        0,
        "at the full frame rate the GOP alone should suffice"
    );
}

#[test]
fn a_slow_guest_still_gets_a_keyframe_every_two_seconds() {
    // The case a frame-counted GOP cannot handle: the guest renders at 5 fps
    // on a machine configured for 30, so the 60-frame GOP would otherwise
    // stretch to twelve seconds between keyframes.
    let display = display();
    let params = VideoEncoderParams::from_config(&display).expect("valid config");
    let mut pipeline = VideoPipeline::open(params, 1, None).expect("opens");

    let start = std::time::Instant::now();
    let slow_fps = 5u32;
    let seconds = 8u32;
    let mut packets = Vec::new();
    let mut emitted_at = Vec::new();

    for index in 0..(slow_fps * seconds) {
        let elapsed = std::time::Duration::from_secs_f64(f64::from(index) / f64::from(slow_fps));
        let produced = pipeline
            .push_scanout_at(&scanout(index), index as i64, start + elapsed)
            .expect("push");
        if !produced.is_empty() {
            emitted_at.push(elapsed);
        }
        packets.extend(produced);
    }

    let keyframes = keyframe_positions(&packets);
    assert!(
        keyframes.len() >= 4,
        "eight seconds at 5 fps needs at least four keyframes, saw {keyframes:?}"
    );

    // Check the actual wall-clock spacing, which is what §7.1 constrains.
    let mut previous = std::time::Duration::ZERO;
    for &position in &keyframes {
        let at = emitted_at[position];
        let gap = at - previous;
        assert!(
            gap <= libvmm_media::pipeline::MAX_KEYFRAME_INTERVAL
                + std::time::Duration::from_millis(400),
            "{gap:?} between keyframes exceeds the two-second requirement"
        );
        previous = at;
    }

    assert!(
        pipeline.forced_keyframes() > 0,
        "the clock should have forced keyframes the GOP would not have produced"
    );
}

#[test]
fn a_gop_longer_than_two_seconds_is_refused() {
    let display = display();
    let mut params = VideoEncoderParams::from_config(&display).expect("valid config");
    // Three seconds of frames.
    params.gop_length = params.framerate * 3;
    let error =
        VideoPipeline::open(params, 1, None).expect_err("a three-second GOP cannot satisfy §7.1");
    assert_eq!(error.code(), 5001);
    assert!(
        error
            .to_string()
            .contains("keyframe at least every 2 seconds"),
        "unexpected error: {error}"
    );
}
