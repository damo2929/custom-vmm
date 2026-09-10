//! Audio encode and decode for §7.1 — Opus preferred, Vorbis as fallback.
//!
//! §7.1 specifies Vorbis. Opus is offered ahead of it because it is a
//! straight improvement for this job — lower delay, less CPU, and an RTP
//! payload format that needs no out-of-band configuration — but Vorbis is
//! kept in full, not as a stub, because a client that cannot do Opus must
//! still get audio.
//!
//! Which one a session uses is decided by negotiation in `libvmm-media`, not
//! here: the server offers what it can encode, the client answers with what
//! it can decode, and the cheapest common codec wins.
//!
//! | | delay at 48 kHz | configuration | decoder start |
//! |---|---|---|---|
//! | Opus 20 ms | ~20 ms | none | any packet |
//! | Vorbis | ~46 ms | 3 header packets, out of band | after the headers |

pub mod opus;
pub mod vorbis;

pub use opus::{
    AudioConfig, FrameDuration, OpusDecoder, OpusEncoder, OpusPacket, CHANNELS, SAMPLE_RATE,
};
pub use vorbis::{VorbisDecoder, VorbisEncoder, VorbisHeaders};

use crate::error::{CodecError, Result};
use crate::raw::ffmpeg as ff;

/// The audio codecs a session can carry.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AudioCodec {
    /// RFC 6716, carried per RFC 7587. Preferred.
    #[default]
    Opus,
    /// §7.1's original codec, carried per RFC 5215.
    Vorbis,
}

impl AudioCodec {
    pub const fn as_str(self) -> &'static str {
        match self {
            AudioCodec::Opus => "opus",
            AudioCodec::Vorbis => "vorbis",
        }
    }

    /// The name this codec takes in an SDP `a=rtpmap` line. Both are
    /// lowercase by registration, unlike the video codecs.
    pub const fn rtp_encoding_name(self) -> &'static str {
        match self {
            AudioCodec::Opus => "opus",
            AudioCodec::Vorbis => "vorbis",
        }
    }

    /// Algorithmic delay in milliseconds at 48 kHz, for the negotiation's
    /// cost model. Opus is quoted at its 20 ms frame setting; Vorbis'
    /// figure is one long block plus the overlap its MDCT requires.
    pub const fn typical_delay_ms(self) -> u32 {
        match self {
            AudioCodec::Opus => 20,
            AudioCodec::Vorbis => 46,
        }
    }

    /// Does a decoder need configuration delivered before it can start?
    ///
    /// This is why Opus is preferred: a Vorbis client that misses the SDP
    /// cannot decode a single packet, while an Opus stream can be joined at
    /// any point.
    pub const fn needs_out_of_band_configuration(self) -> bool {
        match self {
            AudioCodec::Opus => false,
            AudioCodec::Vorbis => true,
        }
    }
}

/// Shared libavcodec plumbing for the audio paths that use it.
pub(crate) struct AvAudio {
    pub(crate) ctx: *mut ff::AVCodecContext,
    pub(crate) frame: *mut ff::AVFrame,
    pub(crate) packet: *mut ff::AVPacket,
}

impl AvAudio {
    pub(crate) fn new() -> Self {
        AvAudio {
            ctx: core::ptr::null_mut(),
            frame: core::ptr::null_mut(),
            packet: core::ptr::null_mut(),
        }
    }

    pub(crate) fn alloc_objects(&mut self, what: &str) -> Result<()> {
        // SAFETY: independent allocations that return null on failure.
        unsafe {
            self.frame = ff::av_frame_alloc();
            self.packet = ff::av_packet_alloc();
        }
        if self.frame.is_null() || self.packet.is_null() {
            return Err(CodecError::init(
                what.to_string(),
                "allocating the reusable frame and packet failed".to_string(),
            ));
        }
        Ok(())
    }
}

impl Drop for AvAudio {
    fn drop(&mut self) {
        // SAFETY: each pointer is null or owned solely here, and the free
        // functions accept a pointer to a null pointer.
        unsafe {
            if !self.packet.is_null() {
                ff::av_packet_free(&mut self.packet);
            }
            if !self.frame.is_null() {
                ff::av_frame_free(&mut self.frame);
            }
            if !self.ctx.is_null() {
                ff::avcodec_free_context(&mut self.ctx);
            }
        }
    }
}

/// Set a stereo (or mono) channel layout on a context or frame.
///
/// # Safety
/// `layout` must point at an `AVChannelLayout` that is either zeroed or
/// already initialised; this overwrites it.
pub(crate) unsafe fn set_channel_layout(layout: *mut ff::AVChannelLayout, channels: u8) {
    // SAFETY: the caller guarantees the pointer is a live layout, and the
    // default layout for a channel count is what is wanted here.
    unsafe {
        ff::av_channel_layout_uninit(layout);
        ff::av_channel_layout_default(layout, channels as core::ffi::c_int);
    }
}

/// Append one decoded audio frame to `out` as interleaved S16.
///
/// Both decoders land here, because libavcodec may hand back either packed
/// S16 or planar float depending on the codec and build.
///
/// # Safety
/// `frame` must be a populated `AVFrame` from a successful
/// `avcodec_receive_frame`.
pub(crate) unsafe fn append_samples(
    frame: *mut ff::AVFrame,
    out: &mut Vec<i16>,
    what: &str,
) -> Result<()> {
    // SAFETY: the caller guarantees the frame is populated.
    let (format, samples, channels, data) = unsafe {
        let f = &*frame;
        (f.format, f.nb_samples, f.ch_layout.nb_channels, f.data)
    };

    if samples <= 0 {
        return Ok(());
    }
    let channels = channels.max(0) as usize;
    let samples = samples as usize;

    if format == ff::AVSampleFormat_AV_SAMPLE_FMT_S16 {
        // SAFETY: data[0] holds samples * channels interleaved i16s.
        let src = unsafe { core::slice::from_raw_parts(data[0] as *const i16, samples * channels) };
        out.extend_from_slice(src);
        return Ok(());
    }
    if format == ff::AVSampleFormat_AV_SAMPLE_FMT_FLTP {
        out.reserve(samples * channels);
        for sample in 0..samples {
            for plane in data.iter().take(channels.min(data.len())) {
                // SAFETY: this plane holds `samples` floats.
                let value = unsafe { *(*plane as *const f32).add(sample) };
                // 32767 rather than 32768: scaling by the larger value would
                // let a full-scale +1.0 sample wrap to negative.
                out.push((value.clamp(-1.0, 1.0) * 32767.0) as i16);
            }
        }
        return Ok(());
    }
    if format == ff::AVSampleFormat_AV_SAMPLE_FMT_S16P {
        out.reserve(samples * channels);
        for sample in 0..samples {
            for plane in data.iter().take(channels.min(data.len())) {
                // SAFETY: this plane holds `samples` i16s.
                out.push(unsafe { *(*plane as *const i16).add(sample) });
            }
        }
        return Ok(());
    }

    Err(CodecError::process(
        what.to_string(),
        format!("the decoder produced sample format {format}, expected S16, S16P or FLTP"),
    ))
}
