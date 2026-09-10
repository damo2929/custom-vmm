//! Codec negotiation: the cheapest pair both ends share.

use libvmm_media::negotiate::{select, Answer, NoCommonCodec, Offer, VideoCandidate};
use vmm_codec_sys::{AudioCodec, Capabilities, Placement, VideoCodec};

fn candidate(codec: VideoCodec, placement: Placement) -> VideoCandidate {
    VideoCandidate { codec, placement }
}

fn answer(video: &[VideoCodec], audio: &[AudioCodec]) -> Answer {
    Answer {
        video: video.to_vec(),
        audio: audio.to_vec(),
    }
}

const ALL_VIDEO: [VideoCodec; 3] = [VideoCodec::H264, VideoCodec::Vp9, VideoCodec::Av1];
const ALL_AUDIO: [AudioCodec; 2] = [AudioCodec::Opus, AudioCodec::Vorbis];

#[test]
fn hardware_beats_software_even_for_a_less_efficient_codec() {
    // The central trade: H.264 in hardware against AV1 on the CPU. Hardware
    // wins, because a whole core costs far more than the bitrate difference.
    let offer = Offer {
        video: vec![
            candidate(VideoCodec::H264, Placement::Hardware),
            candidate(VideoCodec::Av1, Placement::Software),
        ],
        audio: ALL_AUDIO.to_vec(),
    };
    let selection = select(&offer, &answer(&ALL_VIDEO, &ALL_AUDIO)).expect("a pair exists");
    assert_eq!(selection.video, VideoCodec::H264);
    assert_eq!(selection.video_placement, Placement::Hardware);
}

#[test]
fn the_most_efficient_codec_wins_among_equal_hardware() {
    // Both in hardware, so encode cost is identical and the tiebreak is
    // compression: AV1 needs the fewest bits for the same picture.
    let offer = Offer {
        video: vec![
            candidate(VideoCodec::H264, Placement::Hardware),
            candidate(VideoCodec::Av1, Placement::Hardware),
        ],
        audio: ALL_AUDIO.to_vec(),
    };
    let selection = select(&offer, &answer(&ALL_VIDEO, &ALL_AUDIO)).expect("a pair exists");
    assert_eq!(selection.video, VideoCodec::Av1);
    assert_eq!(selection.video_placement, Placement::Hardware);
}

#[test]
fn the_cheapest_software_encoder_wins_when_there_is_no_hardware() {
    // All software: libx264 veryfast is far cheaper than libvpx or SVT-AV1,
    // and on a CPU-bound host that outweighs AV1's better compression.
    let offer = Offer {
        video: ALL_VIDEO
            .iter()
            .map(|c| candidate(*c, Placement::Software))
            .collect(),
        audio: ALL_AUDIO.to_vec(),
    };
    let selection = select(&offer, &answer(&ALL_VIDEO, &ALL_AUDIO)).expect("a pair exists");
    assert_eq!(selection.video, VideoCodec::H264);
    assert_eq!(selection.video_placement, Placement::Software);
}

#[test]
fn a_client_that_cannot_decode_the_best_codec_gets_the_next_one() {
    let offer = Offer {
        video: vec![
            candidate(VideoCodec::Av1, Placement::Hardware),
            candidate(VideoCodec::H264, Placement::Software),
        ],
        audio: ALL_AUDIO.to_vec(),
    };
    // The client is old and only knows H.264, so the hardware AV1 encoder
    // is unusable however cheap it is.
    let selection =
        select(&offer, &answer(&[VideoCodec::H264], &ALL_AUDIO)).expect("a pair exists");
    assert_eq!(selection.video, VideoCodec::H264);
    assert_eq!(selection.video_placement, Placement::Software);
}

#[test]
fn opus_is_preferred_and_vorbis_is_the_fallback() {
    let offer = Offer {
        video: vec![candidate(VideoCodec::H264, Placement::Software)],
        audio: ALL_AUDIO.to_vec(),
    };

    let with_opus = select(&offer, &answer(&ALL_VIDEO, &ALL_AUDIO)).expect("a pair exists");
    assert_eq!(with_opus.audio, AudioCodec::Opus);

    let without_opus =
        select(&offer, &answer(&ALL_VIDEO, &[AudioCodec::Vorbis])).expect("vorbis must still work");
    assert_eq!(without_opus.audio, AudioCodec::Vorbis);
    assert!(
        without_opus.reason.contains("cannot decode opus"),
        "the fallback must say why: {}",
        without_opus.reason
    );
}

#[test]
fn no_common_video_codec_is_reported_with_both_lists() {
    let offer = Offer {
        video: vec![candidate(VideoCodec::Av1, Placement::Hardware)],
        audio: ALL_AUDIO.to_vec(),
    };
    let error =
        select(&offer, &answer(&[VideoCodec::H264], &ALL_AUDIO)).expect_err("nothing in common");
    assert!(matches!(error, NoCommonCodec::Video { .. }));
    let text = error.to_string();
    assert!(text.contains("av1") && text.contains("h264"), "{text}");
}

#[test]
fn no_common_audio_codec_is_reported_separately() {
    let offer = Offer {
        video: vec![candidate(VideoCodec::H264, Placement::Software)],
        audio: vec![AudioCodec::Opus],
    };
    let error =
        select(&offer, &answer(&ALL_VIDEO, &[AudioCodec::Vorbis])).expect_err("nothing in common");
    assert!(matches!(error, NoCommonCodec::Audio { .. }));
}

#[test]
fn the_selection_explains_itself() {
    let offer = Offer {
        video: vec![
            candidate(VideoCodec::Av1, Placement::Hardware),
            candidate(VideoCodec::H264, Placement::Software),
        ],
        audio: ALL_AUDIO.to_vec(),
    };
    let selection = select(&offer, &answer(&ALL_VIDEO, &ALL_AUDIO)).expect("a pair exists");
    let reason = &selection.reason;
    assert!(reason.contains("av1"), "{reason}");
    assert!(reason.contains("hardware encode"), "{reason}");
    assert!(reason.contains("next best"), "{reason}");
    // Displayable in one line for the boot log.
    assert!(selection.to_string().starts_with("av1 (hardware)"));
}

#[test]
fn the_capability_header_round_trips() {
    let original = answer(
        &[VideoCodec::Av1, VideoCodec::H264],
        &[AudioCodec::Opus, AudioCodec::Vorbis],
    );
    let header = original.to_header();
    assert_eq!(header, "video=av1,h264;audio=opus,vorbis");
    assert_eq!(Answer::from_header(&header), original);
}

#[test]
fn an_unknown_codec_name_is_ignored_rather_than_fatal() {
    // A newer client advertising something this server has never heard of
    // must still get a session using the rest of its list.
    let parsed = Answer::from_header("video=h266,av1,h264;audio=flac,opus");
    assert_eq!(parsed.video, vec![VideoCodec::Av1, VideoCodec::H264]);
    assert_eq!(parsed.audio, vec![AudioCodec::Opus]);
}

#[test]
fn a_malformed_header_yields_an_empty_answer_rather_than_panicking() {
    for text in ["", "garbage", "video", "=;=", "video=;audio="] {
        let parsed = Answer::from_header(text);
        assert!(parsed.video.is_empty(), "{text:?} produced {parsed:?}");
        assert!(parsed.audio.is_empty(), "{text:?} produced {parsed:?}");
    }
}

#[test]
fn duplicate_entries_are_collapsed() {
    let parsed = Answer::from_header("video=av1,av1,AV1;audio=opus,opus");
    assert_eq!(parsed.video, vec![VideoCodec::Av1]);
    assert_eq!(parsed.audio, vec![AudioCodec::Opus]);
}

#[test]
fn a_real_host_negotiates_with_itself() {
    // The end-to-end shape: probe this machine, offer what it can encode,
    // answer with what it can decode, and check the result is coherent.
    let caps = Capabilities::probe(None);
    let offer = Offer::from_capabilities(&caps);
    let answer = Answer::from_capabilities(&caps);

    let selection = select(&offer, &answer).expect("a host must be able to serve itself");
    println!("this host would negotiate: {selection}");

    assert!(
        caps.can_encode(selection.video),
        "chose a video codec this host cannot encode"
    );
    assert!(
        caps.can_decode(selection.video),
        "chose a video codec this host cannot decode"
    );
    assert!(caps.can_encode_audio(selection.audio));
    assert!(caps.can_decode_audio(selection.audio));

    // Hardware must be preferred whenever it exists for the chosen codec.
    if caps.can_encode_hardware(selection.video) {
        assert_eq!(selection.video_placement, Placement::Hardware);
    }
    // And if any codec has hardware encode, the chosen one must too —
    // otherwise the negotiation passed up free CPU.
    let any_hardware = [VideoCodec::H264, VideoCodec::Vp9, VideoCodec::Av1]
        .iter()
        .any(|c| caps.can_encode_hardware(*c) && answer.video.contains(c));
    if any_hardware {
        assert_eq!(
            selection.video_placement,
            Placement::Hardware,
            "hardware encode was available but software was chosen"
        );
    }
}

#[test]
fn no_common_codec_carries_error_5011_and_names_both_sets() {
    // A client that decodes only AV1 against a server that encodes only
    // H.264. The operator needs to see both lists to fix it.
    let offer = Offer {
        video: vec![candidate(VideoCodec::H264, Placement::Software)],
        audio: vec![AudioCodec::Opus],
    };
    let client = answer(&[VideoCodec::Av1], &[AudioCodec::Opus]);

    let error = select(&offer, &client).unwrap_err();
    let media: libvmm_core::error::MediaError = error.into();

    assert_eq!(media.code(), 5011);
    let text = media.to_string();
    assert!(
        text.contains("h264"),
        "must name what the server offers: {text}"
    );
    assert!(
        text.contains("av1"),
        "must name what the client accepts: {text}"
    );
}

#[test]
fn a_client_that_sends_no_capability_header_gets_the_revision_a_stream() {
    // The backwards-compatibility guarantee. A pre-negotiation client sends
    // no header at all; it must still get H.264 and Vorbis, not error 5011.
    let offer = Offer {
        video: ALL_VIDEO
            .iter()
            .map(|c| candidate(*c, Placement::Software))
            .collect(),
        audio: ALL_AUDIO.to_vec(),
    };

    let selection = select(&offer, &Answer::from_request_header(None)).unwrap();

    assert_eq!(selection.video, VideoCodec::H264);
    assert_eq!(selection.audio, AudioCodec::Vorbis);
}

#[test]
fn a_present_header_is_taken_at_face_value_even_if_nothing_is_recognised() {
    // Distinct from the absent-header case: this client *said* what it
    // decodes. Silently giving it H.264 would be answering a question it
    // did not ask.
    let offer = Offer {
        video: vec![candidate(VideoCodec::H264, Placement::Software)],
        audio: vec![AudioCodec::Vorbis],
    };
    let client = Answer::from_request_header(Some("video=h266;audio=mp3"));

    assert!(client.video.is_empty());
    assert!(matches!(
        select(&offer, &client).unwrap_err(),
        NoCommonCodec::Video { .. }
    ));
}
