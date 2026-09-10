//! What this host's VA-API stack can encode.
//!
//! [`probe`] opens the DRM render node and enumerates the profiles that
//! carry an encode entrypoint. This is the only reliable answer: "the driver
//! loaded" says nothing about which codecs the fixed-function engine
//! implements, and the two common surprises both bite here — recent AMD and
//! Intel parts ship AV1 encode with no H.264 encode, and Fedora's stock Mesa
//! is built without H.264/HEVC for patent reasons, so a GPU that encodes
//! H.264 perfectly well reports nothing at all.
//!
//! The probe is what makes codec selection honest. Rather than trusting a
//! configured accelerator, the encoder asks the driver what it can do and
//! the negotiation in `libvmm-media` picks from the answer.

use super::config::VideoCodec;
use crate::error::{CodecError, Result};
use crate::raw::va;
use std::ffi::{CStr, CString};
use std::path::{Path, PathBuf};

/// The render nodes to try, in order, when the caller names none.
const DEFAULT_RENDER_NODES: &[&str] = &[
    "/dev/dri/renderD128",
    "/dev/dri/renderD129",
    "/dev/dri/renderD130",
];

/// What a host's VA-API stack can actually do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VaapiCapability {
    pub render_node: PathBuf,
    /// The driver's own description, e.g. the Mesa Gallium version string.
    pub driver: String,
    pub version: (i32, i32),
    /// Profiles carrying an encode entrypoint, per codec.
    pub encode_profiles: Vec<(VideoCodec, &'static str)>,
}

impl VaapiCapability {
    /// Can this host encode `codec` in hardware?
    pub fn can_encode(&self, codec: VideoCodec) -> bool {
        self.encode_profiles.iter().any(|(c, _)| *c == codec)
    }

    /// The profile names available for one codec.
    pub fn profiles_for(&self, codec: VideoCodec) -> Vec<&'static str> {
        self.encode_profiles
            .iter()
            .filter(|(c, _)| *c == codec)
            .map(|(_, name)| *name)
            .collect()
    }

    /// Every codec this host can encode, for the boot log.
    pub fn encodable(&self) -> Vec<VideoCodec> {
        let mut out: Vec<VideoCodec> = Vec::new();
        for (codec, _) in &self.encode_profiles {
            if !out.contains(codec) {
                out.push(*codec);
            }
        }
        out
    }
}

/// An open VA display, closed on drop.
struct VaDisplay {
    display: va::VADisplay,
    fd: core::ffi::c_int,
    version: (i32, i32),
}

impl VaDisplay {
    fn open(node: &Path) -> Result<Self> {
        let path = CString::new(node.as_os_str().as_encoded_bytes()).map_err(|e| {
            CodecError::unavailable("VA-API", format!("render node path is not a C string: {e}"))
        })?;

        // SAFETY: path is a valid NUL-terminated C string. O_RDWR (2) is
        // what libva requires of a render node.
        let fd = unsafe { libc::open(path.as_ptr(), libc::O_RDWR | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(CodecError::unavailable(
                "VA-API",
                format!(
                    "opening {}: {}",
                    node.display(),
                    std::io::Error::last_os_error()
                ),
            ));
        }

        // SAFETY: fd is an open DRM render node.
        let display = unsafe { va::vaGetDisplayDRM(fd) };
        if display.is_null() {
            // SAFETY: fd is open and owned here; nothing else holds it.
            unsafe { libc::close(fd) };
            return Err(CodecError::unavailable(
                "VA-API",
                format!("vaGetDisplayDRM returned no display for {}", node.display()),
            ));
        }

        // libva writes its probe chatter straight to stderr by default,
        // which would interleave with the VMM's own log. Route both streams
        // into `log` before vaInitialize does any of it.
        // SAFETY: display is a valid VADisplay; both callbacks have the
        // signature libva declares and ignore their null user context.
        unsafe {
            va::vaSetInfoCallback(display, Some(va_info_callback), core::ptr::null_mut());
            va::vaSetErrorCallback(display, Some(va_error_callback), core::ptr::null_mut());
        }

        let mut major = 0;
        let mut minor = 0;
        // SAFETY: display came from vaGetDisplayDRM and the out-parameters
        // are live.
        let status = unsafe { va::vaInitialize(display, &mut major, &mut minor) };
        if status != va::VA_STATUS_SUCCESS as va::VAStatus {
            // SAFETY: fd is still open and owned here. The display is not
            // terminated because vaInitialize failed.
            unsafe { libc::close(fd) };
            return Err(CodecError::unavailable(
                "VA-API",
                format!("vaInitialize on {}: {}", node.display(), va_error(status)),
            ));
        }

        Ok(VaDisplay {
            display,
            fd,
            version: (major, minor),
        })
    }

    fn version(&self) -> (i32, i32) {
        self.version
    }

    fn vendor(&self) -> String {
        // SAFETY: the display is initialised; the returned string is owned
        // by the driver and valid until vaTerminate.
        let raw = unsafe { va::vaQueryVendorString(self.display) };
        if raw.is_null() {
            return "unknown".to_string();
        }
        // SAFETY: non-null and NUL-terminated per the libva contract.
        unsafe { CStr::from_ptr(raw) }
            .to_string_lossy()
            .into_owned()
    }

    /// Profiles on this display that support an encode entrypoint.
    fn encode_profiles(&self) -> Result<Vec<(VideoCodec, &'static str)>> {
        // SAFETY: the display is initialised.
        let max_profiles = unsafe { va::vaMaxNumProfiles(self.display) };
        if max_profiles <= 0 {
            return Ok(Vec::new());
        }
        let mut profiles = vec![va::VAProfile_VAProfileNone; max_profiles as usize];
        let mut count: core::ffi::c_int = 0;
        // SAFETY: profiles has max_profiles entries, which is the capacity
        // vaMaxNumProfiles just reported.
        let status =
            unsafe { va::vaQueryConfigProfiles(self.display, profiles.as_mut_ptr(), &mut count) };
        if status != va::VA_STATUS_SUCCESS as va::VAStatus {
            return Err(CodecError::unavailable(
                "VA-API",
                format!("vaQueryConfigProfiles: {}", va_error(status)),
            ));
        }
        profiles.truncate(count.max(0) as usize);

        let mut found = Vec::new();
        for profile in profiles {
            let Some((codec, name)) = encodable_profile(profile) else {
                continue;
            };
            if self.has_encode_entrypoint(profile)? {
                found.push((codec, name));
            }
        }
        Ok(found)
    }

    fn has_encode_entrypoint(&self, profile: va::VAProfile) -> Result<bool> {
        // SAFETY: the display is initialised.
        let max = unsafe { va::vaMaxNumEntrypoints(self.display) };
        if max <= 0 {
            return Ok(false);
        }
        let mut entrypoints = vec![0 as va::VAEntrypoint; max as usize];
        let mut count: core::ffi::c_int = 0;
        // SAFETY: entrypoints has `max` entries.
        let status = unsafe {
            va::vaQueryConfigEntrypoints(
                self.display,
                profile,
                entrypoints.as_mut_ptr(),
                &mut count,
            )
        };
        if status != va::VA_STATUS_SUCCESS as va::VAStatus {
            // A profile that refuses to enumerate is simply not usable.
            return Ok(false);
        }
        entrypoints.truncate(count.max(0) as usize);
        Ok(entrypoints.iter().any(|e| {
            *e == va::VAEntrypoint_VAEntrypointEncSlice
                || *e == va::VAEntrypoint_VAEntrypointEncSliceLP
        }))
    }
}

impl Drop for VaDisplay {
    fn drop(&mut self) {
        // SAFETY: the display was initialised by vaInitialize and the fd is
        // owned here. vaTerminate must come first: it still uses the fd.
        unsafe {
            va::vaTerminate(self.display);
            libc::close(self.fd);
        }
    }
}

/// The VA-API profiles this tree can encode with, and the codec each means.
///
/// Anything not listed is ignored rather than guessed at: a profile we do
/// not have an encoder for is not a capability, however encodable the
/// driver says it is.
fn encodable_profile(profile: va::VAProfile) -> Option<(VideoCodec, &'static str)> {
    match profile {
        va::VAProfile_VAProfileH264ConstrainedBaseline => {
            Some((VideoCodec::H264, "H.264 Constrained Baseline"))
        }
        va::VAProfile_VAProfileH264Main => Some((VideoCodec::H264, "H.264 Main")),
        va::VAProfile_VAProfileH264High => Some((VideoCodec::H264, "H.264 High")),
        va::VAProfile_VAProfileVP9Profile0 => Some((VideoCodec::Vp9, "VP9 Profile 0")),
        va::VAProfile_VAProfileVP9Profile2 => Some((VideoCodec::Vp9, "VP9 Profile 2")),
        va::VAProfile_VAProfileAV1Profile0 => Some((VideoCodec::Av1, "AV1 Profile 0")),
        _ => None,
    }
}

/// libva's info stream. Debug, not info: it is one line per driver probed.
unsafe extern "C" fn va_info_callback(
    _context: *mut core::ffi::c_void,
    message: *const core::ffi::c_char,
) {
    if let Some(text) = va_message(message) {
        log::debug!("libva: {text}");
    }
}

unsafe extern "C" fn va_error_callback(
    _context: *mut core::ffi::c_void,
    message: *const core::ffi::c_char,
) {
    if let Some(text) = va_message(message) {
        log::warn!("libva: {text}");
    }
}

fn va_message(message: *const core::ffi::c_char) -> Option<String> {
    if message.is_null() {
        return None;
    }
    // SAFETY: libva passes a NUL-terminated string valid for the call.
    let text = unsafe { CStr::from_ptr(message) };
    let text = text.to_string_lossy();
    let trimmed = text.trim_end();
    (!trimmed.is_empty()).then(|| trimmed.to_string())
}

fn va_error(status: va::VAStatus) -> String {
    // SAFETY: vaErrorStr accepts any status and returns a static string.
    let raw = unsafe { va::vaErrorStr(status) };
    if raw.is_null() {
        return format!("VA status {status}");
    }
    // SAFETY: non-null and NUL-terminated per the libva contract.
    unsafe { CStr::from_ptr(raw) }
        .to_string_lossy()
        .into_owned()
}

/// Ask the host whether it can encode H.264 in hardware.
///
/// `node` names a DRM render node, or `None` to try the usual ones. An
/// `Err` here is not fatal: it is the signal for [`super::H264Encoder`] to
/// fall back to libx264.
pub fn probe(node: Option<&Path>) -> Result<VaapiCapability> {
    let candidates: Vec<PathBuf> = match node {
        Some(node) => vec![node.to_path_buf()],
        None => DEFAULT_RENDER_NODES.iter().map(PathBuf::from).collect(),
    };

    let mut last: Option<CodecError> = None;
    for candidate in &candidates {
        if !candidate.exists() {
            continue;
        }
        match probe_one(candidate) {
            Ok(capability) => return Ok(capability),
            Err(e) => last = Some(e),
        }
    }

    Err(last.unwrap_or_else(|| {
        CodecError::unavailable(
            "VA-API",
            format!(
                "no DRM render node found (tried {})",
                candidates
                    .iter()
                    .map(|p| p.display().to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        )
    }))
}

fn probe_one(node: &Path) -> Result<VaapiCapability> {
    let display = VaDisplay::open(node)?;
    let profiles = display.encode_profiles()?;
    Ok(VaapiCapability {
        render_node: node.to_path_buf(),
        driver: display.vendor(),
        version: display.version(),
        encode_profiles: profiles,
    })
}
