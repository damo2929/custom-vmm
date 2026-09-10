//! The libavcodec encoder, hardware or software, for any of the three codecs.
//!
//! One implementation covers six combinations, because the only structural
//! difference between them is where the frame lives:
//!
//! * **hardware** — `pix_fmt` is `VAAPI`, so a device context and a pool of
//!   NV12 surfaces are allocated and each frame is uploaded to the GPU
//!   before submission;
//! * **software** — `pix_fmt` is `YUV420P` and the planes are handed over
//!   directly.
//!
//! Everything else — rate control, GOP, the drain protocol — is identical,
//! which is why they share a type rather than being written twice.
//!
//! Driving VA-API's encode entrypoint directly would mean hand-assembling
//! sequence headers and managing reference lists per codec. libavcodec
//! already does that against the same libva this crate links, and gets it
//! right.

use super::config::{EncodedFrame, EncoderConfig, VideoCodec};
use super::probe::VaapiCapability;
use crate::error::{av_error, averror_eagain, averror_eof, CodecError, Result};
use crate::frame::Yuv420Frame;
use crate::raw::ffmpeg as ff;
use std::ffi::CString;

/// Where the encoder runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Placement {
    /// A fixed-function engine reached through VA-API.
    Hardware,
    /// The CPU, through one of libavcodec's own encoders.
    Software,
}

pub struct AvEncoder {
    codec_ctx: *mut ff::AVCodecContext,
    hw_device: *mut ff::AVBufferRef,
    hw_frames: *mut ff::AVBufferRef,
    /// Staging frame in host memory. NV12 for the hardware upload path,
    /// YUV420P when the encoder consumes it directly.
    sw_frame: *mut ff::AVFrame,
    /// The GPU-side frame; null on the software path.
    hw_frame: *mut ff::AVFrame,
    packet: *mut ff::AVPacket,
    config: EncoderConfig,
    placement: Placement,
    encoder_name: String,
    capability: Option<VaapiCapability>,
    /// Whether the end-of-stream frame has been sent. libavcodec accepts a
    /// null frame once; a second returns `AVERROR_EOF`, and `drain` calls
    /// `flush` repeatedly until it reports empty.
    draining: bool,
}

// SAFETY: every field is owned exclusively by this handle, and libavcodec
// permits a context to be used from one thread at a time, which &mut self on
// each entry point enforces.
unsafe impl Send for AvEncoder {}

impl AvEncoder {
    /// Open the VA-API encoder for `config.codec`.
    pub fn open_hardware(config: EncoderConfig, capability: VaapiCapability) -> Result<Self> {
        config.validate()?;
        if !capability.can_encode(config.codec) {
            return Err(CodecError::unavailable(
                format!("VA-API {} encode", config.codec.as_str()),
                format!(
                    "{} exposes no {} encode entrypoint",
                    capability.render_node.display(),
                    config.codec.as_str()
                ),
            ));
        }
        let name = config.codec.vaapi_encoder().to_string();
        Self::open(config, Placement::Hardware, name, Some(capability))
    }

    /// Open libavcodec's software encoder for `config.codec`.
    pub fn open_software(config: EncoderConfig) -> Result<Self> {
        config.validate()?;
        let name = config.codec.software_encoder().ok_or_else(|| {
            CodecError::unavailable(
                format!("{} software encode", config.codec.as_str()),
                "this codec has no libavcodec software encoder in this build".to_string(),
            )
        })?;
        Self::open(config, Placement::Software, name.to_string(), None)
    }

    fn open(
        config: EncoderConfig,
        placement: Placement,
        encoder_name: String,
        capability: Option<VaapiCapability>,
    ) -> Result<Self> {
        crate::logging::install();

        let mut encoder = AvEncoder {
            codec_ctx: core::ptr::null_mut(),
            hw_device: core::ptr::null_mut(),
            hw_frames: core::ptr::null_mut(),
            sw_frame: core::ptr::null_mut(),
            hw_frame: core::ptr::null_mut(),
            packet: core::ptr::null_mut(),
            config,
            placement,
            encoder_name,
            capability,
            draining: false,
        };
        encoder.init()?;
        Ok(encoder)
    }

    fn init(&mut self) -> Result<()> {
        let name = CString::new(self.encoder_name.as_str())
            .map_err(|e| CodecError::init(self.encoder_name.clone(), e.to_string()))?;
        // SAFETY: name is a valid C string; the call only looks up a table.
        let codec = unsafe { ff::avcodec_find_encoder_by_name(name.as_ptr()) };
        if codec.is_null() {
            return Err(CodecError::unavailable(
                self.encoder_name.clone(),
                "this libavcodec was built without that encoder".to_string(),
            ));
        }

        if self.placement == Placement::Hardware {
            self.init_hardware_context()?;
        }

        // SAFETY: codec is non-null, from avcodec_find_encoder_by_name.
        let ctx = unsafe { ff::avcodec_alloc_context3(codec) };
        if ctx.is_null() {
            return Err(CodecError::init(
                self.encoder_name.clone(),
                "avcodec_alloc_context3 failed",
            ));
        }
        self.codec_ctx = ctx;

        // SAFETY: ctx is freshly allocated; every field written here is a
        // documented public member.
        unsafe {
            (*ctx).width = self.config.width as i32;
            (*ctx).height = self.config.height as i32;
            (*ctx).pix_fmt = match self.placement {
                Placement::Hardware => ff::AVPixelFormat_AV_PIX_FMT_VAAPI,
                Placement::Software => ff::AVPixelFormat_AV_PIX_FMT_YUV420P,
            };
            (*ctx).time_base = ff::AVRational {
                num: 1,
                den: self.config.framerate as i32,
            };
            (*ctx).framerate = ff::AVRational {
                num: self.config.framerate as i32,
                den: 1,
            };
            // Constrained VBR (§7.1): bit_rate is the target average and
            // rc_max_rate the ceiling, with the HRD buffer sized from
            // vbv_buffer_bits so instantaneous output cannot breach it.
            (*ctx).bit_rate = i64::from(self.config.target_kbps) * 1000;
            (*ctx).rc_max_rate = i64::from(self.config.max_kbps) * 1000;
            (*ctx).rc_buffer_size = self.config.vbv_buffer_bits as i32;
            (*ctx).gop_size = self.config.gop_length as i32;
            // No B-frames: reordering delay is input lag on a console.
            (*ctx).max_b_frames = 0;
            // AV_CODEC_FLAG_GLOBAL_HEADER is deliberately NOT set. With it
            // off the encoder repeats its sequence header inline before each
            // keyframe, which is what §7.3's RTP stream needs so a client
            // joining mid-session can start without an out-of-band exchange.

            if self.placement == Placement::Hardware {
                (*ctx).hw_frames_ctx = ff::av_buffer_ref(self.hw_frames);
                if (*ctx).hw_frames_ctx.is_null() {
                    return Err(CodecError::init(
                        self.encoder_name.clone(),
                        "av_buffer_ref on the frames context failed",
                    ));
                }
            } else {
                // Let libavcodec size its thread pool from the host.
                (*ctx).thread_count = 0;
            }
        }

        self.apply_low_latency_options(ctx)?;

        // SAFETY: ctx is fully configured and codec matches it.
        let rc = unsafe { ff::avcodec_open2(ctx, codec, core::ptr::null_mut()) };
        if rc < 0 {
            return Err(CodecError::unavailable(
                self.encoder_name.clone(),
                format!("avcodec_open2: {}", av_error(rc)),
            ));
        }

        self.init_frames()
    }

    /// Every encoder here is driven live, so each is pushed to its
    /// lowest-latency operating point. The option names differ per encoder
    /// and an unknown one is an error rather than a silent default, so this
    /// is where a libavcodec upgrade would surface a rename.
    fn apply_low_latency_options(&self, ctx: *mut ff::AVCodecContext) -> Result<()> {
        match (self.placement, self.config.codec) {
            (Placement::Hardware, _) => {
                // How many frames the VA-API encoder keeps in flight before
                // returning any. The default of 2 doubles the pipeline delay
                // for throughput an interactive console does not need.
                self.set_option(ctx, "async_depth", "1")?;
                if self.config.codec == VideoCodec::H264 {
                    self.set_option(ctx, "profile", "high")?;
                }
            }
            (Placement::Software, VideoCodec::Vp9) => {
                // libvpx's default "good" deadline is far too slow to encode
                // live; "realtime" with a high cpu-used is the operating
                // point WebRTC uses for the same job.
                self.set_option(ctx, "deadline", "realtime")?;
                self.set_option(ctx, "cpu-used", "8")?;
                // No alt-ref lookahead: it would buffer frames.
                self.set_option(ctx, "lag-in-frames", "0")?;
                // Row-based multithreading, so threads help latency rather
                // than just throughput.
                self.set_option(ctx, "row-mt", "1")?;
            }
            (Placement::Software, VideoCodec::Av1) => {
                // SVT-AV1's fastest preset. Anything slower cannot keep up
                // with 1080p in real time on a general-purpose CPU.
                self.set_option(ctx, "preset", "12")?;
                // Low-delay prediction structure with no lookahead: the
                // default hierarchical structure reorders frames, which is
                // latency a console cannot spend.
                self.set_option(ctx, "svtav1-params", "pred-struct=1:lookahead=0")?;
            }
            (Placement::Software, VideoCodec::H264) => {
                // Unreachable: H.264 software encode goes through the
                // libx264 shim, not libavcodec. Guarded rather than
                // panicking so a future routing change fails loudly.
                return Err(CodecError::init(
                    self.encoder_name.clone(),
                    "H.264 software encode belongs on the libx264 shim path",
                ));
            }
        }
        Ok(())
    }

    fn init_hardware_context(&mut self) -> Result<()> {
        let node = self
            .capability
            .as_ref()
            .map(|c| c.render_node.clone())
            .ok_or_else(|| {
                CodecError::init(self.encoder_name.clone(), "no render node was probed")
            })?;
        let c_node = CString::new(node.as_os_str().as_encoded_bytes())
            .map_err(|e| CodecError::init(self.encoder_name.clone(), e.to_string()))?;

        // SAFETY: the out-parameter is live, c_node is a valid C string, and
        // a null options dictionary is permitted.
        let rc = unsafe {
            ff::av_hwdevice_ctx_create(
                &mut self.hw_device,
                ff::AVHWDeviceType_AV_HWDEVICE_TYPE_VAAPI,
                c_node.as_ptr(),
                core::ptr::null_mut(),
                0,
            )
        };
        if rc < 0 {
            return Err(CodecError::unavailable(
                self.encoder_name.clone(),
                format!(
                    "av_hwdevice_ctx_create on {}: {}",
                    node.display(),
                    av_error(rc)
                ),
            ));
        }

        // SAFETY: hw_device is a live device context.
        self.hw_frames = unsafe { ff::av_hwframe_ctx_alloc(self.hw_device) };
        if self.hw_frames.is_null() {
            return Err(CodecError::init(
                self.encoder_name.clone(),
                "av_hwframe_ctx_alloc returned no frames context",
            ));
        }
        // SAFETY: hw_frames is a live AVBufferRef whose data is an
        // AVHWFramesContext, as av_hwframe_ctx_alloc guarantees.
        unsafe {
            let frames = (*self.hw_frames).data as *mut ff::AVHWFramesContext;
            (*frames).format = ff::AVPixelFormat_AV_PIX_FMT_VAAPI;
            (*frames).sw_format = ff::AVPixelFormat_AV_PIX_FMT_NV12;
            (*frames).width = self.config.width as i32;
            (*frames).height = self.config.height as i32;
            // Enough surfaces for the reference list plus the one being
            // written; 20 is what FFmpeg's own VA-API examples use.
            (*frames).initial_pool_size = 20;
        }
        // SAFETY: the frames context is fully configured above.
        let rc = unsafe { ff::av_hwframe_ctx_init(self.hw_frames) };
        if rc < 0 {
            return Err(CodecError::init(
                self.encoder_name.clone(),
                format!("av_hwframe_ctx_init: {}", av_error(rc)),
            ));
        }
        Ok(())
    }

    fn init_frames(&mut self) -> Result<()> {
        // SAFETY: independent allocations that return null on failure.
        unsafe {
            self.sw_frame = ff::av_frame_alloc();
            self.packet = ff::av_packet_alloc();
            if self.placement == Placement::Hardware {
                self.hw_frame = ff::av_frame_alloc();
            }
        }
        if self.sw_frame.is_null()
            || self.packet.is_null()
            || (self.placement == Placement::Hardware && self.hw_frame.is_null())
        {
            return Err(CodecError::init(
                self.encoder_name.clone(),
                "allocating the reusable frame and packet objects failed",
            ));
        }

        // The staging frame is NV12 for a hardware upload and YUV420P when
        // the encoder reads it directly.
        // SAFETY: sw_frame is freshly allocated; these are public fields.
        unsafe {
            (*self.sw_frame).format = match self.placement {
                Placement::Hardware => ff::AVPixelFormat_AV_PIX_FMT_NV12,
                Placement::Software => ff::AVPixelFormat_AV_PIX_FMT_YUV420P,
            };
            (*self.sw_frame).width = self.config.width as i32;
            (*self.sw_frame).height = self.config.height as i32;
            let rc = ff::av_frame_get_buffer(self.sw_frame, 0);
            if rc < 0 {
                return Err(CodecError::init(
                    self.encoder_name.clone(),
                    format!(
                        "av_frame_get_buffer for the staging frame: {}",
                        av_error(rc)
                    ),
                ));
            }
        }
        Ok(())
    }

    fn set_option(&self, ctx: *mut ff::AVCodecContext, name: &str, value: &str) -> Result<()> {
        let key = CString::new(name)
            .map_err(|e| CodecError::init(self.encoder_name.clone(), e.to_string()))?;
        let val = CString::new(value)
            .map_err(|e| CodecError::init(self.encoder_name.clone(), e.to_string()))?;
        // SAFETY: ctx is a live, not-yet-opened context; priv_data belongs
        // to the encoder and av_opt_set returns an error for an option it
        // does not recognise rather than trapping.
        let rc = unsafe { ff::av_opt_set((*ctx).priv_data, key.as_ptr(), val.as_ptr(), 0) };
        if rc < 0 {
            return Err(CodecError::init(
                self.encoder_name.clone(),
                format!("the encoder rejected {name}={value}: {}", av_error(rc)),
            ));
        }
        Ok(())
    }

    pub fn config(&self) -> &EncoderConfig {
        &self.config
    }

    pub fn placement(&self) -> Placement {
        self.placement
    }

    /// A description of the encoder, for the boot log.
    pub fn acceleration(&self) -> String {
        match (&self.capability, self.placement) {
            (Some(cap), Placement::Hardware) => format!(
                "VA-API {} on {} ({})",
                self.encoder_name,
                cap.render_node.display(),
                cap.driver
            ),
            _ => format!("{} (software)", self.encoder_name),
        }
    }

    /// Encode one frame. `Ok(None)` when the encoder buffered it.
    pub fn encode(
        &mut self,
        frame: &Yuv420Frame,
        force_keyframe: bool,
    ) -> Result<Option<EncodedFrame>> {
        if frame.width != self.config.width || frame.height != self.config.height {
            return Err(CodecError::invalid(
                self.encoder_name.clone(),
                format!(
                    "frame is {}x{}, encoder was opened for {}x{}",
                    frame.width, frame.height, self.config.width, self.config.height
                ),
            ));
        }
        frame.validate()?;

        self.fill_staging(frame)?;

        let submitted = match self.placement {
            Placement::Software => {
                // SAFETY: sw_frame holds this frame's planes.
                unsafe {
                    (*self.sw_frame).pts = frame.pts;
                    (*self.sw_frame).pict_type = keyframe_type(force_keyframe);
                }
                self.send(self.sw_frame)
            }
            Placement::Hardware => {
                // SAFETY: hw_frames is a live frames context and hw_frame is
                // an empty allocated frame, which is what the call expects.
                let rc = unsafe { ff::av_hwframe_get_buffer(self.hw_frames, self.hw_frame, 0) };
                if rc < 0 {
                    return Err(CodecError::process(
                        self.encoder_name.clone(),
                        format!("av_hwframe_get_buffer: {}", av_error(rc)),
                    ));
                }
                // SAFETY: both frames are live with matching geometry.
                let rc = unsafe { ff::av_hwframe_transfer_data(self.hw_frame, self.sw_frame, 0) };
                if rc < 0 {
                    // SAFETY: hw_frame is live; return the surface to the pool.
                    unsafe { ff::av_frame_unref(self.hw_frame) };
                    return Err(CodecError::process(
                        self.encoder_name.clone(),
                        format!("uploading the frame to the GPU: {}", av_error(rc)),
                    ));
                }
                // SAFETY: hw_frame is live and now holds the uploaded image.
                unsafe {
                    (*self.hw_frame).pts = frame.pts;
                    (*self.hw_frame).pict_type = keyframe_type(force_keyframe);
                }
                let sent = self.send(self.hw_frame);
                // SAFETY: hw_frame is live; unref returns the surface to the
                // pool whether or not the send succeeded.
                unsafe { ff::av_frame_unref(self.hw_frame) };
                sent
            }
        };
        submitted?;

        self.receive()
    }

    /// Copy the caller's planes into the staging frame.
    fn fill_staging(&mut self, frame: &Yuv420Frame) -> Result<()> {
        // SAFETY: sw_frame was allocated with av_frame_get_buffer, so it is
        // writable; make_writable drops any shared reference first.
        let rc = unsafe { ff::av_frame_make_writable(self.sw_frame) };
        if rc < 0 {
            return Err(CodecError::process(
                self.encoder_name.clone(),
                format!("av_frame_make_writable: {}", av_error(rc)),
            ));
        }

        let width = self.config.width as usize;
        let height = self.config.height as usize;

        // SAFETY: sw_frame holds a buffer of the configured geometry in the
        // format set at init, so plane 0 has linesize[0] * height bytes and
        // the chroma planes their documented halves. Every write below stays
        // inside a row of the destination.
        unsafe {
            let y_dst = (*self.sw_frame).data[0];
            let y_pitch = (*self.sw_frame).linesize[0] as usize;
            for row in 0..height {
                let src = frame.y.as_ptr().add(row * frame.y_stride);
                core::ptr::copy_nonoverlapping(src, y_dst.add(row * y_pitch), width);
            }

            match self.placement {
                Placement::Hardware => {
                    // NV12: the two chroma planes interleave into one.
                    let uv_dst = (*self.sw_frame).data[1];
                    let uv_pitch = (*self.sw_frame).linesize[1] as usize;
                    for row in 0..height / 2 {
                        let u = frame.u.as_ptr().add(row * frame.uv_stride);
                        let v = frame.v.as_ptr().add(row * frame.uv_stride);
                        let dst = uv_dst.add(row * uv_pitch);
                        for col in 0..width / 2 {
                            *dst.add(col * 2) = *u.add(col);
                            *dst.add(col * 2 + 1) = *v.add(col);
                        }
                    }
                }
                Placement::Software => {
                    // YUV420P: the planes stay separate.
                    let u_dst = (*self.sw_frame).data[1];
                    let v_dst = (*self.sw_frame).data[2];
                    let u_pitch = (*self.sw_frame).linesize[1] as usize;
                    let v_pitch = (*self.sw_frame).linesize[2] as usize;
                    for row in 0..height / 2 {
                        core::ptr::copy_nonoverlapping(
                            frame.u.as_ptr().add(row * frame.uv_stride),
                            u_dst.add(row * u_pitch),
                            width / 2,
                        );
                        core::ptr::copy_nonoverlapping(
                            frame.v.as_ptr().add(row * frame.uv_stride),
                            v_dst.add(row * v_pitch),
                            width / 2,
                        );
                    }
                }
            }
        }
        Ok(())
    }

    fn send(&mut self, frame: *mut ff::AVFrame) -> Result<()> {
        // SAFETY: the context is open and frame is either live or null.
        let rc = unsafe { ff::avcodec_send_frame(self.codec_ctx, frame) };
        // EOF means the encoder was already told the stream ended, which is
        // not a failure — the queued packets are still collected.
        if rc == averror_eof() {
            return Ok(());
        }
        if rc < 0 {
            return Err(CodecError::process(
                self.encoder_name.clone(),
                format!("avcodec_send_frame: {}", av_error(rc)),
            ));
        }
        Ok(())
    }

    fn receive(&mut self) -> Result<Option<EncodedFrame>> {
        // SAFETY: the context is open and packet is a live AVPacket.
        let rc = unsafe { ff::avcodec_receive_packet(self.codec_ctx, self.packet) };
        if rc == averror_eagain() || rc == averror_eof() {
            return Ok(None);
        }
        if rc < 0 {
            return Err(CodecError::process(
                self.encoder_name.clone(),
                format!("avcodec_receive_packet: {}", av_error(rc)),
            ));
        }

        // SAFETY: a successful receive leaves packet holding `size` bytes at
        // `data`, valid until the next unref.
        let encoded = unsafe {
            let p = &*self.packet;
            EncodedFrame {
                data: core::slice::from_raw_parts(p.data, p.size.max(0) as usize).to_vec(),
                keyframe: p.flags & ff::AV_PKT_FLAG_KEY as i32 != 0,
                pts: p.pts,
                dts: p.dts,
            }
        };
        // SAFETY: packet is live; unref readies it for the next receive.
        unsafe { ff::av_packet_unref(self.packet) };
        Ok(Some(encoded))
    }

    /// Drain one buffered frame. `Ok(None)` when the encoder is empty.
    pub fn flush(&mut self) -> Result<Option<EncodedFrame>> {
        if !self.draining {
            self.send(core::ptr::null_mut())?;
            self.draining = true;
        }
        self.receive()
    }
}

/// Asking libavcodec for a keyframe out of turn.
fn keyframe_type(force: bool) -> ff::AVPictureType {
    if force {
        ff::AVPictureType_AV_PICTURE_TYPE_I
    } else {
        ff::AVPictureType_AV_PICTURE_TYPE_NONE
    }
}

impl Drop for AvEncoder {
    fn drop(&mut self) {
        // SAFETY: each pointer is either null or owned solely by this struct,
        // and the free functions accept a pointer to a null pointer.
        unsafe {
            if !self.packet.is_null() {
                ff::av_packet_free(&mut self.packet);
            }
            if !self.hw_frame.is_null() {
                ff::av_frame_free(&mut self.hw_frame);
            }
            if !self.sw_frame.is_null() {
                ff::av_frame_free(&mut self.sw_frame);
            }
            if !self.codec_ctx.is_null() {
                ff::avcodec_free_context(&mut self.codec_ctx);
            }
            if !self.hw_frames.is_null() {
                ff::av_buffer_unref(&mut self.hw_frames);
            }
            if !self.hw_device.is_null() {
                ff::av_buffer_unref(&mut self.hw_device);
            }
        }
    }
}
