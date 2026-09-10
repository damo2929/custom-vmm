# Design review — the §7 media plane

**Date:** 2026-09-09 · **Status:** **all eight findings closed, 2026-09-10.**
The restructure is implemented and has been run: two clients streamed AV1 +
Opus concurrently from one hardware encode context. What each finding needed
is recorded under "Disposition" at the foot of this document.

Written after building an RTSPS listener by extending what was already there
rather than by working from the specification. The result works — a client
negotiates AV1 + Opus over TLS 1.3 and the server streams to it — but it
diverges from §1.2 and §7.1 in ways that are structural, not cosmetic, and it
left Revision B holding a contradiction nobody had noticed.

This records what is wrong, why, and what the design should be. Sources of
truth: the *Detailed Design Specification* (2026-09-09) and
[Revision B](spec-revision-B-media-codecs.md).

---

## What the specification actually says

**§1.2** fixes the thread taxonomy. Three rows govern the media plane:

| Thread | Count | Role |
|---|---|---|
| `media-capture` | **1** | virtio-gpu/snd frame + PCM capture |
| `media-encode` | **1** | video + audio encode |
| `rtsp-session` | **0..n** | one per active RTSPS media session |

**§7.1** draws the pipeline as a single chain that fans out at the end:

```
virtio-gpu scanout --+
                     +--> encode --> RTP packetise --> RTSPS/TLS 1.3
virtio-snd PCM ------+
```

Read together these are unambiguous: **capture once, encode once, packetise
per session.** One encoder feeds many clients.

---

## Finding 1 — the thread model is not implemented, and is now misreported

`custom-vmm/src/topology.rs` declares `media-capture` and `media-encode`
when the display is enabled, and the boot output prints that plan. **Neither
thread exists.** What was built instead is a single `rtsp-listener` thread —
a name absent from §1.2 — which accepts a connection and then, inline and
serially, captures, encodes, packetises and writes.

Consequences, in order of severity:

1. **One console client at a time.** §1.2 allows `0..n` sessions; the accept
   loop serves one to completion before accepting the next. A second client
   does not get 503, it gets silence until the first disconnects.
2. **Capture and encode are driven by a client's session clock.** With no
   client connected nothing captures at all, and each new session restarts
   the GOP. §7.1's pipeline is continuous by construction.
3. **The printed §1.2 plan is a lie**, which is worse than not printing it.

## Finding 2 — Revision B contradicts §1.2, and the code resolved it silently

§1.2 says **one** `media-encode` thread. Revision B §7.6 says the codec is
negotiated **per client**. Two clients that negotiate different codecs
therefore need two encoders, and the two documents cannot both be satisfied.

I did not notice this when writing Revision B. The implementation resolved it
by putting a full `VideoPipeline` + `AudioPipeline` inside every session,
which means **one hardware encode context per connected client** — on a
machine whose whole reason for negotiating was to use the fixed-function
encoder well.

This is a specification question, not an implementation detail. It needs an
amendment before the restructure, because the answer changes the design.

## Finding 3 — two encoder instances now exist, one of them dead

`boot.rs::open_media()` opens a `VideoPipeline` and `AudioPipeline` at
`DEVICE_INIT`, negotiating against an *assumed* client. The RTSP session
opens a second pair against the *real* client. The first is never used to
stream anything: it exists only so the boot log can report a codec choice.

So the machine currently opens a hardware AV1 encode context it never
encodes with. That is a direct consequence of adding the listener beside the
existing code rather than reconciling with it.

## Finding 4 — codec-agnostic data is carried in codec-named types

To get AV1 and VP9 through the client demux, their output is converted into
`h264::AccessUnit`, and Opus into `vorbis::VorbisPacket`. The fields happen
to line up — bytes, timestamp, and whether a decoder may start here — but the
type names are now false, and `MediaSink::on_video` claims to take "one
complete H.264 access unit" while carrying AV1.

The right shape is a codec-agnostic `CodedUnit { data, timestamp, keyframe }`
and `AudioPacket`, produced by every depacketiser and consumed by the sink.
Vorbis' `ident`/`configuration` fields belong to Vorbis and should not be in
the shared type.

## Finding 5 — the depacketisers have no common contract

Five depacketisers, three different shapes:

| | sequence tracking | dropped counter |
|---|---|---|
| H.264, Vorbis | `SequenceTracker` | `dropped_fragments` |
| VP9, AV1 | *none* (added in this session) | `dropped` |
| Opus | *none* (added in this session) | *none* |

Loss reporting was therefore dead on exactly the codecs negotiation
actually selects. A `Depacketizer` trait with one contract — push a packet,
get zero or more units, report loss — would have made that impossible.

## Finding 6 — the client demux is half-wired

`VideoDemux::for_codec` and `AudioDemux::for_codec` were written and are
never called; clippy reports both as dead code. The client still constructs
H.264 and Vorbis depacketisers regardless of what the SDP announced. This is
the incomplete half of a reactive fix and is the clearest evidence for the
concern that prompted this review.

## Finding 7 — §7.2's state table was amended in code, not in writing

The spec's table has one SETUP arm: `INIT --SETUP--> READY`. A session with
both a video and an audio media section sends **two** SETUPs, so the second
was rejected with 455 and every two-stream session failed. The transition
table was widened to `INIT|READY --SETUP--> READY`.

That change is correct — RTSP sets up each media section separately — but it
amends a numbered requirement, and amendments belong in a revision document
where they can be reviewed, not in a commit that makes a symptom go away.

## Finding 8 — `ConsoleSource` was invented without reference to §7.1

The trait the listener pulls frames from is a reasonable seam, but it was
designed around what the listener needed rather than what §7.1 describes. In
the specification the sources are the virtio-gpu scanout and virtio-snd PCM
capture, feeding a capture *thread*. The trait should be that thread's input,
named accordingly, and not owned by a session.

---

## The design this should be

```
 virtio-gpu scanout ─┐
                     ├─► media-capture ─► frame/PCM handoff ─► media-encode
 virtio-snd PCM   ───┘         (1)                                  (1)
                                                                     │
                                              encoded units, broadcast
                                                                     │
                        ┌────────────────────┬───────────────────────┤
                   rtsp-session 1       rtsp-session 2          rtsp-session n
                   packetise + send     packetise + send        packetise + send
```

* **`media-capture` (1 thread)** owns the scanout and PCM sources and
  produces frames at the configured rate whether or not anyone is watching.
  Its input is the trait now called `ConsoleSource`, renamed and moved.
* **`media-encode` (1 thread)** owns the encoders, the VBV window and the
  keyframe clock, and publishes encoded units. Keyframe timing belongs here
  and not in a session, so a client joining does not perturb the stream for
  everyone else.
* **`rtsp-session` (0..n threads)** each own a socket, a session state
  machine and a packetiser. They subscribe to encoded units; they do not
  encode. A joining client waits for the next keyframe rather than forcing
  one — or requests one, if the amendment says it may.
* **The boot-time pipeline in `open_media()` goes away**, replaced by the
  encode thread it was standing in for.

This is what §1.2 and §7.1 describe. It also fixes Findings 1 and 3 outright
and makes Finding 2 the only open question.

## The open question — settled

**Can two clients watch the same machine with different codecs?** No.

Resolved as [Amendment B.1](spec-revision-B-media-codecs.md#amendment-b1--one-encoder-bound-by-the-first-session):
**negotiation binds the stream, not the session.** One video and one audio
encoder per machine, owned by the single `media-encode` thread. The first
session to DESCRIBE negotiates; later sessions are answered with the codec
already running, or refused with 5011 naming it. The binding releases when
the last session tears down.

This keeps §1.2's thread table unmodified, which is what makes the design
below implementable as specified.

## Sequencing

1. ~~Settle the question above; write it up as an amendment to Revision B.~~
   **Done — Amendment B.1.**
2. Restructure §7 to the thread model — capture, encode, sessions.
3. Introduce `CodedUnit`/`AudioPacket` and a `Depacketizer` trait
   (Findings 4, 5), and finish the client demux (Finding 6).
4. Record the §7.2 SETUP amendment (Finding 7).
5. Only then build the vCPU run loop and OVMF serial capture, feeding
   `media-capture` — so UEFI output streams through a pipeline that matches
   the specification instead of one built around it.

---

## Disposition

| # | What it was | Where it was fixed |
|---|---|---|
| 1 | The thread model was not implemented and was misreported | `libvmm-media/src/plane.rs` — `media-capture` and `media-encode` are real threads for the machine's life; `server.rs` spawns one `rtsp-session` per connection. The plan `topology.rs` prints is now true |
| 2 | Revision B contradicted §1.2 | [Amendment B.1](spec-revision-B-media-codecs.md#amendment-b1--one-encoder-bound-by-the-first-session), implemented as `MediaPlane::join` |
| 3 | Two encoder instances, one dead | `boot.rs::open_media()` is gone. `open_media_plane()` probes and reports but opens nothing; the encoders open once, on the encode thread, at the first DESCRIBE |
| 4 | Codec-agnostic data in codec-named types | `depacketize::{CodedUnit, AudioPacket}` |
| 5 | No common depacketiser contract | `VideoDepacketizer` / `AudioDepacketizer`, with sequence tracking on all five |
| 6 | The client demux was half-wired | `RtspClient::select_depacketizers`, pinned by `demux_follows_negotiation.rs` |
| 7 | §7.2's state table was amended in code, not in writing | [Revision C](spec-revision-C-rtsp-session-state.md), covering both the SETUP arm and PLAY/TEARDOWN under a shared encoder |
| 8 | `ConsoleSource` was invented without reference to §7.1 | Renamed `CaptureSource` and moved to `plane`, where it is the capture thread's input and no session can reach it |

### What the restructure actually changed, measured

Before, on this host: one client at a time; a `VideoPipeline` +
`AudioPipeline` per client; no capture at all with nobody connected; a GOP
restart on every connect.

After: `custom-vmm` served two concurrent `vmm-console-client` sessions from
a single `av1_vaapi` context. The first negotiated, the second was told
`session 2 inherits av1 + opus — bound by an earlier session, not selected
for this one (§7.6.4)`, and the binding released on the last teardown. The
late-joining client discarded frames until a keyframe rather than being
handed inter frames it could not decode.

### What this review did not resolve

The keyframe wait. A session joining mid-GOP waits up to §7.1's two seconds
before its first picture, because Revision C.2 forbids it from forcing a
keyframe on everybody else's behalf. That is the right default for a console
with a steady watcher and an occasional joiner; if joins turn out to be
frequent it is a specification question — an on-demand IDR request — and not
something to add quietly.
