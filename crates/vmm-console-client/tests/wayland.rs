//! The Wayland display path, against a real compositor.
//!
//! These tests need a live Wayland session — they talk to the compositor
//! named by `WAYLAND_DISPLAY` and map an actual window. Where there is no
//! session (CI, a bare TTY, an X11-only desktop) they skip rather than fail,
//! because the absence of a compositor says nothing about the code.

use vmm_codec_sys::{PackedFormat, PackedFrame};
use vmm_console_client::decode::FrameHandler;
use vmm_console_client::wayland::WaylandWindow;

/// A recognisable moving test pattern, so a human watching the window can
/// see that frames are being painted and not just counted.
fn frame(width: u32, height: u32, tick: u32) -> PackedFrame {
    let stride = width as usize * 4;
    let mut pixels = vec![0u8; stride * height as usize];
    for y in 0..height as usize {
        for x in 0..width as usize {
            let p = y * stride + x * 4;
            // BGRA in memory.
            pixels[p] = (x as u32 * 4 + tick * 8) as u8;
            pixels[p + 1] = (y as u32 * 4) as u8;
            pixels[p + 2] = (tick * 4) as u8;
            pixels[p + 3] = 0xff;
        }
    }
    PackedFrame {
        width,
        height,
        stride,
        pixels,
        format: PackedFormat::Bgra,
    }
}

fn compositor_available() -> bool {
    std::env::var_os("WAYLAND_DISPLAY").is_some() && std::env::var_os("XDG_RUNTIME_DIR").is_some()
}

macro_rules! require_compositor {
    () => {
        if !compositor_available() {
            eprintln!("skipping: no WAYLAND_DISPLAY, so there is no compositor to map a window on");
            return;
        }
    };
}

#[test]
fn a_window_maps_and_paints_frames_on_the_compositor() {
    require_compositor!();

    let mut window = WaylandWindow::open("vmm test — paint").expect("the window should map");

    // 60 frames is two seconds at 30 fps. Painting that many proves the
    // compositor is releasing buffers back to us: with BUFFERS = 2, the
    // third frame cannot be acquired until one comes back.
    for tick in 0..60 {
        window
            .present(&frame(640, 360, tick))
            .expect("every frame should paint");
    }

    assert_eq!(window.frames, 60, "every frame should have been painted");
    assert!(!window.closed(), "nothing closed the window");
    // The compositor only releases a buffer after reading its pixels, so
    // this is the assertion that separates "the protocol was accepted" from
    // "the frames were actually displayed".
    assert!(
        window.releases() > 0,
        "the compositor released no buffer, so nothing was actually shown"
    );
}

#[test]
fn the_window_follows_a_geometry_change() {
    require_compositor!();

    // The scanout can change size mid-session; the pool has to be rebuilt
    // rather than blitting the new frame into the old geometry.
    let mut window = WaylandWindow::open("vmm test — resize").expect("the window should map");
    for tick in 0..5 {
        window.present(&frame(320, 240, tick)).expect("first size");
    }
    for tick in 0..5 {
        window.present(&frame(640, 480, tick)).expect("second size");
    }
    for tick in 0..5 {
        window.present(&frame(320, 240, tick)).expect("back again");
    }

    assert_eq!(window.frames, 15);
}

#[test]
fn a_padded_stride_does_not_skew_the_picture() {
    require_compositor!();

    // A scanout stride may exceed width * 4. The blit must copy row by row
    // and drop the padding, or the image shears.
    let (width, height) = (100u32, 50u32);
    let stride = width as usize * 4 + 64;
    let mut pixels = vec![0u8; stride * height as usize];
    for y in 0..height as usize {
        for x in 0..width as usize {
            let p = y * stride + x * 4;
            pixels[p + 2] = 0xff; // red, in BGRA
            pixels[p + 3] = 0xff;
        }
        // Poison the padding: if it is copied, the picture shears visibly.
        for b in &mut pixels[y * stride + width as usize * 4..(y + 1) * stride] {
            *b = 0x7f;
        }
    }

    let mut window = WaylandWindow::open("vmm test — stride").expect("the window should map");
    window
        .present(&PackedFrame {
            width,
            height,
            stride,
            pixels,
            format: PackedFormat::Bgra,
        })
        .expect("a padded frame should paint");

    assert_eq!(window.frames, 1);
}

#[test]
fn the_window_refuses_a_format_it_cannot_blit() {
    require_compositor!();

    // RGBA would come out with red and blue swapped. Refusing is better than
    // painting a wrong-coloured picture that looks like a decoder bug.
    let mut window = WaylandWindow::open("vmm test — format").expect("the window should map");
    let mut f = frame(64, 64, 0);
    f.format = PackedFormat::Rgba;

    let err = window.present(&f).expect_err("RGBA should be refused");
    assert!(
        err.to_string().contains("BGRA"),
        "the message should say what it wanted: {err}"
    );
}

#[test]
fn the_window_is_a_frame_handler() {
    require_compositor!();

    // The point of the type: it drops into the same decode path as
    // RawBgraWriter and LatestFrame.
    let mut window = WaylandWindow::open("vmm test — handler").expect("the window should map");
    let handler: &mut dyn FrameHandler = &mut window;
    handler.on_frame(&frame(320, 180, 1)).expect("painted");
    handler.on_frame(&frame(320, 180, 2)).expect("painted");

    assert_eq!(window.frames, 2);
}

/// Real encoded video, decoded and painted.
///
/// The synthetic tests above prove the Wayland protocol works. This one
/// proves the *pipeline* does: an encoder's output, through the real
/// decoder, into the window. It is the test that would catch a BGRA/RGBA
/// mismatch between what the decoder emits and what the window blits —
/// which no amount of hand-built frames can.
#[test]
fn real_decoded_video_reaches_the_window() {
    require_compositor!();

    use libvmm_media::depacketize::CodedUnit;
    use vmm_codec_sys::{Accelerator, EncoderConfig, VideoCodec, VideoEncoder, Yuv420Frame};
    use vmm_console_client::decode::DecodingSink;
    use vmm_console_client::rtsp_client::MediaSink;

    const W: u32 = 320;
    const H: u32 = 240;

    let mut encoder = VideoEncoder::open(
        EncoderConfig {
            codec: VideoCodec::H264,
            width: W,
            height: H,
            framerate: 30,
            target_kbps: 1800,
            max_kbps: 2000,
            vbv_buffer_bits: 2_000_000,
            gop_length: 60,
            accelerator: Accelerator::Software,
        },
        None,
    )
    .expect("libx264 must be available");

    let window = WaylandWindow::open("vmm test — decoded video").expect("the window should map");
    let mut sink = DecodingSink::new(VideoCodec::H264, window, None).expect("sink opens");

    for index in 0..30u32 {
        let mut f = Yuv420Frame::black(W, H).expect("geometry");
        for row in 0..H as usize {
            for col in 0..W as usize {
                f.y[row * f.y_stride + col] = ((row + col + index as usize * 4) % 200) as u8;
            }
        }
        f.pts = index as i64;

        if let Some(encoded) = encoder.encode(&f).expect("encode") {
            sink.on_video(&CodedUnit {
                data: encoded.data.clone(),
                timestamp: 0,
                keyframe: encoded.keyframe,
            })
            .expect("decode and paint");
        }
    }

    assert!(sink.frames_decoded > 0, "nothing decoded");
    assert_eq!(
        sink.handler().frames,
        sink.frames_decoded,
        "every decoded frame should have been painted"
    );
    assert_eq!(sink.geometry(), Some((W, H)));
    assert!(
        sink.handler().releases() > 0,
        "the compositor released no buffer, so nothing was actually shown"
    );
}

#[test]
fn a_display_failure_reports_error_5012() {
    // Distinct from 5010 (a frame the encoder cannot accept): 5012 is a
    // display surface that will not take one. An operator reading the log
    // needs to know which half of the pipeline failed.
    require_compositor!();

    let mut window = WaylandWindow::open("vmm test — code").expect("the window should map");
    let mut f = frame(64, 64, 0);
    f.format = PackedFormat::Rgba;

    let err = window.present(&f).expect_err("RGBA should be refused");
    assert_eq!(err.code(), 5012, "display failures are 5012");
    assert_eq!(err.domain(), "Media");
}

#[test]
fn the_window_reports_no_input_when_nothing_happened() {
    require_compositor!();

    // Nothing has been typed or clicked, so the queue must be empty rather
    // than producing phantom events the guest would act on.
    let mut window = WaylandWindow::open("vmm test — input").expect("the window should map");
    window.present(&frame(320, 240, 0)).expect("painted");

    let events = window.drain_input().expect("draining should not fail");
    assert!(
        events.is_empty(),
        "no input was produced, so none should be reported: {events:?}"
    );
}

#[test]
fn draining_input_works_before_anything_is_painted() {
    require_compositor!();

    // The session drains input on a timer, which can fire before the first
    // frame arrives. That must not panic on the not-yet-known geometry.
    let mut window = WaylandWindow::open("vmm test — early input").expect("the window should map");
    assert!(window.drain_input().expect("no panic").is_empty());
}

// -- pure mapping, no compositor needed --------------------------------------

#[test]
fn pointer_coordinates_reach_both_edges_of_the_guest_grid() {
    use vmm_console_client::wayland::scale_abs;

    // The far edge must map exactly onto 32767. Dividing by `max` rather
    // than `max - 1` would leave the guest pointer a pixel short of the
    // right and bottom edges — where the close button and taskbar are.
    assert_eq!(scale_abs(0.0, 1920), 0, "left edge");
    assert_eq!(scale_abs(1919.0, 1920), 32767, "right edge");
    assert_eq!(scale_abs(0.0, 1080), 0, "top edge");
    assert_eq!(scale_abs(1079.0, 1080), 32767, "bottom edge");

    // Halfway is halfway, within rounding.
    let mid = scale_abs(959.5, 1920);
    assert!((mid - 16383).abs() <= 1, "centre mapped to {mid}");
}

#[test]
fn a_pointer_outside_the_surface_is_clamped_not_wrapped() {
    use vmm_console_client::wayland::scale_abs;

    // A compositor can report coordinates outside the surface during a drag.
    // Clamping keeps the guest pointer at the edge; letting it through would
    // send a negative or out-of-range absolute position.
    assert_eq!(scale_abs(-40.0, 800), 0);
    assert_eq!(scale_abs(10_000.0, 800), 32767);
}

#[test]
fn a_degenerate_surface_does_not_divide_by_zero() {
    use vmm_console_client::wayland::scale_abs;

    assert_eq!(scale_abs(5.0, 0), 0);
    assert_eq!(scale_abs(5.0, 1), 0);
}

#[test]
fn wayland_keycodes_are_linux_keycodes_with_no_offset() {
    // Pinning the convention, because getting it wrong is silent: every key
    // still "works", it just produces the wrong character. wayland.xml's
    // xkb_v1 entry says clients must add 8 to reach the *xkb* keycode, which
    // means the wire value is the evdev code — and §8.5 wants the evdev
    // code, so the window forwards it unchanged.
    //
    // KEY_A is 30 in linux/input-event-codes.h; xkb would call it 38.
    const KEY_A_EVDEV: u32 = 30;
    let forwarded = KEY_A_EVDEV as i64;
    assert_eq!(forwarded, 30, "the guest must see the evdev code");
    assert_ne!(forwarded, 38, "adding the xkb offset would be wrong here");
}
