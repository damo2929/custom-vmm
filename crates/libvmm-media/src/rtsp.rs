//! RTSP method state machine — §7.2, as amended by [Revision C].
//!
//! ```text
//! INIT --DESCRIBE--> INIT (returns SDP; joins the media plane)
//! INIT|READY --SETUP--> READY (allocate RTP interleaved channels)
//! READY --PLAY-----> PLAYING (start writing this session's stream)
//! PLAYING --PAUSE--> READY
//! READY/PLAYING --TEARDOWN--> INIT (release the subscription + channels)
//! ANY --(auth fail)--> respond 401, stay in current state, no media
//! ```
//!
//! Two arms differ from §7.2 as written, and both are recorded in
//! [Revision C] rather than left as undocumented drift:
//!
//! * **SETUP is accepted in READY as well as INIT.** RTSP sets up each
//!   media section separately, so a session with video and audio sends two;
//!   the table's single `INIT --SETUP--> READY` arm refused the second with
//!   455 and made every two-stream session fail.
//! * **PLAY and TEARDOWN do not start or free an encoder.** Under
//!   [Amendment B.1] the encoder belongs to the machine, not the session:
//!   PLAY starts *writing* what is already being encoded, and TEARDOWN
//!   releases this session's subscription — which frees the encoder only
//!   when it was the last one.
//!
//! Basic Auth is required on **every** request (§7.4), and a missing or
//! invalid header yields 401 *before any media resource is allocated*.
//!
//! [Revision C]: ../../../docs/spec-revision-C-rtsp-session-state.md
//! [Amendment B.1]: ../../../docs/spec-revision-B-media-codecs.md

use libvmm_core::{MediaError, VmmResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    Init,
    Ready,
    Playing,
}

impl SessionState {
    pub const fn as_str(self) -> &'static str {
        match self {
            SessionState::Init => "INIT",
            SessionState::Ready => "READY",
            SessionState::Playing => "PLAYING",
        }
    }

    /// Are encoder resources and RTP channels allocated in this state?
    pub const fn has_resources(self) -> bool {
        matches!(self, SessionState::Ready | SessionState::Playing)
    }

    /// Is the encoder producing frames?
    pub const fn is_streaming(self) -> bool {
        matches!(self, SessionState::Playing)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Options,
    Describe,
    Setup,
    Play,
    Pause,
    Teardown,
}

impl Method {
    pub fn parse(s: &str) -> Option<Self> {
        Some(match s.to_ascii_uppercase().as_str() {
            "OPTIONS" => Method::Options,
            "DESCRIBE" => Method::Describe,
            "SETUP" => Method::Setup,
            "PLAY" => Method::Play,
            "PAUSE" => Method::Pause,
            "TEARDOWN" => Method::Teardown,
            _ => return None,
        })
    }

    pub const fn as_str(self) -> &'static str {
        match self {
            Method::Options => "OPTIONS",
            Method::Describe => "DESCRIBE",
            Method::Setup => "SETUP",
            Method::Play => "PLAY",
            Method::Pause => "PAUSE",
            Method::Teardown => "TEARDOWN",
        }
    }
}

/// The methods advertised in an OPTIONS reply.
pub const SUPPORTED_METHODS: &str = "OPTIONS, DESCRIBE, SETUP, PLAY, PAUSE, TEARDOWN";

/// The request header a client uses to advertise what it can decode.
///
/// An extension to §7.2. `X-` rather than a bare name because it is not
/// IANA-registered, and a header rather than an SDP offer because RTSP's
/// DESCRIBE is server-offers-first: the client has no SDP of its own to put
/// the list in.
pub const CAPABILITIES_HEADER: &str = "X-Codec-Capabilities";

/// The response header naming what the server chose, and why.
pub const SELECTED_HEADER: &str = "X-Codec-Selected";

/// What a method transition asks the session to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// Reply only; nothing changes.
    Respond,
    /// Return the SDP body.
    ReturnSdp,
    /// Allocate the RTP interleaved channels.
    AllocateChannels,
    /// Start writing this session's stream. Not "start the encoder": under
    /// Amendment B.1 the encoder is the machine's and is already running.
    StartStreaming,
    /// Stop writing, keep the channels and the subscription.
    PauseStreaming,
    /// Release this session's subscription and channels. The encoder goes
    /// only if this was the last session holding the stream.
    ReleaseSession,
}

/// One RTSP session (§7.2). One `rtsp-session` thread per active session.
#[derive(Debug)]
pub struct Session {
    state: SessionState,
    pub id: u64,
    /// Set once SETUP has run.
    pub channels_allocated: bool,
    /// Whether this session is writing. The encoder's own state is the
    /// plane's business, not the session's.
    pub streaming: bool,
}

impl Session {
    pub fn new(id: u64) -> Self {
        Session {
            state: SessionState::Init,
            id,
            channels_allocated: false,
            streaming: false,
        }
    }

    pub fn state(&self) -> SessionState {
        self.state
    }

    /// Apply a method.
    ///
    /// `authenticated` reflects the §7.4 check, which the caller performs on
    /// **every** request. When it is false the session answers 401 and stays
    /// in its current state with no media and no resource allocation.
    ///
    /// See the module header for the two arms [Revision C] amends.
    pub fn on(&mut self, method: Method, authenticated: bool) -> VmmResult<Action> {
        if !authenticated {
            // ANY --(auth fail)--> 401, stay in the current state, no media.
            return Err(MediaError::RtspAuth(format!(
                "{} requires Basic Auth (§7.4); session stays in {}",
                method.as_str(),
                self.state.as_str()
            ))
            .into());
        }

        let (next, action) = match (self.state, method) {
            (_, Method::Options) => (self.state, Action::Respond),
            (SessionState::Init, Method::Describe) => (SessionState::Init, Action::ReturnSdp),
            // DESCRIBE is idempotent and harmless in any state.
            (s, Method::Describe) => (s, Action::ReturnSdp),
            // Revision C.1: one SETUP per media section. A client with
            // video and audio sends two, so READY must accept it as well as
            // INIT — rejecting the second is what made every two-stream
            // session fail at the audio SETUP with 455.
            (SessionState::Init | SessionState::Ready, Method::Setup) => {
                (SessionState::Ready, Action::AllocateChannels)
            }
            (SessionState::Ready, Method::Play) => (SessionState::Playing, Action::StartStreaming),
            (SessionState::Playing, Method::Pause) => (SessionState::Ready, Action::PauseStreaming),
            (SessionState::Ready | SessionState::Playing, Method::Teardown) => {
                (SessionState::Init, Action::ReleaseSession)
            }
            (state, method) => {
                return Err(MediaError::BadState {
                    method: method.as_str().to_string(),
                    state: state.as_str(),
                }
                .into())
            }
        };

        match action {
            Action::AllocateChannels => self.channels_allocated = true,
            Action::StartStreaming => self.streaming = true,
            Action::PauseStreaming => self.streaming = false,
            Action::ReleaseSession => {
                self.channels_allocated = false;
                self.streaming = false;
            }
            _ => {}
        }
        self.state = next;
        Ok(action)
    }
}

/// A parsed RTSP request line plus the headers we act on.
#[derive(Debug, Clone)]
pub struct Request {
    pub method: Method,
    pub uri: String,
    pub cseq: u64,
    pub authorization: Option<String>,
    pub transport: Option<String>,
    pub session: Option<String>,
    /// The client's `X-Codec-Capabilities` (§7.2.1), if it sent one. `None`
    /// means a Revision A client, which is not the same as an empty list —
    /// see `Answer::from_request_header`.
    pub capabilities: Option<String>,
}

impl Request {
    pub fn parse(text: &str) -> VmmResult<Self> {
        let mut lines = text.split("\r\n");
        let request_line = lines.next().unwrap_or_default();
        let mut parts = request_line.split_whitespace();
        let method_text = parts.next().unwrap_or_default();
        let uri = parts.next().unwrap_or_default().to_string();
        let version = parts.next().unwrap_or_default();

        let method = Method::parse(method_text)
            .ok_or_else(|| MediaError::BadRequest(format!("unknown method {method_text}")))?;
        if !version.eq_ignore_ascii_case("RTSP/1.0") {
            return Err(MediaError::BadRequest(format!("unsupported version {version}")).into());
        }

        let mut cseq = 0u64;
        let mut authorization = None;
        let mut transport = None;
        let mut session = None;
        let mut capabilities = None;
        for line in lines {
            if line.is_empty() {
                break;
            }
            let Some((name, value)) = line.split_once(':') else {
                continue;
            };
            let value = value.trim().to_string();
            match name.trim().to_ascii_lowercase().as_str() {
                "cseq" => cseq = value.parse().unwrap_or(0),
                "authorization" => authorization = Some(value),
                "transport" => transport = Some(value),
                "session" => session = Some(value),
                name if name == crate::rtsp::CAPABILITIES_HEADER.to_ascii_lowercase() => {
                    capabilities = Some(value)
                }
                _ => {}
            }
        }

        Ok(Request {
            method,
            uri,
            cseq,
            authorization,
            transport,
            session,
            capabilities,
        })
    }
}

/// Build the 401 response §7.4 requires, with the KVM-Secure-Console realm.
pub fn unauthorized(cseq: u64) -> String {
    format!(
        "RTSP/1.0 401 Unauthorized\r\n\
         CSeq: {cseq}\r\n\
         WWW-Authenticate: {}\r\n\r\n",
        libvmm_control::auth::challenge(libvmm_control::auth::RTSP_REALM)
    )
}

pub fn ok(cseq: u64, extra_headers: &[(&str, &str)], body: Option<&str>) -> String {
    let mut r = format!("RTSP/1.0 200 OK\r\nCSeq: {cseq}\r\n");
    for (name, value) in extra_headers {
        r.push_str(&format!("{name}: {value}\r\n"));
    }
    match body {
        Some(b) => {
            r.push_str(&format!("Content-Length: {}\r\n\r\n{b}", b.len()));
        }
        None => r.push_str("\r\n"),
    }
    r
}

pub fn error(cseq: u64, status: u16, reason: &str) -> String {
    format!("RTSP/1.0 {status} {reason}\r\nCSeq: {cseq}\r\n\r\n")
}

// ---------------------------------------------------------------------------
// Client side — request building and response parsing (§7.2)
// ---------------------------------------------------------------------------

/// Builds the client half of the §7.2 exchange.
///
/// §7.4 requires Basic Auth on **every** request, so the credential is held
/// here and attached unconditionally rather than only after a 401.
pub struct RequestBuilder {
    base_uri: String,
    authorization: String,
    cseq: u64,
    session: Option<String>,
}

impl RequestBuilder {
    pub fn new(base_uri: impl Into<String>, username: &str, password: &str) -> Self {
        use base64::Engine as _;
        let credential =
            base64::engine::general_purpose::STANDARD.encode(format!("{username}:{password}"));
        RequestBuilder {
            base_uri: base_uri.into(),
            authorization: format!("Basic {credential}"),
            cseq: 0,
            session: None,
        }
    }

    pub fn session(&self) -> Option<&str> {
        self.session.as_deref()
    }

    /// Record the `Session` header the server assigned at SETUP.
    pub fn set_session(&mut self, session: &str) {
        // The header may carry parameters, e.g. `12345678;timeout=60`.
        let id = session.split(';').next().unwrap_or(session).trim();
        self.session = Some(id.to_string());
    }

    pub fn last_cseq(&self) -> u64 {
        self.cseq
    }

    fn build(&mut self, method: Method, uri: &str, extra: &[(&str, &str)]) -> String {
        self.cseq += 1;
        let mut r = format!(
            "{} {uri} RTSP/1.0\r\nCSeq: {}\r\n",
            method.as_str(),
            self.cseq
        );
        // §7.4: every request carries Authorization.
        r.push_str(&format!("Authorization: {}\r\n", self.authorization));
        r.push_str("User-Agent: vmm-console-client/1.0\r\n");
        if let Some(s) = &self.session {
            r.push_str(&format!("Session: {s}\r\n"));
        }
        for (name, value) in extra {
            r.push_str(&format!("{name}: {value}\r\n"));
        }
        r.push_str("\r\n");
        r
    }

    pub fn options(&mut self) -> String {
        let uri = self.base_uri.clone();
        self.build(Method::Options, &uri, &[])
    }

    pub fn describe(&mut self) -> String {
        let uri = self.base_uri.clone();
        self.build(Method::Describe, &uri, &[("Accept", "application/sdp")])
    }

    /// DESCRIBE carrying the client's decode capabilities.
    ///
    /// This is the feedback channel that makes codec choice a negotiation
    /// rather than a configuration item: the server scores what it can
    /// encode against what the client says it can decode, and answers with
    /// an SDP naming the winner. A DESCRIBE without the header is treated as
    /// a client that can only do the specification's original H.264 and
    /// Vorbis, so older clients keep working unchanged.
    pub fn describe_with_capabilities(&mut self, capabilities: &str) -> String {
        let uri = self.base_uri.clone();
        self.build(
            Method::Describe,
            &uri,
            &[
                ("Accept", "application/sdp"),
                (CAPABILITIES_HEADER, capabilities),
            ],
        )
    }

    /// SETUP asking for RTP interleaved on the §7.3 channel pair.
    pub fn setup(&mut self, control: &str, channels: crate::rtp::ChannelPair) -> String {
        let transport = format!(
            "RTP/AVP/TCP;unicast;interleaved={}-{}",
            channels.rtp, channels.rtcp
        );
        let uri = self.absolute(control);
        self.build(Method::Setup, &uri, &[("Transport", &transport)])
    }

    pub fn play(&mut self) -> String {
        let uri = self.base_uri.clone();
        self.build(Method::Play, &uri, &[("Range", "npt=0.000-")])
    }

    pub fn pause(&mut self) -> String {
        let uri = self.base_uri.clone();
        self.build(Method::Pause, &uri, &[])
    }

    pub fn teardown(&mut self) -> String {
        let uri = self.base_uri.clone();
        self.build(Method::Teardown, &uri, &[])
    }

    /// Resolve an SDP `a=control:` value against the base URI.
    fn absolute(&self, control: &str) -> String {
        if control.starts_with("rtsp://") || control.starts_with("rtsps://") {
            control.to_string()
        } else if let Some(rest) = control.strip_prefix('/') {
            // An absolute path replaces the base's path.
            match self
                .base_uri
                .find("://")
                .and_then(|i| self.base_uri[i + 3..].find('/').map(|j| i + 3 + j))
            {
                Some(path_start) => format!("{}/{rest}", &self.base_uri[..path_start]),
                None => format!("{}/{rest}", self.base_uri),
            }
        } else {
            format!("{}/{control}", self.base_uri.trim_end_matches('/'))
        }
    }
}

/// A parsed RTSP response.
#[derive(Debug, Clone)]
pub struct Response {
    pub status: u16,
    pub reason: String,
    pub cseq: u64,
    pub session: Option<String>,
    pub transport: Option<String>,
    pub content_type: Option<String>,
    pub body: String,
    /// Bytes this response occupied, so the caller can advance its buffer.
    pub consumed: usize,
}

impl Response {
    pub const fn is_ok(&self) -> bool {
        self.status == 200
    }

    pub const fn is_unauthorized(&self) -> bool {
        self.status == 401
    }

    /// Parse one response, or `None` if `buffer` does not hold a complete one.
    ///
    /// Returns the byte count consumed so an interleaved stream on the same
    /// connection can be resumed at the right offset (§7.3).
    pub fn parse(buffer: &[u8]) -> VmmResult<Option<Self>> {
        let Some(head_end) = find_header_end(buffer) else {
            return Ok(None);
        };
        let head = std::str::from_utf8(&buffer[..head_end])
            .map_err(|e| MediaError::BadRequest(format!("response headers are not UTF-8: {e}")))?;

        let mut lines = head.split("\r\n");
        let status_line = lines.next().unwrap_or_default();
        let mut parts = status_line.splitn(3, ' ');
        let version = parts.next().unwrap_or_default();
        if !version.eq_ignore_ascii_case("RTSP/1.0") {
            return Err(
                MediaError::BadRequest(format!("unexpected status line {status_line:?}")).into(),
            );
        }
        let status: u16 = parts
            .next()
            .and_then(|s| s.parse().ok())
            .ok_or_else(|| MediaError::BadRequest(format!("no status code in {status_line:?}")))?;
        let reason = parts.next().unwrap_or_default().to_string();

        let mut cseq = 0u64;
        let mut session = None;
        let mut transport = None;
        let mut content_type = None;
        let mut content_length = 0usize;
        for line in lines {
            if line.is_empty() {
                continue;
            }
            let Some((name, value)) = line.split_once(':') else {
                continue;
            };
            let value = value.trim();
            match name.trim().to_ascii_lowercase().as_str() {
                "cseq" => cseq = value.parse().unwrap_or(0),
                "session" => session = Some(value.to_string()),
                "transport" => transport = Some(value.to_string()),
                "content-type" => content_type = Some(value.to_string()),
                "content-length" => content_length = value.parse().unwrap_or(0),
                _ => {}
            }
        }

        if buffer.len() < head_end + content_length {
            return Ok(None);
        }
        let body =
            String::from_utf8_lossy(&buffer[head_end..head_end + content_length]).into_owned();

        Ok(Some(Response {
            status,
            reason,
            cseq,
            session,
            transport,
            content_type,
            body,
            consumed: head_end + content_length,
        }))
    }
}

/// Offset just past the blank line that ends the header block.
fn find_header_end(buffer: &[u8]) -> Option<usize> {
    buffer
        .windows(4)
        .position(|w| w == b"\r\n\r\n")
        .map(|i| i + 4)
}

/// The `a=control:` values and media kinds advertised in an SDP body.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MediaDescription {
    /// "video" or "audio".
    pub kind: String,
    pub payload_type: u8,
    pub control: String,
    pub encoding: String,
}

/// Extract the media sections from an SDP body, so SETUP can address each one.
pub fn parse_sdp(sdp: &str) -> Vec<MediaDescription> {
    let mut out: Vec<MediaDescription> = Vec::new();
    for line in sdp.lines() {
        let line = line.trim_end();
        if let Some(rest) = line.strip_prefix("m=") {
            let mut parts = rest.split_whitespace();
            let kind = parts.next().unwrap_or_default().to_string();
            let _port = parts.next();
            let _proto = parts.next();
            let payload_type = parts.next().and_then(|p| p.parse().ok()).unwrap_or(0);
            out.push(MediaDescription {
                kind,
                payload_type,
                control: String::new(),
                encoding: String::new(),
            });
        } else if let Some(rest) = line.strip_prefix("a=control:") {
            if let Some(last) = out.last_mut() {
                last.control = rest.to_string();
            }
        } else if let Some(rest) = line.strip_prefix("a=rtpmap:") {
            if let Some(last) = out.last_mut() {
                // `96 H264/90000` -> `H264`
                if let Some((_, encoding)) = rest.split_once(' ') {
                    last.encoding = encoding.split('/').next().unwrap_or_default().to_string();
                }
            }
        }
    }
    out
}
