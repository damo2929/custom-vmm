//! §7 remote console: encoder rate control, the RTSP state machine, Basic
//! Auth on every request, and interleaved transport framing.

use libvmm_config::MachineConfig;
use libvmm_media::encoder::*;
use libvmm_media::rtp::{self, *};
use libvmm_media::rtsp::{self, *};

fn reference() -> MachineConfig {
    MachineConfig::from_toml_str(include_str!("../../../config/reference-vm.toml")).unwrap()
}

// -- §7.1 rate control -------------------------------------------------------

#[test]
fn constrained_vbr_targets_1800_under_a_hard_2000_ceiling() {
    let cfg = reference();
    let p = VideoEncoderParams::from_config(&cfg.display).unwrap();
    assert_eq!(p.target_kbps, 1800);
    assert_eq!(p.max_kbps, 2000);
    assert_eq!(p.max_kbps, HARD_CAP_KBPS);
    assert!(
        p.target_kbps < p.max_kbps,
        "VBR must always stay below the ceiling"
    );
    assert_eq!(p.rate_control, libvmm_config::RateControl::Vbr);
    assert_eq!((p.width, p.height), (1920, 1080));
}

#[test]
fn the_vbv_buffer_is_sized_to_the_cap() {
    // §7.1: "HRD/VBV buffer sized to the cap."
    let cfg = reference();
    let p = VideoEncoderParams::from_config(&cfg.display).unwrap();
    assert_eq!(p.vbv_buffer_bits, p.max_kbps * 1000);
}

#[test]
fn an_instantaneous_rate_above_the_cap_is_detectable() {
    let cfg = reference();
    let p = VideoEncoderParams::from_config(&cfg.display).unwrap();
    assert!(!p.exceeds_cap(1999));
    assert!(!p.exceeds_cap(2000));
    assert!(p.exceeds_cap(2001));
}

#[test]
fn the_encoder_refuses_to_initialise_above_the_hard_cap() {
    let mut cfg = reference();
    cfg.display.encoder.max_bitrate_kbps = 5000;
    let e = VideoEncoderParams::from_config(&cfg.display).unwrap_err();
    assert_eq!(e.code(), 5001, "must be Media(EncoderInit)");
}

#[test]
fn the_encoder_refuses_a_target_at_or_above_its_ceiling() {
    let mut cfg = reference();
    cfg.display.encoder.bitrate_kbps = 2000;
    assert_eq!(
        VideoEncoderParams::from_config(&cfg.display)
            .unwrap_err()
            .code(),
        5001
    );
}

#[test]
fn audio_is_vorbis_128k_at_48khz_stereo() {
    let cfg = reference();
    let a = AudioEncoderParams::from_config(&cfg.display);
    assert_eq!(
        (a.sample_rate, a.channels, a.bitrate_kbps),
        (48_000, 2, 128)
    );
}

#[test]
fn the_scanout_is_a_1920x1080_argb_framebuffer() {
    let cfg = reference();
    let s = ScanoutFormat::from_config(&cfg.display);
    assert_eq!((s.width, s.height, s.bits_per_pixel), (1920, 1080, 32));
    assert_eq!(s.frame_bytes(), 1920 * 1080 * 4);
}

// -- §7.2 state machine ------------------------------------------------------

#[test]
fn the_spec_7_2_happy_path() {
    let mut s = Session::new(1);
    assert_eq!(s.state(), SessionState::Init);

    // INIT --DESCRIBE--> INIT (returns SDP)
    assert_eq!(s.on(Method::Describe, true).unwrap(), Action::ReturnSdp);
    assert_eq!(s.state(), SessionState::Init);
    assert!(!s.channels_allocated);

    // INIT --SETUP--> READY (allocate RTP interleaved channels)
    assert_eq!(s.on(Method::Setup, true).unwrap(), Action::AllocateChannels);
    assert_eq!(s.state(), SessionState::Ready);
    assert!(s.channels_allocated);
    assert!(!s.streaming);

    // READY --PLAY--> PLAYING (start writing this session's stream)
    assert_eq!(s.on(Method::Play, true).unwrap(), Action::StartStreaming);
    assert_eq!(s.state(), SessionState::Playing);
    assert!(s.streaming);
    assert!(s.state().is_streaming());

    // PLAYING --PAUSE--> READY
    assert_eq!(s.on(Method::Pause, true).unwrap(), Action::PauseStreaming);
    assert_eq!(s.state(), SessionState::Ready);
    assert!(!s.streaming);
    assert!(s.channels_allocated, "PAUSE keeps the channels");

    // READY --TEARDOWN--> INIT (release the subscription + channels)
    assert_eq!(
        s.on(Method::Teardown, true).unwrap(),
        Action::ReleaseSession
    );
    assert_eq!(s.state(), SessionState::Init);
    assert!(!s.channels_allocated);
}

#[test]
fn teardown_from_playing_frees_everything() {
    let mut s = Session::new(1);
    s.on(Method::Setup, true).unwrap();
    s.on(Method::Play, true).unwrap();
    assert_eq!(
        s.on(Method::Teardown, true).unwrap(),
        Action::ReleaseSession
    );
    assert_eq!(s.state(), SessionState::Init);
    assert!(!s.streaming && !s.channels_allocated);
}

#[test]
fn a_two_stream_session_sends_two_setups_and_both_are_accepted() {
    // Revision C.1. RTSP sets up each media section separately, and §7.1's
    // console has two — video and audio — so a client sends SETUP twice.
    // §7.2's table as written has one `INIT --SETUP--> READY` arm, which
    // refused the second with 455 and broke every two-stream session.
    let mut s = Session::new(1);
    assert_eq!(s.on(Method::Setup, true).unwrap(), Action::AllocateChannels);
    assert_eq!(s.state(), SessionState::Ready);
    assert_eq!(
        s.on(Method::Setup, true).unwrap(),
        Action::AllocateChannels,
        "the audio media section's SETUP must be accepted from READY"
    );
    assert_eq!(s.state(), SessionState::Ready);
    assert_eq!(s.on(Method::Play, true).unwrap(), Action::StartStreaming);
}

#[test]
fn play_before_setup_is_refused() {
    let mut s = Session::new(1);
    let e = s.on(Method::Play, true).unwrap_err();
    assert_eq!(e.code(), 5004, "must be Media(BadState)");
    assert_eq!(s.state(), SessionState::Init);
}

#[test]
fn options_is_answered_in_every_state_without_changing_it() {
    for setup_steps in 0..3 {
        let mut s = Session::new(1);
        if setup_steps > 0 {
            s.on(Method::Setup, true).unwrap();
        }
        if setup_steps > 1 {
            s.on(Method::Play, true).unwrap();
        }
        let before = s.state();
        assert_eq!(s.on(Method::Options, true).unwrap(), Action::Respond);
        assert_eq!(s.state(), before);
    }
}

// -- §7.4 authentication -----------------------------------------------------

#[test]
fn auth_failure_yields_401_and_allocates_no_resources() {
    // §7.4: "before any media resource is allocated".
    let mut s = Session::new(1);
    for method in [
        Method::Options,
        Method::Describe,
        Method::Setup,
        Method::Play,
    ] {
        let e = s.on(method, false).unwrap_err();
        assert_eq!(
            e.code(),
            5005,
            "{} must be Media(RtspAuth)",
            method.as_str()
        );
        assert_eq!(s.state(), SessionState::Init, "the state must not advance");
        assert!(!s.channels_allocated, "no channels may be allocated");
        assert!(!s.streaming, "no session may start writing");
    }
}

#[test]
fn auth_failure_while_playing_does_not_stop_the_stream() {
    // "stay in current state, no media" — the failed request gets no media,
    // but an established session is not torn down by one bad request.
    let mut s = Session::new(1);
    s.on(Method::Setup, true).unwrap();
    s.on(Method::Play, true).unwrap();
    assert!(s.on(Method::Describe, false).is_err());
    assert_eq!(s.state(), SessionState::Playing);
}

#[test]
fn the_401_response_carries_the_kvm_secure_console_realm() {
    let r = rtsp::unauthorized(4);
    assert!(r.starts_with("RTSP/1.0 401 Unauthorized"));
    assert!(r.contains("CSeq: 4"));
    assert!(r.contains(r#"WWW-Authenticate: Basic realm="KVM-Secure-Console""#));
}

#[test]
fn rtsp_requests_parse() {
    let text = "DESCRIBE rtsps://[::]:8554/live RTSP/1.0\r\n\
                CSeq: 2\r\n\
                Authorization: Basic YWRtaW46aHlwZXJ2aXNvckAwMQ==\r\n\
                Accept: application/sdp\r\n\r\n";
    let r = rtsp::Request::parse(text).unwrap();
    assert_eq!(r.method, Method::Describe);
    assert_eq!(r.cseq, 2);
    assert_eq!(r.uri, "rtsps://[::]:8554/live");
    let (u, p) = libvmm_control::auth::parse_basic(r.authorization.as_deref().unwrap()).unwrap();
    assert_eq!((u.as_str(), p.as_str()), ("admin", "hypervisor@01"));
}

#[test]
fn an_unknown_method_is_a_bad_request() {
    let e = rtsp::Request::parse("FROB / RTSP/1.0\r\n\r\n").unwrap_err();
    assert_eq!(e.code(), 5003);
}

// -- §7.3 interleaved framing -----------------------------------------------

#[test]
fn channel_assignment_matches_the_spec_7_3() {
    assert_eq!(CHANNEL_VIDEO_RTP, 0);
    assert_eq!(CHANNEL_VIDEO_RTCP, 1);
    assert_eq!(CHANNEL_AUDIO_RTP, 2);
    assert_eq!(CHANNEL_AUDIO_RTCP, 3);
    assert_eq!(VIDEO_CHANNELS, ChannelPair { rtp: 0, rtcp: 1 });
    assert_eq!(AUDIO_CHANNELS, ChannelPair { rtp: 2, rtcp: 3 });
}

#[test]
fn interleaved_frames_use_a_dollar_magic_and_big_endian_length() {
    let packet = vec![0xAAu8; 300];
    let framed = rtp::frame(CHANNEL_VIDEO_RTP, &packet).unwrap();
    assert_eq!(framed[0], b'$');
    assert_eq!(framed[1], CHANNEL_VIDEO_RTP);
    // §0.1: RTP/RTSP length prefixes are big-endian.
    assert_eq!(u16::from_be_bytes([framed[2], framed[3]]), 300);
    assert_eq!(&framed[4..], &packet[..]);

    let decoded = rtp::parse(&framed).unwrap().unwrap();
    assert_eq!(decoded.channel, CHANNEL_VIDEO_RTP);
    assert_eq!(decoded.payload, packet);
    assert_eq!(decoded.consumed, framed.len());
}

#[test]
fn a_truncated_interleaved_frame_asks_for_more_bytes() {
    let framed = rtp::frame(CHANNEL_AUDIO_RTP, &[1, 2, 3, 4]).unwrap();
    assert!(rtp::parse(&framed[..3]).unwrap().is_none());
    assert!(rtp::parse(&framed[..6]).unwrap().is_none());
    assert!(rtp::parse(&framed).unwrap().is_some());
}

#[test]
fn a_frame_without_the_dollar_magic_is_rejected() {
    assert_eq!(rtp::parse(&[b'R', 0, 0, 1]).unwrap_err().code(), 5003);
}

#[test]
fn an_rtp_header_is_twelve_bytes_with_version_two() {
    let h = RtpHeader {
        payload_type: PAYLOAD_TYPE_H264,
        marker: true,
        sequence: 0x1234,
        timestamp: 0xDEAD_BEEF,
        ssrc: 0xCAFE_BABE,
    };
    let mut out = Vec::new();
    h.write_into(&mut out);
    assert_eq!(out.len(), 12);
    assert_eq!(out[0] >> 6, 2, "RTP version 2");
    assert_eq!(out[1] & 0x7F, PAYLOAD_TYPE_H264);
    assert_ne!(out[1] & 0x80, 0, "marker bit");
    assert_eq!(u16::from_be_bytes([out[2], out[3]]), 0x1234);
}

// -- SDP ---------------------------------------------------------------------

#[test]
fn the_sdp_describes_both_streams() {
    let cfg = reference();
    let v = VideoEncoderParams::from_config(&cfg.display).unwrap();
    let a = AudioEncoderParams::from_config(&cfg.display);
    let sdp = sdp(
        &cfg.vm.name,
        &cfg.display.rtsps.stream_path,
        &v,
        &a,
        vmm_codec_sys::VideoCodec::H264,
        vmm_codec_sys::AudioCodec::Vorbis,
        None,
    );

    assert!(sdp.contains("m=video 0 RTP/AVP 96"));
    assert!(sdp.contains("a=rtpmap:96 H264/90000"));
    assert!(sdp.contains("a=framesize:96 1920-1080"));
    assert!(sdp.contains("m=audio 0 RTP/AVP 97"));
    assert!(sdp.contains("a=rtpmap:97 vorbis/48000/2"));
    assert!(sdp.contains(&format!("s={}", cfg.vm.name)));
    assert!(sdp.contains("a=control:/live"));
}

#[test]
fn options_advertises_the_spec_7_2_method_set() {
    for m in ["OPTIONS", "DESCRIBE", "SETUP", "PLAY", "PAUSE", "TEARDOWN"] {
        assert!(SUPPORTED_METHODS.contains(m), "{m} must be advertised");
    }
}
