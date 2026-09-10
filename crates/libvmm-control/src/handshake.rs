//! WSS handshake, concurrency and arbitration — §8.3.
//!
//! ```text
//! GET /console HTTP/1.1
//! Upgrade: websocket  Connection: Upgrade
//! Sec-WebSocket-Key: <b64>  Sec-WebSocket-Version: 13
//! Authorization: Basic <base64(user:pass)>
//!  -> valid   : 101 Switching Protocols (Sec-WebSocket-Accept)
//!  -> invalid : 401 + WWW-Authenticate: Basic realm="KVM-Control"
//!  -> at cap  : 503 Service Unavailable (before upgrade) / close 1013 (post-race)
//! ```
//!
//! While a source IP is locked out (§8.4) the answer is 429 and **no
//! credential check happens at all**.

use crate::auth;
use crate::lockout::{Decision, LockoutTable};
use base64::Engine as _;
use sha1::{Digest, Sha1};
use std::net::IpAddr;

/// The single control endpoint (§8.1).
pub const ENDPOINT: &str = "/console";

/// RFC 6455's fixed GUID for the accept-key derivation.
const WS_GUID: &str = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11";

/// WebSocket close code for "try again later", used when a client loses the
/// race for the last slot after the upgrade completed (§8.3).
pub const CLOSE_TRY_AGAIN_LATER: u16 = 1013;

/// A parsed upgrade request.
#[derive(Debug, Clone, Default)]
pub struct UpgradeRequest {
    pub method: String,
    pub path: String,
    pub upgrade: Option<String>,
    pub connection: Option<String>,
    pub websocket_key: Option<String>,
    pub websocket_version: Option<String>,
    pub authorization: Option<String>,
}

impl UpgradeRequest {
    /// Parse the request line and headers of an HTTP/1.1 upgrade.
    pub fn parse(text: &str) -> Option<Self> {
        let mut lines = text.split("\r\n");
        let request_line = lines.next()?;
        let mut parts = request_line.split_whitespace();
        let method = parts.next()?.to_string();
        let path = parts.next()?.to_string();

        let mut r = UpgradeRequest {
            method,
            path,
            ..Default::default()
        };
        for line in lines {
            if line.is_empty() {
                break;
            }
            let (name, value) = line.split_once(':')?;
            let value = value.trim().to_string();
            match name.trim().to_ascii_lowercase().as_str() {
                "upgrade" => r.upgrade = Some(value),
                "connection" => r.connection = Some(value),
                "sec-websocket-key" => r.websocket_key = Some(value),
                "sec-websocket-version" => r.websocket_version = Some(value),
                "authorization" => r.authorization = Some(value),
                _ => {}
            }
        }
        Some(r)
    }

    fn is_websocket_upgrade(&self) -> bool {
        self.method.eq_ignore_ascii_case("GET")
            && self
                .upgrade
                .as_deref()
                .is_some_and(|u| u.eq_ignore_ascii_case("websocket"))
            && self
                .connection
                .as_deref()
                .is_some_and(|c| c.to_ascii_lowercase().contains("upgrade"))
            && self.websocket_version.as_deref() == Some("13")
            && self.websocket_key.is_some()
    }
}

/// What the listener should send back.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HandshakeOutcome {
    /// 101 Switching Protocols, with the computed accept key.
    Accept { accept_key: String },
    /// 401 + `WWW-Authenticate: Basic realm="KVM-Control"`, no upgrade.
    Unauthorized,
    /// 503 Service Unavailable, before the upgrade (§8.3).
    AtCapacity,
    /// 429 Too Many Requests — locked out, credentials never examined (§8.4).
    LockedOut { remaining_secs: u64 },
    /// 400 Bad Request — not a well-formed WebSocket upgrade.
    BadRequest(&'static str),
    /// 404 — the only endpoint is `/console`.
    NotFound,
}

impl HandshakeOutcome {
    pub const fn status(&self) -> u16 {
        match self {
            HandshakeOutcome::Accept { .. } => 101,
            HandshakeOutcome::Unauthorized => 401,
            HandshakeOutcome::AtCapacity => 503,
            HandshakeOutcome::LockedOut { .. } => 429,
            HandshakeOutcome::BadRequest(_) => 400,
            HandshakeOutcome::NotFound => 404,
        }
    }

    pub const fn upgraded(&self) -> bool {
        matches!(self, HandshakeOutcome::Accept { .. })
    }

    /// Render the complete HTTP response.
    pub fn to_http(&self) -> String {
        match self {
            HandshakeOutcome::Accept { accept_key } => format!(
                "HTTP/1.1 101 Switching Protocols\r\n\
                 Upgrade: websocket\r\n\
                 Connection: Upgrade\r\n\
                 Sec-WebSocket-Accept: {accept_key}\r\n\r\n"
            ),
            HandshakeOutcome::Unauthorized => format!(
                "HTTP/1.1 401 Unauthorized\r\n\
                 WWW-Authenticate: {}\r\n\
                 Content-Length: 0\r\n\r\n",
                auth::challenge(auth::WSS_REALM)
            ),
            HandshakeOutcome::AtCapacity => "HTTP/1.1 503 Service Unavailable\r\n\
                 Content-Length: 0\r\n\r\n"
                .to_string(),
            HandshakeOutcome::LockedOut { remaining_secs } => format!(
                "HTTP/1.1 429 Too Many Requests\r\n\
                 Retry-After: {remaining_secs}\r\n\
                 Content-Length: 0\r\n\r\n"
            ),
            HandshakeOutcome::BadRequest(_) => {
                "HTTP/1.1 400 Bad Request\r\nContent-Length: 0\r\n\r\n".to_string()
            }
            HandshakeOutcome::NotFound => {
                "HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\n\r\n".to_string()
            }
        }
    }
}

/// Evaluate one upgrade attempt.
///
/// The order matters and is the order §8.3/§8.4 specify:
/// 1. lockout (no credential check while locked)
/// 2. endpoint and upgrade well-formedness
/// 3. capacity — rejected *before* the upgrade
/// 4. credentials
pub fn evaluate(
    request: &UpgradeRequest,
    source: IpAddr,
    lockout: &mut LockoutTable,
    current_clients: u32,
    cfg: &libvmm_config::ControlWss,
) -> HandshakeOutcome {
    if let Decision::Locked { remaining } = lockout.check(source) {
        return HandshakeOutcome::LockedOut {
            remaining_secs: remaining.as_secs().max(1),
        };
    }

    if request.path != ENDPOINT {
        return HandshakeOutcome::NotFound;
    }
    if !request.is_websocket_upgrade() {
        return HandshakeOutcome::BadRequest("not a WebSocket/13 upgrade");
    }

    // §8.3: a 3rd client is rejected at handshake, before the upgrade.
    if current_clients >= cfg.max_clients {
        return HandshakeOutcome::AtCapacity;
    }

    if cfg.auth_required {
        let supplied = request.authorization.as_deref().and_then(auth::parse_basic);
        let ok = supplied
            .as_ref()
            .is_some_and(|c| auth::credentials_match(c, &cfg.username, &cfg.password));
        if !ok {
            if let Decision::Locked { remaining } = lockout.record_failure(source) {
                return HandshakeOutcome::LockedOut {
                    remaining_secs: remaining.as_secs().max(1),
                };
            }
            return HandshakeOutcome::Unauthorized;
        }
        lockout.record_success(source);
    }

    let key = request.websocket_key.as_deref().unwrap_or_default();
    HandshakeOutcome::Accept {
        accept_key: accept_key(key),
    }
}

/// RFC 6455 `Sec-WebSocket-Accept`: base64(SHA1(key + GUID)).
pub fn accept_key(client_key: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.update(client_key.as_bytes());
    hasher.update(WS_GUID.as_bytes());
    base64::engine::general_purpose::STANDARD.encode(hasher.finalize())
}

/// Tracks the authenticated clients and enforces the hard cap (§8.3).
#[derive(Debug, Default)]
pub struct ClientRegistry {
    max: u32,
    clients: Vec<u64>,
    next_id: u64,
}

impl ClientRegistry {
    pub fn new(max_clients: u32) -> Self {
        ClientRegistry {
            max: max_clients,
            clients: Vec::new(),
            next_id: 1,
        }
    }

    /// Claim a slot. `None` when at capacity — the post-upgrade race in §8.3,
    /// where the caller closes with 1013.
    pub fn admit(&mut self) -> Option<u64> {
        if self.clients.len() as u32 >= self.max {
            return None;
        }
        let id = self.next_id;
        self.next_id += 1;
        self.clients.push(id);
        Some(id)
    }

    pub fn release(&mut self, id: u64) {
        self.clients.retain(|c| *c != id);
    }

    pub fn count(&self) -> u32 {
        self.clients.len() as u32
    }

    pub fn is_full(&self) -> bool {
        self.count() >= self.max
    }
}

/// Input arbitration — §8.3.
///
/// Both clients share one input stream, last-write-wins; there is no
/// primary/observer role. The latest input frame from either client wins.
#[derive(Debug, Default)]
pub struct InputArbiter {
    /// Which client produced the most recent input frame.
    last_writer: Option<u64>,
    accepted: u64,
}

impl InputArbiter {
    pub fn new() -> Self {
        Self::default()
    }

    /// Accept an input frame from `client`. Always true: last-write-wins
    /// means no frame is ever dropped for arbitration reasons.
    pub fn accept(&mut self, client: u64) -> bool {
        self.last_writer = Some(client);
        self.accepted += 1;
        true
    }

    pub fn last_writer(&self) -> Option<u64> {
        self.last_writer
    }

    pub fn accepted(&self) -> u64 {
        self.accepted
    }
}
