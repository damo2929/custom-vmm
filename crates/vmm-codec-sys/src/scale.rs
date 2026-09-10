//! Colour conversion via libswscale.
//!
//! The capture side converts the virtio-gpu BGRA scanout to I420 for the
//! encoder; the client converts decoded I420 back to BGRA for its surface.
//! Both directions are the same `SwsContext` machinery, so they share one
//! type. The context caches its geometry, which matters: rebuilding it per
//! frame is the single most expensive mistake in a swscale pipeline.

use crate::error::{CodecError, Result};
use crate::frame::{check_even_geometry, PackedFormat, PackedFrame, Yuv420Frame};
use crate::raw::ffmpeg as ff;
use std::ptr::NonNull;

/// `SWS_BILINEAR`. FFmpeg 8 moved the scaler flags from `#define`s into an
/// `SwsFlags` enum, which changes the name bindgen emits but not the value;
/// spelling the ABI constant out here keeps one source building against
/// both. The value has been `1 << 1` for the whole life of the library.
const SWS_BILINEAR: core::ffi::c_int = 1 << 1;

/// A cached scaler for one fixed conversion.
pub struct Scaler {
    ctx: NonNull<ff::SwsContext>,
    src_width: u32,
    src_height: u32,
    dst_width: u32,
    dst_height: u32,
    src_format: i32,
    dst_format: i32,
}

// SAFETY: an SwsContext is not internally synchronised, but it is owned
// exclusively by this handle and every entry point takes &mut self. Moving
// it between threads is sound; sharing it is prevented by the borrow.
unsafe impl Send for Scaler {}

impl Scaler {
    fn new(src: (u32, u32, i32), dst: (u32, u32, i32), flags: core::ffi::c_int) -> Result<Self> {
        crate::logging::install();

        let (src_width, src_height, src_format) = src;
        let (dst_width, dst_height, dst_format) = dst;

        // SAFETY: sws_getContext takes only scalars and null filters, and
        // returns NULL rather than trapping on a combination it cannot do.
        let ctx = unsafe {
            ff::sws_getContext(
                src_width as i32,
                src_height as i32,
                src_format,
                dst_width as i32,
                dst_height as i32,
                dst_format,
                flags,
                core::ptr::null_mut(),
                core::ptr::null_mut(),
                core::ptr::null(),
            )
        };
        let ctx = NonNull::new(ctx).ok_or_else(|| {
            CodecError::init(
                "libswscale",
                format!(
                    "no conversion from format {src_format} at {src_width}x{src_height} \
                     to format {dst_format} at {dst_width}x{dst_height}"
                ),
            )
        })?;

        Ok(Scaler {
            ctx,
            src_width,
            src_height,
            dst_width,
            dst_height,
            src_format,
            dst_format,
        })
    }

    /// Packed 32-bit to I420, for the capture side of §7.1.
    ///
    /// Bilinear is the right trade here: the conversion runs once per
    /// captured frame at up to 60 fps, and the encoder's quantiser hides
    /// any difference a slower kernel would buy.
    pub fn to_i420(width: u32, height: u32, format: PackedFormat) -> Result<Self> {
        check_even_geometry("Scaler::to_i420", width, height)?;
        Scaler::new(
            (width, height, format.to_av()),
            (width, height, ff::AVPixelFormat_AV_PIX_FMT_YUV420P),
            SWS_BILINEAR,
        )
    }

    /// I420 to packed 32-bit, for the client's display surface.
    pub fn from_i420(width: u32, height: u32, format: PackedFormat) -> Result<Self> {
        check_even_geometry("Scaler::from_i420", width, height)?;
        Scaler::new(
            (width, height, ff::AVPixelFormat_AV_PIX_FMT_YUV420P),
            (width, height, format.to_av()),
            SWS_BILINEAR,
        )
    }

    /// Convert a packed frame into `dst`, reusing its allocation.
    pub fn convert_to_i420(&mut self, src: &PackedFrame, dst: &mut Yuv420Frame) -> Result<()> {
        self.check_src(src.width, src.height, src.format.to_av())?;
        self.check_dst(dst.width, dst.height, ff::AVPixelFormat_AV_PIX_FMT_YUV420P)?;
        dst.validate()?;

        let src_planes = [
            src.pixels.as_ptr(),
            core::ptr::null(),
            core::ptr::null(),
            core::ptr::null(),
        ];
        let src_strides = [src.stride as i32, 0, 0, 0];
        let dst_planes = [
            dst.y.as_mut_ptr(),
            dst.u.as_mut_ptr(),
            dst.v.as_mut_ptr(),
            core::ptr::null_mut(),
        ];
        let dst_strides = [
            dst.y_stride as i32,
            dst.uv_stride as i32,
            dst.uv_stride as i32,
            0,
        ];

        // SAFETY: the geometry checks above establish that every plane holds
        // at least stride * height bytes, and the context was built for
        // exactly these formats and dimensions.
        let rows = unsafe {
            ff::sws_scale(
                self.ctx.as_ptr(),
                src_planes.as_ptr(),
                src_strides.as_ptr(),
                0,
                src.height as i32,
                dst_planes.as_ptr(),
                dst_strides.as_ptr(),
            )
        };
        self.check_rows(rows, self.dst_height)
    }

    /// Convert a decoded I420 frame into a packed buffer, reusing `dst`.
    pub fn convert_from_i420(&mut self, src: &Yuv420Frame, dst: &mut PackedFrame) -> Result<()> {
        self.check_src(src.width, src.height, ff::AVPixelFormat_AV_PIX_FMT_YUV420P)?;
        self.check_dst(dst.width, dst.height, dst.format.to_av())?;
        src.validate()?;

        let src_planes = [
            src.y.as_ptr(),
            src.u.as_ptr(),
            src.v.as_ptr(),
            core::ptr::null(),
        ];
        let src_strides = [
            src.y_stride as i32,
            src.uv_stride as i32,
            src.uv_stride as i32,
            0,
        ];
        let dst_planes = [
            dst.pixels.as_mut_ptr(),
            core::ptr::null_mut(),
            core::ptr::null_mut(),
            core::ptr::null_mut(),
        ];
        let dst_strides = [dst.stride as i32, 0, 0, 0];

        // SAFETY: as above — planes are validated against the strides and
        // the context matches the formats.
        let rows = unsafe {
            ff::sws_scale(
                self.ctx.as_ptr(),
                src_planes.as_ptr(),
                src_strides.as_ptr(),
                0,
                src.height as i32,
                dst_planes.as_ptr(),
                dst_strides.as_ptr(),
            )
        };
        self.check_rows(rows, self.dst_height)
    }

    fn check_src(&self, width: u32, height: u32, format: i32) -> Result<()> {
        if (width, height, format) != (self.src_width, self.src_height, self.src_format) {
            return Err(CodecError::invalid(
                "libswscale",
                format!(
                    "source is {width}x{height} format {format}, this scaler was built \
                     for {}x{} format {}",
                    self.src_width, self.src_height, self.src_format
                ),
            ));
        }
        Ok(())
    }

    fn check_dst(&self, width: u32, height: u32, format: i32) -> Result<()> {
        if (width, height, format) != (self.dst_width, self.dst_height, self.dst_format) {
            return Err(CodecError::invalid(
                "libswscale",
                format!(
                    "destination is {width}x{height} format {format}, this scaler was \
                     built for {}x{} format {}",
                    self.dst_width, self.dst_height, self.dst_format
                ),
            ));
        }
        Ok(())
    }

    fn check_rows(&self, produced: i32, expected: u32) -> Result<()> {
        if produced != expected as i32 {
            return Err(CodecError::process(
                "libswscale",
                format!("converted {produced} rows, expected {expected}"),
            ));
        }
        Ok(())
    }
}

impl Drop for Scaler {
    fn drop(&mut self) {
        // SAFETY: the context was produced by sws_getContext and this is the
        // only owner, so it has not been freed already.
        unsafe { ff::sws_freeContext(self.ctx.as_ptr()) };
    }
}
