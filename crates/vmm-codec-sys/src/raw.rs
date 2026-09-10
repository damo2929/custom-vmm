//! Generated bindings, one module per library.
//!
//! Nothing outside this crate should use these directly: the safe wrappers
//! in the sibling modules own every lifetime and every error path.

#![allow(non_upper_case_globals, non_camel_case_types, non_snake_case)]
#![allow(dead_code, clippy::all)]

pub mod ffmpeg {
    include!(concat!(env!("OUT_DIR"), "/ffmpeg.rs"));
}
/// The hand-written C shim over libx264, not libx264's own headers.
/// See `shim/vmm_x264.h` for why x264 is wrapped and the others are not.
pub mod x264 {
    include!(concat!(env!("OUT_DIR"), "/x264.rs"));
}
pub mod vorbis {
    include!(concat!(env!("OUT_DIR"), "/vorbis.rs"));
}
pub mod va {
    include!(concat!(env!("OUT_DIR"), "/va.rs"));
}
