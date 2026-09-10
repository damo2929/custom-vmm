//! Video decode for the console client — H.264, VP9 or AV1.
//!
//! The client reassembles whole frames from RTP and hands them here.
//! libavcodec decodes the bitstream; the caller converts the decoded I420 to
//! its surface format with a [`Scaler`](crate::Scaler).
//!
//! Decoding is deliberately software for all three. §7.1's stream is 1080p
//! at up to 2 Mbps, which every one of these decoders handles on a fraction
//! of a core — libdav1d in particular is faster than the hardware import
//! path would be once surface mapping is counted — and a hardware decoder
//! would add a GPU dependency on the *client*, which is the machine least
//! likely to have one. Decode cost is therefore near-flat across codecs,
//! which is what lets the negotiation in `libvmm-media` decide almost
//! entirely on encode cost.

use crate::error::{
    av_error, averror_eagain, averror_eof, averror_invaliddata, CodecError, Result,
};
use crate::frame::Yuv420Frame;
use crate::raw::ffmpeg as ff;
use crate::video::VideoCodec;

/// The libavcodec identifier for a codec.
const fn codec_id(codec: VideoCodec) -> ff::AVCodecID {
    match codec {
        VideoCodec::H264 => ff::AVCodecID_AV_CODEC_ID_H264,
        VideoCodec::Vp9 => ff::AVCodecID_AV_CODEC_ID_VP9,
        VideoCodec::Av1 => ff::AVCodecID_AV_CODEC_ID_AV1,
    }
}

/// Is a decoder for `codec` present in this libavcodec build?
pub(crate) fn decoder_is_available(codec: VideoCodec) -> bool {
    // SAFETY: the identifier is valid and the call only looks up a table.
    !unsafe { ff::avcodec_find_decoder(codec_id(codec)) }.is_null()
}

/// Is a decoder for an audio codec present in this build?
pub(crate) fn audio_decoder_is_available(codec: crate::audio::AudioCodec) -> bool {
    let id = match codec {
        crate::audio::AudioCodec::Opus => ff::AVCodecID_AV_CODEC_ID_OPUS,
        crate::audio::AudioCodec::Vorbis => ff::AVCodecID_AV_CODEC_ID_VORBIS,
    };
    // SAFETY: the identifier is valid and the call only looks up a table.
    !unsafe { ff::avcodec_find_decoder(id) }.is_null()
}

/// Is a named encoder present in this libavcodec build?
pub(crate) fn encoder_is_available(name: &str) -> bool {
    let Ok(c_name) = std::ffi::CString::new(name) else {
        return false;
    };
    // SAFETY: c_name is a valid C string; the call only looks up a table.
    !unsafe { ff::avcodec_find_encoder_by_name(c_name.as_ptr()) }.is_null()
}

pub struct VideoDecoder {
    codec: VideoCodec,
    ctx: *mut ff::AVCodecContext,
    /// Reused across calls so the datapath does not allocate per frame.
    frame: *mut ff::AVFrame,
    packet: *mut ff::AVPacket,
    /// Set once the decoder has produced its first frame, so geometry
    /// mismatches can be reported against something concrete.
    geometry: Option<(u32, u32)>,
    /// Access units libavcodec could not parse. See [`H264Decoder::decode`].
    undecodable_units: u64,
}

// SAFETY: every pointer is owned exclusively by this handle, and libavcodec
// permits one thread at a time per context, which &mut self enforces.
unsafe impl Send for VideoDecoder {}

impl VideoDecoder {
    /// Open a decoder for `codec`.
    pub fn open(codec: VideoCodec) -> Result<Self> {
        crate::logging::install();
        // SAFETY: the identifier is valid; the call only looks up a table.
        let found = unsafe { ff::avcodec_find_decoder(codec_id(codec)) };
        if found.is_null() {
            return Err(CodecError::unavailable(
                format!("{} decode", codec.as_str()),
                "this libavcodec was built without that decoder".to_string(),
            ));
        }
        let codec_impl = found;

        // SAFETY: codec_impl is non-null.
        let ctx = unsafe { ff::avcodec_alloc_context3(codec_impl) };
        if ctx.is_null() {
            return Err(CodecError::init(
                format!("{} decode", codec.as_str()),
                "avcodec_alloc_context3 failed".to_string(),
            ));
        }

        let mut decoder = VideoDecoder {
            codec,
            ctx,
            frame: core::ptr::null_mut(),
            packet: core::ptr::null_mut(),
            geometry: None,
            undecodable_units: 0,
        };

        // SAFETY: ctx is freshly allocated; these are public fields.
        unsafe {
            // Slice threading keeps latency low: frame threading would hold
            // frames back to fill its pipeline, which on an interactive
            // console shows up as input lag.
            (*ctx).thread_type = ff::FF_THREAD_SLICE as i32;
            (*ctx).thread_count = 0; // let libavcodec size it from the host
        }

        // SAFETY: ctx is configured and codec_impl matches it.
        let rc = unsafe { ff::avcodec_open2(ctx, codec_impl, core::ptr::null_mut()) };
        if rc < 0 {
            return Err(CodecError::init(
                format!("{} decode", codec.as_str()),
                format!("avcodec_open2: {}", av_error(rc)),
            ));
        }

        // SAFETY: independent allocations that return null on failure.
        unsafe {
            decoder.frame = ff::av_frame_alloc();
            decoder.packet = ff::av_packet_alloc();
        }
        if decoder.frame.is_null() || decoder.packet.is_null() {
            return Err(CodecError::init(
                format!("{} decode", codec.as_str()),
                "allocating the reusable frame and packet failed".to_string(),
            ));
        }

        Ok(decoder)
    }

    /// Which codec this decoder was opened for.
    pub fn codec(&self) -> VideoCodec {
        self.codec
    }

    /// Geometry of the stream, once the first frame has been decoded.
    pub fn geometry(&self) -> Option<(u32, u32)> {
        self.geometry
    }

    /// How many access units libavcodec could not parse.
    ///
    /// A handful at the start of a session is a normal mid-GOP join. A count
    /// that keeps climbing after the first decoded frame means real packet
    /// loss, which is worth surfacing to the operator.
    pub fn undecodable_units(&self) -> u64 {
        self.undecodable_units
    }

    /// Decode one whole coded frame, returning every picture it yielded.
    ///
    /// An empty vector is normal, not an error. Two ordinary situations
    /// produce one: the decoder is still waiting for more input, and — at
    /// the start of a stream — the unit references parameter sets that have
    /// not arrived. A client joining an RTP session mid-GOP sees a run of
    /// slices it cannot decode until the next IDR, so an unparseable unit is
    /// counted in [`H264Decoder::undecodable_units`] rather than raised: it
    /// is the expected shape of a mid-stream join, and failing the session
    /// over it would make joining impossible.
    pub fn decode(&mut self, access_unit: &[u8]) -> Result<Vec<Yuv420Frame>> {
        if access_unit.is_empty() {
            return Ok(Vec::new());
        }

        // SAFETY: packet is live. Pointing it at the caller's buffer without
        // copying is sound because avcodec_send_packet consumes the data
        // during the call, and `access_unit` outlives it.
        unsafe {
            (*self.packet).data = access_unit.as_ptr() as *mut u8;
            (*self.packet).size = access_unit.len() as i32;
        }

        // SAFETY: the context is open and packet is live.
        let rc = unsafe { ff::avcodec_send_packet(self.ctx, self.packet) };
        // SAFETY: clearing the borrowed pointer before it can outlive the
        // caller's slice.
        unsafe {
            (*self.packet).data = core::ptr::null_mut();
            (*self.packet).size = 0;
        }
        if rc == averror_invaliddata() {
            self.undecodable_units += 1;
        } else if rc < 0 && rc != averror_eagain() {
            return Err(CodecError::process(
                self.what(),
                format!("avcodec_send_packet: {}", av_error(rc)),
            ));
        }

        self.drain()
    }

    /// Flush the decoder at end of stream and collect what is left.
    pub fn finish(&mut self) -> Result<Vec<Yuv420Frame>> {
        // SAFETY: a null packet is libavcodec's documented drain signal.
        let rc = unsafe { ff::avcodec_send_packet(self.ctx, core::ptr::null()) };
        if rc < 0 && rc != averror_eof() {
            return Err(CodecError::process(
                self.what(),
                format!("flushing the decoder: {}", av_error(rc)),
            ));
        }
        self.drain()
    }

    /// The subject used in this decoder's errors.
    fn what(&self) -> String {
        format!("{} decode", self.codec.as_str())
    }

    fn drain(&mut self) -> Result<Vec<Yuv420Frame>> {
        let mut frames = Vec::new();
        loop {
            // SAFETY: the context is open and frame is live.
            let rc = unsafe { ff::avcodec_receive_frame(self.ctx, self.frame) };
            if rc == averror_eagain() || rc == averror_eof() {
                return Ok(frames);
            }
            if rc < 0 {
                return Err(CodecError::process(
                    self.what(),
                    format!("avcodec_receive_frame: {}", av_error(rc)),
                ));
            }
            let decoded = self.copy_out()?;
            self.geometry = Some((decoded.width, decoded.height));
            frames.push(decoded);
            // SAFETY: frame is live; unref readies it for the next receive.
            unsafe { ff::av_frame_unref(self.frame) };
        }
    }

    /// Copy the decoded planes out of libavcodec's reference-counted buffer.
    fn copy_out(&self) -> Result<Yuv420Frame> {
        // SAFETY: a successful receive leaves the frame populated.
        let (format, width, height, pts) = unsafe {
            let f = &*self.frame;
            (f.format, f.width, f.height, f.pts)
        };

        if format != ff::AVPixelFormat_AV_PIX_FMT_YUV420P {
            return Err(CodecError::process(
                self.what(),
                format!(
                    "decoder produced pixel format {format}, expected YUV420P. \
                     A 4:2:2 or 10-bit stream is outside what §7.1 encodes."
                ),
            ));
        }
        if width <= 0 || height <= 0 {
            return Err(CodecError::process(
                self.what(),
                format!("decoder reported {width}x{height}"),
            ));
        }

        let width = width as u32;
        let height = height as u32;
        let mut out = Yuv420Frame::black(width, height)?;
        out.pts = pts;

        // libavcodec's linesize is padded for alignment and is generally
        // wider than the frame, so each row is copied individually rather
        // than the plane in one block.
        // SAFETY: the frame is populated with YUV420P data, so data[0..3]
        // are non-null and each row of `linesize[n]` bytes is readable.
        // Every copy below is bounded by the destination's own stride.
        unsafe {
            let f = &*self.frame;
            copy_plane(
                f.data[0],
                f.linesize[0] as usize,
                &mut out.y,
                out.y_stride,
                width as usize,
                height as usize,
            )?;
            copy_plane(
                f.data[1],
                f.linesize[1] as usize,
                &mut out.u,
                out.uv_stride,
                width as usize / 2,
                height as usize / 2,
            )?;
            copy_plane(
                f.data[2],
                f.linesize[2] as usize,
                &mut out.v,
                out.uv_stride,
                width as usize / 2,
                height as usize / 2,
            )?;
        }

        Ok(out)
    }
}

/// Copy `rows` rows of `row_bytes` from a padded C plane into a packed one.
///
/// # Safety
/// `src` must be non-null and readable for `src_stride * rows` bytes.
unsafe fn copy_plane(
    src: *const u8,
    src_stride: usize,
    dst: &mut [u8],
    dst_stride: usize,
    row_bytes: usize,
    rows: usize,
) -> Result<()> {
    if src.is_null() {
        return Err(CodecError::process(
            "H.264 decode",
            "the decoder produced a frame with a missing plane",
        ));
    }
    if src_stride < row_bytes || dst_stride < row_bytes {
        return Err(CodecError::process(
            "H.264 decode",
            format!("stride {src_stride}/{dst_stride} is narrower than a {row_bytes}-byte row"),
        ));
    }
    if dst.len() < dst_stride * rows {
        return Err(CodecError::process(
            "H.264 decode",
            format!(
                "destination plane is {} bytes, {} required",
                dst.len(),
                dst_stride * rows
            ),
        ));
    }
    for row in 0..rows {
        // SAFETY: bounded by the checks above on both sides.
        core::ptr::copy_nonoverlapping(
            src.add(row * src_stride),
            dst.as_mut_ptr().add(row * dst_stride),
            row_bytes,
        );
    }
    Ok(())
}

impl Drop for VideoDecoder {
    fn drop(&mut self) {
        // SAFETY: each pointer is either null or owned solely by this
        // struct, and the free functions take a pointer to a null pointer.
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
