//! The two frame representations that cross this crate's boundary.
//!
//! §7.1 captures the virtio-gpu scanout as a packed 32-bit framebuffer and
//! encodes H.264, so every path here is ARGB in, I420 to the encoder, and
//! back out again on the client. Keeping both shapes owned and checked means
//! no raw pointer or stride ever escapes.

use crate::error::{CodecError, Result};

/// A planar 8-bit 4:2:0 frame — what both H.264 encoders consume and what
/// the decoder produces.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Yuv420Frame {
    pub width: u32,
    pub height: u32,
    /// Luma, `y_stride * height` bytes.
    pub y: Vec<u8>,
    /// Blue-difference chroma, `uv_stride * (height / 2)` bytes.
    pub u: Vec<u8>,
    pub v: Vec<u8>,
    pub y_stride: usize,
    pub uv_stride: usize,
    /// Presentation timestamp in the encoder's timebase (frame numbers).
    pub pts: i64,
}

impl Yuv420Frame {
    /// Allocate a black frame with tightly packed planes.
    ///
    /// Chroma is initialised to 128 rather than 0: 0 is full green in
    /// unsigned chroma, and a green first frame is a very visible artefact
    /// on a console that has not painted yet.
    pub fn black(width: u32, height: u32) -> Result<Self> {
        let (w, h) = check_even_geometry("Yuv420Frame::black", width, height)?;
        let y_stride = w;
        let uv_stride = w / 2;
        Ok(Yuv420Frame {
            width,
            height,
            y: vec![0u8; y_stride * h],
            u: vec![128u8; uv_stride * (h / 2)],
            v: vec![128u8; uv_stride * (h / 2)],
            y_stride,
            uv_stride,
            pts: 0,
        })
    }

    /// Bytes in the luma plane, for bounds checks at the FFI boundary.
    pub fn luma_len(&self) -> usize {
        self.y_stride * self.height as usize
    }

    pub fn chroma_len(&self) -> usize {
        self.uv_stride * (self.height as usize / 2)
    }

    /// Reject a frame whose planes do not match its declared geometry
    /// before its pointers are handed to a C encoder.
    pub fn validate(&self) -> Result<()> {
        let expected_y = self.luma_len();
        let expected_uv = self.chroma_len();
        if self.y.len() < expected_y {
            return Err(CodecError::invalid(
                "Yuv420Frame",
                format!(
                    "luma plane is {} bytes, {expected_y} required for {}x{} at stride {}",
                    self.y.len(),
                    self.width,
                    self.height,
                    self.y_stride
                ),
            ));
        }
        if self.u.len() < expected_uv || self.v.len() < expected_uv {
            return Err(CodecError::invalid(
                "Yuv420Frame",
                format!(
                    "chroma planes are {}/{} bytes, {expected_uv} required each",
                    self.u.len(),
                    self.v.len()
                ),
            ));
        }
        Ok(())
    }
}

/// A packed 32-bit frame, as the virtio-gpu scanout presents it (§7.1) and
/// as the client hands it to a display surface.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackedFrame {
    pub width: u32,
    pub height: u32,
    /// Bytes per row, which may exceed `width * 4` for an aligned scanout.
    pub stride: usize,
    pub pixels: Vec<u8>,
    pub format: PackedFormat,
}

/// The packed layouts this crate converts between. Named for the byte order
/// in memory on a little-endian host, which is how FFmpeg names them too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PackedFormat {
    /// B, G, R, A — the virtio-gpu `B8G8R8A8_UNORM` scanout of §7.1.
    Bgra,
    /// R, G, B, A.
    Rgba,
}

impl PackedFormat {
    pub(crate) const fn to_av(self) -> i32 {
        // AV_PIX_FMT_BGRA = 28, AV_PIX_FMT_RGBA = 26. Taken from the
        // generated enum rather than hardcoded; see `av_pixel_format`.
        match self {
            PackedFormat::Bgra => crate::raw::ffmpeg::AVPixelFormat_AV_PIX_FMT_BGRA,
            PackedFormat::Rgba => crate::raw::ffmpeg::AVPixelFormat_AV_PIX_FMT_RGBA,
        }
    }

    pub const fn bytes_per_pixel(self) -> usize {
        4
    }
}

impl PackedFrame {
    /// Wrap an owned buffer, checking it is big enough for the geometry.
    pub fn new(
        width: u32,
        height: u32,
        stride: usize,
        pixels: Vec<u8>,
        format: PackedFormat,
    ) -> Result<Self> {
        let minimum = stride
            .checked_mul(height as usize)
            .ok_or_else(|| CodecError::invalid("PackedFrame", "stride * height overflows"))?;
        if stride < width as usize * format.bytes_per_pixel() {
            return Err(CodecError::invalid(
                "PackedFrame",
                format!(
                    "stride {stride} is shorter than one {width}-pixel row \
                     ({} bytes)",
                    width as usize * format.bytes_per_pixel()
                ),
            ));
        }
        if pixels.len() < minimum {
            return Err(CodecError::invalid(
                "PackedFrame",
                format!(
                    "buffer is {} bytes, {minimum} required for {width}x{height} \
                     at stride {stride}",
                    pixels.len()
                ),
            ));
        }
        Ok(PackedFrame {
            width,
            height,
            stride,
            pixels,
            format,
        })
    }

    /// Wrap a tightly packed buffer.
    pub fn packed(width: u32, height: u32, pixels: Vec<u8>, format: PackedFormat) -> Result<Self> {
        let stride = width as usize * format.bytes_per_pixel();
        PackedFrame::new(width, height, stride, pixels, format)
    }
}

/// H.264 rejects odd dimensions in 4:2:0, so catch it before libx264 does.
pub(crate) fn check_even_geometry(what: &str, width: u32, height: u32) -> Result<(usize, usize)> {
    if width == 0 || height == 0 {
        return Err(CodecError::invalid(
            what,
            format!("{width}x{height} has a zero dimension"),
        ));
    }
    if width % 2 != 0 || height % 2 != 0 {
        return Err(CodecError::invalid(
            what,
            format!("{width}x{height}: 4:2:0 requires even dimensions"),
        ));
    }
    Ok((width as usize, height as usize))
}
