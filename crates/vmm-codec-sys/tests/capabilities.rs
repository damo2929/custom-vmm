//! What this host can actually do, printed and checked.

use vmm_codec_sys::{AudioCodec, Capabilities, VideoCodec};

#[test]
fn report_the_full_codec_matrix() {
    let caps = Capabilities::probe(None);

    match (&caps.vaapi, &caps.vaapi_error) {
        (Some(v), _) => println!(
            "VA-API: {} on {}\n  hardware encode: {:?}",
            v.driver,
            v.render_node.display(),
            v.encodable().iter().map(|c| c.as_str()).collect::<Vec<_>>()
        ),
        (None, Some(e)) => println!("VA-API unavailable: {e}"),
        (None, None) => println!("VA-API: no result"),
    }

    println!("video:");
    for codec in [VideoCodec::H264, VideoCodec::Vp9, VideoCodec::Av1] {
        println!(
            "  {:<5} encode: {:<9} decode: {}",
            codec.as_str(),
            if caps.can_encode_hardware(codec) {
                "hardware"
            } else if caps.can_encode_software(codec) {
                "software"
            } else {
                "-"
            },
            if caps.can_decode(codec) {
                "software"
            } else {
                "-"
            }
        );
    }
    println!("audio:");
    for codec in [AudioCodec::Opus, AudioCodec::Vorbis] {
        println!(
            "  {:<6} encode: {:<9} decode: {:<9} delay ~{} ms",
            codec.as_str(),
            if caps.can_encode_audio(codec) {
                "software"
            } else {
                "-"
            },
            if caps.can_decode_audio(codec) {
                "software"
            } else {
                "-"
            },
            codec.typical_delay_ms()
        );
    }
}

#[test]
fn every_codec_has_a_working_path_on_this_host() {
    let caps = Capabilities::probe(None);

    // Each video codec must be encodable somehow: hardware where the driver
    // offers it, software otherwise. A codec with neither would be one the
    // negotiation could offer and then fail to honour.
    for codec in [VideoCodec::H264, VideoCodec::Vp9, VideoCodec::Av1] {
        assert!(
            caps.can_encode(codec),
            "{} has no encode path at all",
            codec.as_str()
        );
        assert!(caps.can_decode(codec), "{} has no decoder", codec.as_str());
    }

    // Vorbis is the guaranteed audio fallback: libvorbisenc is linked
    // directly, so it cannot be missing.
    assert!(caps.can_encode_audio(AudioCodec::Vorbis));
    assert!(caps.can_decode_audio(AudioCodec::Vorbis));
}

#[test]
fn the_capability_probe_never_fails() {
    // An absent or unusable GPU is a result, not an error: every codec still
    // has a software path, so the caller must never have to handle a probe
    // failure as fatal.
    let caps = Capabilities::probe(Some(std::path::Path::new("/dev/null/nonexistent")));
    assert!(caps.vaapi.is_none());
    assert!(caps.vaapi_error.is_some());
    assert!(caps.can_encode(VideoCodec::H264), "software must remain");
}
