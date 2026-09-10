//! The client's decode path (§7), driven with real encoded video.
//!
//! The sink is fed the same shape the RTSP demux produces, so what is
//! exercised here is exactly what a live session would hit.

use libvmm_media::depacketize::{AudioPacket, CodedUnit};
use vmm_codec_sys::{
    Accelerator, EncodedFrame, EncoderConfig, PackedFormat, Scaler, VideoCodec, VideoEncoder,
    Yuv420Frame,
};
use vmm_console_client::decode::{DecodingSink, LatestFrame, RawBgraWriter};
use vmm_console_client::rtsp_client::MediaSink;

const WIDTH: u32 = 160;
const HEIGHT: u32 = 120;

fn encoder() -> VideoEncoder {
    VideoEncoder::open(
        EncoderConfig {
            codec: VideoCodec::H264,
            width: WIDTH,
            height: HEIGHT,
            framerate: 30,
            target_kbps: 1800,
            max_kbps: 2000,
            vbv_buffer_bits: 2_000_000,
            gop_length: 60,
            accelerator: Accelerator::Software,
        },
        None,
    )
    .expect("libx264 must be available")
}

fn frame(index: u32) -> Yuv420Frame {
    let mut f = Yuv420Frame::black(WIDTH, HEIGHT).expect("geometry");
    for row in 0..HEIGHT as usize {
        for col in 0..WIDTH as usize {
            f.y[row * f.y_stride + col] = ((row + col + index as usize * 3) % 200) as u8;
        }
    }
    f.pts = index as i64;
    f
}

fn access_unit(encoded: &EncodedFrame) -> CodedUnit {
    CodedUnit {
        data: encoded.data.clone(),
        timestamp: 0,
        keyframe: encoded.keyframe,
    }
}

#[test]
fn a_decoded_frame_reaches_the_handler_as_bgra() {
    let mut encoder = encoder();
    let encoded = encoder
        .encode(&frame(0))
        .expect("encode")
        .expect("first frame");

    let mut sink =
        DecodingSink::new(VideoCodec::H264, LatestFrame::default(), None).expect("sink opens");
    sink.on_video(&access_unit(&encoded)).expect("on_video");

    assert_eq!(sink.frames_decoded, 1);
    assert_eq!(sink.geometry(), Some((WIDTH, HEIGHT)));

    let held = sink.handler().frame.as_ref().expect("a frame was kept");
    assert_eq!((held.width, held.height), (WIDTH, HEIGHT));
    assert_eq!(held.format, PackedFormat::Bgra);
    assert_eq!(held.pixels.len(), (WIDTH * HEIGHT * 4) as usize);

    // Alpha is opaque and the image is not a flat field.
    assert!(held.pixels.chunks_exact(4).all(|p| p[3] == 0xff));
    let first = held.pixels[0];
    assert!(
        held.pixels.chunks_exact(4).any(|p| p[0] != first),
        "the decoded surface is uniform"
    );
}

#[test]
fn the_sink_waits_for_the_first_keyframe() {
    // A client joining mid-GOP receives inter frames it cannot decode. The
    // sink must skip them rather than feed the decoder garbage.
    let mut sink =
        DecodingSink::new(VideoCodec::H264, LatestFrame::default(), None).expect("sink opens");
    let orphan = CodedUnit {
        data: vec![0, 0, 0, 1, 0x41, 0x9a, 0x00, 0x10],
        timestamp: 0,
        keyframe: false,
    };
    sink.on_video(&orphan).expect("must not error");

    assert_eq!(sink.video_units, 0, "a pre-keyframe unit must be skipped");
    assert_eq!(sink.frames_decoded, 0);
    assert!(sink.handler().frame.is_none());
}

#[test]
fn a_sequence_decodes_every_frame() {
    let mut encoder = encoder();
    let mut sink =
        DecodingSink::new(VideoCodec::H264, LatestFrame::default(), None).expect("sink opens");

    let count = 20u32;
    for index in 0..count {
        if let Some(encoded) = encoder.encode(&frame(index)).expect("encode") {
            sink.on_video(&access_unit(&encoded)).expect("on_video");
        }
    }
    for encoded in encoder.drain().expect("drain") {
        sink.on_video(&access_unit(&encoded)).expect("on_video");
    }

    assert_eq!(sink.frames_decoded, u64::from(count));
    assert_eq!(sink.handler().count, u64::from(count));
    assert_eq!(sink.undecodable_units(), 0);
    assert!(sink.last_error.is_none());
}

#[test]
fn a_corrupt_access_unit_does_not_end_the_session() {
    let mut encoder = encoder();
    let mut sink =
        DecodingSink::new(VideoCodec::H264, LatestFrame::default(), None).expect("sink opens");

    // A good keyframe, then rubbish, then another good frame.
    let first = encoder.encode(&frame(0)).expect("encode").expect("frame");
    sink.on_video(&access_unit(&first)).expect("on_video");

    let corrupt = CodedUnit {
        data: vec![0, 0, 0, 1, 0x65, 0xff, 0xff, 0xff, 0xff, 0xff],
        timestamp: 0,
        keyframe: true,
    };
    sink.on_video(&corrupt)
        .expect("a corrupt unit must not error");

    let next = encoder.encode(&frame(1)).expect("encode").expect("frame");
    sink.on_video(&access_unit(&next)).expect("on_video");

    assert!(
        sink.frames_decoded >= 2,
        "decoding must recover after a corrupt unit, got {}",
        sink.frames_decoded
    );
}

#[test]
fn a_snapshot_is_written_as_a_readable_ppm() {
    let mut encoder = encoder();
    let encoded = encoder.encode(&frame(0)).expect("encode").expect("frame");
    let mut sink =
        DecodingSink::new(VideoCodec::H264, LatestFrame::default(), None).expect("sink opens");
    sink.on_video(&access_unit(&encoded)).expect("on_video");

    let dir = std::env::temp_dir().join(format!("vmm-decode-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("snapshot.ppm");
    sink.handler().write_ppm(&path).expect("write_ppm");

    let bytes = std::fs::read(&path).expect("read back");
    let header = format!("P6\n{WIDTH} {HEIGHT}\n255\n");
    assert!(
        bytes.starts_with(header.as_bytes()),
        "unexpected PPM header: {:?}",
        String::from_utf8_lossy(&bytes[..20.min(bytes.len())])
    );
    assert_eq!(
        bytes.len(),
        header.len() + (WIDTH * HEIGHT * 3) as usize,
        "a P6 PPM is three bytes per pixel after the header"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn raw_frames_are_written_without_stride_padding() {
    let mut encoder = encoder();
    let dir = std::env::temp_dir().join(format!("vmm-raw-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("frames.bgra");

    {
        let writer = RawBgraWriter::create(&path).expect("create");
        let mut sink = DecodingSink::new(VideoCodec::H264, writer, None).expect("sink opens");
        for index in 0..3 {
            if let Some(encoded) = encoder.encode(&frame(index)).expect("encode") {
                sink.on_video(&access_unit(&encoded)).expect("on_video");
            }
        }
        assert_eq!(sink.handler().frames, 3);
    }

    let bytes = std::fs::metadata(&path).expect("stat").len();
    assert_eq!(
        bytes,
        u64::from(WIDTH * HEIGHT * 4) * 3,
        "each frame must be exactly width * height * 4 bytes"
    );

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn audio_packets_are_length_prefixed() {
    let dir = std::env::temp_dir().join(format!("vmm-audio-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let path = dir.join("audio.vorbis");

    {
        let mut sink = DecodingSink::new(VideoCodec::H264, LatestFrame::default(), Some(&path))
            .expect("sink opens");
        for length in [10usize, 300, 7] {
            sink.on_audio(&AudioPacket {
                data: vec![0xaa; length],
                timestamp: 0,
                configuration: false,
            })
            .expect("on_audio");
        }
        assert_eq!(sink.audio_packets, 3);
        assert_eq!(sink.audio_bytes, 317);
    }

    let bytes = std::fs::read(&path).expect("read back");
    let mut at = 0usize;
    let mut lengths = Vec::new();
    while at + 4 <= bytes.len() {
        let n =
            u32::from_be_bytes([bytes[at], bytes[at + 1], bytes[at + 2], bytes[at + 3]]) as usize;
        at += 4 + n;
        lengths.push(n);
    }
    assert_eq!(lengths, vec![10, 300, 7]);
    assert_eq!(at, bytes.len(), "the framing must consume the whole file");

    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn the_full_client_path_recovers_the_original_picture() {
    // Convert a BGRA surface to I420, encode, decode, convert back, and
    // check the picture survived — the client's half of the §7 round trip.
    let mut pixels = vec![0u8; (WIDTH * HEIGHT * 4) as usize];
    for row in 0..HEIGHT as usize {
        for col in 0..WIDTH as usize {
            let at = (row * WIDTH as usize + col) * 4;
            pixels[at] = 40; // B
            pixels[at + 1] = (row * 2 % 256) as u8; // G
            pixels[at + 2] = (col % 256) as u8; // R
            pixels[at + 3] = 0xff;
        }
    }
    let original = vmm_codec_sys::PackedFrame::packed(WIDTH, HEIGHT, pixels, PackedFormat::Bgra)
        .expect("geometry");

    let mut to_yuv = Scaler::to_i420(WIDTH, HEIGHT, PackedFormat::Bgra).expect("scaler");
    let mut yuv = Yuv420Frame::black(WIDTH, HEIGHT).expect("geometry");
    to_yuv
        .convert_to_i420(&original, &mut yuv)
        .expect("convert");

    let mut encoder = encoder();
    let encoded = encoder.encode(&yuv).expect("encode").expect("frame");

    let mut sink =
        DecodingSink::new(VideoCodec::H264, LatestFrame::default(), None).expect("sink opens");
    sink.on_video(&access_unit(&encoded)).expect("on_video");
    let recovered = sink.handler().frame.as_ref().expect("a frame");

    // Compare the green channel, which carries the row gradient.
    let mut total = 0u64;
    for row in 0..HEIGHT as usize {
        for col in 0..WIDTH as usize {
            let at = row * recovered.stride + col * 4;
            let expected = (row * 2 % 256) as i32;
            total += expected.abs_diff(recovered.pixels[at + 1] as i32) as u64;
        }
    }
    let mad = total as f64 / f64::from(WIDTH * HEIGHT);
    assert!(mad < 12.0, "recovered picture differs by {mad:.2} levels");
}
