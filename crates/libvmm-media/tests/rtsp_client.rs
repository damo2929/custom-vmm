//! The client half of §7.2: request building, response parsing and SDP.

use libvmm_media::rtp::{AUDIO_CHANNELS, VIDEO_CHANNELS};
use libvmm_media::rtsp::{self, RequestBuilder, Response};

fn builder() -> RequestBuilder {
    RequestBuilder::new("rtsps://[::1]:8554/live", "admin", "hypervisor@01")
}

#[test]
fn every_request_carries_basic_auth_and_a_monotonic_cseq() {
    // §7.4: "Basic Auth on every request: OPTIONS, DESCRIBE, SETUP, PLAY each
    // MUST carry Authorization: Basic."
    let mut b = builder();
    let requests = [
        b.options(),
        b.describe(),
        b.setup("/live/video", VIDEO_CHANNELS),
        b.play(),
    ];

    for (i, request) in requests.iter().enumerate() {
        assert!(
            request.contains("Authorization: Basic YWRtaW46aHlwZXJ2aXNvckAwMQ=="),
            "request {i} is missing Basic Auth:\n{request}"
        );
        assert!(
            request.contains(&format!("CSeq: {}", i + 1)),
            "CSeq must be monotonic:\n{request}"
        );
        assert!(request.ends_with("\r\n\r\n"));
    }
    assert_eq!(b.last_cseq(), 4);
}

#[test]
fn setup_asks_for_the_spec_7_3_interleaved_channels() {
    let mut b = builder();
    let video = b.setup("/live/video", VIDEO_CHANNELS);
    assert!(
        video.contains("Transport: RTP/AVP/TCP;unicast;interleaved=0-1"),
        "{video}"
    );

    let audio = b.setup("/live/audio", AUDIO_CHANNELS);
    assert!(audio.contains("interleaved=2-3"), "{audio}");
}

#[test]
fn the_session_header_is_echoed_on_later_requests() {
    let mut b = builder();
    assert!(!b.setup("/live/video", VIDEO_CHANNELS).contains("Session:"));

    // The server assigns a session at SETUP, possibly with parameters.
    b.set_session("12345678;timeout=60");
    assert_eq!(b.session(), Some("12345678"), "parameters must be stripped");
    assert!(b.play().contains("Session: 12345678"));
    assert!(b.teardown().contains("Session: 12345678"));
}

#[test]
fn relative_and_absolute_control_urls_both_resolve() {
    let mut b = builder();
    assert!(b
        .setup("video", VIDEO_CHANNELS)
        .starts_with("SETUP rtsps://[::1]:8554/live/video "));
    assert!(b
        .setup("/live/audio", AUDIO_CHANNELS)
        .starts_with("SETUP rtsps://[::1]:8554/live/audio "));
    assert!(b
        .setup("rtsps://other:8554/x", VIDEO_CHANNELS)
        .starts_with("SETUP rtsps://other:8554/x "));
}

#[test]
fn a_response_with_a_body_parses_and_reports_what_it_consumed() {
    let sdp = "v=0\r\nm=video 0 RTP/AVP 96\r\n";
    let raw = format!(
        "RTSP/1.0 200 OK\r\nCSeq: 2\r\nContent-Type: application/sdp\r\nContent-Length: {}\r\n\r\n{sdp}EXTRA",
        sdp.len()
    );
    let r = Response::parse(raw.as_bytes()).unwrap().unwrap();

    assert!(r.is_ok());
    assert_eq!(r.cseq, 2);
    assert_eq!(r.content_type.as_deref(), Some("application/sdp"));
    assert_eq!(r.body, sdp);
    // "EXTRA" belongs to whatever comes next on the connection.
    assert_eq!(&raw[r.consumed..], "EXTRA");
}

#[test]
fn a_partial_response_asks_for_more_bytes() {
    assert!(Response::parse(b"RTSP/1.0 200 OK\r\nCSeq: 1\r\n")
        .unwrap()
        .is_none());
    // Headers complete but the body has not arrived.
    assert!(
        Response::parse(b"RTSP/1.0 200 OK\r\nCSeq: 1\r\nContent-Length: 10\r\n\r\nshort")
            .unwrap()
            .is_none()
    );
}

#[test]
fn a_401_is_recognised_as_an_auth_failure() {
    let raw = "RTSP/1.0 401 Unauthorized\r\nCSeq: 1\r\nWWW-Authenticate: Basic realm=\"KVM-Secure-Console\"\r\n\r\n";
    let r = Response::parse(raw.as_bytes()).unwrap().unwrap();
    assert!(r.is_unauthorized());
    assert!(!r.is_ok());
}

#[test]
fn the_session_header_is_read_off_a_setup_reply() {
    let raw = "RTSP/1.0 200 OK\r\nCSeq: 3\r\nSession: ABCD1234;timeout=60\r\nTransport: RTP/AVP/TCP;interleaved=0-1\r\n\r\n";
    let r = Response::parse(raw.as_bytes()).unwrap().unwrap();
    assert_eq!(r.session.as_deref(), Some("ABCD1234;timeout=60"));
    assert!(r.transport.as_deref().unwrap().contains("interleaved=0-1"));
}

#[test]
fn a_non_rtsp_status_line_is_rejected() {
    assert!(Response::parse(b"HTTP/1.1 200 OK\r\nCSeq: 1\r\n\r\n").is_err());
}

// -- SDP ---------------------------------------------------------------------

#[test]
fn the_hypervisors_own_sdp_parses_back_into_two_media_sections() {
    // Generate the SDP with the server-side builder, then parse it with the
    // client-side parser: the two halves must agree.
    let cfg = libvmm_config::MachineConfig::from_toml_str(include_str!(
        "../../../config/reference-vm.toml"
    ))
    .unwrap();
    let video = libvmm_media::encoder::VideoEncoderParams::from_config(&cfg.display).unwrap();
    let audio = libvmm_media::encoder::AudioEncoderParams::from_config(&cfg.display);
    let sdp = libvmm_media::encoder::sdp(
        &cfg.vm.name,
        &cfg.display.rtsps.stream_path,
        &video,
        &audio,
        vmm_codec_sys::VideoCodec::H264,
        vmm_codec_sys::AudioCodec::Vorbis,
        None,
    );

    let media = rtsp::parse_sdp(&sdp);
    assert_eq!(media.len(), 2);

    assert_eq!(media[0].kind, "video");
    assert_eq!(media[0].encoding, "H264");
    assert_eq!(media[0].payload_type, 96);
    assert_eq!(media[0].control, "/live/video");

    assert_eq!(media[1].kind, "audio");
    assert_eq!(media[1].encoding, "vorbis");
    assert_eq!(media[1].payload_type, 97);
    assert_eq!(media[1].control, "/live/audio");
}

#[test]
fn an_sdp_with_no_media_yields_no_sections() {
    assert!(rtsp::parse_sdp("v=0\r\no=- 0 0 IN IP6 ::\r\n").is_empty());
}
