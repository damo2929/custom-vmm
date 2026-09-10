# Revision C — §7.2's session state table under a shared encoder

**Date:** 2026-09-10 · **Amends:** §7.2 of the *Detailed Design
Specification* · **Depends on:** [Amendment B.1](spec-revision-B-media-codecs.md#amendment-b1--one-encoder-bound-by-the-first-session)

Two arms of §7.2's RTSP state table are wrong. The first has been wrong
since the section was written; the second became wrong when Amendment B.1
moved the encoder out of the session. Both were amended in code before they
were amended in writing, which is the failure this document exists to
correct — see [the design review](design-review-media-plane.md), Finding 7.

The table as specified:

```text
INIT --DESCRIBE--> INIT (returns SDP)
INIT --SETUP-----> READY (allocate RTP interleaved channels)
READY --PLAY-----> PLAYING (start encoder feed)
PLAYING --PAUSE--> READY
READY/PLAYING --TEARDOWN--> INIT (free encoder + channels)
ANY --(auth fail)--> respond 401, stay in current state, no media
```

---

## C.1 — SETUP is per media section, so READY must accept it

**Amends:** the `INIT --SETUP--> READY` arm.

RTSP sets up each media section separately: the client issues one SETUP per
`m=` line in the SDP, addressed to that section's `a=control:` URI. §7.1's
console has two — video and audio — so **every** conforming client sends
SETUP twice, the second while the session is already in READY.

The table admits SETUP only from INIT, so the second one was answered
`455 Method Not Valid In This State` and every two-stream session failed at
its audio SETUP. Nothing in §7 intends that; the arm is simply written as
though a session had one stream.

### Resolution

The transition becomes:

```text
INIT|READY --SETUP--> READY (allocate that media section's channels)
```

A session **MUST** accept a SETUP in READY as well as in INIT, allocating
the interleaved channel pair the request's `Transport` header names. §7.3
fixes those pairs at `0-1` for video and `2-3` for audio, so a server that
answered with a fixed range would put both streams on the same channels; the
range the client proposed **MUST** be echoed.

SETUP remains invalid in PLAYING. Renegotiating transport mid-stream is a
capability §7 does not describe and this revision does not add.

### Why it is not a security or resource question

A repeated SETUP allocates nothing new: the channels are a property of the
connection, and the session's media subscription is created at DESCRIBE, not
at SETUP. A client that sends SETUP a hundred times gets READY a hundred
times and consumes nothing but the responses.

---

## C.2 — PLAY and TEARDOWN act on the session, not on the encoder

**Amends:** the parenthesised effects of the PLAY, PAUSE and TEARDOWN arms.

§7.2 describes PLAY as "start encoder feed" and TEARDOWN as "free encoder +
channels". Both were accurate when each session owned an encoder. Under
Amendment B.1 there is exactly one video and one audio encoder per machine,
owned by §1.2's single `media-encode` thread, and it runs for as long as any
session holds the stream. A session can no longer start it, and must not
free it.

### Resolution

The three arms keep their states and change their effects:

```text
READY --PLAY-----> PLAYING (begin writing the encoded stream to this session)
PLAYING --PAUSE--> READY   (stop writing; keep the channels and the subscription)
READY/PLAYING --TEARDOWN--> INIT (release this session's subscription and channels)
```

* PLAY **MUST NOT** start, restart or reconfigure an encoder, and **MUST
  NOT** request a keyframe. A session joining a stream already in progress
  **MUST** discard coded frames until it receives one it may begin at.
  §7.1's two-second keyframe interval bounds that wait; forcing an early
  keyframe would spend bits on everyone else's behalf to shorten one
  client's join.
* TEARDOWN **MUST** release the session's subscription. The encoders are
  released only when the *last* session does so, which is Amendment B.1's
  binding release and not a property of any one TEARDOWN.
* A session that disconnects without TEARDOWN **MUST** be treated as having
  sent one. Otherwise a client that crashes holds the machine's codec
  binding until the machine stops.

### Consequences for §7.4

§7.4 requires 401 "before any encoder resource is allocated". With no
encoder resource in the session, the requirement is now that 401 is answered
before the session joins the media plane — which is stricter in the useful
direction, since joining is what can bind the machine's codec. The
implementation authenticates before dispatching any method at all, so both
readings hold.

---

## What does not change

The states themselves (`INIT`, `READY`, `PLAYING`), the method set, the
authentication rule, §7.3's channel assignment, and the 455 answer for a
method that genuinely does not belong in the current state. This revision
widens one arm and restates three effects; it adds no method, no state and
no error code.

## Tests

* `a_two_stream_session_sends_two_setups_and_both_are_accepted` pins C.1.
* `a_session_joining_a_running_stream_starts_on_a_keyframe` and
  `one_encoder_feeds_every_session_the_same_stream` pin C.2's substance:
  PLAY writes an existing stream rather than starting one.
