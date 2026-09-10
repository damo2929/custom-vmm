//! The §7 media plane: capture once, encode once, packetise per session.
//!
//! ```text
//!  virtio-gpu scanout ─┐
//!                      ├─► media-capture ─► handoff ─► media-encode
//!  virtio-snd PCM   ───┘         (1)                        (1)
//!                                                            │
//!                                        StreamUnits, broadcast
//!                                                            │
//!         ┌──────────────────────┬─────────────────────────┬─┘
//!    rtsp-session 1         rtsp-session 2            rtsp-session n
//!    frame + write          frame + write             frame + write
//! ```
//!
//! This is §1.2's thread table drawn as code: **one** `media-capture`,
//! **one** `media-encode`, `0..n` `rtsp-session`. It replaces a listener
//! that captured, encoded, packetised and wrote inline on the accepting
//! thread — which served one client at a time, drove the encoder from that
//! client's session clock, and restarted the GOP whenever somebody
//! reconnected. See [`docs/design-review-media-plane.md`] for how that came
//! about; the short version is that the plane was built by extension rather
//! than from §7.1.
//!
//! # What binds the codec
//!
//! [Amendment B.1] resolves the contradiction between Revision B (negotiate
//! per client) and §1.2 (one encode thread): **negotiation binds the
//! stream, not the session.** The first session to DESCRIBE negotiates by
//! the §7.6 cost model and its choice becomes the machine's stream codec; a
//! later session is answered with the codec already running, or refused
//! 5011 naming it. [`MediaPlane::join`] is where that lives.
//!
//! [Amendment B.1]: ../../../docs/spec-revision-B-media-codecs.md
//!
//! # Why the encoders live on the encode thread and are reached by message
//!
//! [`MediaPlane::join`] does not construct a [`VideoPipeline`]; it asks the
//! `media-encode` thread to, and waits for the answer. Opening an encoder
//! is the one place a codec choice can still fail — a driver that advertises
//! an AV1 entrypoint and then refuses to create the context — and a DESCRIBE
//! must be able to return that failure to the client rather than answer with
//! an SDP for a stream that will never arrive. Doing it by rendezvous keeps
//! the C encode contexts created, used and destroyed on exactly one thread,
//! which is the property that makes them safe to hold at all.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, Weak};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use libvmm_config::MachineConfig;
use libvmm_core::{MediaError, VmmResult};
use vmm_codec_sys::{Capabilities, PackedFrame, Placement};

use crate::encoder::{sdp, AudioEncoderParams, VideoEncoderParams};
use crate::negotiate::{self, Answer, Offer, Selection};
use crate::packetize::Packet;
use crate::pipeline::{AudioPipeline, VideoPipeline};
use crate::rtp;

/// Where the console's pictures and sound come from (§7.1).
///
/// The sources the specification names are the virtio-gpu scanout at
/// `01:00.0` and virtio-snd PCM capture at `01:00.1`. This is the
/// `media-capture` thread's *input*, which is why it is named for the
/// thread's job rather than for the console: it was previously called
/// `ConsoleSource` and owned by an RTSP session, which put guest capture
/// behind a client connection (Finding 8).
///
/// Until a guest executes, a generated pattern stands in. The trait exists
/// so that swap is a one-line change in the VMM rather than surgery here.
pub trait CaptureSource: Send {
    /// The next scanout, or `None` if the guest has not redrawn.
    fn next_frame(&mut self) -> VmmResult<Option<PackedFrame>>;
    /// Interleaved 48 kHz S16 stereo captured since the last call.
    fn next_audio(&mut self) -> VmmResult<Vec<i16>>;
}

/// The SSRCs §7.3 streams carry.
///
/// Fixed per stream rather than per session: every session watches the same
/// encoded stream, so they must all see the same synchronisation source.
pub const SSRC_VIDEO: u32 = 0x5644_0001;
pub const SSRC_AUDIO: u32 = 0x4155_0001;

/// Encoded frames one session may fall behind before it is resynchronised.
///
/// A unit is one coded frame, so at §7.1's 30 fps this is about four
/// seconds of either stream. A session that cannot drain that fast is not
/// going to catch up — the encoder does not slow down for it — so the queue
/// is emptied and the session made to wait for the next keyframe. Blocking
/// the encode thread instead would let one stalled client freeze the console
/// for everybody else, which is precisely what fanning out was meant to
/// prevent.
const MAX_QUEUED_UNITS: usize = 256;

/// How long a blocked thread waits before rechecking `stop`.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// One encoded frame's worth of RTP, ready for any session to frame and
/// write (§7.3).
///
/// Packetisation happens **before** the fan-out, which is what §7.1's
/// diagram shows: sessions frame and write, they do not encode and they do
/// not packetise. It also means the RTP sequence numbers and timestamps are
/// generated once, so two clients watching the same machine see the same
/// stream rather than two divergent ones.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StreamUnit {
    /// The §7.3 interleaved channel: `CHANNEL_VIDEO_RTP` or
    /// `CHANNEL_AUDIO_RTP`.
    pub channel: u8,
    pub packets: Vec<Packet>,
    /// True when a decoder may begin at this unit. Always false for audio,
    /// which needs no such gate.
    pub keyframe: bool,
}

impl StreamUnit {
    pub fn is_video(&self) -> bool {
        self.channel == rtp::CHANNEL_VIDEO_RTP
    }

    fn bytes(&self) -> u64 {
        self.packets.iter().map(|p| p.data.len() as u64).sum()
    }
}

/// What a session needs to answer a DESCRIBE, once it has joined.
#[derive(Debug, Clone)]
pub struct StreamDescription {
    pub selection: Selection,
    pub video: VideoEncoderParams,
    pub audio: AudioEncoderParams,
    /// RFC 5215's base64 codebook configuration, for the SDP `fmtp` line.
    /// `None` for Opus, which needs no out-of-band configuration.
    pub vorbis_configuration: Option<String>,
    /// False when this session negotiated the codec, true when it was
    /// handed the one already running (Amendment B.1). §7.6.4 requires the
    /// difference to be reported: an operator debugging a refused client
    /// needs to know the machine is serving a codec chosen for somebody
    /// else.
    pub inherited: bool,
}

impl StreamDescription {
    /// The SDP body announcing this stream (§7.2).
    pub fn sdp(&self, vm_name: &str, stream_path: &str) -> String {
        sdp(
            vm_name,
            stream_path,
            &self.video,
            &self.audio,
            self.selection.video,
            self.selection.audio,
            self.vorbis_configuration.as_deref(),
        )
    }
}

// ---------------------------------------------------------------------------
// Counters
// ---------------------------------------------------------------------------

/// Cumulative plane counters, across every binding the machine has had.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PlaneStats {
    pub frames_captured: u64,
    /// Scanouts the handoff replaced before the encoder took them.
    pub frames_dropped: u64,
    pub frames_encoded: u64,
    pub video_bytes: u64,
    /// Windows in which output breached the §7.1 ceiling.
    pub ceiling_breaches: u64,
    /// Keyframes the two-second clock forced ahead of the GOP.
    pub forced_keyframes: u64,
    pub pcm_samples: u64,
    pub audio_packets: u64,
    pub audio_bytes: u64,
    /// Sessions admitted over the machine's life.
    pub sessions_total: u64,
    /// Sessions holding the stream right now.
    pub sessions_live: u64,
}

#[derive(Default)]
struct Counters {
    frames_captured: AtomicU64,
    frames_dropped: AtomicU64,
    frames_encoded: AtomicU64,
    video_bytes: AtomicU64,
    ceiling_breaches: AtomicU64,
    forced_keyframes: AtomicU64,
    pcm_samples: AtomicU64,
    audio_packets: AtomicU64,
    audio_bytes: AtomicU64,
    sessions_total: AtomicU64,
}

// ---------------------------------------------------------------------------
// The capture/encode handoff
// ---------------------------------------------------------------------------

/// A command the plane sends to the `media-encode` thread.
///
/// Both carry a reply channel and both are waited on. A bind must report
/// whether the encoder opened, and a release must be known to have happened
/// before the next bind is issued — otherwise a client reconnecting quickly
/// could race a still-open hardware context.
enum Command {
    Bind {
        video: VideoEncoderParams,
        audio: AudioEncoderParams,
        reply: std::sync::mpsc::Sender<VmmResult<BindReport>>,
    },
    Release {
        reply: std::sync::mpsc::Sender<()>,
    },
}

/// What opening the encoders told us, for the SDP and the log.
struct BindReport {
    acceleration: String,
    fallback_reason: Option<String>,
    vorbis_configuration: Option<String>,
}

/// The latest-wins frame slot and append-only PCM queue between the two
/// threads.
///
/// The asymmetry is deliberate and is a property of the media, not of the
/// implementation. A console frame that the encoder did not get to is
/// **stale** — the next one supersedes it entirely, and delivering both
/// would only add latency — so the slot holds one. Audio has no such
/// property: a dropped PCM block is an audible gap, and there is no later
/// block that contains it, so the queue appends.
struct Handoff {
    state: Mutex<HandoffState>,
    signal: Condvar,
}

struct HandoffState {
    frame: Option<PackedFrame>,
    pcm: Vec<i16>,
    command: Option<Command>,
    stop: bool,
    /// Whether a codec is bound, i.e. whether there is an encoder to feed.
    bound: bool,
}

impl Handoff {
    fn new() -> Self {
        Handoff {
            state: Mutex::new(HandoffState {
                frame: None,
                pcm: Vec::new(),
                command: None,
                stop: false,
                bound: false,
            }),
            signal: Condvar::new(),
        }
    }
}

/// Take a lock, tolerating poison.
///
/// A poisoned lock means some other thread panicked while holding it. What
/// these locks protect is plain data — counters, a frame slot, a queue — so
/// carrying on with it is strictly better than turning one thread's panic
/// into a panic on the datapath, which §0.1 forbids outright.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn wait<'a, T>(c: &Condvar, g: MutexGuard<'a, T>, d: Duration) -> MutexGuard<'a, T> {
    c.wait_timeout(g, d)
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .0
}

// ---------------------------------------------------------------------------
// The per-session inbox and the broadcast
// ---------------------------------------------------------------------------

struct Inbox {
    state: Mutex<InboxState>,
    signal: Condvar,
}

struct InboxState {
    units: VecDeque<Arc<StreamUnit>>,
    /// Units discarded because this session fell too far behind.
    dropped: u64,
    /// Set when units were discarded: the session must resynchronise on the
    /// next keyframe before it writes any more video.
    desynchronised: bool,
    /// Why the stream stopped, when it stopped because of a fault.
    ///
    /// §0.1's rule is that a capability which exists and then fails is a
    /// fault and must be raised. An encoder that dies mid-stream would
    /// otherwise show up as a console that quietly stops moving — the
    /// hardest kind of failure to attribute — so it is pushed to every
    /// session, which ends with the reason on the wire and in its log.
    fault: Option<String>,
}

impl Inbox {
    fn new() -> Self {
        Inbox {
            state: Mutex::new(InboxState {
                units: VecDeque::new(),
                dropped: 0,
                desynchronised: false,
                fault: None,
            }),
            signal: Condvar::new(),
        }
    }

    fn deliver(&self, unit: &Arc<StreamUnit>) {
        let mut state = lock(&self.state);
        if state.units.len() >= MAX_QUEUED_UNITS {
            state.dropped += state.units.len() as u64;
            state.units.clear();
            state.desynchronised = true;
        }
        state.units.push_back(Arc::clone(unit));
        drop(state);
        self.signal.notify_all();
    }
}

/// The fan-out. Subscribers are held weakly so a session that goes away
/// unsubscribes by being dropped, with no teardown protocol to get wrong.
#[derive(Default)]
struct Broadcast {
    subscribers: Mutex<Vec<Weak<Inbox>>>,
}

impl Broadcast {
    fn subscribe(&self, inbox: &Arc<Inbox>) {
        lock(&self.subscribers).push(Arc::downgrade(inbox));
    }

    /// Tell every session the stream has failed.
    fn fail(&self, reason: &str) {
        let subscribers = lock(&self.subscribers);
        for weak in subscribers.iter() {
            if let Some(inbox) = weak.upgrade() {
                let mut state = lock(&inbox.state);
                if state.fault.is_none() {
                    state.fault = Some(reason.to_string());
                }
                drop(state);
                inbox.signal.notify_all();
            }
        }
    }

    fn publish(&self, unit: StreamUnit) -> usize {
        let unit = Arc::new(unit);
        let mut subscribers = lock(&self.subscribers);
        subscribers.retain(|weak| match weak.upgrade() {
            Some(inbox) => {
                inbox.deliver(&unit);
                true
            }
            None => false,
        });
        subscribers.len()
    }
}

// ---------------------------------------------------------------------------
// The plane
// ---------------------------------------------------------------------------

/// What the machine is currently streaming, if anything.
struct Binding {
    description: StreamDescription,
    acceleration: String,
    sessions: u64,
}

struct PlaneState {
    binding: Option<Binding>,
    /// Monotonic session identifier, for the log.
    next_session: u64,
}

/// The §7 media plane for one machine.
///
/// Construct at DEVICE_INIT (it probes the host and reports what it can
/// encode), start at LISTENERS_UP, and hand it to the RTSPS listener. It
/// owns both §1.2 media threads for the life of the machine — which is what
/// makes the thread plan `topology.rs` prints true, rather than a plan for
/// threads nothing ever spawned.
pub struct MediaPlane {
    config: MachineConfig,
    render_node: Option<PathBuf>,
    offer: Offer,
    capabilities: Capabilities,
    state: Mutex<PlaneState>,
    handoff: Arc<Handoff>,
    broadcast: Arc<Broadcast>,
    counters: Arc<Counters>,
    threads: Mutex<Vec<JoinHandle<()>>>,
    started: AtomicBool,
}

impl MediaPlane {
    /// Probe the host and build the plane. Nothing runs yet.
    ///
    /// This is DEVICE_INIT's share of §7.1. It cannot prove that the
    /// negotiated encoder opens — under Amendment B.1 no codec is chosen
    /// until the first DESCRIBE — but it can and does prove that the machine
    /// can encode *something*, which is the part that is a device fault
    /// rather than a client mismatch.
    pub fn new(config: MachineConfig, render_node: Option<PathBuf>) -> VmmResult<Arc<Self>> {
        let capabilities = Capabilities::probe(render_node.as_deref());
        if let Some(reason) = &capabilities.vaapi_error {
            log::info!("  no hardware encode available: {reason}");
        }
        let offer = Offer::from_capabilities(&capabilities);

        log::info!(
            "  can encode: video {:?}, audio {:?}",
            offer
                .video
                .iter()
                .map(|c| format!(
                    "{}/{}",
                    c.codec.as_str(),
                    match c.placement {
                        Placement::Hardware => "hw",
                        Placement::Software => "sw",
                    }
                ))
                .collect::<Vec<_>>(),
            offer.audio.iter().map(|c| c.as_str()).collect::<Vec<_>>()
        );

        // A display device that can encode no video at all, or no audio at
        // all, is broken now — not when a client arrives. §7.1 codes that
        // 5001 against the accelerator that was asked for.
        if offer.video.is_empty() || offer.audio.is_empty() {
            return Err(MediaError::EncoderInit {
                accelerator: crate::encoder::accelerator_name(
                    config.display.encoder.hardware_accelerator,
                ),
                detail: format!(
                    "this host can encode no {} at all, so no console session could ever be \
                     formed",
                    if offer.video.is_empty() {
                        "video"
                    } else {
                        "audio"
                    }
                ),
            }
            .into());
        }

        Ok(Arc::new(MediaPlane {
            config,
            render_node,
            offer,
            capabilities,
            state: Mutex::new(PlaneState {
                binding: None,
                next_session: 0,
            }),
            handoff: Arc::new(Handoff::new()),
            broadcast: Arc::new(Broadcast::default()),
            counters: Arc::new(Counters::default()),
            threads: Mutex::new(Vec::new()),
            started: AtomicBool::new(false),
        }))
    }

    /// What this host can encode (§7.6).
    pub fn offer(&self) -> &Offer {
        &self.offer
    }

    /// The probed host capabilities, for the boot report.
    pub fn capabilities(&self) -> &Capabilities {
        &self.capabilities
    }

    pub fn config(&self) -> &MachineConfig {
        &self.config
    }

    /// Spawn `media-capture` and `media-encode` (§1.2).
    ///
    /// Both live for the machine's life, not for a session's: that is the
    /// whole point of the restructure. `media-encode` sits idle until a
    /// codec is bound, because under Amendment B.1 there is nothing it could
    /// encode with before then, but it is the same thread throughout — the
    /// thread table says one, and one is what exists.
    pub fn start(self: &Arc<Self>, source: Box<dyn CaptureSource>) -> VmmResult<()> {
        if self.started.swap(true, Ordering::SeqCst) {
            return Ok(());
        }

        let capture = {
            let handoff = Arc::clone(&self.handoff);
            let counters = Arc::clone(&self.counters);
            let interval = Duration::from_micros(
                1_000_000 / u64::from(self.config.display.framerate_cap.max(1)),
            );
            std::thread::Builder::new()
                .name("media-capture".to_string())
                .spawn(move || capture_loop(source, handoff, counters, interval))
                .map_err(|e| MediaError::Capture {
                    detail: format!("spawning the media-capture thread: {e}"),
                })?
        };

        let encode = {
            let handoff = Arc::clone(&self.handoff);
            let broadcast = Arc::clone(&self.broadcast);
            let counters = Arc::clone(&self.counters);
            let render_node = self.render_node.clone();
            std::thread::Builder::new()
                .name("media-encode".to_string())
                .spawn(move || encode_loop(handoff, broadcast, counters, render_node))
                .map_err(|e| MediaError::Capture {
                    detail: format!("spawning the media-encode thread: {e}"),
                })?
        };

        let mut threads = lock(&self.threads);
        threads.push(capture);
        threads.push(encode);
        Ok(())
    }

    /// Sessions currently holding the stream.
    pub fn sessions(&self) -> u64 {
        lock(&self.state).binding.as_ref().map_or(0, |b| b.sessions)
    }

    /// The codec pair currently bound, if any.
    pub fn bound(&self) -> Option<Selection> {
        lock(&self.state)
            .binding
            .as_ref()
            .map(|b| b.description.selection.clone())
    }

    /// Admit a session (§7.6, Amendment B.1).
    ///
    /// The first caller negotiates against `answer` and its choice becomes
    /// the machine's stream codec. Every later caller is handed that same
    /// choice — or refused 5011 naming it, so the client is told what it
    /// would have to accept rather than merely that it failed.
    pub fn join(self: &Arc<Self>, answer: &Answer) -> VmmResult<SessionStream> {
        let mut state = lock(&self.state);
        state.next_session += 1;
        let id = state.next_session;

        // A binding whose encoder has since failed is not a binding. Without
        // this the plane would hand a new session a codec that nothing is
        // encoding any more, and it would wait on a stream that never comes.
        if state.binding.is_some() && !lock(&self.handoff.state).bound {
            log::warn!("media plane: the bound encoder is gone; session {id} negotiates afresh");
            state.binding = None;
        }

        let description = match &state.binding {
            // Already streaming: the codec is a lookup, not a choice.
            Some(binding) => {
                let running = &binding.description.selection;
                if !answer.video.contains(&running.video) {
                    return Err(bound_codec_refusal(
                        "video",
                        running.video.as_str(),
                        answer.video.iter().map(|c| c.as_str()),
                    ));
                }
                if !answer.audio.contains(&running.audio) {
                    return Err(bound_codec_refusal(
                        "audio",
                        running.audio.as_str(),
                        answer.audio.iter().map(|c| c.as_str()),
                    ));
                }
                let mut description = binding.description.clone();
                description.inherited = true;
                log::info!(
                    "media plane: session {id} inherits {} + {} — bound by an earlier session, \
                     not selected for this one (§7.6.4)",
                    running.video.as_str(),
                    running.audio.as_str()
                );
                description
            }
            // Nothing is streaming: this session's answer decides.
            None => {
                let selection = negotiate::select(&self.offer, answer)
                    .map_err(libvmm_core::error::MediaError::from)?;
                log::info!("media plane: session {id} negotiated {selection}");

                let video = VideoEncoderParams::from_config(&self.config.display)?
                    .for_codec(selection.video);
                let audio = AudioEncoderParams::from_config(&self.config.display)
                    .for_codec(selection.audio);

                // The rendezvous of the module header: the encode thread
                // opens the encoders and tells us whether it could.
                let report = self.bind_encoders(video, audio)?;

                let description = StreamDescription {
                    selection,
                    video,
                    audio,
                    vorbis_configuration: report.vorbis_configuration,
                    inherited: false,
                };
                log::info!(
                    "media plane: {} encode on {}{}",
                    description.selection.video.as_str(),
                    report.acceleration,
                    report
                        .fallback_reason
                        .as_deref()
                        .map(|r| format!(" (hardware unavailable: {r})"))
                        .unwrap_or_default()
                );
                state.binding = Some(Binding {
                    description: description.clone(),
                    acceleration: report.acceleration,
                    sessions: 0,
                });
                description
            }
        };

        if let Some(binding) = state.binding.as_mut() {
            binding.sessions += 1;
        }
        self.counters.sessions_total.fetch_add(1, Ordering::Relaxed);
        drop(state);

        let inbox = Arc::new(Inbox::new());
        self.broadcast.subscribe(&inbox);

        Ok(SessionStream {
            id,
            plane: Arc::clone(self),
            inbox,
            description,
            synchronised: false,
            discarded: 0,
        })
    }

    /// Ask `media-encode` to open the encoders, and wait for the verdict.
    fn bind_encoders(
        &self,
        video: VideoEncoderParams,
        audio: AudioEncoderParams,
    ) -> VmmResult<BindReport> {
        let (reply, answer) = std::sync::mpsc::channel();
        self.send(Command::Bind {
            video,
            audio,
            reply,
        })?;
        answer.recv().map_err(|_| MediaError::EncoderInit {
            accelerator: crate::encoder::accelerator_name(
                self.config.display.encoder.hardware_accelerator,
            ),
            detail: "the media-encode thread stopped before the encoders were opened".to_string(),
        })?
    }

    fn send(&self, command: Command) -> VmmResult<()> {
        let mut state = lock(&self.handoff.state);
        if state.stop {
            return Err(MediaError::Capture {
                detail: "the media plane has shut down".to_string(),
            }
            .into());
        }
        // Commands are issued under the plane's own state lock, so the slot
        // is always free; the assertion is left implicit by overwriting
        // nothing.
        state.command = Some(command);
        drop(state);
        self.handoff.signal.notify_all();
        Ok(())
    }

    /// Called by [`SessionStream`]'s `Drop`. Releases the binding when the
    /// last session goes (Amendment B.1).
    fn leave(&self, id: u64) {
        let mut state = lock(&self.state);
        let Some(binding) = state.binding.as_mut() else {
            return;
        };
        binding.sessions = binding.sessions.saturating_sub(1);
        if binding.sessions > 0 {
            log::info!(
                "media plane: session {id} left, {} still streaming {}",
                binding.sessions,
                binding.description.selection.video.as_str()
            );
            return;
        }

        let selection = binding.description.selection.clone();
        let acceleration = binding.acceleration.clone();
        state.binding = None;

        // The plane lock is deliberately still held. It is what serialises
        // this release against a `join` arriving at the same moment: were it
        // dropped first, that join could put a Bind in the command slot and
        // one of the two would be silently overwritten — leaving either a
        // leaked encode context or a session bound to no encoder. The
        // encode thread never takes this lock, so holding it across the
        // rendezvous cannot deadlock.
        let (reply, done) = std::sync::mpsc::channel();
        if self.send(Command::Release { reply }).is_ok() {
            // A release that never completes would leak a hardware context;
            // waiting is what makes the next bind safe.
            let _ = done.recv();
        }
        drop(state);
        log::info!(
            "media plane: session {id} was the last; released {} ({acceleration}) + {}",
            selection.video.as_str(),
            selection.audio.as_str()
        );
    }

    /// Cumulative counters, for the shutdown report.
    pub fn stats(&self) -> PlaneStats {
        let c = &self.counters;
        PlaneStats {
            frames_captured: c.frames_captured.load(Ordering::Relaxed),
            frames_dropped: c.frames_dropped.load(Ordering::Relaxed),
            frames_encoded: c.frames_encoded.load(Ordering::Relaxed),
            video_bytes: c.video_bytes.load(Ordering::Relaxed),
            ceiling_breaches: c.ceiling_breaches.load(Ordering::Relaxed),
            forced_keyframes: c.forced_keyframes.load(Ordering::Relaxed),
            pcm_samples: c.pcm_samples.load(Ordering::Relaxed),
            audio_packets: c.audio_packets.load(Ordering::Relaxed),
            audio_bytes: c.audio_bytes.load(Ordering::Relaxed),
            sessions_total: c.sessions_total.load(Ordering::Relaxed),
            sessions_live: self.sessions(),
        }
    }

    /// Stop both threads and wait for them.
    pub fn shutdown(&self) {
        {
            let mut state = lock(&self.handoff.state);
            state.stop = true;
        }
        self.handoff.signal.notify_all();
        let handles: Vec<JoinHandle<()>> = lock(&self.threads).drain(..).collect();
        for handle in handles {
            let _ = handle.join();
        }
    }
}

/// The 5011 a client gets when it cannot decode what the machine is already
/// streaming (Amendment B.1).
///
/// `offered` names the bound codec rather than everything this host could
/// encode, because that list would be a lie: while the stream is bound,
/// those other codecs are not on offer to anybody.
fn bound_codec_refusal<'a>(
    stream: &'static str,
    bound: &str,
    wanted: impl Iterator<Item = &'a str>,
) -> libvmm_core::VmmError {
    let wanted: Vec<&str> = wanted.collect();
    log::warn!(
        "media plane: refusing a session that cannot decode {bound}, which is bound to this \
         machine's stream; it offered [{}]",
        wanted.join(", ")
    );
    MediaError::NoCommonCodec {
        stream,
        offered: format!("{bound} (bound to this machine's stream by an earlier session)"),
        wanted: wanted.join(", "),
    }
    .into()
}

// ---------------------------------------------------------------------------
// media-capture
// ---------------------------------------------------------------------------

/// The `media-capture` thread (§1.2).
///
/// It runs for the life of the machine and is paced by its own clock, never
/// by a session's. What it is *not* is unconditional: it captures only while
/// a codec is bound, because a scanout nobody can encode is a 1920x1080
/// BGRA allocation — eight megabytes at §7.1's geometry — made thirty times
/// a second and thrown away. The property Finding 1 was about is that
/// capture is independent of any individual client: with a stream bound,
/// frames are produced at the configured rate no matter how many sessions
/// exist, what state they are in, or when they joined.
fn capture_loop(
    mut source: Box<dyn CaptureSource>,
    handoff: Arc<Handoff>,
    counters: Arc<Counters>,
    interval: Duration,
) {
    log::debug!("media-capture: up, {interval:?} per frame");
    let mut next = Instant::now();
    loop {
        // Wait out the frame interval on the condvar rather than sleeping,
        // so shutdown is noticed immediately instead of a frame later.
        {
            let mut state = lock(&handoff.state);
            loop {
                if state.stop {
                    log::debug!("media-capture: down");
                    return;
                }
                let now = Instant::now();
                if state.bound && now >= next {
                    break;
                }
                let idle = if state.bound {
                    next.saturating_duration_since(now)
                } else {
                    POLL_INTERVAL
                };
                state = wait(&handoff.signal, state, idle.min(POLL_INTERVAL));
            }
        }
        next = Instant::now() + interval;

        match source.next_frame() {
            Ok(Some(frame)) => {
                counters.frames_captured.fetch_add(1, Ordering::Relaxed);
                let mut state = lock(&handoff.state);
                if state.frame.replace(frame).is_some() {
                    // The encoder had not taken the previous one. Dropping
                    // the stale frame is right for a console: the newer one
                    // supersedes it, and queueing both would only add
                    // latency to a picture that is already out of date.
                    counters.frames_dropped.fetch_add(1, Ordering::Relaxed);
                }
            }
            // Nothing redrawn is the normal state of an idle desktop, not an
            // error; the encoder's two-second keyframe clock covers the gap.
            Ok(None) => {}
            Err(e) => {
                log::error!("media-capture: {e} (error {})", e.code());
                return;
            }
        }

        match source.next_audio() {
            Ok(pcm) if pcm.is_empty() => {}
            Ok(pcm) => {
                counters
                    .pcm_samples
                    .fetch_add(pcm.len() as u64, Ordering::Relaxed);
                lock(&handoff.state).pcm.extend_from_slice(&pcm);
            }
            Err(e) => {
                log::error!("media-capture: {e} (error {})", e.code());
                return;
            }
        }

        handoff.signal.notify_all();
    }
}

// ---------------------------------------------------------------------------
// media-encode
// ---------------------------------------------------------------------------

/// The single encoder pair, owned only ever by the `media-encode` thread.
struct Encoders {
    video: VideoPipeline,
    audio: AudioPipeline,
    /// Frames pushed, which is the presentation timestamp the packetisers
    /// convert to the 90 kHz clock.
    pts: i64,
    /// Last values read from the pipeline, so the cumulative counters can be
    /// advanced by the difference rather than overwritten.
    breaches: u64,
    forced: u64,
}

/// The `media-encode` thread (§1.2).
///
/// It owns the encoders, the VBV window and the keyframe clock — all of
/// which used to live inside a session, so that a client connecting reset
/// the GOP for everybody. Here the stream is continuous and a joining client
/// waits for the next keyframe instead of forcing one.
fn encode_loop(
    handoff: Arc<Handoff>,
    broadcast: Arc<Broadcast>,
    counters: Arc<Counters>,
    render_node: Option<PathBuf>,
) {
    log::debug!("media-encode: up");
    let mut encoders: Option<Encoders> = None;

    loop {
        let (command, frame, pcm) = {
            let mut state = lock(&handoff.state);
            loop {
                if state.stop {
                    log::debug!("media-encode: down");
                    return;
                }
                if state.command.is_some() || state.frame.is_some() || !state.pcm.is_empty() {
                    break;
                }
                state = wait(&handoff.signal, state, POLL_INTERVAL);
            }
            (
                state.command.take(),
                state.frame.take(),
                std::mem::take(&mut state.pcm),
            )
        };

        match command {
            Some(Command::Bind {
                video,
                audio,
                reply,
            }) => {
                let opened = open_encoders(video, audio, render_node.as_deref());
                let report = match opened {
                    Ok((pipelines, report)) => {
                        encoders = Some(pipelines);
                        let mut state = lock(&handoff.state);
                        state.bound = true;
                        // Anything captured before the bind belongs to no
                        // stream; starting the GOP on a stale frame would
                        // put a two-second-old picture in the keyframe.
                        state.frame = None;
                        state.pcm.clear();
                        drop(state);
                        handoff.signal.notify_all();
                        Ok(report)
                    }
                    Err(e) => Err(e),
                };
                // A receiver that has gone means the DESCRIBE gave up; the
                // encoders are then released on the next command.
                let _ = reply.send(report);
                continue;
            }
            Some(Command::Release { reply }) => {
                {
                    let mut state = lock(&handoff.state);
                    state.bound = false;
                    state.frame = None;
                    state.pcm.clear();
                }
                // Dropping the pipelines here, on this thread, is what makes
                // the C encode contexts safe to have held at all.
                encoders = None;
                let _ = reply.send(());
                continue;
            }
            None => {}
        }

        let Some(active) = encoders.as_mut() else {
            // Unbound: whatever arrived belongs to no stream. Discarding it
            // is not a loss — capture is paused while unbound, so this is
            // only the tail of the previous binding.
            continue;
        };

        if let Some(frame) = frame {
            active.pts += 1;
            let pts = active.pts;
            match active.video.encode_scanout(&frame, pts, Instant::now()) {
                Ok(output) if output.packets.is_empty() => {}
                Ok(output) => {
                    counters.frames_encoded.fetch_add(1, Ordering::Relaxed);
                    let unit = StreamUnit {
                        channel: rtp::CHANNEL_VIDEO_RTP,
                        packets: output.packets,
                        keyframe: output.keyframe,
                    };
                    counters
                        .video_bytes
                        .fetch_add(unit.bytes(), Ordering::Relaxed);
                    broadcast.publish(unit);
                }
                Err(e) => {
                    // §7.1 encode failure is a fault, not a fallback
                    // condition. Report it, tell every session why, and stop
                    // rather than spinning on a broken encoder.
                    log::error!("media-encode: {e} (error {})", e.code());
                    encoders = None;
                    lock(&handoff.state).bound = false;
                    broadcast.fail(&format!("{e} (error {})", e.code()));
                    continue;
                }
            }

            // The pipeline counts these internally; move the difference into
            // the cumulative totals so they survive a rebind.
            let (_, _, _, breaches) = active.video.stats();
            let forced = active.video.forced_keyframes();
            counters
                .ceiling_breaches
                .fetch_add(breaches.saturating_sub(active.breaches), Ordering::Relaxed);
            counters
                .forced_keyframes
                .fetch_add(forced.saturating_sub(active.forced), Ordering::Relaxed);
            active.breaches = breaches;
            active.forced = forced;
        }

        if !pcm.is_empty() {
            match active.audio.push_pcm(&pcm) {
                Ok(packets) if packets.is_empty() => {}
                Ok(packets) => {
                    counters
                        .audio_packets
                        .fetch_add(packets.len() as u64, Ordering::Relaxed);
                    let unit = StreamUnit {
                        channel: rtp::CHANNEL_AUDIO_RTP,
                        packets,
                        keyframe: false,
                    };
                    counters
                        .audio_bytes
                        .fetch_add(unit.bytes(), Ordering::Relaxed);
                    broadcast.publish(unit);
                }
                Err(e) => {
                    log::error!("media-encode: {e} (error {})", e.code());
                    encoders = None;
                    lock(&handoff.state).bound = false;
                    broadcast.fail(&format!("{e} (error {})", e.code()));
                }
            }
        }
    }
}

fn open_encoders(
    video_params: VideoEncoderParams,
    audio_params: AudioEncoderParams,
    render_node: Option<&std::path::Path>,
) -> VmmResult<(Encoders, BindReport)> {
    let video = VideoPipeline::open(video_params, SSRC_VIDEO, render_node)?;
    let audio = AudioPipeline::open(audio_params, SSRC_AUDIO)?;

    // RFC 5215 carries the packed codebooks base64-encoded in the SDP fmtp
    // line. Only Vorbis has any; Opus needs nothing (§7.1.3).
    let vorbis_configuration = audio.vorbis_headers().map(|h| {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.encode(h.packed_configuration())
    });

    let report = BindReport {
        acceleration: video.acceleration().to_string(),
        fallback_reason: video.fallback_reason().map(str::to_string),
        vorbis_configuration,
    };
    Ok((
        Encoders {
            video,
            audio,
            pts: -1,
            breaches: 0,
            forced: 0,
        },
        report,
    ))
}

// ---------------------------------------------------------------------------
// The session's end of the plane
// ---------------------------------------------------------------------------

/// One `rtsp-session`'s subscription to the encoded stream.
///
/// Dropping it leaves the plane, and when it is the last one the codec
/// binding is released (Amendment B.1). A session therefore has nothing to
/// remember to clean up.
pub struct SessionStream {
    id: u64,
    plane: Arc<MediaPlane>,
    inbox: Arc<Inbox>,
    description: StreamDescription,
    /// False until this session has seen a keyframe it may start on.
    synchronised: bool,
    discarded: u64,
}

impl SessionStream {
    pub fn id(&self) -> u64 {
        self.id
    }

    pub fn description(&self) -> &StreamDescription {
        &self.description
    }

    pub fn selection(&self) -> &Selection {
        &self.description.selection
    }

    /// The SDP body for this session's DESCRIBE.
    pub fn sdp(&self, vm_name: &str, stream_path: &str) -> String {
        self.description.sdp(vm_name, stream_path)
    }

    /// Units this session was never given, because it could not keep up.
    pub fn dropped(&self) -> u64 {
        lock(&self.inbox.state).dropped
    }

    /// Units discarded while waiting for a keyframe to start on.
    pub fn discarded(&self) -> u64 {
        self.discarded
    }

    /// Why the stream stopped, if it stopped because the encoder failed.
    ///
    /// A session that sees this must end rather than sit on a socket that
    /// will never carry another frame.
    pub fn fault(&self) -> Option<String> {
        lock(&self.inbox.state).fault.clone()
    }

    /// The next unit this session may write, waiting up to `timeout`.
    ///
    /// Video before the first keyframe is discarded here rather than in the
    /// caller. A session joining a running stream would otherwise hand its
    /// decoder inter frames referencing pictures it never received, which
    /// shows up as a frozen or smeared console rather than as an error —
    /// exactly the kind of failure that is hard to attribute. §7.1's
    /// two-second keyframe clock bounds the wait.
    pub fn next_unit(&mut self, timeout: Duration) -> Option<Arc<StreamUnit>> {
        let deadline = Instant::now() + timeout;
        loop {
            let inbox = Arc::clone(&self.inbox);
            let (unit, resynchronised) = {
                let mut state = lock(&inbox.state);
                let resynchronised = std::mem::take(&mut state.desynchronised);
                loop {
                    if let Some(unit) = state.units.pop_front() {
                        break (Some(unit), resynchronised);
                    }
                    let now = Instant::now();
                    if now >= deadline {
                        break (None, resynchronised);
                    }
                    state = wait(&inbox.signal, state, deadline - now);
                }
            };

            if resynchronised {
                // Whatever this session had queued is gone, so its decoder's
                // reference pictures are gone with it.
                self.synchronised = false;
                log::warn!(
                    "media plane: session {} fell behind and was resynchronised; waiting for \
                     the next keyframe",
                    self.id
                );
            }

            let unit = unit?;
            if unit.is_video() && !self.synchronised {
                if !unit.keyframe {
                    self.discarded += 1;
                    continue;
                }
                self.synchronised = true;
                log::debug!(
                    "media plane: session {} synchronised on a keyframe after discarding {} \
                     frame(s)",
                    self.id,
                    self.discarded
                );
            }
            return Some(unit);
        }
    }
}

impl Drop for SessionStream {
    fn drop(&mut self) {
        self.plane.leave(self.id);
    }
}

impl std::fmt::Debug for SessionStream {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SessionStream")
            .field("id", &self.id)
            .field("codec", &self.description.selection.video.as_str())
            .field("synchronised", &self.synchronised)
            .finish()
    }
}

/// A [`CaptureSource`] that produces nothing, for tests and for a machine
/// whose display is enabled but has no guest yet.
#[derive(Debug, Default)]
pub struct SilentSource;

impl CaptureSource for SilentSource {
    fn next_frame(&mut self) -> VmmResult<Option<PackedFrame>> {
        Ok(None)
    }
    fn next_audio(&mut self) -> VmmResult<Vec<i16>> {
        Ok(Vec::new())
    }
}
