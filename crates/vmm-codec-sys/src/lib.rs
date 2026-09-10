//! Codec bindings for §7.1's capture/encode pipeline and the client's
//! decode path.
//!
//! §1.1 originally forbade linking C, which left every codec here with no
//! implementable path. That rule was lifted. This crate is the entire C
//! surface for media: the rest of the tree sees only safe Rust types, and
//! `unsafe` does not leak past this boundary.
//!
//! # Codecs
//!
//! | | codecs | encode | decode |
//! |---|---|---|---|
//! | [`video`] | H.264, VP9, AV1 | VA-API, else software | software |
//! | [`audio`] | Opus, Vorbis | software | software |
//!
//! None of these is fixed by configuration. [`Capabilities`] reports what
//! this host can actually do — which is a question only the driver can
//! answer, since a GPU's codec support varies by silicon *and* by how the
//! distribution built Mesa — and `libvmm-media` negotiates the cheapest
//! codec the two ends share.

pub mod raw;

pub mod audio;
mod decoder;
mod error;
mod frame;
pub mod logging;
mod scale;
pub mod video;

pub use audio::{
    AudioCodec, AudioConfig, FrameDuration, OpusDecoder, OpusEncoder, OpusPacket, VorbisDecoder,
    VorbisEncoder, VorbisHeaders, CHANNELS, SAMPLE_RATE,
};
pub use decoder::VideoDecoder;
pub use error::{CodecError, Result};
pub use frame::{PackedFormat, PackedFrame, Yuv420Frame};
pub use scale::Scaler;
pub use video::{
    Accelerator, Backend, Capabilities, EncodedFrame, EncoderConfig, Placement, VaapiCapability,
    VideoCodec, VideoEncoder, HARD_CAP_KBPS, MAX_KEYFRAME_SECONDS,
};
