//! Encoder configuration, shared by every codec and backend.

use crate::error::{CodecError, Result};
use crate::frame::check_even_geometry;

/// The video codecs §7.1's stream can carry.
///
/// H.264 is the specification's own choice and stays the default. VP9 and
/// AV1 are royalty-free alternatives that compress better at the same
/// bitrate, which matters against a 2000 kbps ceiling — and, unlike H.264,
/// they are present in a stock Fedora Mesa build, so hardware encode works
/// without adding a third-party repository.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum VideoCodec {
    #[default]
    H264,
    Vp9,
    Av1,
}

impl VideoCodec {
    pub const fn as_str(self) -> &'static str {
        match self {
            VideoCodec::H264 => "h264",
            VideoCodec::Vp9 => "vp9",
            VideoCodec::Av1 => "av1",
        }
    }

    /// The name this codec takes in an SDP `a=rtpmap` line (RFC 4855 and
    /// the VP9/AV1 payload registrations), which is case-sensitive.
    pub const fn rtp_encoding_name(self) -> &'static str {
        match self {
            VideoCodec::H264 => "H264",
            VideoCodec::Vp9 => "VP9",
            VideoCodec::Av1 => "AV1",
        }
    }

    /// The libavcodec encoder to use on the VA-API path.
    pub const fn vaapi_encoder(self) -> &'static str {
        match self {
            VideoCodec::H264 => "h264_vaapi",
            VideoCodec::Vp9 => "vp9_vaapi",
            VideoCodec::Av1 => "av1_vaapi",
        }
    }

    /// The software encoder to use when hardware is unavailable.
    ///
    /// H.264 is the exception: it goes through the libx264 shim rather than
    /// libavcodec, because that path is tuned for latency and is the one
    /// this tree has verified most heavily.
    pub const fn software_encoder(self) -> Option<&'static str> {
        match self {
            VideoCodec::H264 => None,
            // libvpx's realtime deadline is what makes VP9 usable live;
            // the default "good" deadline is far too slow for a console.
            VideoCodec::Vp9 => Some("libvpx-vp9"),
            // SVT-AV1 is the only AV1 encoder fast enough for live use.
            VideoCodec::Av1 => Some("libsvtav1"),
        }
    }
}

/// The hard ceiling from §7.1. Not configurable upwards, in either backend.
pub const HARD_CAP_KBPS: u32 = 2000;

/// §7.1: a keyframe at least every two seconds. A GOP longer than this many
/// frames could not deliver that even at the full configured frame rate.
pub const MAX_KEYFRAME_SECONDS: u32 = 2;

/// Which backend §7.1's `hardware_accelerator` asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Accelerator {
    /// VA-API, falling back to software when the host cannot encode H.264.
    Vaapi,
    /// NVENC. Not implemented; falls back to software with a warning.
    Nvenc,
    /// libx264, chosen explicitly.
    Software,
}

impl Accelerator {
    pub const fn as_str(self) -> &'static str {
        match self {
            Accelerator::Vaapi => "vaapi",
            Accelerator::Nvenc => "nvenc",
            Accelerator::Software => "software",
        }
    }
}

/// Constrained VBR, as §7.1 requires: `target_kbps` is the average the
/// encoder aims for and `max_kbps` is programmed as a hard ceiling that
/// instantaneous output must stay below.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EncoderConfig {
    pub codec: VideoCodec,
    pub width: u32,
    pub height: u32,
    pub framerate: u32,
    pub target_kbps: u32,
    pub max_kbps: u32,
    /// HRD/VBV buffer size in bits. §7.1 sizes it to the ceiling.
    pub vbv_buffer_bits: u32,
    /// Frames between IDRs. §7.1 asks for one keyframe every two seconds.
    pub gop_length: u32,
    pub accelerator: Accelerator,
}

impl EncoderConfig {
    /// Validate everything both backends assume, so neither has to repeat it.
    pub fn validate(&self) -> Result<()> {
        check_even_geometry("EncoderConfig", self.width, self.height)?;

        if self.max_kbps > HARD_CAP_KBPS {
            return Err(CodecError::invalid(
                "EncoderConfig",
                format!(
                    "max_kbps {} exceeds the hard {HARD_CAP_KBPS} kbps ceiling (§7.1)",
                    self.max_kbps
                ),
            ));
        }
        if self.target_kbps == 0 || self.max_kbps == 0 {
            return Err(CodecError::invalid(
                "EncoderConfig",
                "target_kbps and max_kbps must both be non-zero",
            ));
        }
        if self.target_kbps >= self.max_kbps {
            return Err(CodecError::invalid(
                "EncoderConfig",
                format!(
                    "target {} kbps must stay below the {} kbps ceiling",
                    self.target_kbps, self.max_kbps
                ),
            ));
        }
        if self.vbv_buffer_bits == 0 {
            return Err(CodecError::invalid(
                "EncoderConfig",
                "vbv_buffer_bits must be non-zero: without an HRD buffer the \
                 encoder cannot hold instantaneous output below the ceiling",
            ));
        }
        if self.framerate == 0 {
            return Err(CodecError::invalid(
                "EncoderConfig",
                "framerate must be non-zero",
            ));
        }
        if self.gop_length == 0 {
            return Err(CodecError::invalid(
                "EncoderConfig",
                "gop_length must be non-zero",
            ));
        }
        // A GOP longer than two seconds' worth of frames cannot satisfy
        // §7.1 even when the guest renders at the full rate. Callers that
        // fall behind the rate are covered separately, by forcing an IDR on
        // the clock; this catches the configuration being wrong to begin
        // with.
        let ceiling = self.framerate.saturating_mul(MAX_KEYFRAME_SECONDS);
        if self.gop_length > ceiling {
            return Err(CodecError::invalid(
                "EncoderConfig",
                format!(
                    "a GOP of {} frames at {} fps is {:.1} seconds; §7.1 requires a \
                     keyframe at least every {MAX_KEYFRAME_SECONDS} seconds \
                     (at most {ceiling} frames)",
                    self.gop_length,
                    self.framerate,
                    self.gop_length as f64 / self.framerate as f64
                ),
            ));
        }
        Ok(())
    }
}

/// One encoded access unit, in Annex-B byte-stream format.
///
/// Annex-B is what §7.3's RTP packetiser wants: RFC 6184 start-code
/// splitting operates directly on it, and the client's decoder accepts it
/// unchanged after depacketisation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EncodedFrame {
    pub data: Vec<u8>,
    /// An IDR, which the client may start decoding from.
    pub keyframe: bool,
    pub pts: i64,
    pub dts: i64,
}

impl EncodedFrame {
    /// Instantaneous rate this frame represents, for the §7.1 ceiling check.
    pub fn instantaneous_kbps(&self, framerate: u32) -> u32 {
        let bits = (self.data.len() as u64).saturating_mul(8);
        let per_second = bits.saturating_mul(framerate as u64);
        (per_second / 1000).min(u32::MAX as u64) as u32
    }
}
