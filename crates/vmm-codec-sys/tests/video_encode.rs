//! End-to-end H.264 encode against the real libraries.
//!
//! These are not mocks: they open a real encoder and check the bitstream it
//! produces. The VA-API cases adapt to the host, because a machine with no
//! H.264 encode entrypoint must still pass — exercising the fallback is the
//! point of §7.1's accelerator handling.

use vmm_codec_sys::video::{probe_vaapi, Backend};
use vmm_codec_sys::{
    Accelerator, EncoderConfig, PackedFormat, PackedFrame, Scaler, VideoCodec, VideoEncoder,
    Yuv420Frame,
};

const WIDTH: u32 = 320;
const HEIGHT: u32 = 240;

fn config(accelerator: Accelerator) -> EncoderConfig {
    codec_config(VideoCodec::H264, accelerator)
}

fn codec_config(codec: VideoCodec, accelerator: Accelerator) -> EncoderConfig {
    EncoderConfig {
        codec,
        width: WIDTH,
        height: HEIGHT,
        framerate: 30,
        target_kbps: 1800,
        max_kbps: 2000,
        vbv_buffer_bits: 2_000_000,
        gop_length: 60,
        accelerator,
    }
}

/// A frame with content that changes per index, so the encoder has real
/// residual to code rather than a static image it can trivially skip.
fn moving_frame(index: u32) -> Yuv420Frame {
    let mut frame = Yuv420Frame::black(WIDTH, HEIGHT).expect("geometry is valid");
    for row in 0..HEIGHT as usize {
        for col in 0..WIDTH as usize {
            let value = (row * 3 + col * 5 + index as usize * 17) as u8;
            frame.y[row * frame.y_stride + col] = value;
        }
    }
    for row in 0..HEIGHT as usize / 2 {
        for col in 0..WIDTH as usize / 2 {
            frame.u[row * frame.uv_stride + col] = (col + index as usize) as u8;
            frame.v[row * frame.uv_stride + col] = (row + index as usize) as u8;
        }
    }
    frame.pts = index as i64;
    frame
}

/// Split an Annex-B buffer into (nal_type, length) pairs.
fn annexb_nals(data: &[u8]) -> Vec<(u8, usize)> {
    let mut starts = Vec::new();
    let mut i = 0;
    while i + 3 <= data.len() {
        if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
            starts.push(i + 3);
            i += 3;
        } else if i + 4 <= data.len() && data[i..i + 4] == [0, 0, 0, 1] {
            starts.push(i + 4);
            i += 4;
        } else {
            i += 1;
        }
    }
    let mut out = Vec::new();
    for (n, &start) in starts.iter().enumerate() {
        let end = starts.get(n + 1).copied().unwrap_or(data.len());
        if start < data.len() {
            out.push((data[start] & 0x1f, end - start));
        }
    }
    out
}

#[test]
fn software_encoder_produces_an_idr_with_parameter_sets() {
    let mut encoder =
        VideoEncoder::open(config(Accelerator::Software), None).expect("libx264 must be available");
    assert_eq!(encoder.backend(), Backend::Software);

    let first = encoder
        .encode(&moving_frame(0))
        .expect("encoding the first frame")
        .expect("zerolatency must emit the first frame immediately");

    assert!(first.keyframe, "the first frame must be an IDR");

    let nals = annexb_nals(&first.data);
    let types: Vec<u8> = nals.iter().map(|(t, _)| *t).collect();
    assert!(
        types.contains(&7),
        "expected an SPS (type 7), got {types:?}"
    );
    assert!(types.contains(&8), "expected a PPS (type 8), got {types:?}");
    assert!(
        types.contains(&5),
        "expected an IDR slice (type 5), got {types:?}"
    );
}

#[test]
fn software_encoder_reports_its_simd() {
    let encoder =
        VideoEncoder::open(config(Accelerator::Software), None).expect("libx264 must be available");
    let acceleration = encoder.acceleration();
    assert!(
        acceleration.starts_with("libx264 "),
        "unexpected description: {acceleration}"
    );
    assert!(
        !acceleration.contains("scalar"),
        "libx264 selected no SIMD kernels on a host that should have them: {acceleration}"
    );
}

#[test]
fn a_gop_stays_under_the_hard_bitrate_ceiling() {
    let cfg = config(Accelerator::Software);
    let mut encoder = VideoEncoder::open(cfg, None).expect("libx264 must be available");

    let mut frames = 0u32;
    let mut total_bytes = 0usize;
    for index in 0..90 {
        if let Some(frame) = encoder.encode(&moving_frame(index)).expect("encoding") {
            frames += 1;
            total_bytes += frame.data.len();
        }
    }
    for frame in encoder.drain().expect("draining") {
        frames += 1;
        total_bytes += frame.data.len();
    }

    assert!(frames > 0, "the encoder produced nothing");

    // Average over the run, which is what ABR targets. Instantaneous rate is
    // bounded by the VBV, not by any single frame, so a keyframe legitimately
    // exceeds the average.
    let seconds = frames as f64 / cfg.framerate as f64;
    let average_kbps = (total_bytes as f64 * 8.0 / seconds) / 1000.0;
    assert!(
        average_kbps <= cfg.max_kbps as f64,
        "average {average_kbps:.0} kbps exceeded the {} kbps ceiling over {frames} frames",
        cfg.max_kbps
    );
}

#[test]
fn keyframes_arrive_at_the_configured_gop() {
    let mut cfg = config(Accelerator::Software);
    cfg.gop_length = 15;
    let mut encoder = VideoEncoder::open(cfg, None).expect("libx264 must be available");

    let mut keyframe_positions = Vec::new();
    let mut emitted = 0u32;
    for index in 0..45 {
        if let Some(frame) = encoder.encode(&moving_frame(index)).expect("encoding") {
            if frame.keyframe {
                keyframe_positions.push(emitted);
            }
            emitted += 1;
        }
    }

    assert!(
        keyframe_positions.len() >= 3,
        "expected an IDR every {} frames over 45 frames, saw {keyframe_positions:?}",
        cfg.gop_length
    );
    assert_eq!(keyframe_positions[0], 0, "the stream must open on an IDR");
}

#[test]
fn odd_dimensions_are_rejected_before_the_encoder_sees_them() {
    let mut cfg = config(Accelerator::Software);
    cfg.width = 321;
    let error = VideoEncoder::open(cfg, None).expect_err("4:2:0 cannot encode an odd width");
    assert!(
        error.to_string().contains("even dimensions"),
        "unexpected error: {error}"
    );
}

#[test]
fn a_bitrate_above_the_hard_cap_is_refused() {
    let mut cfg = config(Accelerator::Software);
    cfg.max_kbps = 4000;
    let error = VideoEncoder::open(cfg, None).expect_err("§7.1 caps max_kbps at 2000");
    assert!(
        error.to_string().contains("hard 2000 kbps ceiling"),
        "unexpected error: {error}"
    );
}

#[test]
fn requesting_vaapi_always_yields_a_working_encoder() {
    // The point of this test is that §7.1's vaapi setting never leaves the
    // caller without an encoder, whatever the host can do.
    let mut encoder = VideoEncoder::open(config(Accelerator::Vaapi), None)
        .expect("vaapi must fall back rather than fail");

    match encoder.backend() {
        Backend::Vaapi => {
            assert!(
                encoder.fallback_reason().is_none(),
                "hardware was selected but a fallback reason was recorded"
            );
        }
        Backend::Software => {
            let reason = encoder
                .fallback_reason()
                .expect("a fallback must say why it happened");
            assert!(!reason.is_empty());
        }
    }

    // The hardware backend holds a frame in its pipeline, so push until one
    // comes out rather than assuming the software path's zero latency.
    let mut first = None;
    for index in 0..8 {
        if let Some(frame) = encoder.encode(&moving_frame(index)).expect("encoding") {
            first = Some(frame);
            break;
        }
    }
    let frame = first.expect("a frame must be emitted within 8 pushes");
    assert!(frame.keyframe, "the first emitted frame must be an IDR");
    assert!(!frame.data.is_empty());
}

#[test]
fn nvenc_falls_back_to_software_and_says_so() {
    let encoder = VideoEncoder::open(config(Accelerator::Nvenc), None).expect("must fall back");
    assert_eq!(encoder.backend(), Backend::Software);
    let reason = encoder.fallback_reason().expect("a reason is required");
    assert!(reason.contains("NVENC"), "unexpected reason: {reason}");
}

#[test]
fn the_vaapi_probe_agrees_with_the_backend_chosen() {
    let capability = probe_vaapi(None);
    let encoder = VideoEncoder::open(config(Accelerator::Vaapi), None).expect("must open");

    match (&capability, encoder.backend()) {
        (Ok(cap), Backend::Vaapi) => assert!(
            cap.can_encode(VideoCodec::H264),
            "hardware was chosen but the probe found no H.264 encode entrypoint"
        ),
        (Ok(cap), Backend::Software) => assert!(
            !cap.can_encode(VideoCodec::H264),
            "the probe found H.264 encode on {} but software was chosen",
            cap.render_node.display()
        ),
        (Err(_), Backend::Software) => {}
        (Err(e), Backend::Vaapi) => panic!("hardware was chosen though the probe failed: {e}"),
    }
}

#[test]
fn a_captured_bgra_scanout_round_trips_into_the_encoder() {
    // The real capture path of §7.1: a packed BGRA scanout, converted by
    // libswscale, then encoded.
    let mut pixels = vec![0u8; (WIDTH * HEIGHT * 4) as usize];
    for (index, chunk) in pixels.chunks_exact_mut(4).enumerate() {
        chunk[0] = (index % 256) as u8;
        chunk[1] = ((index / 3) % 256) as u8;
        chunk[2] = ((index / 7) % 256) as u8;
        chunk[3] = 0xff;
    }
    let scanout =
        PackedFrame::packed(WIDTH, HEIGHT, pixels, PackedFormat::Bgra).expect("valid geometry");

    let mut scaler = Scaler::to_i420(WIDTH, HEIGHT, PackedFormat::Bgra).expect("scaler");
    let mut yuv = Yuv420Frame::black(WIDTH, HEIGHT).expect("geometry");
    scaler
        .convert_to_i420(&scanout, &mut yuv)
        .expect("BGRA to I420");

    // A non-trivial image must not convert to a flat luma plane.
    let first = yuv.y[0];
    assert!(
        yuv.y.iter().any(|&v| v != first),
        "the converted luma plane is uniform, so the conversion did nothing"
    );

    let mut encoder =
        VideoEncoder::open(config(Accelerator::Software), None).expect("libx264 available");
    let encoded = encoder
        .encode(&yuv)
        .expect("encoding")
        .expect("first frame emitted");
    assert!(encoded.keyframe);
}

#[test]
fn report_the_hosts_encode_capability() {
    // Not an assertion: this prints what the host actually offers, so a
    // `cargo test -- --nocapture` run documents which path was exercised.
    match probe_vaapi(None) {
        Ok(cap) => println!(
            "VA-API: {} on {} — H.264 encode profiles: {:?}",
            cap.driver,
            cap.render_node.display(),
            cap.profiles_for(VideoCodec::H264)
        ),
        Err(e) => println!("VA-API unavailable: {e}"),
    }
    let encoder = VideoEncoder::open(config(Accelerator::Vaapi), None).expect("must open");
    println!(
        "chosen backend: {} — {}",
        encoder.backend().as_str(),
        encoder.acceleration()
    );
    if let Some(reason) = encoder.fallback_reason() {
        println!("fallback reason: {reason}");
    }
}

#[test]
fn report_the_encoder_pipeline_delay() {
    // How many frames must be submitted before the first comes back. The
    // software path is zero-latency by construction; a hardware encoder has
    // a real pipeline and this prints how deep it is on this host.
    let mut encoder = VideoEncoder::open(config(Accelerator::Vaapi), None).expect("opens");
    let mut submitted = 0u32;
    let delay = loop {
        submitted += 1;
        if encoder
            .encode(&moving_frame(submitted))
            .expect("encoding")
            .is_some()
        {
            break submitted - 1;
        }
        assert!(submitted < 60, "no output after {submitted} frames");
    };
    println!(
        "{} backend: {delay} frame(s) of pipeline delay",
        encoder.backend().as_str()
    );
}
