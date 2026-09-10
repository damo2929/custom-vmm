//! RTSPS console client — the client half of §7.
//!
//! Drives the §7.2 method sequence over TLS 1.3, then reads the §7.3
//! interleaved stream and reassembles both elementary streams.
//!
//! Rendering is out: there is no pure-Rust H.264 decoder, and §1.1 forbids
//! linking one. The client therefore reassembles Annex-B video and Vorbis
//! audio and hands them to a sink — a file by default — which an external
//! player can consume. Everything up to the decoder boundary is real.

use crate::transport::{self, Stream};
use libvmm_control::tls::CertPolicy;
use libvmm_core::{MediaError, VmmResult};
use libvmm_media::depacketize::{
    self, AudioDepacketizer, AudioPacket, CodedUnit, RtpPacket, VideoDepacketizer,
};
use libvmm_media::rtp::{self, AUDIO_CHANNELS, VIDEO_CHANNELS};
use libvmm_media::rtsp::{self, MediaDescription, RequestBuilder, Response};
use std::io::{Read, Write};
use std::time::Duration;

/// Where reassembled media goes.
pub trait MediaSink {
    /// One complete coded video unit, in whatever codec was negotiated.
    fn on_video(&mut self, unit: &CodedUnit) -> std::io::Result<()>;
    /// One complete coded audio packet.
    fn on_audio(&mut self, packet: &AudioPacket) -> std::io::Result<()>;
}

/// Writes the two elementary streams to files.
pub struct FileSink {
    pub video: Option<std::fs::File>,
    pub audio: Option<std::fs::File>,
    pub video_units: u64,
    pub audio_packets: u64,
    pub video_bytes: u64,
    pub audio_bytes: u64,
    pub first_keyframe_seen: bool,
}

impl FileSink {
    pub fn new(
        video_path: Option<&std::path::Path>,
        audio_path: Option<&std::path::Path>,
    ) -> std::io::Result<Self> {
        Ok(FileSink {
            video: video_path.map(std::fs::File::create).transpose()?,
            audio: audio_path.map(std::fs::File::create).transpose()?,
            video_units: 0,
            audio_packets: 0,
            video_bytes: 0,
            audio_bytes: 0,
            first_keyframe_seen: false,
        })
    }
}

impl MediaSink for FileSink {
    fn on_video(&mut self, unit: &CodedUnit) -> std::io::Result<()> {
        // Writing before the first keyframe produces a file no decoder can
        // start on, so drop the leading partial GOP.
        if !self.first_keyframe_seen {
            if !unit.keyframe {
                return Ok(());
            }
            self.first_keyframe_seen = true;
        }
        self.video_units += 1;
        self.video_bytes += unit.data.len() as u64;
        match &mut self.video {
            Some(f) => f.write_all(&unit.data),
            None => Ok(()),
        }
    }

    fn on_audio(&mut self, packet: &AudioPacket) -> std::io::Result<()> {
        self.audio_packets += 1;
        self.audio_bytes += packet.data.len() as u64;
        match &mut self.audio {
            // Length-prefixed so packet boundaries survive, which raw
            // concatenation would destroy.
            Some(f) => {
                f.write_all(&(packet.data.len() as u32).to_be_bytes())?;
                f.write_all(&packet.data)
            }
            None => Ok(()),
        }
    }
}

/// A connected RTSPS session.
pub struct RtspClient {
    /// What this build can decode, sent on DESCRIBE so the server can
    /// negotiate rather than assume.
    capabilities: libvmm_media::negotiate::Answer,
    stream: Stream,
    builder: RequestBuilder,
    inbox: Vec<u8>,
    pub media: Vec<MediaDescription>,
    pub sdp: String,
    /// Chosen from the SDP once DESCRIBE says what was negotiated (§7.6).
    /// A session that agreed AV1 and then ran RTP through the H.264
    /// depacketiser produces malformed-packet warnings and no picture, which
    /// looks exactly like a broken encoder.
    video: Box<dyn VideoDepacketizer>,
    audio: Box<dyn AudioDepacketizer>,
    pub video_channels: Option<rtp::ChannelPair>,
    pub audio_channels: Option<rtp::ChannelPair>,
    /// How long `pump` waits on a silent stream before returning.
    ///
    /// The default suits a session that only stores or decodes. An
    /// interactive console lowers it, because this is also the worst-case
    /// delay before the caller can drain the window's keyboard and pointer
    /// events and send them to the guest.
    pub read_timeout: Duration,
}

impl RtspClient {
    /// The decode capabilities this client advertised.
    pub fn capabilities(&self) -> &libvmm_media::negotiate::Answer {
        &self.capabilities
    }

    /// Connect and run OPTIONS + DESCRIBE (§7.2).
    pub fn connect(
        addr: &str,
        stream_path: &str,
        username: &str,
        password: &str,
        policy: CertPolicy,
        timeout: Duration,
    ) -> VmmResult<Self> {
        let host = transport::host_of(addr);
        let stream = Stream::connect(addr, &host, Some(policy), timeout)?;
        log::debug!("media transport to {addr}: {}", stream.describe());
        let base_uri = format!("rtsps://{addr}{stream_path}");
        let builder = RequestBuilder::new(base_uri, username, password);

        let mut client = RtspClient {
            read_timeout: Duration::from_millis(500),
            capabilities: libvmm_media::negotiate::Answer::from_capabilities(
                &vmm_codec_sys::Capabilities::probe(None),
            ),
            stream,
            builder,
            inbox: Vec::new(),
            media: Vec::new(),
            sdp: String::new(),
            // Replaced from the SDP below. The Revision A codecs are the
            // right starting point: a server that ignores the capability
            // header serves exactly those.
            video: depacketize::video_for(vmm_codec_sys::VideoCodec::H264),
            audio: depacketize::audio_for(vmm_codec_sys::AudioCodec::Vorbis),
            video_channels: None,
            audio_channels: None,
        };

        let options = client.builder.options();
        client.exchange(&options)?;

        // Advertise what this build can decode, so the server can pick the
        // cheapest codec the pair share rather than assuming H.264.
        let capabilities = client.capabilities.to_header();
        log::debug!("advertising decode capabilities: {capabilities}");
        let describe = client.builder.describe_with_capabilities(&capabilities);
        let response = client.exchange(&describe)?;
        client.sdp = response.body.clone();
        client.media = rtsp::parse_sdp(&client.sdp);
        if client.media.is_empty() {
            return Err(MediaError::BadRequest(
                "DESCRIBE returned an SDP with no media sections".into(),
            )
            .into());
        }

        // The SDP is the answer to the capabilities advertised above, so it
        // decides the demux. Leaving the Revision A defaults in place here is
        // what made an AV1 session reassemble as H.264 and show nothing.
        client.select_depacketizers()?;
        Ok(client)
    }

    /// Point the depacketisers at the codecs the SDP announced.
    ///
    /// An unrecognised encoding name is fatal: the server picked from what
    /// this client advertised, so anything else means the two ends disagree
    /// about the negotiation, and guessing would produce a silent black
    /// stream rather than an error anyone could act on.
    fn select_depacketizers(&mut self) -> VmmResult<()> {
        for description in &self.media {
            match description.kind.as_str() {
                "video" => {
                    let codec = libvmm_media::negotiate::video_from_str(&description.encoding)
                        .ok_or_else(|| {
                            MediaError::BadRequest(format!(
                                "the SDP announced video codec {:?}, which this client cannot \
                                 depacketise",
                                description.encoding
                            ))
                        })?;
                    log::debug!("video demux: {}", codec.as_str());
                    self.video = depacketize::video_for(codec);
                }
                "audio" => {
                    let codec = libvmm_media::negotiate::audio_from_str(&description.encoding)
                        .ok_or_else(|| {
                            MediaError::BadRequest(format!(
                                "the SDP announced audio codec {:?}, which this client cannot \
                                 depacketise",
                                description.encoding
                            ))
                        })?;
                    log::debug!("audio demux: {}", codec.as_str());
                    self.audio = depacketize::audio_for(codec);
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// SETUP each advertised stream on its §7.3 channel pair, then PLAY.
    pub fn setup_and_play(&mut self) -> VmmResult<()> {
        let media = self.media.clone();
        for description in &media {
            let channels = match description.kind.as_str() {
                "video" => VIDEO_CHANNELS,
                "audio" => AUDIO_CHANNELS,
                other => {
                    log::warn!("ignoring unknown media kind {other:?} in the SDP");
                    continue;
                }
            };
            let request = self.builder.setup(&description.control, channels);
            let response = self.exchange(&request)?;
            if let Some(session) = response.session.as_deref() {
                self.builder.set_session(session);
            }
            match description.kind.as_str() {
                "video" => self.video_channels = Some(channels),
                "audio" => self.audio_channels = Some(channels),
                _ => {}
            }
            log::info!(
                "SETUP {} ({}) on interleaved channels {}-{}",
                description.kind,
                description.encoding,
                channels.rtp,
                channels.rtcp
            );
        }

        let play = self.builder.play();
        self.exchange(&play)?;
        Ok(())
    }

    /// Read interleaved media until `deadline`, feeding `sink`.
    pub fn pump(
        &mut self,
        sink: &mut dyn MediaSink,
        deadline: std::time::Instant,
    ) -> VmmResult<()> {
        self.stream.set_read_timeout(Some(self.read_timeout));
        while std::time::Instant::now() < deadline {
            match rtp::parse(&self.inbox)? {
                Some(frame) => {
                    self.inbox.drain(..frame.consumed);
                    self.dispatch(frame.channel, &frame.payload, sink)?;
                }
                None => {
                    if !self.fill()? {
                        return Ok(());
                    }
                }
            }
        }
        Ok(())
    }

    fn dispatch(&mut self, channel: u8, payload: &[u8], sink: &mut dyn MediaSink) -> VmmResult<()> {
        // RTCP channels carry sender reports we do not act on yet.
        if channel == rtp::CHANNEL_VIDEO_RTCP || channel == rtp::CHANNEL_AUDIO_RTCP {
            return Ok(());
        }
        let packet = match RtpPacket::parse(payload) {
            Ok(p) => p,
            Err(e) => {
                // A malformed packet must not kill the session; the next one
                // may be fine.
                log::debug!("dropping a malformed RTP packet on channel {channel}: {e}");
                return Ok(());
            }
        };

        if channel == rtp::CHANNEL_VIDEO_RTP {
            match self.video.push(&packet) {
                Ok(Some(unit)) => sink
                    .on_video(&unit)
                    .map_err(|e| MediaError::BadRequest(e.to_string()))?,
                Ok(None) => {}
                Err(e) => log::debug!("{} depacketiser: {e}", self.video.codec().as_str()),
            }
        } else if channel == rtp::CHANNEL_AUDIO_RTP {
            match self.audio.push(&packet) {
                Ok(packets) => {
                    for p in &packets {
                        sink.on_audio(p)
                            .map_err(|e| MediaError::BadRequest(e.to_string()))?;
                    }
                }
                Err(e) => log::debug!("{} depacketiser: {e}", self.audio.codec().as_str()),
            }
        }
        Ok(())
    }

    pub fn teardown(&mut self) -> VmmResult<()> {
        let request = self.builder.teardown();
        self.exchange(&request).map(|_| ())
    }

    pub fn pause(&mut self) -> VmmResult<()> {
        let request = self.builder.pause();
        self.exchange(&request).map(|_| ())
    }

    /// Loss counters, so a session can report what it dropped.
    pub fn stats(&self) -> (u64, u64, u64) {
        let v = self.video.stats();
        let a = self.audio.stats();
        (
            v.received + a.received,
            v.lost + a.lost,
            self.video.dropped() + self.audio.dropped(),
        )
    }

    /// Send a request and read its response, skipping any interleaved media
    /// that arrives in between (§7.3 allows both on one connection).
    fn exchange(&mut self, request: &str) -> VmmResult<Response> {
        let method = request.split_whitespace().next().unwrap_or("?").to_string();
        self.stream
            .write_all(request.as_bytes())
            .and_then(|_| self.stream.flush())
            .map_err(|e| MediaError::BadRequest(format!("sending {method}: {e}")))?;

        loop {
            // Interleaved data is framed with '$'; a response starts "RTSP/".
            if self.inbox.first() == Some(&rtp::MAGIC) {
                match rtp::parse(&self.inbox)? {
                    Some(frame) => {
                        self.inbox.drain(..frame.consumed);
                        continue;
                    }
                    None => {
                        self.fill()?;
                        continue;
                    }
                }
            }

            if let Some(response) = Response::parse(&self.inbox)? {
                self.inbox.drain(..response.consumed);
                if response.is_unauthorized() {
                    // §7.4: Basic Auth is on every request, so a 401 here
                    // means the credentials are wrong, not that we need to
                    // retry with them.
                    return Err(MediaError::RtspAuth(format!(
                        "{method} rejected: {} {}",
                        response.status, response.reason
                    ))
                    .into());
                }
                if !response.is_ok() {
                    return Err(MediaError::BadRequest(format!(
                        "{method} failed: {} {}",
                        response.status, response.reason
                    ))
                    .into());
                }
                return Ok(response);
            }

            if !self.fill()? {
                return Err(MediaError::BadRequest(format!(
                    "connection closed waiting for the {method} response"
                ))
                .into());
            }
        }
    }

    /// Read more bytes. Returns false at end of stream.
    fn fill(&mut self) -> VmmResult<bool> {
        let mut chunk = [0u8; 8192];
        match self.stream.read(&mut chunk) {
            Ok(0) => Ok(false),
            Ok(n) => {
                self.inbox.extend_from_slice(&chunk[..n]);
                Ok(true)
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                Ok(false)
            }
            Err(e) => Err(MediaError::BadRequest(format!("reading the RTSP stream: {e}")).into()),
        }
    }
}
