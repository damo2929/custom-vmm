//! Codec negotiation — picking the cheapest pair the two ends share.
//!
//! §7.1 names H.264 and Vorbis outright. This module replaces that fixed
//! choice with a negotiated one, because the right codec is not a property
//! of the specification: it depends on what silicon each end has, and on how
//! the distribution built its drivers. On one host tested here, stock Fedora
//! Mesa offers hardware AV1 encode and *no* hardware H.264 at all — so
//! obeying §7.1 literally would burn a CPU core to send a worse picture.
//!
//! # How the choice is made
//!
//! The server advertises what it can encode ([`Offer`]); the client answers
//! with what it can decode ([`Answer`]); [`select`] scores every codec both
//! ends support and takes the lowest. The chosen codec, and the reason, go
//! in the SDP and the boot log so the decision is never a mystery.
//!
//! ## The cost model
//!
//! The goal is least resources and lowest latency, in that order. Those pull
//! in opposite directions exactly once — a hardware encoder saves an entire
//! CPU core but adds one frame of pipeline delay — and the model resolves it
//! in favour of hardware, because 33 ms at 30 fps is far cheaper than the
//! core, and because a saturated CPU adds far more than 33 ms of jitter.
//!
//! Scores are **ordinal**, not measured throughput. They encode the relative
//! cost of the operating points this tree actually configures — libx264
//! `veryfast`/`zerolatency`, libvpx `realtime cpu-used=8`, SVT-AV1
//! `preset 12` — and are meant to order the options correctly, not to
//! predict frame times. Each is justified where it is defined.

use std::fmt;
use vmm_codec_sys::{AudioCodec, Capabilities, Placement, VideoCodec};

// ---------------------------------------------------------------------------
// Cost constants
// ---------------------------------------------------------------------------

/// Encoding on a fixed-function engine. Not zero — it still costs the upload
/// and the driver round trip — but an order below any software encoder.
const ENCODE_HARDWARE: u32 = 10;

/// Software encode cost, per codec, at the operating points configured in
/// `vmm-codec-sys`.
///
/// The ordering is the part that matters and it is not controversial:
/// libx264 `veryfast` is the cheapest of the three by a wide margin, libvpx
/// `realtime` at `cpu-used=8` is roughly twice its cost, and SVT-AV1 at
/// `preset 12` is dearer again. All three can sustain 1080p30 on a modern
/// multi-core CPU; none can do it for free.
const fn encode_software(codec: VideoCodec) -> u32 {
    match codec {
        VideoCodec::H264 => 100,
        VideoCodec::Vp9 => 220,
        VideoCodec::Av1 => 260,
    }
}

/// Decode cost, per codec. All decoding here is software.
///
/// These are close together and deliberately so: at §7.1's 1080p/2 Mbps
/// every one of these decoders costs a fraction of a core. dav1d is placed
/// *below* the H.264 decoder because it genuinely is faster at this
/// resolution — which is why AV1 is not penalised on the client side.
const fn decode_cost(codec: VideoCodec) -> u32 {
    match codec {
        VideoCodec::H264 => 30,
        VideoCodec::Vp9 => 35,
        VideoCodec::Av1 => 25,
    }
}

/// Bits needed for equal quality, relative to H.264 at 100.
///
/// This is the one term that is not about CPU. Against §7.1's hard 2000 kbps
/// ceiling a more efficient codec does not save bandwidth — it spends the
/// same budget on a better picture — so it is weighted low and acts mainly
/// as a tiebreak between options that cost the same to run.
const fn bitrate_index(codec: VideoCodec) -> u32 {
    match codec {
        VideoCodec::H264 => 100,
        VideoCodec::Vp9 => 70,
        VideoCodec::Av1 => 55,
    }
}

/// Penalty for the extra frame a hardware encoder holds in its pipeline.
///
/// Deliberately small relative to [`encode_software`]: it must be able to
/// break a tie between two hardware options, but never outweigh the cost of
/// falling back to a CPU encoder.
const HARDWARE_LATENCY_PENALTY: u32 = 15;

/// Weight applied to [`bitrate_index`] when summing. A tenth, so a 45-point
/// efficiency gap moves the score by 4 — enough to separate otherwise equal
/// candidates, not enough to override a placement difference.
const BITRATE_WEIGHT: u32 = 10;

// ---------------------------------------------------------------------------
// Offers and answers
// ---------------------------------------------------------------------------

/// One codec the server can encode, and how.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoCandidate {
    pub codec: VideoCodec,
    pub placement: Placement,
}

impl VideoCandidate {
    /// The score for this candidate paired with a client that will decode it.
    ///
    /// Lower is better. See the module documentation for what the terms mean.
    pub fn score(&self) -> u32 {
        let encode = match self.placement {
            Placement::Hardware => ENCODE_HARDWARE + HARDWARE_LATENCY_PENALTY,
            Placement::Software => encode_software(self.codec),
        };
        encode + decode_cost(self.codec) + bitrate_index(self.codec) / BITRATE_WEIGHT
    }
}

/// What the server can encode.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Offer {
    pub video: Vec<VideoCandidate>,
    pub audio: Vec<AudioCodec>,
}

impl Offer {
    /// Build an offer from a probed host.
    pub fn from_capabilities(caps: &Capabilities) -> Self {
        let mut video = Vec::new();
        for codec in [VideoCodec::H264, VideoCodec::Vp9, VideoCodec::Av1] {
            if caps.can_encode_hardware(codec) {
                video.push(VideoCandidate {
                    codec,
                    placement: Placement::Hardware,
                });
            } else if caps.can_encode_software(codec) {
                video.push(VideoCandidate {
                    codec,
                    placement: Placement::Software,
                });
            }
        }
        Offer {
            video,
            audio: caps.encodable_audio(),
        }
    }
}

/// What the client can decode.
///
/// Order carries no meaning: the server scores, not the client. A client
/// that wants to force a codec advertises only that one.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Answer {
    pub video: Vec<VideoCodec>,
    pub audio: Vec<AudioCodec>,
}

impl Answer {
    /// Build an answer from a probed host.
    pub fn from_capabilities(caps: &Capabilities) -> Self {
        Answer {
            video: caps.decodable_video(),
            audio: caps.decodable_audio(),
        }
    }

    /// The answer a client sends over the wire, as an RTSP header value.
    ///
    /// `video=av1,vp9,h264;audio=opus,vorbis` — a deliberately dull format,
    /// because it has to survive being read by a human debugging a session.
    pub fn to_header(&self) -> String {
        let video: Vec<&str> = self.video.iter().map(|c| c.as_str()).collect();
        let audio: Vec<&str> = self.audio.iter().map(|c| c.as_str()).collect();
        format!("video={};audio={}", video.join(","), audio.join(","))
    }

    /// What a client that sends no capability header is assumed to support.
    ///
    /// Revision A of the specification fixed the codecs at H.264 and Vorbis,
    /// so a client built against it advertises nothing and expects exactly
    /// those. Defaulting to the empty set instead would hand it error 5011
    /// and break every pre-negotiation client on the first upgrade.
    pub fn legacy() -> Self {
        Answer {
            video: vec![VideoCodec::H264],
            audio: vec![AudioCodec::Vorbis],
        }
    }

    /// The answer to negotiate against, given the DESCRIBE's capability
    /// header if it carried one.
    ///
    /// An absent header means a Revision A client, so it takes the legacy
    /// set. A header that is *present* is taken at face value even when
    /// nothing in it is recognised: that client has stated what it decodes,
    /// and answering it with H.264 it never asked for would be worse than
    /// telling it plainly that there is no codec in common.
    pub fn from_request_header(value: Option<&str>) -> Self {
        match value {
            Some(header) => Answer::from_header(header),
            None => Answer::legacy(),
        }
    }

    /// Parse the header a client sent.
    ///
    /// Unknown codec names are ignored rather than rejected: a newer client
    /// advertising something this server has never heard of must still get a
    /// session, using whatever else it listed.
    pub fn from_header(value: &str) -> Self {
        let mut answer = Answer::default();
        for part in value.split(';') {
            let Some((key, list)) = part.split_once('=') else {
                continue;
            };
            for name in list.split(',').map(str::trim).filter(|s| !s.is_empty()) {
                match key.trim() {
                    "video" => {
                        if let Some(codec) = video_from_str(name) {
                            if !answer.video.contains(&codec) {
                                answer.video.push(codec);
                            }
                        }
                    }
                    "audio" => {
                        if let Some(codec) = audio_from_str(name) {
                            if !answer.audio.contains(&codec) {
                                answer.audio.push(codec);
                            }
                        }
                    }
                    _ => {}
                }
            }
        }
        answer
    }
}

pub fn video_from_str(name: &str) -> Option<VideoCodec> {
    match name.to_ascii_lowercase().as_str() {
        "h264" | "avc" => Some(VideoCodec::H264),
        "vp9" => Some(VideoCodec::Vp9),
        "av1" => Some(VideoCodec::Av1),
        _ => None,
    }
}

pub fn audio_from_str(name: &str) -> Option<AudioCodec> {
    match name.to_ascii_lowercase().as_str() {
        "opus" => Some(AudioCodec::Opus),
        "vorbis" => Some(AudioCodec::Vorbis),
        _ => None,
    }
}

// ---------------------------------------------------------------------------
// Selection
// ---------------------------------------------------------------------------

/// What a session settled on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    pub video: VideoCodec,
    pub video_placement: Placement,
    pub audio: AudioCodec,
    /// Why, in one line, for the boot log and the session record.
    pub reason: String,
}

impl fmt::Display for Selection {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} ({}) + {} — {}",
            self.video.as_str(),
            match self.video_placement {
                Placement::Hardware => "hardware",
                Placement::Software => "software",
            },
            self.audio.as_str(),
            self.reason
        )
    }
}

/// Why no session could be formed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum NoCommonCodec {
    Video {
        offered: Vec<String>,
        wanted: Vec<String>,
    },
    Audio {
        offered: Vec<String>,
        wanted: Vec<String>,
    },
}

impl fmt::Display for NoCommonCodec {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (kind, offered, wanted) = match self {
            NoCommonCodec::Video { offered, wanted } => ("video", offered, wanted),
            NoCommonCodec::Audio { offered, wanted } => ("audio", offered, wanted),
        };
        write!(
            f,
            "no {kind} codec in common: this server encodes [{}], the client decodes [{}]",
            offered.join(", "),
            wanted.join(", ")
        )
    }
}

impl std::error::Error for NoCommonCodec {}

impl From<NoCommonCodec> for libvmm_core::error::MediaError {
    fn from(value: NoCommonCodec) -> Self {
        let (stream, offered, wanted) = match value {
            NoCommonCodec::Video { offered, wanted } => ("video", offered, wanted),
            NoCommonCodec::Audio { offered, wanted } => ("audio", offered, wanted),
        };
        libvmm_core::error::MediaError::NoCommonCodec {
            stream,
            offered: offered.join(", "),
            wanted: wanted.join(", "),
        }
    }
}

/// Choose the cheapest codec pair both ends support.
pub fn select(offer: &Offer, answer: &Answer) -> Result<Selection, NoCommonCodec> {
    let best = offer
        .video
        .iter()
        .filter(|candidate| answer.video.contains(&candidate.codec))
        // min_by_key takes the first minimum, and `offer.video` is built in
        // a fixed codec order, so an exact tie resolves the same way every
        // time rather than depending on iteration order.
        .min_by_key(|candidate| candidate.score())
        .ok_or_else(|| NoCommonCodec::Video {
            offered: offer
                .video
                .iter()
                .map(|c| c.codec.as_str().to_string())
                .collect(),
            wanted: answer
                .video
                .iter()
                .map(|c| c.as_str().to_string())
                .collect(),
        })?;

    // Audio is a straight preference: Opus unless the client cannot take it.
    // There is no cost model here because there is nothing to trade — Opus
    // is cheaper, lower-delay and needs no out-of-band configuration, so it
    // wins on every axis at once.
    let audio = [AudioCodec::Opus, AudioCodec::Vorbis]
        .into_iter()
        .find(|codec| offer.audio.contains(codec) && answer.audio.contains(codec))
        .ok_or_else(|| NoCommonCodec::Audio {
            offered: offer.audio.iter().map(|c| c.as_str().to_string()).collect(),
            wanted: answer
                .audio
                .iter()
                .map(|c| c.as_str().to_string())
                .collect(),
        })?;

    Ok(Selection {
        video: best.codec,
        video_placement: best.placement,
        audio,
        reason: explain(best, offer, answer, audio),
    })
}

/// One line saying why this pair won, naming the runner-up when there was one.
fn explain(chosen: &VideoCandidate, offer: &Offer, answer: &Answer, audio: AudioCodec) -> String {
    let mut viable: Vec<&VideoCandidate> = offer
        .video
        .iter()
        .filter(|c| answer.video.contains(&c.codec))
        .collect();
    viable.sort_by_key(|c| c.score());

    let placement = match chosen.placement {
        Placement::Hardware => "hardware encode",
        Placement::Software => "software encode",
    };

    let video_reason = match viable.get(1) {
        Some(runner_up) => format!(
            "{} chosen for {} (score {}); next best {} at {}",
            chosen.codec.as_str(),
            placement,
            chosen.score(),
            runner_up.codec.as_str(),
            runner_up.score()
        ),
        None => format!(
            "{} is the only video codec in common ({})",
            chosen.codec.as_str(),
            placement
        ),
    };

    let audio_reason = match audio {
        AudioCodec::Opus => "opus preferred: lower delay and no out-of-band configuration",
        AudioCodec::Vorbis => "vorbis: the client cannot decode opus",
    };

    format!("{video_reason}; {audio_reason}")
}
