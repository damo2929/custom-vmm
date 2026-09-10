//! Video encode for §7.1 — three codecs, hardware first with a software
//! fallback for each.
//!
//! | codec | hardware | software |
//! |---|---|---|
//! | H.264 | `h264_vaapi` | libx264, through the shim in `shim/` |
//! | VP9 | `vp9_vaapi` | `libvpx-vp9`, realtime deadline |
//! | AV1 | `av1_vaapi` | `libsvtav1`, preset 12, low-delay |
//!
//! H.264 is the one codec whose software path does not go through
//! libavcodec: the libx264 shim is tuned `zerolatency` and returns every
//! frame on the call that submitted it, which nothing else here matches.
//!
//! Which codec to use is not decided here. [`Capabilities`] reports what
//! this host can actually do, and `libvmm-media`'s negotiation picks from
//! the intersection of the two ends' capabilities — see its `negotiate`
//! module for the cost model.

mod av;
mod config;
pub mod probe;
mod x264;

pub use av::Placement;
pub use config::{
    Accelerator, EncodedFrame, EncoderConfig, VideoCodec, HARD_CAP_KBPS, MAX_KEYFRAME_SECONDS,
};
pub use probe::{probe as probe_vaapi, VaapiCapability};

use crate::error::{CodecError, Result};
use crate::frame::Yuv420Frame;
use av::AvEncoder;
use std::path::Path;
use x264::X264Encoder;

/// Which backend an encoder ended up on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Backend {
    Vaapi,
    Software,
}

impl Backend {
    pub const fn as_str(self) -> &'static str {
        match self {
            Backend::Vaapi => "vaapi",
            Backend::Software => "software",
        }
    }

    pub const fn is_hardware(self) -> bool {
        matches!(self, Backend::Vaapi)
    }
}

/// What this host can encode, and how.
///
/// Built once and consulted by the negotiation rather than re-probing: the
/// libva probe opens a DRM device, which is not something to do per frame or
/// per client.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Capabilities {
    /// `Ok` when a VA-API display was opened, `Err` with the reason if not.
    pub vaapi: Option<VaapiCapability>,
    /// Why VA-API is unavailable, when it is.
    pub vaapi_error: Option<String>,
}

impl Capabilities {
    /// Probe the host. Never fails: an absent or unusable GPU is a result,
    /// not an error, because every codec still has a software path.
    pub fn probe(render_node: Option<&Path>) -> Self {
        match probe::probe(render_node) {
            Ok(cap) => Capabilities {
                vaapi: Some(cap),
                vaapi_error: None,
            },
            Err(e) => Capabilities {
                vaapi: None,
                vaapi_error: Some(e.to_string()),
            },
        }
    }

    /// Can this host encode `codec` on a fixed-function engine?
    pub fn can_encode_hardware(&self, codec: VideoCodec) -> bool {
        self.vaapi.as_ref().is_some_and(|c| c.can_encode(codec))
    }

    /// Can this host encode `codec` at all?
    pub fn can_encode(&self, codec: VideoCodec) -> bool {
        self.can_encode_hardware(codec) || self.can_encode_software(codec)
    }

    /// Is a software encoder for `codec` present in this build?
    pub fn can_encode_software(&self, codec: VideoCodec) -> bool {
        match codec {
            // Always: the shim links libx264 directly.
            VideoCodec::H264 => true,
            _ => codec
                .software_encoder()
                .is_some_and(crate::decoder::encoder_is_available),
        }
    }

    /// Can this host decode `codec`? Decode is always software here — see
    /// [`crate::VideoDecoder`] for why.
    pub fn can_decode(&self, codec: VideoCodec) -> bool {
        crate::decoder::decoder_is_available(codec)
    }

    /// Can this host encode `codec`?
    ///
    /// Both audio codecs are software-only. Vorbis is always present, since
    /// libvorbisenc is linked directly; Opus depends on libavcodec having
    /// been built with the libopus wrapper, which is not universal.
    pub fn can_encode_audio(&self, codec: crate::audio::AudioCodec) -> bool {
        match codec {
            crate::audio::AudioCodec::Vorbis => true,
            crate::audio::AudioCodec::Opus => crate::decoder::encoder_is_available("libopus"),
        }
    }

    /// Can this host decode `codec`?
    pub fn can_decode_audio(&self, codec: crate::audio::AudioCodec) -> bool {
        crate::decoder::audio_decoder_is_available(codec)
    }

    /// Every video codec this host can encode, hardware or software.
    pub fn encodable_video(&self) -> Vec<VideoCodec> {
        [VideoCodec::H264, VideoCodec::Vp9, VideoCodec::Av1]
            .into_iter()
            .filter(|c| self.can_encode(*c))
            .collect()
    }

    /// Every video codec this host can decode.
    pub fn decodable_video(&self) -> Vec<VideoCodec> {
        [VideoCodec::H264, VideoCodec::Vp9, VideoCodec::Av1]
            .into_iter()
            .filter(|c| self.can_decode(*c))
            .collect()
    }

    /// Every audio codec this host can encode.
    pub fn encodable_audio(&self) -> Vec<crate::audio::AudioCodec> {
        [
            crate::audio::AudioCodec::Opus,
            crate::audio::AudioCodec::Vorbis,
        ]
        .into_iter()
        .filter(|c| self.can_encode_audio(*c))
        .collect()
    }

    /// Every audio codec this host can decode.
    pub fn decodable_audio(&self) -> Vec<crate::audio::AudioCodec> {
        [
            crate::audio::AudioCodec::Opus,
            crate::audio::AudioCodec::Vorbis,
        ]
        .into_iter()
        .filter(|c| self.can_decode_audio(*c))
        .collect()
    }
}

enum Inner {
    Av(Box<AvEncoder>),
    X264(Box<X264Encoder>),
}

/// The video encoder §7.1's capture pipeline drives.
pub struct VideoEncoder {
    inner: Inner,
    codec: VideoCodec,
    backend: Backend,
    /// What was chosen, for the boot log.
    selection: String,
    /// Set when a hardware request fell back, so the caller can surface it.
    fallback_reason: Option<String>,
}

impl std::fmt::Debug for VideoEncoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VideoEncoder")
            .field("codec", &self.codec.as_str())
            .field("backend", &self.backend.as_str())
            .field("acceleration", &self.selection)
            .field("fallback_reason", &self.fallback_reason)
            .finish()
    }
}

impl VideoEncoder {
    /// Open an encoder for `config.codec`, preferring hardware.
    ///
    /// The fallback to software is deliberately narrow. Only
    /// [`CodecError::Unavailable`] crosses over: a host that has the encoder
    /// but fails to open it has a real fault, and degrading silently would
    /// hide it behind a performance regression nobody would attribute
    /// correctly.
    pub fn open(config: EncoderConfig, render_node: Option<&Path>) -> Result<Self> {
        config.validate()?;

        match config.accelerator {
            Accelerator::Vaapi => match Self::open_hardware(config, render_node) {
                Ok(encoder) => Ok(encoder),
                Err(e) if e.is_recoverable() => Self::open_software(config, Some(e.to_string())),
                Err(e) => Err(e),
            },
            Accelerator::Nvenc => Self::open_software(
                config,
                Some(
                    "NVENC is not implemented in this build; §7.1's nvenc \
                     accelerator falls back to software"
                        .to_string(),
                ),
            ),
            Accelerator::Software => Self::open_software(config, None),
        }
    }

    fn open_hardware(config: EncoderConfig, render_node: Option<&Path>) -> Result<Self> {
        let capability = probe::probe(render_node)?;
        let encoder = AvEncoder::open_hardware(config, capability)?;
        let selection = encoder.acceleration();
        Ok(VideoEncoder {
            inner: Inner::Av(Box::new(encoder)),
            codec: config.codec,
            backend: Backend::Vaapi,
            selection,
            fallback_reason: None,
        })
    }

    fn open_software(config: EncoderConfig, fallback_reason: Option<String>) -> Result<Self> {
        // H.264 alone uses the shim; the other two use libavcodec.
        if config.codec == VideoCodec::H264 {
            let encoder = X264Encoder::open(config)?;
            let selection = encoder.acceleration();
            return Ok(VideoEncoder {
                inner: Inner::X264(Box::new(encoder)),
                codec: config.codec,
                backend: Backend::Software,
                selection,
                fallback_reason,
            });
        }
        let encoder = AvEncoder::open_software(config)?;
        let selection = encoder.acceleration();
        Ok(VideoEncoder {
            inner: Inner::Av(Box::new(encoder)),
            codec: config.codec,
            backend: Backend::Software,
            selection,
            fallback_reason,
        })
    }

    pub fn codec(&self) -> VideoCodec {
        self.codec
    }

    pub fn backend(&self) -> Backend {
        self.backend
    }

    /// Where the encoder runs: a fixed-function engine, or the CPU.
    pub fn placement(&self) -> Placement {
        match &self.inner {
            Inner::Av(e) => e.placement(),
            // The libx264 shim is only ever the software path.
            Inner::X264(_) => Placement::Software,
        }
    }

    /// What the encoder is actually running on, for the boot log.
    pub fn acceleration(&self) -> &str {
        &self.selection
    }

    /// Why a hardware request fell back to software, if it did.
    pub fn fallback_reason(&self) -> Option<&str> {
        self.fallback_reason.as_deref()
    }

    pub fn config(&self) -> &EncoderConfig {
        match &self.inner {
            Inner::Av(e) => e.config(),
            Inner::X264(e) => e.config(),
        }
    }

    /// Encode one frame. `Ok(None)` means the encoder buffered it.
    ///
    /// The backends differ and callers must handle both. The libx264 shim is
    /// tuned `zerolatency` and returns every frame on the call that
    /// submitted it. The VA-API encoders have a real pipeline — submit, GPU,
    /// retrieve — and return `None` for the first frame, then one frame per
    /// call. `async_depth` is pinned to 1 to hold that at its floor.
    pub fn encode(&mut self, frame: &Yuv420Frame) -> Result<Option<EncodedFrame>> {
        self.encode_with(frame, false)
    }

    /// Encode one frame, forcing a keyframe.
    ///
    /// §7.1 asks for one at least every two seconds. The encoders carry a
    /// GOP of `framerate * 2` frames, which delivers that only while the
    /// guest renders at the configured rate; a caller tracking elapsed time
    /// uses this to hold the interval to the wall clock instead.
    pub fn encode_keyframe(&mut self, frame: &Yuv420Frame) -> Result<Option<EncodedFrame>> {
        self.encode_with(frame, true)
    }

    fn encode_with(&mut self, frame: &Yuv420Frame, force: bool) -> Result<Option<EncodedFrame>> {
        match &mut self.inner {
            Inner::Av(e) => e.encode(frame, force),
            Inner::X264(e) => e.encode(frame, force),
        }
    }

    /// Drain one buffered frame. `Ok(None)` means the encoder is empty.
    pub fn flush(&mut self) -> Result<Option<EncodedFrame>> {
        match &mut self.inner {
            Inner::Av(e) => e.flush(),
            Inner::X264(e) => e.flush(),
        }
    }

    /// Drain everything the encoder is still holding.
    pub fn drain(&mut self) -> Result<Vec<EncodedFrame>> {
        let mut out = Vec::new();
        // Bounded so a backend that never reports empty cannot spin here. A
        // GOP's worth of delay is far beyond what any of these buffer.
        let limit = self.config().gop_length.max(1) as usize + 64;
        for _ in 0..limit {
            match self.flush()? {
                Some(frame) => out.push(frame),
                None => return Ok(out),
            }
        }
        Err(CodecError::process(
            "VideoEncoder::drain",
            format!("encoder still had frames buffered after {limit} flushes"),
        ))
    }
}
