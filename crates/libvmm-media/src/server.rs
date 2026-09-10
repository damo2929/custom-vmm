//! The RTSPS listener and its `rtsp-session` threads (§7.2, §7.3, §7.4).
//!
//! §1.2 allows `0..n` concurrent sessions and §7.1 fans one encoded stream
//! out to all of them. So this module does two things and no more: it
//! accepts TLS connections, and it runs one session per connection on its
//! own thread. Everything a session used to do besides that — capture,
//! encode, packetise, choose a codec — now belongs to
//! [`MediaPlane`](crate::plane::MediaPlane), and a session only frames what
//! the plane produced and writes it.
//!
//! That division is the whole of the §7 restructure. What was here before
//! served one client at a time, held a full encoder pair per client, and
//! drove capture from the client's own session clock, so with nobody
//! connected the machine captured nothing and every reconnect restarted the
//! GOP.
//!
//! Codec choice is [Amendment B.1]: the first DESCRIBE binds the stream, a
//! later one is answered with the codec already running or refused 5011
//! naming it. A session obtains both by calling `MediaPlane::join`.
//!
//! [Amendment B.1]: ../../../docs/spec-revision-B-media-codecs.md

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use libvmm_core::{MediaError, VmmResult};

use crate::negotiate::Answer;
use crate::packetize::Packet;
use crate::plane::{MediaPlane, SessionStream};
use crate::rtp;
use crate::rtsp::{self, Action, Method, Request, Session};

/// How long a session waits for an encoded unit before checking its socket
/// again. Short enough that a TEARDOWN is never left waiting behind a frame.
const UNIT_WAIT: Duration = Duration::from_millis(5);

/// Units one pass may write before returning to the request pump.
///
/// Two seconds of both streams at §7.1's 30 fps, which is enough to absorb
/// a stall without letting a backlogged session ignore its socket.
const MAX_UNITS_PER_PASS: u64 = 128;

/// A bound RTSPS listener.
pub struct RtspServer {
    listener: TcpListener,
    tls: Arc<rustls::ServerConfig>,
    credentials: Credentials,
    plane: Arc<MediaPlane>,
    stop: Arc<AtomicBool>,
    accepted: AtomicU64,
    /// Session threads currently running, so shutdown can wait for them.
    live: Arc<AtomicU64>,
}

#[derive(Clone)]
pub struct Credentials {
    pub username: String,
    pub password: String,
}

impl Credentials {
    /// The `Authorization: Basic` value this server accepts.
    fn expected_header(&self) -> String {
        use base64::Engine;
        let raw = format!("{}:{}", self.username, self.password);
        format!(
            "Basic {}",
            base64::engine::general_purpose::STANDARD.encode(raw)
        )
    }
}

impl RtspServer {
    pub fn bind(
        addr: &str,
        tls: Arc<rustls::ServerConfig>,
        credentials: Credentials,
        plane: Arc<MediaPlane>,
    ) -> VmmResult<Self> {
        let listener = TcpListener::bind(addr).map_err(|e| {
            MediaError::BadRequest(format!("binding the RTSPS listener to {addr}: {e}"))
        })?;
        Ok(RtspServer {
            listener,
            tls,
            credentials,
            plane,
            stop: Arc::new(AtomicBool::new(false)),
            accepted: AtomicU64::new(0),
            live: Arc::new(AtomicU64::new(0)),
        })
    }

    pub fn local_addr(&self) -> VmmResult<std::net::SocketAddr> {
        self.listener.local_addr().map_err(|e| {
            MediaError::BadRequest(format!("reading the listener address: {e}")).into()
        })
    }

    /// A handle that makes `accept_loop` return and every session stop.
    pub fn stop_handle(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.stop)
    }

    /// Sessions whose threads are still running.
    pub fn live_sessions(&self) -> u64 {
        self.live.load(Ordering::Relaxed)
    }

    /// Accept connections until stopped, spawning one `rtsp-session` thread
    /// each (§1.2).
    ///
    /// A failing session is logged and the listener stays up: one client
    /// disconnecting rudely must not take the console away from the next.
    pub fn accept_loop(self: &Arc<Self>) {
        // A short accept timeout is what lets `stop` be noticed at all.
        if let Err(e) = self.listener.set_nonblocking(true) {
            log::error!("RTSPS listener: {e}");
            return;
        }
        while !self.stop.load(Ordering::Relaxed) {
            match self.listener.accept() {
                Ok((stream, peer)) => {
                    if let Err(e) = stream.set_nonblocking(false) {
                        log::warn!("RTSPS {peer}: {e}");
                        continue;
                    }
                    let id = self.accepted.fetch_add(1, Ordering::Relaxed) + 1;
                    log::info!("RTSPS session {id} from {peer}");
                    let server = Arc::clone(self);
                    let live = Arc::clone(&self.live);
                    live.fetch_add(1, Ordering::Relaxed);
                    let spawned = std::thread::Builder::new()
                        .name(format!("rtsp-session-{id}"))
                        .spawn(move || {
                            match server.serve(stream, id) {
                                Ok(()) => log::info!("RTSPS session {id} closed"),
                                Err(e) => {
                                    log::warn!("RTSPS session {id} ended: {e} (error {})", e.code())
                                }
                            }
                            live.fetch_sub(1, Ordering::Relaxed);
                        });
                    if let Err(e) = spawned {
                        self.live.fetch_sub(1, Ordering::Relaxed);
                        log::error!("RTSPS session {id}: spawning its thread failed: {e}");
                    }
                }
                Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                Err(e) => {
                    log::error!("RTSPS accept: {e}");
                    return;
                }
            }
        }
    }

    /// Run one session to completion on the calling thread.
    pub fn serve(&self, stream: TcpStream, id: u64) -> VmmResult<()> {
        let mut conn = rustls::ServerConnection::new(Arc::clone(&self.tls))
            .map_err(|e| MediaError::Tls(format!("starting the RTSPS session: {e}")))?;
        // A short read timeout is what makes the session loop work at all:
        // once PLAYING, the client sends nothing until TEARDOWN, so a
        // blocking read would sit there forever and no frame would ever be
        // written. rustls keeps its partial record across a timed-out read.
        stream
            .set_read_timeout(Some(Duration::from_millis(2)))
            .map_err(|e| {
                MediaError::BadRequest(format!("setting the session read timeout: {e}"))
            })?;
        let mut socket = stream;
        let mut tls = rustls::Stream::new(&mut conn, &mut socket);
        SessionLoop::new(id, &self.credentials, &self.plane).run(&mut tls, &self.stop)
    }
}

/// One client's session: a socket, a §7.2 state machine, and a subscription
/// to the encoded stream.
///
/// What it deliberately does *not* own is an encoder. That is the entire
/// point of the restructure — see the module header.
struct SessionLoop<'a> {
    id: u64,
    credentials: &'a Credentials,
    plane: &'a Arc<MediaPlane>,
    session: Session,
    /// Set at DESCRIBE, dropped at TEARDOWN or disconnect. Dropping it is
    /// what releases the codec binding when it is the last one.
    stream: Option<SessionStream>,
    /// The SDP answered at DESCRIBE, kept so a repeated DESCRIBE gets the
    /// same answer rather than rejoining the plane.
    sdp: Option<String>,
    inbox: Vec<u8>,
    playing: bool,
    teardown_seen: bool,
    units_written: u64,
}

impl<'a> SessionLoop<'a> {
    fn new(id: u64, credentials: &'a Credentials, plane: &'a Arc<MediaPlane>) -> Self {
        SessionLoop {
            id,
            credentials,
            plane,
            session: Session::new(id),
            stream: None,
            sdp: None,
            inbox: Vec::new(),
            playing: false,
            teardown_seen: false,
            units_written: 0,
        }
    }

    fn run(&mut self, tls: &mut dyn ReadWrite, stop: &AtomicBool) -> VmmResult<()> {
        while !stop.load(Ordering::Relaxed) {
            // Requests first: a TEARDOWN must not wait behind a frame.
            match self.pump_requests(tls)? {
                Flow::Continue => {}
                Flow::Closed => break,
            }

            if self.playing {
                self.pump_media(tls)?;
            } else {
                std::thread::sleep(Duration::from_millis(5));
            }
        }
        log::info!(
            "RTSPS session {}: {} unit(s) written",
            self.id,
            self.units_written
        );
        Ok(())
    }

    /// Read and answer whatever requests have arrived.
    fn pump_requests(&mut self, tls: &mut dyn ReadWrite) -> VmmResult<Flow> {
        let mut chunk = [0u8; 4096];
        match tls.read_timeout(&mut chunk, Duration::from_millis(1)) {
            Ok(0) => return Ok(Flow::Closed),
            Ok(n) => self.inbox.extend_from_slice(&chunk[..n]),
            Err(e) if would_block(&e) => {}
            Err(e) => {
                return Err(
                    MediaError::BadRequest(format!("reading the RTSPS session: {e}")).into(),
                )
            }
        }

        while let Some(end) = find_headers_end(&self.inbox) {
            let text = String::from_utf8_lossy(&self.inbox[..end]).to_string();
            self.inbox.drain(..end);
            let response = self.handle(&text)?;
            tls.write_all(response.as_bytes())
                .map_err(|e| MediaError::BadRequest(format!("writing the response: {e}")))?;
            tls.flush().ok();
            if matches!(self.session.state(), rtsp::SessionState::Init) && self.teardown_seen {
                return Ok(Flow::Closed);
            }
        }
        Ok(Flow::Continue)
    }

    /// Write whatever the plane has produced for this session.
    ///
    /// Everything interesting has already happened elsewhere: the frame was
    /// captured by `media-capture`, encoded and packetised by
    /// `media-encode`, and gated on a keyframe by
    /// [`SessionStream::next_unit`]. This only frames and writes.
    fn pump_media(&mut self, tls: &mut dyn ReadWrite) -> VmmResult<()> {
        let Some(stream) = self.stream.as_mut() else {
            return Ok(());
        };
        // An encoder that died mid-stream must end the session, not leave it
        // holding a socket that will never carry another frame.
        if let Some(reason) = stream.fault() {
            return Err(MediaError::Encode {
                stream: "media plane",
                detail: reason,
            }
            .into());
        }
        // Drain what is queued rather than one unit per pass, so a session
        // that has fallen behind catches up instead of trickling. The batch
        // is bounded so a backlog cannot starve the request pump: a
        // TEARDOWN arriving mid-catch-up must still be answered.
        let mut written = 0u64;
        while written < MAX_UNITS_PER_PASS {
            let Some(unit) = stream.next_unit(UNIT_WAIT) else {
                break;
            };
            write_packets(tls, unit.channel, &unit.packets)?;
            written += 1;
        }
        self.units_written += written;
        if written == 0 {
            // Nothing to send: the encoder is between frames, or this
            // session is still waiting for a keyframe to start on.
            std::thread::sleep(Duration::from_millis(1));
        }
        Ok(())
    }

    fn handle(&mut self, text: &str) -> VmmResult<String> {
        let request = match Request::parse(text) {
            Ok(r) => r,
            // A malformed request is the client's fault, not ours: answer
            // 400 and keep the session rather than dropping the socket.
            Err(e) => {
                log::warn!("RTSPS session {}: {e}", self.id);
                return Ok(rtsp::error(0, 400, "Bad Request"));
            }
        };

        // §7.4: every request carries Basic auth, and 401 comes back before
        // any encoder resource is touched.
        let authenticated =
            request.authorization.as_deref() == Some(self.credentials.expected_header().as_str());
        if !authenticated {
            log::warn!(
                "RTSPS session {}: 401 for {}",
                self.id,
                request.method.as_str()
            );
            return Ok(rtsp::unauthorized(request.cseq));
        }

        let action = match self.session.on(request.method, authenticated) {
            Ok(a) => a,
            Err(e) => {
                log::warn!("RTSPS session {}: {e}", self.id);
                return Ok(rtsp::error(
                    request.cseq,
                    455,
                    "Method Not Valid In This State",
                ));
            }
        };

        match action {
            Action::Respond if request.method == Method::Options => Ok(rtsp::ok(
                request.cseq,
                &[("Public", rtsp::SUPPORTED_METHODS)],
                None,
            )),
            Action::Respond => Ok(rtsp::ok(request.cseq, &[], None)),
            Action::ReturnSdp => self.describe(&request),
            Action::AllocateChannels => {
                // Echo the interleaved range the client proposed. It sends
                // one SETUP per media section — 0-1 for video, 2-3 for audio
                // — so answering with a fixed range would put both streams
                // on the same channels.
                let transport = request
                    .transport
                    .as_deref()
                    .and_then(interleaved_range)
                    .map(|(a, b)| format!("RTP/AVP/TCP;unicast;interleaved={a}-{b}"))
                    .unwrap_or_else(|| "RTP/AVP/TCP;unicast;interleaved=0-1".to_string());
                Ok(rtsp::ok(
                    request.cseq,
                    &[
                        ("Transport", &transport),
                        ("Session", &format!("{}", self.id)),
                    ],
                    None,
                ))
            }
            Action::StartStreaming => {
                // The encoder is already running — it belongs to the machine,
                // not to this session. PLAY only starts *writing*.
                self.playing = true;
                log::info!("RTSPS session {}: PLAYING", self.id);
                Ok(rtsp::ok(
                    request.cseq,
                    &[("Session", &format!("{}", self.id))],
                    None,
                ))
            }
            Action::PauseStreaming => {
                self.playing = false;
                Ok(rtsp::ok(request.cseq, &[], None))
            }
            Action::ReleaseSession => {
                self.playing = false;
                // Dropping the subscription releases the codec binding if
                // this was the last session (Amendment B.1).
                self.stream = None;
                self.sdp = None;
                self.teardown_seen = true;
                Ok(rtsp::ok(request.cseq, &[], None))
            }
        }
    }

    /// Join the plane and answer with the SDP for whatever it is streaming.
    fn describe(&mut self, request: &Request) -> VmmResult<String> {
        // DESCRIBE is idempotent (§7.2). Rejoining would count this client
        // twice against the binding and hand it a second subscription.
        if let (Some(stream), Some(body)) = (self.stream.as_ref(), self.sdp.clone()) {
            let header = stream.selection().to_string();
            return Ok(rtsp::ok(
                request.cseq,
                &[
                    ("Content-Type", "application/sdp"),
                    (rtsp::SELECTED_HEADER, &header),
                ],
                Some(&body),
            ));
        }

        let answer = Answer::from_request_header(request.capabilities.as_deref());
        let stream = self.plane.join(&answer)?;
        let config = self.plane.config();
        let body = stream.sdp(&config.vm.name, &config.display.rtsps.stream_path);

        log::info!(
            "RTSPS session {}: {} {} — client advertised {}",
            self.id,
            if stream.description().inherited {
                "inherited"
            } else {
                "negotiated"
            },
            stream.selection(),
            request
                .capabilities
                .as_deref()
                .unwrap_or("nothing (Revision A)")
        );

        let header = stream.selection().to_string();
        self.stream = Some(stream);
        self.sdp = Some(body.clone());

        Ok(rtsp::ok(
            request.cseq,
            &[
                ("Content-Type", "application/sdp"),
                (rtsp::SELECTED_HEADER, &header),
            ],
            Some(&body),
        ))
    }
}

fn write_packets(tls: &mut dyn ReadWrite, channel: u8, packets: &[Packet]) -> VmmResult<()> {
    for packet in packets {
        let framed = rtp::frame(channel, &packet.data)?;
        tls.write_all(&framed)
            .map_err(|e| MediaError::BadRequest(format!("writing RTP: {e}")))?;
    }
    if !packets.is_empty() {
        // Not `.ok()`: a failed flush means the frames never left, which is
        // indistinguishable from an encoder producing nothing.
        tls.flush()
            .map_err(|e| MediaError::BadRequest(format!("flushing RTP: {e}")))?;
    }
    Ok(())
}

/// Pull `interleaved=A-B` out of a Transport header.
fn interleaved_range(transport: &str) -> Option<(u8, u8)> {
    let tail = transport.split("interleaved=").nth(1)?;
    let value = tail.split(';').next()?;
    let (a, b) = value.split_once('-')?;
    Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
}

enum Flow {
    Continue,
    Closed,
}

/// RTSP headers end at the first blank line; there is no body on any request
/// this server accepts.
fn find_headers_end(buffer: &[u8]) -> Option<usize> {
    buffer
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|p| p + 4)
}

fn would_block(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    )
}

/// The bits of a TLS stream this module needs, so a test can drive a session
/// over a plain socket without standing up a certificate.
pub trait ReadWrite: Write {
    fn read_timeout(&mut self, buf: &mut [u8], timeout: Duration) -> std::io::Result<usize>;
}

impl<T: Read + Write> ReadWrite for T {
    fn read_timeout(&mut self, buf: &mut [u8], _timeout: Duration) -> std::io::Result<usize> {
        self.read(buf)
    }
}
