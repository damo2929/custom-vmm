//! The WSS control listener — the server half of §8.
//!
//! `wss://[::]:8080/console`: accept, TLS 1.3, HTTP Basic Auth at the
//! upgrade, then protocol v1 over WebSocket text frames.
//!
//! Runs on the single `wss-listener` thread of §1.2. The §8.3 hard cap of two
//! clients is enforced twice — once before the upgrade (503) and once when a
//! session claims its slot (close 1013) — because the two checks race.

use crate::handshake::{self, ClientRegistry, HandshakeOutcome, InputArbiter, UpgradeRequest};
use crate::lockout::LockoutTable;
use crate::proto::{self, ClientFrame, ServerFrame};
use crate::ws::{self, Decoded, Frame, Role};
use libvmm_config::ControlWss;
use libvmm_core::{ControlError, VmmResult};
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};

/// What the VMM does when a control action arrives.
///
/// The listener owns no VM state; it validates and dispatches. Returning
/// `Err` produces an error frame carrying the Appendix A code, and never
/// closes the socket (§8.5 robustness rule).
pub trait ActionHandler: Send + Sync {
    /// A validated input event, already range-checked (§8.5).
    fn on_input(&self, client: u64, frame: &proto::InputFrame) -> Result<(), ControlError>;
    /// ACPI Power Button SCI.
    fn on_powerdown(&self, client: u64) -> Result<(), ControlError>;
    /// Pulse FADT.RESET_REG.
    fn on_reboot(&self, client: u64) -> Result<(), ControlError>;
    /// Start a backup. Progress is reported through `progress`.
    fn on_backup(
        &self,
        client: u64,
        path: &str,
        progress: &dyn Fn(ServerFrame),
    ) -> Result<(), ControlError>;
}

/// The listener's shared state, held across the accept loop.
pub struct ControlListener {
    config: ControlWss,
    lockout: Mutex<LockoutTable>,
    registry: Mutex<ClientRegistry>,
    arbiter: Mutex<InputArbiter>,
    handler: Arc<dyn ActionHandler>,
    tls: Arc<rustls::ServerConfig>,
}

impl ControlListener {
    /// Build a listener. TLS 1.3 is mandatory (§8.2), so this fails rather
    /// than serving cleartext when no provider is compiled in.
    pub fn new(
        config: ControlWss,
        vm_name: &str,
        handler: Arc<dyn ActionHandler>,
    ) -> VmmResult<Self> {
        crate::tls::check_tls_min("control_wss", &config.tls_min)?;
        // §8.2: the hypervisor generates and self-signs its certificate at
        // boot, so a fresh identity is minted here rather than loaded.
        let identity = crate::tls::SelfSignedIdentity::generate(vm_name)?;
        let tls = crate::tls::server_config(&identity)?;

        Ok(ControlListener {
            lockout: Mutex::new(LockoutTable::from_config(&config)),
            registry: Mutex::new(ClientRegistry::new(config.max_clients)),
            arbiter: Mutex::new(InputArbiter::new()),
            handler,
            tls,
            config,
        })
    }

    pub fn bind_address(&self) -> String {
        format!("{}:{}", self.config.ipv6_bind, self.config.port)
    }

    /// Bind the socket. Separated from [`serve`] so §1.5 can confirm the
    /// listener is up before starting vCPUs.
    pub fn bind(&self) -> VmmResult<TcpListener> {
        let addr = self.bind_address();
        TcpListener::bind(&addr).map_err(|e| {
            ControlError::Bind {
                addr,
                detail: e.to_string(),
            }
            .into()
        })
    }

    /// Accept and serve until the listener is dropped.
    pub fn serve(self: Arc<Self>, listener: TcpListener) {
        for connection in listener.incoming() {
            match connection {
                Ok(stream) => {
                    let this = Arc::clone(&self);
                    // One thread per client; the §8.3 cap bounds it at two.
                    std::thread::Builder::new()
                        .name("wss-client".to_string())
                        .spawn(move || this.serve_one(stream))
                        .ok();
                }
                Err(e) => log::warn!("wss-listener: accept failed: {e}"),
            }
        }
    }

    fn serve_one(&self, tcp: TcpStream) {
        let peer = tcp.peer_addr().ok();
        let source = peer.map(|a| a.ip());
        tcp.set_nodelay(true).ok();

        let mut session = match self.upgrade(tcp, source) {
            Ok(Some(s)) => s,
            Ok(None) => return,
            Err(e) => {
                log::warn!("wss-listener: [{} {}] {e}", e.domain(), e.code());
                return;
            }
        };

        log::info!("control client {} connected from {:?}", session.id, peer);
        if let Err(e) = self.pump(&mut session) {
            log::warn!(
                "control client {}: [{} {}] {e}",
                session.id,
                e.domain(),
                e.code()
            );
        }
        self.registry.lock().map(|mut r| r.release(session.id)).ok();
        log::info!("control client {} disconnected", session.id);
    }

    /// Run the §8.3 handshake and claim a client slot.
    fn upgrade(
        &self,
        tcp: TcpStream,
        source: Option<std::net::IpAddr>,
    ) -> VmmResult<Option<Session>> {
        let mut stream = self.wrap_tls(tcp)?;

        let mut buffer = Vec::with_capacity(1024);
        let head_end = loop {
            if let Some(i) = find_header_end(&buffer) {
                break i;
            }
            let mut chunk = [0u8; 512];
            let n = stream.read(&mut chunk).map_err(|e| {
                ControlError::BadUpgrade(format!("reading the upgrade request: {e}"))
            })?;
            if n == 0 {
                return Ok(None);
            }
            buffer.extend_from_slice(&chunk[..n]);
            if buffer.len() > 16 * 1024 {
                return Err(ControlError::BadUpgrade(
                    "upgrade request headers are too large".into(),
                )
                .into());
            }
        };

        let text = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
        let request = UpgradeRequest::parse(&text)
            .ok_or_else(|| ControlError::BadUpgrade("malformed request line".to_string()))?;

        let source = source.unwrap_or(std::net::IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED));
        let current = self
            .registry
            .lock()
            .map(|r| r.count())
            .unwrap_or(self.config.max_clients);

        let outcome = {
            let mut lockout = self.lockout.lock().map_err(|_| ControlError::Internal {
                action: "lockout".into(),
                detail: "poisoned".into(),
            })?;
            handshake::evaluate(&request, source, &mut lockout, current, &self.config)
        };

        stream.write_all(outcome.to_http().as_bytes()).ok();
        stream.flush().ok();

        if !outcome.upgraded() {
            log::info!("wss-listener: {source} rejected with {}", outcome.status());
            return Ok(None);
        }
        debug_assert!(matches!(outcome, HandshakeOutcome::Accept { .. }));

        // The capacity check above and this claim race, so a client can still
        // lose here: §8.3 says close 1013 rather than serve a third client.
        let id = match self.registry.lock().ok().and_then(|mut r| r.admit()) {
            Some(id) => id,
            None => {
                let close = ws::encode(&Frame::Close {
                    code: handshake::CLOSE_TRY_AGAIN_LATER,
                    reason: "at capacity".to_string(),
                });
                stream.write_all(&close).ok();
                stream.flush().ok();
                return Ok(None);
            }
        };

        Ok(Some(Session {
            id,
            stream,
            inbox: buffer[head_end..].to_vec(),
        }))
    }

    fn wrap_tls(&self, tcp: TcpStream) -> VmmResult<ServerStream> {
        let connection = rustls::ServerConnection::new(Arc::clone(&self.tls))
            .map_err(|e| ControlError::Tls(format!("starting the TLS 1.3 handshake: {e}")))?;
        Ok(ServerStream::Tls(Box::new(rustls::StreamOwned::new(
            connection, tcp,
        ))))
    }

    /// Read frames until the client goes away.
    ///
    /// §8.5: a bad frame yields an error frame and MUST NOT close the socket.
    fn pump(&self, session: &mut Session) -> VmmResult<()> {
        loop {
            let frame = match ws::decode_as(&session.inbox, Role::Server) {
                Ok(Decoded::Frame(f, consumed)) => {
                    session.inbox.drain(..consumed);
                    f
                }
                Ok(Decoded::Incomplete) => {
                    let mut chunk = [0u8; 4096];
                    match session.stream.read(&mut chunk) {
                        Ok(0) => return Ok(()),
                        Ok(n) => {
                            session.inbox.extend_from_slice(&chunk[..n]);
                            continue;
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                        Err(e) => {
                            return Err(ControlError::Internal {
                                action: "recv".into(),
                                detail: e.to_string(),
                            }
                            .into())
                        }
                    }
                }
                // A framing error is fatal to the stream: we cannot find the
                // next boundary. This is distinct from a bad *control frame*,
                // which §8.5 says must not close the socket.
                Err(e) => return Err(e),
            };

            match frame {
                Frame::Text(text) => self.dispatch(session, &text)?,
                Frame::Ping(payload) => session.send_raw(&ws::encode(&Frame::Pong(payload)))?,
                Frame::Pong(_) => {}
                Frame::Close { .. } => return Ok(()),
                Frame::Binary(_) => {
                    let reply = ServerFrame::error_with(
                        0,
                        6400,
                        "the control channel carries text frames only",
                    );
                    session.send(&reply)?;
                }
            }
        }
    }

    /// Parse and act on one protocol-v1 frame.
    fn dispatch(&self, session: &mut Session, text: &str) -> VmmResult<()> {
        let frame = match proto::parse_client_frame(text) {
            Ok(f) => f,
            Err((seq, e)) => {
                // §8.5: answer with an error frame, keep the socket open.
                return session.send(&ServerFrame::error(seq, &e));
            }
        };

        let seq = frame.seq();
        let id = session.id;
        let result = match &frame {
            ClientFrame::Input(input) => {
                // §8.3: shared input stream, last-write-wins.
                self.arbiter.lock().map(|mut a| a.accept(id)).ok();
                self.handler.on_input(id, input)
            }
            ClientFrame::Powerdown { .. } => self.handler.on_powerdown(id),
            ClientFrame::Reboot { .. } => self.handler.on_reboot(id),
            ClientFrame::Backup { path, .. } => {
                // Progress frames go straight back to this client.
                let sent = Mutex::new(Vec::new());
                let result = self.handler.on_backup(id, path, &|f| {
                    sent.lock().map(|mut s| s.push(f)).ok();
                });
                if let Ok(frames) = sent.into_inner() {
                    for f in frames {
                        session.send(&f)?;
                    }
                }
                result
            }
        };

        match result {
            Ok(()) => {
                // §8.5: input frames are acknowledged only on error, to keep
                // the datapath light. Every other action gets an ack.
                if !matches!(frame, ClientFrame::Input(_)) {
                    session.send(&ServerFrame::ack(seq))?;
                }
                Ok(())
            }
            Err(e) => session.send(&ServerFrame::error(seq, &e)),
        }
    }
}

/// One authenticated control session.
struct Session {
    id: u64,
    stream: ServerStream,
    inbox: Vec<u8>,
}

impl Session {
    fn send(&mut self, frame: &ServerFrame) -> VmmResult<()> {
        self.send_raw(&ws::encode(&Frame::Text(frame.to_json())))
    }

    fn send_raw(&mut self, bytes: &[u8]) -> VmmResult<()> {
        self.stream
            .write_all(bytes)
            .and_then(|_| self.stream.flush())
            .map_err(|e| {
                ControlError::Internal {
                    action: "send".into(),
                    detail: e.to_string(),
                }
                .into()
            })
    }
}

/// The accepted stream. There is only a TLS variant: §8.2 permits no
/// cleartext fallback, so a control listener without TLS cannot exist — which
/// is why this whole module is gated on a provider being compiled in.
enum ServerStream {
    Tls(Box<rustls::StreamOwned<rustls::ServerConnection, TcpStream>>),
}

impl Read for ServerStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            ServerStream::Tls(s) => s.read(buf),
        }
    }
}

impl Write for ServerStream {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        match self {
            ServerStream::Tls(s) => s.write(buf),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            ServerStream::Tls(s) => s.flush(),
        }
    }
}

fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| i + 4)
}
