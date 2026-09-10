//! `libvmm-control` — the unified control channel (§8).
//!
//! `wss://[::]:8080/console`, a single bi-directional channel gated by HTTP
//! Basic Auth at the HTTP/1.1 -> WebSocket upgrade, carrying JSON protocol v1.

// §0.1: datapath code MUST NOT unwrap, expect or panic. Test code may.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod auth;
pub mod handshake;
// §8.2 permits no cleartext fallback, so the control listener only exists
// where a TLS 1.3 provider is compiled in.
pub mod listener;
pub mod lockout;
pub mod proto;
pub mod tls;
pub mod ws;

pub use handshake::{ClientRegistry, HandshakeOutcome, InputArbiter, UpgradeRequest};
pub use listener::{ActionHandler, ControlListener};
pub use lockout::{Decision, LockoutTable};
pub use proto::{parse_client_frame, ClientFrame, InputFrame, ServerFrame, PROTOCOL_VERSION};
pub use ws::Frame;

/// A fresh 32-bit WebSocket masking key (RFC 6455 §5.3).
///
/// The mask exists to stop intermediaries from being confused by
/// attacker-chosen plaintext, so it must be unpredictable. This reads the
/// kernel CSPRNG directly rather than pulling in a PRNG crate.
pub fn mask_key() -> [u8; 4] {
    let mut key = [0u8; 4];
    // SAFETY: writes exactly `key.len()` bytes into a live local buffer.
    let n = unsafe { libc::getrandom(key.as_mut_ptr().cast(), key.len(), 0) };
    if n != key.len() as isize {
        // getrandom cannot fail for a 4-byte request without a blocking flag,
        // but never fall back to a constant mask: derive one from the clock
        // rather than sending predictable frames.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0x9E37_79B9);
        key = now.rotate_left(7).to_ne_bytes();
    }
    key
}
