//! The §7 media plane: one encoder, many sessions (§1.2, §7.1, Amendment B.1).
//!
//! These pin the requirements the restructure exists to satisfy, so they are
//! written against the plane's own contract rather than against the RTSP
//! wire: that there is one encoder however many clients arrive, that the
//! first DESCRIBE binds the codec and a later one inherits or is refused
//! naming it, that the binding releases with the last session, and that a
//! session joining a running stream starts on a keyframe.
//!
//! They run a real encoder — the plane has no simulated mode, and a plane
//! test that did not encode would not be testing the plane.

use std::sync::Arc;
use std::time::{Duration, Instant};

use libvmm_config::MachineConfig;
use libvmm_core::VmmResult;
use libvmm_media::negotiate::Answer;
use libvmm_media::plane::{CaptureSource, MediaPlane, SessionStream, StreamUnit};
use vmm_codec_sys::{AudioCodec, PackedFormat, PackedFrame, VideoCodec};

const REFERENCE: &str = include_str!("../../../config/reference-vm.toml");
const WIDTH: u32 = 320;
const HEIGHT: u32 = 240;

/// How long a test will wait for the encoder to produce something before
/// declaring the plane broken. Generous: a hardware encoder holds its first
/// frame, and §7.1's keyframe clock is two seconds.
const PATIENCE: Duration = Duration::from_secs(20);

/// The §11 reference machine at a smaller geometry, so the tests stay quick
/// while every rate-control and codec setting stays exactly as specified.
fn config() -> MachineConfig {
    let mut config =
        MachineConfig::from_toml_str(REFERENCE).expect("the reference config must load");
    config.display.width = WIDTH;
    config.display.height = HEIGHT;
    config
}

/// A capture source that always has a new picture, so the encoder never
/// stalls waiting for the guest to redraw.
struct TestSource {
    tick: u32,
    samples_per_frame: usize,
}

impl TestSource {
    fn boxed(framerate: u32) -> Box<dyn CaptureSource> {
        Box::new(TestSource {
            tick: 0,
            samples_per_frame: 48_000 / framerate.max(1) as usize,
        })
    }
}

impl CaptureSource for TestSource {
    fn next_frame(&mut self) -> VmmResult<Option<PackedFrame>> {
        let index = self.tick;
        self.tick = self.tick.wrapping_add(1);
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
        Ok(Some(
            PackedFrame::packed(WIDTH, HEIGHT, pixels, PackedFormat::Bgra).expect("valid geometry"),
        ))
    }

    fn next_audio(&mut self) -> VmmResult<Vec<i16>> {
        Ok(vec![0i16; self.samples_per_frame * 2])
    }
}

/// A plane with its two §1.2 threads running.
fn running_plane() -> Arc<MediaPlane> {
    let config = config();
    let framerate = config.display.framerate_cap;
    let plane = MediaPlane::new(config, None).expect("this host must be able to encode something");
    plane
        .start(TestSource::boxed(framerate))
        .expect("the media threads must start");
    plane
}

/// A client that can decode anything this tree implements.
fn any_client() -> Answer {
    Answer {
        video: vec![VideoCodec::H264, VideoCodec::Vp9, VideoCodec::Av1],
        audio: vec![AudioCodec::Opus, AudioCodec::Vorbis],
    }
}

/// A client that decodes exactly one video codec, so a test can choose what
/// gets bound rather than depend on what the host's driver offers.
fn client_wanting(video: VideoCodec) -> Answer {
    Answer {
        video: vec![video],
        audio: vec![AudioCodec::Opus, AudioCodec::Vorbis],
    }
}

/// Read units until one satisfies `wanted`, or `PATIENCE` runs out.
fn next_matching(
    stream: &mut SessionStream,
    wanted: impl Fn(&StreamUnit) -> bool,
) -> Option<Arc<StreamUnit>> {
    let deadline = Instant::now() + PATIENCE;
    while Instant::now() < deadline {
        if let Some(unit) = stream.next_unit(Duration::from_millis(100)) {
            if wanted(&unit) {
                return Some(unit);
            }
        }
    }
    None
}

// -- Amendment B.1: negotiation binds the stream, not the session -----------

#[test]
fn the_first_session_to_describe_binds_the_machines_stream_codec() {
    let plane = running_plane();
    assert!(
        plane.bound().is_none(),
        "nothing may be bound before any session has described"
    );

    let session = plane.join(&any_client()).expect("the first session joins");
    let bound = plane
        .bound()
        .expect("the first DESCRIBE must bind the stream");
    assert_eq!(
        bound.video,
        session.selection().video,
        "the bound codec must be the one this session negotiated"
    );
    assert!(
        !session.description().inherited,
        "the first session negotiates; it inherits nothing"
    );

    drop(session);
    plane.shutdown();
}

#[test]
fn a_later_session_is_served_the_codec_already_running() {
    let plane = running_plane();
    let first = plane.join(&any_client()).expect("the first session joins");
    let chosen = first.selection().clone();

    let second = plane.join(&any_client()).expect("the second session joins");
    assert_eq!(
        second.selection().video,
        chosen.video,
        "a second session must be served the codec already running, not its own choice"
    );
    assert_eq!(second.selection().audio, chosen.audio);
    assert!(
        second.description().inherited,
        "§7.6.4: the session must record that the codec was inherited, not selected"
    );
    assert_eq!(plane.sessions(), 2);

    drop(second);
    drop(first);
    plane.shutdown();
}

#[test]
fn a_client_that_cannot_decode_the_bound_codec_is_refused_5011_naming_it() {
    let plane = running_plane();

    // Bind H.264 deliberately: every host in this tree can encode it in
    // software, so what the driver offers cannot change the outcome.
    let first = plane
        .join(&client_wanting(VideoCodec::H264))
        .expect("an H.264 client must be able to bind the stream");
    assert_eq!(first.selection().video, VideoCodec::H264);

    let refused = plane
        .join(&client_wanting(VideoCodec::Av1))
        .expect_err("a client that cannot decode the bound codec must be refused");
    assert_eq!(
        refused.code(),
        5011,
        "Amendment B.1 refuses with 5011, got: {refused}"
    );
    let message = refused.to_string();
    assert!(
        message.contains("h264"),
        "the message must name the codec being served — a client cannot fix a mismatch it \
         cannot see. Got: {message}"
    );
    assert!(
        message.contains("av1"),
        "the message must also name what the client offered. Got: {message}"
    );
    assert_eq!(
        plane.sessions(),
        1,
        "a refused session must not be counted against the binding"
    );

    drop(first);
    plane.shutdown();
}

#[test]
fn the_binding_is_released_when_the_last_session_tears_down() {
    let plane = running_plane();

    let first = plane.join(&client_wanting(VideoCodec::H264)).expect("join");
    let second = plane.join(&client_wanting(VideoCodec::H264)).expect("join");
    assert!(plane.bound().is_some());

    drop(first);
    assert!(
        plane.bound().is_some(),
        "the binding must survive while any session still holds the stream"
    );

    drop(second);
    assert!(
        plane.bound().is_none(),
        "the binding must release when the last session tears down"
    );

    // And the next DESCRIBE negotiates afresh, which is what "released"
    // has to mean if it is to mean anything.
    let third = plane.join(&any_client()).expect("join after release");
    assert!(
        !third.description().inherited,
        "after a release the next session negotiates rather than inheriting"
    );

    drop(third);
    plane.shutdown();
}

// -- §1.2/§7.1: one encoder, fanned out -------------------------------------

#[test]
fn one_encoder_feeds_every_session_the_same_stream() {
    let plane = running_plane();
    let mut first = plane.join(&any_client()).expect("join");
    let mut second = plane.join(&any_client()).expect("join");

    let a = next_matching(&mut first, |u| u.is_video()).expect("the first session must get video");
    let b =
        next_matching(&mut second, |u| u.is_video()).expect("the second session must get video");

    // Both subscribed before anything was encoded, so both must have been
    // handed the very same unit — one encoder, one packetiser, one sequence
    // of RTP. Two encoders would give two different first frames.
    assert_eq!(
        a.packets, b.packets,
        "both sessions must receive byte-identical RTP: §7.1 encodes and packetises once"
    );
    assert!(
        a.keyframe,
        "a session present from the start must begin on the encoder's opening keyframe"
    );

    drop(first);
    drop(second);
    plane.shutdown();
}

#[test]
fn a_session_joining_a_running_stream_starts_on_a_keyframe() {
    let plane = running_plane();
    let mut first = plane.join(&any_client()).expect("join");

    // Get the stream properly running first.
    assert!(
        next_matching(&mut first, |u| u.is_video() && u.keyframe).is_some(),
        "the stream must be running before a second session can join it"
    );

    let mut late = plane.join(&any_client()).expect("a late session joins");

    // Drive the stream past at least one inter frame. The broadcast hands
    // every subscriber the same units, so a non-keyframe the first session
    // sees is one the late session was also given and must discard.
    assert!(
        next_matching(&mut first, |u| u.is_video() && !u.keyframe).is_some(),
        "the encoder must produce inter frames for this test to mean anything"
    );

    let unit = next_matching(&mut late, |u| u.is_video())
        .expect("the late session must eventually get video");
    assert!(
        unit.keyframe,
        "a session joining mid-GOP must start on a keyframe: inter frames reference \
         pictures it never received"
    );
    assert!(
        late.discarded() > 0,
        "the late session was handed inter frames and must have discarded them, not \
         written them; discarded = {}",
        late.discarded()
    );

    drop(late);
    drop(first);
    plane.shutdown();
}

// -- §7.1: the pipeline is continuous ---------------------------------------

#[test]
fn capture_runs_on_its_own_clock_and_not_a_sessions() {
    let plane = running_plane();
    let session = plane.join(&any_client()).expect("join");

    // Nothing reads from this session at all. If capture or encode were
    // driven by a session's clock — which is what Finding 1 was about —
    // both would stall here.
    let deadline = Instant::now() + PATIENCE;
    while Instant::now() < deadline && plane.stats().frames_encoded < 4 {
        std::thread::sleep(Duration::from_millis(20));
    }

    let stats = plane.stats();
    assert!(
        stats.frames_captured >= 4,
        "capture must run while no session reads: captured {}",
        stats.frames_captured
    );
    assert!(
        stats.frames_encoded >= 4,
        "encode must run while no session reads: encoded {}",
        stats.frames_encoded
    );
    assert_eq!(stats.sessions_live, 1);
    assert_eq!(stats.sessions_total, 1);

    drop(session);
    plane.shutdown();
}

#[test]
fn nothing_is_captured_while_no_codec_is_bound() {
    // The other half of the same requirement. Capture is independent of any
    // *session*, but there is nothing to encode a scanout with until a codec
    // is bound, and allocating an 8 MB framebuffer thirty times a second for
    // an encoder that does not exist is waste, not continuity.
    let plane = running_plane();
    std::thread::sleep(Duration::from_millis(200));
    assert_eq!(
        plane.stats().frames_captured,
        0,
        "an unbound plane must not capture"
    );

    let session = plane.join(&any_client()).expect("join");
    let deadline = Instant::now() + PATIENCE;
    while Instant::now() < deadline && plane.stats().frames_captured == 0 {
        std::thread::sleep(Duration::from_millis(20));
    }
    assert!(
        plane.stats().frames_captured > 0,
        "capture must start as soon as a codec is bound"
    );

    drop(session);
    plane.shutdown();
}

// -- §7.2: the SDP describes what is actually running ------------------------

#[test]
fn the_sdp_announces_the_bound_codec_to_every_session() {
    let plane = running_plane();
    let first = plane.join(&client_wanting(VideoCodec::H264)).expect("join");
    let second = plane.join(&any_client()).expect("join");

    let a = first.sdp("reference", "/live");
    let b = second.sdp("reference", "/live");
    assert_eq!(
        a, b,
        "two sessions on one bound stream must be told about the same stream"
    );
    assert!(
        a.contains("H264/90000"),
        "the SDP must announce the bound codec, not a default. Got:\n{a}"
    );

    drop(second);
    drop(first);
    plane.shutdown();
}
