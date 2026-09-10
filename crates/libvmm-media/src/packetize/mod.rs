//! RTP packetisation — the transmit side of §7.3.
//!
//! The client's [`depacketize`](crate::depacketize) module is the exact
//! inverse of this one, and the round-trip tests pair them so a change to
//! either has to keep both consistent.

pub mod av1;
pub mod h264;
pub mod opus;
pub mod vorbis;
pub mod vp9;

use libvmm_core::{MediaError, VmmResult};

/// Largest RTP payload written into one interleaved frame.
///
/// RTSP interleaves media over the TCP control connection (§7.3), so there
/// is no path MTU to discover — but bounding the payload still matters: an
/// interleaved frame carries a 16-bit length, and small frames keep one
/// large keyframe from stalling the control channel behind it.
pub const DEFAULT_MTU: usize = 1400;

/// Smallest payload that can carry anything useful once headers are
/// subtracted. Below this the packetiser would make no forward progress.
pub const MIN_MTU: usize = 64;

/// The 12-byte RTP header every packet carries.
pub const RTP_HEADER_LEN: usize = 12;

pub(crate) fn check_mtu(what: &'static str, mtu: usize) -> VmmResult<()> {
    if mtu < MIN_MTU {
        return Err(MediaError::Packetize {
            detail: format!("{what}: an MTU of {mtu} is below the {MIN_MTU}-byte minimum"),
        }
        .into());
    }
    Ok(())
}

/// One RTP packet, header included, ready for `rtp::frame`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Packet {
    pub data: Vec<u8>,
    /// Set on the last packet of an access unit (RFC 3550 §5.1).
    pub marker: bool,
}
