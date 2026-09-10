//! Route FFmpeg's and libva's diagnostics into the `log` crate.
//!
//! Both libraries write to stderr by default, which on a VMM interleaves
//! codec chatter with the machine's own log and bypasses whatever the
//! operator configured. [`install`] redirects FFmpeg once per process;
//! libva is redirected per display, in [`crate::h264::vaapi`], because its
//! callbacks are set on the display rather than globally.

use crate::raw::ffmpeg as ff;
use std::sync::Once;

static INSTALLED: Once = Once::new();

/// Redirect FFmpeg's log into `log`. Safe to call repeatedly; only the
/// first call has any effect.
pub fn install() {
    INSTALLED.call_once(|| {
        // SAFETY: the callback has exactly the signature av_log_set_callback
        // declares, and is valid for the life of the process.
        unsafe {
            ff::av_log_set_callback(Some(av_log_callback));
            // Let the callback decide what to keep: filtering here would
            // discard lines before `log`'s own level could see them.
            ff::av_log_set_level(ff::AV_LOG_VERBOSE as i32);
        }
    });
}

/// # Safety
/// Called by FFmpeg with a format string and its matching `va_list`.
unsafe extern "C" fn av_log_callback(
    avcl: *mut core::ffi::c_void,
    level: core::ffi::c_int,
    fmt: *const core::ffi::c_char,
    args: *mut ff::__va_list_tag,
) {
    if fmt.is_null() {
        return;
    }

    // Formatting is left to FFmpeg: av_log_format_line2 consumes the
    // va_list correctly, which is not something to reimplement in Rust.
    let mut line = [0i8; 1024];
    let mut print_prefix: core::ffi::c_int = 1;
    // SAFETY: `line` is a live buffer of the length passed alongside it, and
    // fmt/args are the pair FFmpeg just handed us.
    let written = unsafe {
        ff::av_log_format_line2(
            avcl,
            level,
            fmt,
            args,
            line.as_mut_ptr(),
            line.len() as core::ffi::c_int,
            &mut print_prefix,
        )
    };
    if written <= 0 {
        return;
    }

    // SAFETY: av_log_format_line2 NUL-terminates within the buffer.
    let text = unsafe { core::ffi::CStr::from_ptr(line.as_ptr()) };
    let text = text.to_string_lossy();
    let text = text.trim_end();
    if text.is_empty() {
        return;
    }

    // FFmpeg's levels rise as severity falls.
    match level {
        l if l <= ff::AV_LOG_ERROR as i32 => log::error!("ffmpeg: {text}"),
        l if l <= ff::AV_LOG_WARNING as i32 => log::warn!("ffmpeg: {text}"),
        l if l <= ff::AV_LOG_INFO as i32 => log::info!("ffmpeg: {text}"),
        _ => log::debug!("ffmpeg: {text}"),
    }
}
