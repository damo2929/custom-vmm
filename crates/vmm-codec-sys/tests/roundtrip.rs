//! Encode/decode round trips and Vorbis encode, against the real libraries.

use vmm_codec_sys::{
    Accelerator, CodecError, EncoderConfig, PackedFormat, PackedFrame, Scaler, VideoCodec,
    VideoDecoder, VideoEncoder, VorbisEncoder, Yuv420Frame,
};

const WIDTH: u32 = 320;
const HEIGHT: u32 = 240;

fn config() -> EncoderConfig {
    EncoderConfig {
        codec: VideoCodec::H264,
        width: WIDTH,
        height: HEIGHT,
        framerate: 30,
        target_kbps: 1800,
        max_kbps: 2000,
        vbv_buffer_bits: 2_000_000,
        gop_length: 30,
        accelerator: Accelerator::Software,
    }
}

/// A smooth gradient plus a moving block: smooth enough that H.264 codes it
/// accurately, structured enough that a decode failure is obvious.
fn test_frame(index: u32) -> Yuv420Frame {
    let mut frame = Yuv420Frame::black(WIDTH, HEIGHT).expect("geometry is valid");
    for row in 0..HEIGHT as usize {
        for col in 0..WIDTH as usize {
            frame.y[row * frame.y_stride + col] = ((row + col) / 2) as u8;
        }
    }
    let offset = (index as usize * 4) % (WIDTH as usize / 2);
    for row in 40..80 {
        for col in offset..offset + 40 {
            frame.y[row * frame.y_stride + col] = 230;
        }
    }
    frame.pts = index as i64;
    frame
}

/// Mean absolute difference between two luma planes.
fn luma_mad(a: &Yuv420Frame, b: &Yuv420Frame) -> f64 {
    let mut total = 0u64;
    let mut count = 0u64;
    for row in 0..a.height as usize {
        for col in 0..a.width as usize {
            let x = a.y[row * a.y_stride + col] as i32;
            let y = b.y[row * b.y_stride + col] as i32;
            total += x.abs_diff(y) as u64;
            count += 1;
        }
    }
    total as f64 / count as f64
}

#[test]
fn an_encoded_frame_decodes_back_to_the_original() {
    let mut encoder = VideoEncoder::open(config(), None).expect("libx264 available");
    let mut decoder =
        VideoDecoder::open(VideoCodec::H264).expect("libavcodec has an H.264 decoder");

    let original = test_frame(0);
    let encoded = encoder
        .encode(&original)
        .expect("encoding")
        .expect("zerolatency emits the first frame immediately");

    let decoded = decoder.decode(&encoded.data).expect("decoding");
    assert_eq!(decoded.len(), 1, "one access unit must yield one frame");

    let decoded = &decoded[0];
    assert_eq!((decoded.width, decoded.height), (WIDTH, HEIGHT));
    assert_eq!(decoder.geometry(), Some((WIDTH, HEIGHT)));

    // H.264 at this bitrate is lossy but close; a broken pipeline would be
    // off by far more than a few levels.
    let mad = luma_mad(&original, decoded);
    assert!(
        mad < 8.0,
        "decoded frame differs from the original by {mad:.2} levels on average"
    );
}

#[test]
fn a_sequence_round_trips_frame_for_frame() {
    let mut encoder = VideoEncoder::open(config(), None).expect("libx264 available");
    let mut decoder = VideoDecoder::open(VideoCodec::H264).expect("decoder");

    let originals: Vec<Yuv420Frame> = (0..20).map(test_frame).collect();
    let mut decoded = Vec::new();

    for original in &originals {
        if let Some(packet) = encoder.encode(original).expect("encoding") {
            decoded.extend(decoder.decode(&packet.data).expect("decoding"));
        }
    }
    for packet in encoder.drain().expect("draining the encoder") {
        decoded.extend(decoder.decode(&packet.data).expect("decoding"));
    }
    decoded.extend(decoder.finish().expect("flushing the decoder"));

    assert_eq!(
        decoded.len(),
        originals.len(),
        "every submitted frame must come back out"
    );

    for (index, (original, result)) in originals.iter().zip(&decoded).enumerate() {
        let mad = luma_mad(original, result);
        assert!(mad < 8.0, "frame {index} differs by {mad:.2} levels");
    }
}

#[test]
fn the_decoder_tolerates_a_stream_that_starts_without_parameter_sets() {
    let mut decoder = VideoDecoder::open(VideoCodec::H264).expect("decoder");
    // A lone non-IDR slice with no SPS/PPS: the decoder cannot produce a
    // frame, but it must not error, because a client joining mid-stream
    // sees exactly this until the next IDR arrives.
    let orphan_slice = [0x00, 0x00, 0x00, 0x01, 0x41, 0x9a, 0x00, 0x10];
    let frames = decoder.decode(&orphan_slice).expect("must not error");
    assert!(frames.is_empty());
}

#[test]
fn an_empty_access_unit_is_a_no_op() {
    let mut decoder = VideoDecoder::open(VideoCodec::H264).expect("decoder");
    assert!(decoder.decode(&[]).expect("must not error").is_empty());
}

#[test]
fn the_full_capture_path_round_trips_to_a_displayable_surface() {
    // The whole §7.1 chain: BGRA scanout, convert, encode, decode, convert
    // back to BGRA for the client's surface.
    let mut pixels = vec![0u8; (WIDTH * HEIGHT * 4) as usize];
    for row in 0..HEIGHT as usize {
        for col in 0..WIDTH as usize {
            let at = (row * WIDTH as usize + col) * 4;
            pixels[at] = (col / 2) as u8; // B
            pixels[at + 1] = (row / 2) as u8; // G
            pixels[at + 2] = 128; // R
            pixels[at + 3] = 0xff; // A
        }
    }
    let scanout = PackedFrame::packed(WIDTH, HEIGHT, pixels, PackedFormat::Bgra).expect("geometry");

    let mut to_yuv = Scaler::to_i420(WIDTH, HEIGHT, PackedFormat::Bgra).expect("scaler");
    let mut yuv = Yuv420Frame::black(WIDTH, HEIGHT).expect("geometry");
    to_yuv.convert_to_i420(&scanout, &mut yuv).expect("convert");

    let mut encoder = VideoEncoder::open(config(), None).expect("encoder");
    let mut decoder = VideoDecoder::open(VideoCodec::H264).expect("decoder");
    let packet = encoder.encode(&yuv).expect("encode").expect("first frame");
    let decoded = decoder.decode(&packet.data).expect("decode");
    assert_eq!(decoded.len(), 1);

    let mut to_bgra = Scaler::from_i420(WIDTH, HEIGHT, PackedFormat::Bgra).expect("scaler");
    let mut surface = PackedFrame::packed(
        WIDTH,
        HEIGHT,
        vec![0u8; (WIDTH * HEIGHT * 4) as usize],
        PackedFormat::Bgra,
    )
    .expect("geometry");
    to_bgra
        .convert_from_i420(&decoded[0], &mut surface)
        .expect("convert back");

    // The recovered surface must resemble the original scanout. Compare the
    // green channel, which carries the row gradient.
    let mut total = 0u64;
    let mut count = 0u64;
    for row in 0..HEIGHT as usize {
        for col in 0..WIDTH as usize {
            let at = (row * WIDTH as usize + col) * 4;
            let expected = (row / 2) as i32;
            let actual = surface.pixels[at + 1] as i32;
            total += expected.abs_diff(actual) as u64;
            count += 1;
        }
    }
    let mad = total as f64 / count as f64;
    assert!(mad < 12.0, "recovered surface differs by {mad:.2} levels");
}

#[test]
fn a_scaler_refuses_geometry_it_was_not_built_for() {
    let mut scaler = Scaler::to_i420(WIDTH, HEIGHT, PackedFormat::Bgra).expect("scaler");
    let wrong =
        PackedFrame::packed(64, 64, vec![0u8; 64 * 64 * 4], PackedFormat::Bgra).expect("geometry");
    let mut dst = Yuv420Frame::black(WIDTH, HEIGHT).expect("geometry");
    let error = scaler
        .convert_to_i420(&wrong, &mut dst)
        .expect_err("mismatched geometry must be refused");
    assert!(matches!(error, CodecError::Invalid { .. }));
}

#[test]
fn vorbis_produces_headers_and_packets() {
    let mut encoder = VorbisEncoder::open_default(128).expect("libvorbisenc available");
    assert_eq!(encoder.sample_rate(), 48_000);
    assert_eq!(encoder.channels(), 2);

    let headers = encoder.headers().expect("header packets");
    // Every Vorbis header packet begins with its type byte followed by the
    // "vorbis" signature (§4.2 of the Vorbis I specification).
    assert_eq!(headers.identification[0], 1);
    assert_eq!(&headers.identification[1..7], b"vorbis");
    assert_eq!(headers.comment[0], 3);
    assert_eq!(&headers.comment[1..7], b"vorbis");
    assert_eq!(headers.setup[0], 5);
    assert_eq!(&headers.setup[1..7], b"vorbis");

    // A second of a 440 Hz tone, interleaved stereo.
    let frames = 48_000usize;
    let mut pcm = Vec::with_capacity(frames * 2);
    for n in 0..frames {
        let t = n as f32 / 48_000.0;
        let sample = ((t * 440.0 * std::f32::consts::TAU).sin() * 12_000.0) as i16;
        pcm.push(sample);
        pcm.push(sample);
    }

    let mut packets = encoder.encode(&pcm).expect("encoding");
    packets.extend(encoder.finish().expect("draining"));

    assert!(!packets.is_empty(), "a second of audio produced no packets");
    let total: usize = packets.iter().map(|p| p.len()).sum();
    // 128 kbps for one second is ~16 kB. Allow a wide band: managed-bitrate
    // Vorbis undershoots badly on a pure tone, which is exactly right.
    assert!(
        (1_000..64_000).contains(&total),
        "{total} bytes for one second at 128 kbps is outside any plausible range"
    );
}

#[test]
fn the_vorbis_packed_configuration_is_xiph_laced() {
    let mut encoder = VorbisEncoder::open_default(128).expect("encoder");
    let headers = encoder.headers().expect("headers");
    let packed = headers.packed_configuration();

    // Re-read the two lacing values and confirm they address the bodies.
    let mut at = 0usize;
    let mut lengths = [0usize; 2];
    for length in &mut lengths {
        loop {
            let byte = packed[at];
            at += 1;
            *length += byte as usize;
            if byte != 255 {
                break;
            }
        }
    }
    assert_eq!(lengths[0], headers.identification.len());
    assert_eq!(lengths[1], headers.comment.len());
    assert_eq!(
        packed.len() - at,
        headers.identification.len() + headers.comment.len() + headers.setup.len()
    );
    assert_eq!(&packed[at..at + 7], &headers.identification[..7]);
}

#[test]
fn vorbis_rejects_a_partial_frame() {
    let mut encoder = VorbisEncoder::open_default(128).expect("encoder");
    // An odd sample count cannot be a whole number of stereo frames.
    let error = encoder
        .encode(&[0i16; 3])
        .expect_err("a partial frame must be refused");
    assert!(matches!(error, CodecError::Invalid { .. }));
}

#[test]
fn vorbis_rejects_an_unsupported_channel_count() {
    let error = VorbisEncoder::open(48_000, 6, 128).expect_err("§7.1 captures stereo");
    assert!(error.to_string().contains("6 channels"));
}
