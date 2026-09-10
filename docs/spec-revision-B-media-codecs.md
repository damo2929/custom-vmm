# Detailed Design Specification — Revision B

**Legacy-Free Native Rust KVM Hypervisor & Client Suite**

| | |
|---|---|
| Supersedes | implementation-ready draft, 2026-09-09 (Revision A) |
| Revision | **B**, 2026-09-09 |
| Scope | §7 media pipeline: video codecs, audio codecs, and codec negotiation |
| Status | implemented and tested; see [IMPLEMENTATION-STATUS.md](../IMPLEMENTATION-STATUS.md) |

Revision A fixed the media codecs: H.264 for video, Vorbis for audio, chosen
in configuration. Revision B replaces both fixed choices with a negotiated
one, adds VP9 and AV1 alongside H.264, adds Opus alongside Vorbis, and
defines the feedback channel the two ends use to agree.

Sections not listed here are unchanged from Revision A. Requirement
keywords are RFC 2119.

---

## Why this revision exists

Revision A's §7.1 names one video codec and one audio codec. Two problems
emerged in implementation, both of which make a fixed choice actively worse
than a negotiated one.

**The right video codec is a property of the hardware, not the
specification.** On the reference development host — an AMD Radeon 860M
under Fedora 44 — the stock Mesa build exposes hardware encode for **AV1 and
nothing else**. H.264 and HEVC are absent because Fedora compiles Mesa
without patent-encumbered codecs, not because the silicon lacks them.
Following Revision A literally on that machine means encoding H.264 on the
CPU, at roughly a core of load and a worse picture, while a perfectly good
fixed-function AV1 encoder sits idle. A different host inverts the situation.
No fixed choice is right for both.

**Vorbis costs latency and robustness for nothing.** Vorbis carries about
46 ms of algorithmic delay against Opus' 20 ms, costs more CPU at both ends,
and — the operational difference — cannot decode a single packet until its
three header packets have arrived out of band. A client joining a session
mid-stream is deaf until it re-fetches the configuration. Opus has none of
these properties and is otherwise a straight substitute.

Revision B therefore makes codec choice a **negotiation**, and defines the
channel over which it happens.

---

## §7.1 Capture and encode *(revised)*

### §7.1.1 Video codecs

The implementation **MUST** support all three of:

| codec | hardware encode | software encode | decode |
|---|---|---|---|
| H.264 | VA-API `h264_vaapi` | libx264 | libavcodec |
| VP9 | VA-API `vp9_vaapi` | libvpx (`realtime`) | libavcodec |
| AV1 | VA-API `av1_vaapi` | SVT-AV1 (`preset 12`) | dav1d |

Each codec **MUST** have a working software encoder, so that any negotiated
codec can be honoured on a host with no fixed-function engine.

The encoder **MUST** prefer a fixed-function engine over a software encoder
for the same codec. It **MUST NOT** fall back to software because a hardware
encoder that exists failed to open: only *absence* of hardware support is a
fallback condition. A host that has the encoder but cannot start it has a
fault, and degrading silently would hide it behind a performance regression.

Hardware support **MUST** be determined by querying the driver for a profile
carrying an encode entrypoint, not inferred from the device, the driver name
or configuration. Rationale: the reference host's driver loads, reports a
vendor string, and offers no H.264 entrypoint whatever; nothing short of the
query distinguishes that from a driver that offers one.

The rate control of Revision A is unchanged: constrained VBR, target
`bitrate_kbps`, hard ceiling `max_bitrate_kbps` of 2000 kbps, HRD buffer
sized to the ceiling.

### §7.1.2 Keyframe interval *(clarified)*

Revision A requires a keyframe at least every two seconds and sizes the GOP
at `framerate × 2` frames. That satisfies the requirement **only while the
guest renders at the configured rate.** An idle desktop producing 5 fps on a
30 fps machine would go twelve seconds between keyframes under a 60-frame
GOP, and a client joining in that window sees nothing.

The implementation **MUST** hold the keyframe interval against the wall
clock, not only the frame counter: if two seconds have elapsed since the last
keyframe, the next frame **MUST** be coded as a keyframe regardless of GOP
position. A configured GOP longer than `framerate × 2` frames **MUST** be
rejected at encoder init.

To avoid coding a redundant keyframe every GOP, the clock **SHOULD NOT**
force one when the GOP is due to deliver on the next frame anyway.

### §7.1.3 Audio codecs

The implementation **MUST** support:

| codec | RTP payload format | delay at 48 kHz | out-of-band configuration |
|---|---|---|---|
| **Opus** | RFC 7587 | ~20 ms | none |
| Vorbis | RFC 5215 | ~46 ms | three header packets, required |

Opus **MUST** be preferred wherever the client supports it. Vorbis **MUST**
remain fully implemented — not stubbed — as the fallback for clients that
cannot decode Opus.

Opus **MUST** be configured for interactive use:

* **`RESTRICTED_LOWDELAY` application.** Disables the SILK layer and the
  prediction that costs a frame of lookahead, leaving only CELT — a little
  coding efficiency traded for the lowest algorithmic delay the codec offers.
* **20 ms frames.**
* **Discontinuous transmission OFF.** DTX would stop sending during silence,
  breaking the RTP timestamp continuity the client relies on to keep audio
  aligned with video.
* **In-band FEC OFF.** FEC spends bitrate to survive packet loss, and §7.3
  interleaves media over the TCP control connection, where a lost packet is
  retransmitted rather than dropped. There is no loss for it to conceal.

Capture format is unchanged: 48 kHz, S16LE, stereo. Both codecs are native
48 kHz, so no resampling is performed.

---

## §7.2 RTSP *(revised)*

### §7.2.1 Capability advertisement — the feedback channel

RTSP's DESCRIBE is server-offers-first: the client has no SDP of its own in
which to state what it can decode. Revision B adds a request header for it.

A client **SHOULD** send on DESCRIBE:

```
X-Codec-Capabilities: video=<list>;audio=<list>
```

where each list is comma-separated codec names from `h264`, `vp9`, `av1` and
`opus`, `vorbis`. Names are case-insensitive. Example:

```
X-Codec-Capabilities: video=av1,vp9,h264;audio=opus,vorbis
```

Order carries **no** meaning: the server scores, and a client that wishes to
force a codec advertises only that one.

The server:

* **MUST** treat a DESCRIBE with no such header as a client supporting
  exactly Revision A's codecs — `video=h264;audio=vorbis`. This is what keeps
  Revision A clients working unchanged.
* **MUST** ignore unrecognised codec names rather than rejecting the request,
  so a client from a later revision still gets a session on the codecs both
  ends do share.
* **MUST** answer with an SDP naming the codecs it selected.
* **SHOULD** include `X-Codec-Selected` in the response, carrying the
  selection and the reason, for diagnosis.

If the two ends share no video codec, or no audio codec, the server **MUST**
fail the DESCRIBE with `454 Session Not Found`-style refusal carrying error
**5011**, and the message **MUST** list both sets. A client cannot fix a
mismatch it cannot see.

### §7.2.2 SDP

The SDP **MUST** announce the selected codecs, with these `rtpmap` names:

| codec | rtpmap |
|---|---|
| H.264 | `H264/90000` |
| VP9 | `VP9/90000` |
| AV1 | `AV1/90000` |
| Opus | `opus/48000/2` |
| Vorbis | `vorbis/48000/2` |

When Vorbis is selected, the SDP **MUST** carry the packed configuration in
the `configuration` fmtp parameter (RFC 5215 §3.2). When Opus is selected
there is nothing to carry.

---

## §7.3 RTP transport *(revised)*

### §7.3.1 Payload type assignments

| payload type | codec |
|---|---|
| 96 | H.264 |
| 97 | Vorbis |
| 98 | VP9 |
| 99 | AV1 |
| 100 | Opus |

Interleaved channel assignment is unchanged: video on 0–1, audio on 2–3.

### §7.3.2 Payload formats

* **H.264** — RFC 6184. Single NAL unit, STAP-A for aggregated parameter
  sets, FU-A for fragmentation. FU-B **MUST NOT** be used: the interleaved
  transport delivers in order, so its DON field is dead weight.
* **VP9** — draft-ietf-payload-vp9. Non-flexible, single-layer: the
  descriptor is the flag byte plus a 15-bit extended picture ID. A receiver
  **MUST** reject a packet carrying a scalability structure rather than
  guess at its length.
* **AV1** — AOM "RTP Payload Format For AV1". One OBU element per packet
  (`W = 1`), fragmented across packets with the `Z` and `Y` flags. `N`
  **MUST** be set on the first packet of a temporal unit that begins a new
  coded video sequence.
* **Opus** — RFC 7587. The payload **is** the Opus packet: no descriptor, no
  fragmentation. The RTP clock is **48 kHz regardless of capture rate**. The
  marker bit means the start of a talkspurt, not the end of a frame.
* **Vorbis** — RFC 5215, unchanged from Revision A.

For every video codec, the marker bit **MUST** be set on the last packet of
a frame and on no other.

---

## §7.6 Codec negotiation *(new)*

### §7.6.1 Requirement

The server **MUST** select the codec pair that costs least to run across
*both* ends, from the intersection of what it can encode and what the client
advertised. It **MUST NOT** select a codec the client did not advertise.

### §7.6.2 Cost model

Selection **MUST** be deterministic: the same offer and answer always yield
the same choice. Each candidate is scored, lowest wins.

The objective is least resources and lowest latency, in that order. Those
conflict exactly once — a hardware encoder saves a CPU core but adds one
frame of pipeline delay — and the model **MUST** resolve it in favour of
hardware. At 30 fps that frame is 33 ms, against a core of load whose
scheduling jitter under contention exceeds it.

Scores are **ordinal**. They order the options; they do not predict frame
times.

| term | value |
|---|---|
| hardware encode | 10 |
| hardware pipeline delay penalty | +15 |
| software encode, H.264 (libx264 `veryfast`) | 100 |
| software encode, VP9 (libvpx `realtime cpu-used=8`) | 220 |
| software encode, AV1 (SVT-AV1 `preset 12`) | 260 |
| decode, H.264 | 30 |
| decode, VP9 | 35 |
| decode, AV1 (dav1d) | 25 |
| bitrate index (H.264 / VP9 / AV1) | 100 / 70 / 55, weighted ÷10 |

`score = encode + decode + bitrate_index / 10`

Notes on the constants:

* The hardware penalty is deliberately smaller than any software encode
  cost. It **MUST** be able to break a tie between two hardware options and
  **MUST NOT** outweigh falling back to a CPU encoder.
* dav1d scores *below* the H.264 decoder because it genuinely is faster at
  1080p. AV1 is therefore not penalised on the client side.
* The bitrate index is weighted low and acts mainly as a tiebreak. Against a
  fixed 2000 kbps ceiling a more efficient codec does not save bandwidth; it
  spends the same budget on a better picture.

Audio requires no cost model. Opus is cheaper, lower-delay and needs no
out-of-band configuration, so it wins on every axis simultaneously; the rule
is simply Opus unless the client cannot decode it.

### §7.6.3 Worked example

The reference development host, stock Fedora Mesa, against a client
supporting everything:

```
offer:  video [h264/sw, vp9/sw, av1/hw]   audio [opus, vorbis]
answer: video [h264, vp9, av1]            audio [opus, vorbis]

  av1/hw   10 + 15 + 25 +  5 =  55   <- selected
  h264/sw       100 + 30 + 10 = 140
  vp9/sw        220 + 35 +  7 = 262

selected: av1 (hardware) + opus
```

With RPM Fusion's `mesa-va-drivers-freeworld` installed, H.264 also becomes
a hardware candidate and scores 65 — still behind AV1's 55, because at equal
placement AV1's efficiency decides it.

A Revision A client sending no capability header:

```
answer: video [h264]   audio [vorbis]     (the compatibility default)
selected: h264 (software) + vorbis
```

### §7.6.4 Reporting

The selection and its reason **MUST** be recorded where an operator can see
them — the boot log, and `X-Codec-Selected` on the DESCRIBE response. A
negotiated system that cannot explain its choice is harder to operate than a
fixed one, which would defeat the purpose of this revision.

---

## Appendix A — error codes *(additions)*

| code | meaning |
|---|---|
| 5008 | RTP packetisation failed |
| 5009 | Frame encode failed |
| 5010 | Captured frame geometry did not match the encoder |
| 5011 | No codec in common between server and client |

Codes 5001–5007 are unchanged.

---

## Compatibility with Revision A

| change | effect on a Revision A client |
|---|---|
| VP9 and AV1 added | none — it advertises neither, so neither is selected |
| Opus added | none — absent capability header means Vorbis |
| Negotiation added | none — the no-header default reproduces Revision A exactly |
| Payload types 98–100 assigned | none — unused unless negotiated |
| Wall-clock keyframe interval | strictly more keyframes than before, never fewer |

A Revision A client therefore receives exactly the Revision A stream. The
converse does not hold: a Revision B client against a Revision A server sends
a header the server ignores and receives H.264 and Vorbis, which it supports.
Both directions interoperate.

## Requirements this revision does not change

* The 2000 kbps hard ceiling (§7.1), and constrained VBR beneath it.
* 48 kHz S16LE stereo capture (§7.1).
* The RTSP state machine and method sequence (§7.2).
* Interleaved framing and channel assignment (§7.3).
* TLS 1.3-only transport and Basic authentication (§7.4).

---

# Amendment B.1 — one encoder, bound by the first session

**Date:** 2026-09-09 · **Amends:** §7.6 of this revision, and clarifies §1.2

## The contradiction

Revision B §7.6 requires the codec to be negotiated **per client**. §1.2 of
the base specification allows exactly **one** `media-encode` thread. Two
clients negotiating different codecs cannot both be served by one encoder, so
as written the two requirements are unsatisfiable together. Revision B did
not notice this, and an implementation that follows §7.6 literally ends up
opening one hardware encode context per connected client — on a machine whose
reason for negotiating in the first place was to use the fixed-function
encoder well.

## Resolution

**Negotiation binds the stream, not the session.**

* There **MUST** be exactly one video encoder and one audio encoder per
  machine, owned by the single `media-encode` thread of §1.2.
* The **first** session to complete a DESCRIBE negotiates the codecs, by the
  §7.6 cost model, against its own advertised capabilities. That choice
  becomes the machine's stream codec.
* While at least one session holds the stream, a later DESCRIBE **MUST** be
  answered with the codec already running. If that client cannot decode it,
  the DESCRIBE **MUST** fail with **5011**, and the message **MUST** name the
  codec being served as well as what the client offered — "no common codec"
  is not actionable unless the client is told what it would have to accept.
* When the last session tears down, the binding is released. The next
  DESCRIBE negotiates afresh.

The server **SHOULD** log the difference between a codec it *chose* and one
it *inherited*, because an operator debugging a refused client needs to know
the machine is serving a codec that was selected for somebody else.

## Why this way

The alternative — an encoder per distinct codec, bounded at three — preserves
Revision B's promise for every client but amends §1.2's thread table, and
buys little: the normal case is one operator on one console, and the second
viewer is nearly always the same client build. Paying a hardware encode
context to avoid a rare refusal is the wrong trade for a console.

This keeps §1.2 exactly as specified and costs only that a second client with
disjoint codec support is refused rather than served. It is refused
*explicitly*, with the reason and the remedy in the message.

## Consequences for §7.6

§7.6.1's "the server MUST select the codec pair that costs least across both
ends" applies **to the first session only**. For subsequent sessions the
selection is not a choice but a lookup, and §7.6.4's reporting requirement
covers both cases: the log must say which happened.

Nothing else in Revision B changes. The cost model, the capability header,
the payload types and the compatibility rules are unaffected.

## Consequences for §1.2

None. This amendment exists so that §1.2's thread table stands unmodified:
`media-capture` 1, `media-encode` 1, `rtsp-session` 0..n.

## Consequences for §1.5

One, and it is not optional: **DEVICE_INIT can no longer prove that the
encoder opens.**

§7.1 places the encoders in DEVICE_INIT, and the reasoning is good — an
encoder that cannot open should fail the phase, not the first DESCRIBE with
a client already waiting. But under this amendment no codec is chosen until
that first DESCRIBE, so there is nothing to open at DEVICE_INIT. Opening one
anyway is what produced the dead boot-time encoder of Finding 3: a hardware
AV1 context negotiated against an imagined client and never encoded with.

So DEVICE_INIT's obligation is narrowed to what it can still discharge
honestly:

* It **MUST** probe the host and record what the machine can encode, in the
  boot log, per codec and placement.
* It **MUST** fail the phase with **5001** if the machine can encode no
  video at all, or no audio at all — a display device in that state is
  broken now, and no session could ever be formed with it.
* It **MUST NOT** open an encode context, because it does not yet know which
  one to open.

A codec-specific open failure — a driver that advertises an AV1 entrypoint
and then refuses to create the context — therefore surfaces at the first
DESCRIBE, as **5001**, and that DESCRIBE fails. This is a real loss of
earliness and is the price of negotiating at all: the alternative is opening
every encoder the host advertises at boot to find out, which costs more than
the failure it detects.
