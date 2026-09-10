//! Codec errors, mapped onto the §7.1 media error space.
//!
//! Every C call in this crate funnels its failure through here, so the
//! caller never sees a bare negative integer.

use std::fmt;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodecError {
    /// A codec, device or encoder the host does not provide.
    Unavailable { what: String, detail: String },
    /// Opening or configuring an encoder/decoder failed.
    Init { what: String, detail: String },
    /// A frame could not be encoded or decoded.
    Process { what: String, detail: String },
    /// The caller handed us a buffer or geometry the codec cannot accept.
    Invalid { what: String, detail: String },
}

impl CodecError {
    pub fn unavailable(what: impl Into<String>, detail: impl Into<String>) -> Self {
        CodecError::Unavailable {
            what: what.into(),
            detail: detail.into(),
        }
    }
    pub fn init(what: impl Into<String>, detail: impl Into<String>) -> Self {
        CodecError::Init {
            what: what.into(),
            detail: detail.into(),
        }
    }
    pub fn process(what: impl Into<String>, detail: impl Into<String>) -> Self {
        CodecError::Process {
            what: what.into(),
            detail: detail.into(),
        }
    }
    pub fn invalid(what: impl Into<String>, detail: impl Into<String>) -> Self {
        CodecError::Invalid {
            what: what.into(),
            detail: detail.into(),
        }
    }

    /// Which subsystem failed, for the caller's own error mapping.
    pub fn what(&self) -> &str {
        match self {
            CodecError::Unavailable { what, .. }
            | CodecError::Init { what, .. }
            | CodecError::Process { what, .. }
            | CodecError::Invalid { what, .. } => what,
        }
    }

    /// Is this the kind of failure a fallback backend could recover from?
    ///
    /// Only [`CodecError::Unavailable`] is: it means the host lacks the
    /// hardware or the build lacks the codec, both of which the software
    /// path can answer. An init or process failure on an available device
    /// is a real fault and must surface.
    pub fn is_recoverable(&self) -> bool {
        matches!(self, CodecError::Unavailable { .. })
    }
}

impl fmt::Display for CodecError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CodecError::Unavailable { what, detail } => {
                write!(f, "{what} is unavailable on this host: {detail}")
            }
            CodecError::Init { what, detail } => write!(f, "initialising {what}: {detail}"),
            CodecError::Process { what, detail } => write!(f, "{what}: {detail}"),
            CodecError::Invalid { what, detail } => write!(f, "invalid input to {what}: {detail}"),
        }
    }
}

impl std::error::Error for CodecError {}

pub type Result<T> = std::result::Result<T, CodecError>;

/// Render an FFmpeg return code as text, using `av_strerror` when it can.
pub(crate) fn av_error(code: i32) -> String {
    let mut buf = [0i8; 256];
    // SAFETY: av_strerror writes at most buf.len() bytes into buf and always
    // NUL-terminates. A negative return means it had no description, in
    // which case we fall back to the raw code.
    let described = unsafe { crate::raw::ffmpeg::av_strerror(code, buf.as_mut_ptr(), buf.len()) };
    if described < 0 {
        return format!("error {code}");
    }
    // SAFETY: buf is NUL-terminated by the call above.
    let text = unsafe { std::ffi::CStr::from_ptr(buf.as_ptr()) };
    format!("{} ({code})", text.to_string_lossy())
}

/// `AVERROR(EAGAIN)` — the encoder or decoder needs more input.
///
/// FFmpeg's `AVERROR` macro negates the errno on POSIX.
pub(crate) const fn averror_eagain() -> i32 {
    -libc::EAGAIN
}

/// `AVERROR_INVALIDDATA` — the bitstream could not be parsed.
pub(crate) const fn averror_invaliddata() -> i32 {
    -((b'I' as i32) | ((b'N' as i32) << 8) | ((b'D' as i32) << 16) | ((b'A' as i32) << 24))
}

/// `AVERROR_EOF`, which FFmpeg defines as `FFERRTAG('E','O','F',' ')`.
pub(crate) const fn averror_eof() -> i32 {
    -((b'E' as i32) | ((b'O' as i32) << 8) | ((b'F' as i32) << 16) | ((b' ' as i32) << 24))
}
