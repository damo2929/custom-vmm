//! WSS control client — the client half of §8.
//!
//! Opens `wss://host:port/console`, authenticates with HTTP Basic at the
//! upgrade, then speaks protocol v1 (§8.5) over masked WebSocket text frames.

use crate::transport::{self, Stream};
use libvmm_control::proto::{ClientFrame, ServerFrame};
use libvmm_control::ws::{self, Decoded, Frame, Role};
use libvmm_control::{handshake, tls::CertPolicy};
use libvmm_core::{ControlError, VmmResult};
use std::io::{Read, Write};
use std::time::Duration;

/// A connected, authenticated control session.
pub struct ControlClient {
    stream: Stream,
    /// Bytes received but not yet consumed as complete frames.
    inbox: Vec<u8>,
    seq: u64,
    pub endpoint: String,
}

impl ControlClient {
    /// Connect and perform the §8.3 upgrade.
    ///
    /// Interprets the server's pre-upgrade answers as the spec defines them:
    /// 401 is bad credentials, 503 is the two-client cap, 429 is the §8.4
    /// lockout.
    pub fn connect(
        addr: &str,
        username: &str,
        password: &str,
        policy: CertPolicy,
        timeout: Duration,
    ) -> VmmResult<Self> {
        use base64::Engine as _;

        let host = transport::host_of(addr);
        let mut stream = Stream::connect(addr, &host, Some(policy), timeout)?;
        log::debug!("control transport to {addr}: {}", stream.describe());

        let credential =
            base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"));
        let key =
            base64::engine::general_purpose::STANDARD.encode(libvmm_control::mask_key().repeat(4));

        let request = format!(
            "GET {} HTTP/1.1\r\n\
             Host: {addr}\r\n\
             Upgrade: websocket\r\n\
             Connection: Upgrade\r\n\
             Sec-WebSocket-Key: {key}\r\n\
             Sec-WebSocket-Version: 13\r\n\
             Authorization: Basic {credential}\r\n\r\n",
            handshake::ENDPOINT
        );
        stream
            .write_all(request.as_bytes())
            .and_then(|_| stream.flush())
            .map_err(|e| ControlError::BadUpgrade(format!("sending the upgrade request: {e}")))?;

        // Read until the header block ends; anything after it is already
        // WebSocket data.
        let mut buffer = Vec::with_capacity(1024);
        let head_end = loop {
            if let Some(i) = find_header_end(&buffer) {
                break i;
            }
            let mut chunk = [0u8; 512];
            let n = stream.read(&mut chunk).map_err(|e| {
                ControlError::BadUpgrade(format!("reading the upgrade response: {e}"))
            })?;
            if n == 0 {
                return Err(
                    ControlError::BadUpgrade("server closed during the upgrade".into()).into(),
                );
            }
            buffer.extend_from_slice(&chunk[..n]);
            if buffer.len() > 16 * 1024 {
                return Err(ControlError::BadUpgrade(
                    "upgrade response headers are too large".into(),
                )
                .into());
            }
        };

        let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
        let status = parse_status(&head)
            .ok_or_else(|| ControlError::BadUpgrade(format!("no status line in {head:?}")))?;

        match status {
            101 => {}
            // §8.3: invalid credentials.
            401 => return Err(ControlError::NotAuthenticated.into()),
            // §8.3: at the two-client cap, rejected before the upgrade.
            503 => return Err(ControlError::AtClientCap { max: 2 }.into()),
            // §8.4: locked out; the server did not even check the password.
            429 => {
                let retry = header_value(&head, "retry-after")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                return Err(ControlError::LockedOut {
                    peer: addr.to_string(),
                    remaining_secs: retry,
                }
                .into());
            }
            other => {
                return Err(ControlError::BadUpgrade(format!(
                    "unexpected status {other} at the upgrade"
                ))
                .into())
            }
        }

        // Verify the accept key so we cannot be talked into a non-WebSocket
        // stream by a confused or hostile server.
        let expected = handshake::accept_key(&key);
        match header_value(&head, "sec-websocket-accept") {
            Some(got) if got == expected => {}
            Some(got) => {
                return Err(ControlError::BadUpgrade(format!(
                    "Sec-WebSocket-Accept mismatch: got {got:?}, expected {expected:?}"
                ))
                .into())
            }
            None => {
                return Err(
                    ControlError::BadUpgrade("no Sec-WebSocket-Accept header".into()).into(),
                )
            }
        }

        Ok(ControlClient {
            stream,
            inbox: buffer[head_end..].to_vec(),
            seq: 0,
            endpoint: format!("wss://{addr}{}", handshake::ENDPOINT),
        })
    }

    /// The seq the next frame will carry. §8.5 requires it to be monotonic.
    pub fn next_seq(&self) -> u64 {
        self.seq + 1
    }

    /// Send a protocol-v1 frame, assigning it the next seq.
    pub fn send(&mut self, make: impl FnOnce(u64) -> ClientFrame) -> VmmResult<u64> {
        self.seq += 1;
        let frame = make(self.seq);
        let json =
            libvmm_control::proto::to_json(&frame).ok_or_else(|| ControlError::Internal {
                action: frame.action().to_string(),
                detail: "serialising the frame".into(),
            })?;
        self.send_text(&json)?;
        Ok(self.seq)
    }

    fn send_text(&mut self, text: &str) -> VmmResult<()> {
        // Client frames are masked (RFC 6455 §5.1).
        let bytes = ws::encode_client(&Frame::Text(text.to_string()));
        self.stream
            .write_all(&bytes)
            .and_then(|_| self.stream.flush())
            .map_err(|e| ControlError::Internal {
                action: "send".into(),
                detail: e.to_string(),
            })?;
        Ok(())
    }

    /// Receive the next frame from the server, answering pings transparently.
    ///
    /// Returns `Ok(None)` on a clean close.
    pub fn recv(&mut self, timeout: Option<Duration>) -> VmmResult<Option<ServerFrame>> {
        self.stream.set_read_timeout(timeout);
        loop {
            // Server frames are never masked.
            match ws::decode_as(&self.inbox, Role::Client)? {
                Decoded::Frame(frame, consumed) => {
                    self.inbox.drain(..consumed);
                    match frame {
                        Frame::Text(text) => {
                            let parsed: ServerFrame = serde_json::from_str(&text).map_err(|e| {
                                ControlError::BadFrame(format!("server frame {text:?}: {e}"))
                            })?;
                            return Ok(Some(parsed));
                        }
                        Frame::Ping(payload) => {
                            let pong = ws::encode_client(&Frame::Pong(payload));
                            self.stream.write_all(&pong).ok();
                            self.stream.flush().ok();
                            continue;
                        }
                        Frame::Pong(_) => continue,
                        Frame::Close { code, reason } => {
                            if code == handshake::CLOSE_TRY_AGAIN_LATER {
                                // §8.3: lost the race for the last slot.
                                return Err(ControlError::AtClientCap { max: 2 }.into());
                            }
                            log::debug!("server closed: {code} {reason}");
                            return Ok(None);
                        }
                        Frame::Binary(_) => {
                            return Err(ControlError::BadFrame(
                                "the control channel carries text frames only (§8.5)".into(),
                            )
                            .into())
                        }
                    }
                }
                Decoded::Incomplete => {
                    let mut chunk = [0u8; 4096];
                    match self.stream.read(&mut chunk) {
                        Ok(0) => return Ok(None),
                        Ok(n) => self.inbox.extend_from_slice(&chunk[..n]),
                        Err(e)
                            if matches!(
                                e.kind(),
                                std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                            ) =>
                        {
                            return Ok(None)
                        }
                        Err(e) => {
                            return Err(ControlError::Internal {
                                action: "recv".into(),
                                detail: e.to_string(),
                            }
                            .into())
                        }
                    }
                }
            }
        }
    }

    /// Send a close frame and drop the session.
    pub fn close(mut self) {
        let bytes = ws::encode_client(&Frame::Close {
            code: 1000,
            reason: String::new(),
        });
        self.stream.write_all(&bytes).ok();
        self.stream.flush().ok();
    }
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| i + 4)
}

fn parse_status(head: &str) -> Option<u16> {
    head.lines().next()?.split_whitespace().nth(1)?.parse().ok()
}

fn header_value(head: &str, name: &str) -> Option<String> {
    head.lines()
        .skip(1)
        .filter_map(|l| l.split_once(':'))
        .find(|(n, _)| n.trim().eq_ignore_ascii_case(name))
        .map(|(_, v)| v.trim().to_string())
}
