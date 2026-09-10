//! The libx264 software H.264 encoder — §7.1's fallback path.
//!
//! This runs whenever the configured accelerator cannot encode H.264 on the
//! host, which the VA-API probe in [`super::vaapi`] decides. "Software" here
//! means no fixed-function video engine; libx264 still runs hand-written
//! SIMD kernels — up to AVX-512 — across every core, and
//! [`X264Encoder::acceleration`] reports which the library selected so the
//! choice is observable in a boot log rather than assumed.
//!
//! libx264 is reached through the C shim in `shim/vmm_x264.c` rather than
//! generated bindings; `shim/vmm_x264.h` records why. Everything below talks
//! to that flat ABI, so no libx264 struct layout is assumed here.

use super::config::{EncodedFrame, EncoderConfig};
use crate::error::{CodecError, Result};
use crate::frame::Yuv420Frame;
use crate::raw::x264 as sys;

pub struct X264Encoder {
    /// Owned by the shim; freed by `vmm_x264_close`.
    handle: *mut sys::vmm_x264_encoder,
    config: EncoderConfig,
    info: sys::vmm_x264_info,
}

// SAFETY: the encoder is owned exclusively by this handle. libx264 is
// internally threaded but a single encoder must not be driven concurrently,
// which &mut self on every entry point enforces.
unsafe impl Send for X264Encoder {}

impl X264Encoder {
    pub fn open(config: EncoderConfig) -> Result<Self> {
        config.validate()?;

        let cfg = sys::vmm_x264_config {
            width: config.width as i32,
            height: config.height as i32,
            framerate: config.framerate as i32,
            gop_length: config.gop_length as i32,
            target_kbps: config.target_kbps as i32,
            max_kbps: config.max_kbps as i32,
            vbv_buffer_bits: config.vbv_buffer_bits as i32,
        };

        let mut handle: *mut sys::vmm_x264_encoder = core::ptr::null_mut();
        let mut info = sys::vmm_x264_info::default();

        // SAFETY: cfg, handle and info are live locals, which is the whole
        // contract vmm_x264_open documents.
        let rc = unsafe { sys::vmm_x264_open(&cfg, &mut handle, &mut info) };
        if rc != 0 {
            return Err(CodecError::init(
                "libx264",
                format!(
                    "{} for {}x{} at {} kbps (ceiling {} kbps)",
                    shim_error(rc),
                    config.width,
                    config.height,
                    config.target_kbps,
                    config.max_kbps
                ),
            ));
        }
        if handle.is_null() {
            return Err(CodecError::init(
                "libx264",
                "vmm_x264_open reported success but returned no encoder",
            ));
        }

        Ok(X264Encoder {
            handle,
            config,
            info,
        })
    }

    /// Human-readable description of the SIMD libx264 is actually using.
    pub fn acceleration(&self) -> String {
        // X264_CPU_* values, from x264.h. Only the widest ISA present is
        // reported: the narrower flags are always set alongside it, and
        // listing them all is noise in a boot log.
        const AVX512: u32 = 1 << 16;
        const AVX2: u32 = 1 << 15;
        const AVX: u32 = 1 << 9;
        const SSE42: u32 = 1 << 8;
        const SSSE3: u32 = 1 << 6;
        const SSE2: u32 = 1 << 3;

        let simd = [
            (AVX512, "AVX-512"),
            (AVX2, "AVX2"),
            (AVX, "AVX"),
            (SSE42, "SSE4.2"),
            (SSSE3, "SSSE3"),
            (SSE2, "SSE2"),
        ]
        .into_iter()
        .find(|(flag, _)| self.info.cpu_flags & flag != 0)
        .map(|(_, name)| name)
        .unwrap_or("scalar");

        format!("libx264 {simd}, {} threads", self.info.threads)
    }

    pub fn config(&self) -> &EncoderConfig {
        &self.config
    }

    /// Encode one frame. Returns `Ok(None)` when libx264 buffered it.
    ///
    /// `force_idr` codes this frame as an IDR regardless of the GOP.
    pub fn encode(&mut self, frame: &Yuv420Frame, force_idr: bool) -> Result<Option<EncodedFrame>> {
        if frame.width != self.config.width || frame.height != self.config.height {
            return Err(CodecError::invalid(
                "libx264",
                format!(
                    "frame is {}x{}, encoder was opened for {}x{}",
                    frame.width, frame.height, self.config.width, self.config.height
                ),
            ));
        }
        frame.validate()?;

        let mut out = sys::vmm_x264_output::default();
        // SAFETY: the handle is live, the three plane pointers each address
        // at least stride * height bytes (validate() just established that)
        // and stay borrowed for the duration of the call, and `out` is a
        // live local.
        let rc = unsafe {
            sys::vmm_x264_encode(
                self.handle,
                frame.y.as_ptr(),
                frame.u.as_ptr(),
                frame.v.as_ptr(),
                frame.y_stride as i32,
                frame.uv_stride as i32,
                frame.pts,
                i32::from(force_idr),
                &mut out,
            )
        };
        self.collect(rc, &out)
    }

    /// Drain a frame libx264 is still holding. `Ok(None)` when it has none.
    pub fn flush(&mut self) -> Result<Option<EncodedFrame>> {
        let mut out = sys::vmm_x264_output::default();
        // SAFETY: the handle is live and `out` is a live local.
        let rc = unsafe { sys::vmm_x264_flush(self.handle, &mut out) };
        self.collect(rc, &out)
    }

    /// Turn one shim return code plus its output block into a frame.
    fn collect(&self, rc: i32, out: &sys::vmm_x264_output) -> Result<Option<EncodedFrame>> {
        if rc < 0 {
            return Err(CodecError::process("libx264", shim_error(rc)));
        }
        if rc == 0 {
            // Buffered, not an error.
            return Ok(None);
        }
        if out.data.is_null() || out.size <= 0 {
            return Err(CodecError::process(
                "libx264",
                "the shim reported a frame but produced no bytes",
            ));
        }

        // Copy out of the encoder's buffer: the shim's header documents it
        // as valid only until the next call on this encoder.
        // SAFETY: a positive return guarantees `size` initialised bytes at
        // `data`, and nothing has called into the encoder since.
        let data = unsafe { core::slice::from_raw_parts(out.data, out.size as usize) }.to_vec();

        Ok(Some(EncodedFrame {
            data,
            keyframe: out.keyframe != 0,
            pts: out.pts,
            dts: out.dts,
        }))
    }
}

impl Drop for X264Encoder {
    fn drop(&mut self) {
        // SAFETY: the handle came from vmm_x264_open and this is the only
        // owner, so it has not been closed already. The shim tolerates null.
        unsafe { sys::vmm_x264_close(self.handle) };
    }
}

/// Render a `VMM_X264_E_*` code from the shim.
fn shim_error(rc: i32) -> String {
    let reason = match rc {
        v if v == sys::VMM_X264_E_ALLOC => "out of memory",
        v if v == sys::VMM_X264_E_PRESET => "libx264 rejected the veryfast/zerolatency preset",
        v if v == sys::VMM_X264_E_PROFILE => "libx264 rejected the high profile",
        v if v == sys::VMM_X264_E_OPEN => "x264_encoder_open failed",
        v if v == sys::VMM_X264_E_ENCODE => "x264_encoder_encode failed",
        v if v == sys::VMM_X264_E_NO_NAL => "libx264 reported bytes but produced no NAL units",
        v if v == sys::VMM_X264_E_INVALID => "the shim was passed an invalid argument",
        _ => "unknown shim error",
    };
    format!("{reason} (code {rc})")
}
