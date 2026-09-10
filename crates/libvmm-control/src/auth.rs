//! HTTP Basic authentication for the WSS and RTSPS listeners (§7.4, §8.3).
//!
//! Known limitation (§8.4): credentials are inline plaintext in the TOML and
//! may be reused across services. Basic Auth is protected on the wire only by
//! the mandatory TLS 1.3. The comparison below is timing-safe, which is one
//! of the four hardening steps §8.4 lists; hashed storage, per-service
//! separation and external secret sourcing remain future work.

use base64::Engine as _;

/// The realm the WSS listener advertises (§8.3).
pub const WSS_REALM: &str = "KVM-Control";
/// The realm the RTSPS listener advertises (§7.4).
pub const RTSP_REALM: &str = "KVM-Secure-Console";

/// Decode an `Authorization: Basic <base64(user:pass)>` header value.
pub fn parse_basic(header_value: &str) -> Option<(String, String)> {
    let encoded = header_value
        .strip_prefix("Basic ")
        .or_else(|| header_value.strip_prefix("basic "))?;
    let decoded = base64::engine::general_purpose::STANDARD
        .decode(encoded.trim())
        .ok()?;
    let text = String::from_utf8(decoded).ok()?;
    let (user, pass) = text.split_once(':')?;
    Some((user.to_string(), pass.to_string()))
}

/// Constant-time byte comparison, so a wrong password cannot be recovered
/// one character at a time from response timing.
pub fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        // Still compare something so the early return does not itself leak
        // more than the length, which the ciphertext already reveals.
        let mut diff = 1u8;
        for (x, y) in a.iter().zip(b.iter()) {
            diff |= x ^ y;
        }
        return diff == 0 && a.len() == b.len();
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

/// Check a decoded credential pair against the configured one.
pub fn credentials_match(supplied: &(String, String), user: &str, password: &str) -> bool {
    let u = constant_time_eq(supplied.0.as_bytes(), user.as_bytes());
    let p = constant_time_eq(supplied.1.as_bytes(), password.as_bytes());
    // Both are evaluated unconditionally: `&&` would short-circuit and leak
    // whether the username alone was right.
    u & p
}

/// The `WWW-Authenticate` header for a realm.
pub fn challenge(realm: &str) -> String {
    format!("Basic realm=\"{realm}\"")
}
