//! The demux must follow what was negotiated (§7.6, Revision B).
//!
//! A session that agreed AV1 and then reassembled as H.264 produced a stream
//! of "malformed packet" warnings and no picture — indistinguishable from a
//! broken encoder. These pin the chain that prevents it: SDP encoding name →
//! codec → depacketiser.

use libvmm_media::depacketize::{audio_for, video_for};
use libvmm_media::encoder::{sdp, AudioEncoderParams, VideoEncoderParams};
use libvmm_media::negotiate::{audio_from_str, video_from_str};
use libvmm_media::rtsp::parse_sdp;
use vmm_codec_sys::{AudioCodec, VideoCodec};

fn params() -> (VideoEncoderParams, AudioEncoderParams) {
    let cfg = libvmm_config::MachineConfig::from_toml_str(include_str!(
        "../../../config/reference-vm.toml"
    ))
    .expect("the reference machine parses");
    (
        VideoEncoderParams::from_config(&cfg.display).expect("video params"),
        AudioEncoderParams::from_config(&cfg.display),
    )
}

#[test]
fn a_depacketiser_reports_the_codec_it_was_asked_for() {
    for codec in [VideoCodec::H264, VideoCodec::Vp9, VideoCodec::Av1] {
        assert_eq!(
            video_for(codec).codec(),
            codec,
            "video factory for {codec:?}"
        );
    }
    for codec in [AudioCodec::Opus, AudioCodec::Vorbis] {
        assert_eq!(
            audio_for(codec).codec(),
            codec,
            "audio factory for {codec:?}"
        );
    }
}

#[test]
fn every_negotiable_codec_survives_the_sdp_round_trip() {
    // The whole chain a client walks: the server announces what it selected,
    // the client reads it back and picks a depacketiser. Any codec that
    // negotiation can choose must come out the far end as itself.
    let (video, audio) = params();

    for v in [VideoCodec::H264, VideoCodec::Vp9, VideoCodec::Av1] {
        for a in [AudioCodec::Opus, AudioCodec::Vorbis] {
            let text = sdp("vm", "/live", &video, &audio, v, a, Some("AAAA"));
            let media = parse_sdp(&text);

            let announced_video = media
                .iter()
                .find(|m| m.kind == "video")
                .unwrap_or_else(|| panic!("no video section for {v:?}"));
            let announced_audio = media
                .iter()
                .find(|m| m.kind == "audio")
                .unwrap_or_else(|| panic!("no audio section for {a:?}"));

            let back_v = video_from_str(&announced_video.encoding)
                .unwrap_or_else(|| panic!("{:?} is unreadable", announced_video.encoding));
            let back_a = audio_from_str(&announced_audio.encoding)
                .unwrap_or_else(|| panic!("{:?} is unreadable", announced_audio.encoding));

            assert_eq!(back_v, v, "video codec survived the SDP");
            assert_eq!(back_a, a, "audio codec survived the SDP");
            assert_eq!(video_for(back_v).codec(), v);
            assert_eq!(audio_for(back_a).codec(), a);
        }
    }
}

#[test]
fn the_sdp_payload_type_matches_the_codec() {
    // Payload types are assigned per codec by Revision B §7.3.1. A mismatch
    // here would send the client's demux to the right codec on the wrong
    // channel payload type.
    let (video, audio) = params();
    for (v, pt) in [
        (VideoCodec::H264, 96u8),
        (VideoCodec::Vp9, 98),
        (VideoCodec::Av1, 99),
    ] {
        let text = sdp("vm", "/live", &video, &audio, v, AudioCodec::Opus, None);
        let media = parse_sdp(&text);
        let section = media.iter().find(|m| m.kind == "video").expect("video");
        assert_eq!(section.payload_type, pt, "{v:?} payload type");
    }
    for (a, pt) in [(AudioCodec::Vorbis, 97u8), (AudioCodec::Opus, 100)] {
        let text = sdp(
            "vm",
            "/live",
            &video,
            &audio,
            VideoCodec::H264,
            a,
            Some("AAAA"),
        );
        let media = parse_sdp(&text);
        let section = media.iter().find(|m| m.kind == "audio").expect("audio");
        assert_eq!(section.payload_type, pt, "{a:?} payload type");
    }
}
