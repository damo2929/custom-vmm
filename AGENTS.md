# Working in this tree

Conventions for anyone — human or agent — changing this repository. It
assumes you have read [README.md](README.md) for what the project is and
[IMPLEMENTATION-STATUS.md](IMPLEMENTATION-STATUS.md) for what currently
works.

---

## What this project is

A KVM hypervisor and its client suite, written to a *Detailed Design
Specification* that numbers its requirements. **The specification is the
authority.** Almost every module, test and error code traces back to a
numbered section, and the tests are written to pin *stated requirements*
rather than incidental behaviour.

One Cargo workspace, edition 2021, `x86_64-unknown-linux-gnu`. The two
binaries and eight libraries of §1.1 are named and scoped by the
specification — **do not add or rename one casually**, because the crate list
is itself a specified thing. Three further crates exist only to hold what
§1.1 did not anticipate: `vmm-codec-sys` and `vmm-rbd-sys` contain the entire
C surface, and `vmm-sysdeps` is build-script support that locates it.

The spec has been revised once by implementation experience:
[Revision B](docs/spec-revision-B-media-codecs.md) replaced the fixed media
codecs with negotiated ones. That is the model to follow when the spec turns
out to be wrong: write the revision down, with the reasoning, rather than
letting the code and the document drift apart.

---

## Build

```sh
cargo build --workspace
cargo test  --workspace            # 367 tests, all must pass
cargo clippy --workspace --all-targets   # must be warning-free
cargo fmt --all
```

`cargo` is at `~/.cargo/bin`; add it to `PATH` if the shell has not.

### The C libraries

The build links FFmpeg, libva, libx264, Vorbis/Ogg and Ceph. Get them with
the `-devel` packages ([HOST-REQUIREMENTS.md §6](HOST-REQUIREMENTS.md)), or,
where root is not available, stage them without it:

```sh
./scripts/setup-local-sysroot.sh          # writes .cargo/config.toml
./scripts/preflight.sh                    # check the host
./scripts/c-dependency-inventory.sh       # what C is linked, and why
```

The sysroot script points `VMM_SYSROOT` at a scratch directory via
`.cargo/config.toml`. Cargo reads it there; a plain shell does not, which is
why both scripts fall back to parsing that file. Keep that behaviour if you
touch them — without it the build works while preflight reports every library
missing, which reads as a broken host.

### Adding a dependency

`deny.toml` is an **allow-list**, not a blocklist. §1.1's original
no-C-linkage rule was lifted, and what replaced it is deliberate consent: a
new `-sys` crate arriving transitively fails the build. If you genuinely need
one, add it to `skip` with a comment naming the spec section it buys, and add
it to `scripts/c-dependency-inventory.sh` with a reason. An alternative TLS
or crypto stack (`openssl-sys`, `aws-lc-sys`) is never acceptable.

Prefer a pure-Rust crate where a production-grade one exists. C is for the
cases where it does not.

---

## Design rules

These are not style preferences. Each one exists because violating it
produced a real bug in this tree.

### Ask the system, do not assume

Capability is discovered at runtime, never inferred from configuration, a
device name or a driver string. `hardware_accelerator = "vaapi"` is a
*request*: the code opens the render node and asks the driver, per codec,
whether a profile carries an encode entrypoint.

This matters more than it sounds. On the reference host, stock Fedora Mesa
reports a healthy driver and a capable GPU and offers **no H.264 encode
entrypoint at all**, because Fedora builds Mesa without patent-encumbered
codecs. Nothing short of the query distinguishes that from hardware that
cannot encode.

### Fall back on absence, never on failure

A missing capability is a fallback condition. A capability that exists and
then fails is a **fault, and must be raised**.

Only `CodecError::Unavailable` crosses over to the software encoder. A host
that *has* an H.264 encoder but cannot open it gets error 5001, because
degrading silently would hide a real fault behind a performance regression
nobody would attribute correctly.

### Say what you chose and why

Any code that picks between options at runtime must be able to explain the
choice, in the log and — where a protocol allows it — on the wire. The codec
negotiator emits its score, the runner-up and the reason:

```
codec selection: av1 (hardware) + opus — av1 chosen for hardware encode
(score 55); next best h264 at 140
```

A negotiated system that cannot explain itself is harder to operate than a
fixed one, which would defeat the point of negotiating.

### Errors carry stable numeric codes

Every fallible operation returns `Result<T, VmmError>`, and every variant has
an Appendix A code — see [docs/error-codes.md](docs/error-codes.md) for all
89 and for how to add one. Codes never change meaning and retired numbers are
never reused: logs and tickets outlive the code that produced them.

Error messages name the offending value, and where a mismatch has two sides,
**both sides**. Error 5011 lists what the server offers *and* what the client
accepts, because a client cannot fix a mismatch it cannot see.

### Verify the thing you actually care about

Check the property that matters, not a proxy for it. The sysroot script
verified with `pkg-config --modversion`, which answers from a `.pc` file
alone — so it passed on a sysroot missing six linker symlinks and handed back
something that failed later at `cargo build`. It now links a real program.

The same rule drives the tests: `libvmm-media` pushes a real BGRA frame
through convert → encode → RTP → depacketise → decode and compares the
picture that comes out against the one that went in.

### Isolate `unsafe`

All C interaction lives in `vmm-codec-sys` and `vmm-rbd-sys`. `unsafe` does
not leak past them, and every block carries a `// SAFETY:` comment stating
the invariant that makes it sound.

Two specifics worth knowing before you touch that code:

* **libx264 goes through a hand-written C shim** (`vmm-codec-sys/shim/`), not
  bindgen. `x264.h` forward-declares `x264_param_t` via `x264_zone_t`'s
  back-pointer, and bindgen materialises it as an opaque one-byte struct it
  never upgrades — reproduced on 0.70, 0.71 and 0.72. The shim's ABI is ours:
  scalars and pointers only, which is what makes it safe to bind by hand.
* **Self-referential C structs must not move.** libvorbis' `dsp` points into
  `info`, and `block` into `dsp`; returning them by value segfaults. They live
  in a `Box` for address stability, with explicit tracking of which of the
  four structs were initialised so teardown after a partial init is correct
  rather than accidentally correct.

---

## Tests

Name a test after the requirement it pins, as a sentence:

```
a_third_client_is_rejected_with_503_before_the_upgrade
volatile_tpm_engine_aborts_with_1020
a_client_that_sends_no_capability_header_gets_the_revision_a_stream
queues_always_equal_vcpus
```

If a test name does not read like a sentence from the specification, it is
probably testing the wrong thing. Assertions carry a message explaining what
broke, not just which value differed.

Do not weaken a test to make it pass. If hardware behaviour forced a change —
VA-API buffers the first frame, so tests push until output appears — say so
in a comment at the assertion.

---

## Shell scripts

The three scripts in `scripts/` are part of the deliverable and are held to
the same standard as the Rust.

* All use `set -uo pipefail`. **Never write `producer | grep -q`** under
  `pipefail`: `grep -q` exits on the first match and SIGPIPEs the producer
  (141), which becomes the pipeline's status. The result is a check that
  passes or fails at random — it took a dozen runs to see it. Use a
  herestring: `grep -q needle <<<"$haystack"`.
* Invoke `ffmpeg` with `-nostdin`. It reads stdin by default.
* Do not swallow errors with `|| true` unless the failure is genuinely
  expected, and say which failures those are. `setup-local-sysroot.sh`
  tolerates cpio failures because `--alldeps` drags in base-OS packages that
  legitimately cannot unpack — and it says so, and gates on a link test
  instead.

---

## Writing it down

Every module header names the specification section it implements. Keep that
up when you move code.

Comments explain **why**, not what. The codebase's convention is to record
the reasoning that is not recoverable from the code — why `async_depth` is
pinned to 1, why DTX is off, why 32767 rather than 32768 — and to name the
symptom when a bug motivated the line, so the next person recognises it.

When you change behaviour, update the document that describes it in the same
change:

| you changed | update |
|---|---|
| what works, per spec section | [IMPLEMENTATION-STATUS.md](IMPLEMENTATION-STATUS.md) |
| anything needed outside Rust | [HOST-REQUIREMENTS.md](HOST-REQUIREMENTS.md) and `scripts/preflight.sh` |
| an error variant | [docs/error-codes.md](docs/error-codes.md) |
| media behaviour the spec fixes | a new revision under `docs/`, in spec voice |
| a linked C library | `deny.toml` and `scripts/c-dependency-inventory.sh` |

`IMPLEMENTATION-STATUS.md` is an **honest ledger**. Mark work `partial` with
the reason, not `done`, when part of it is missing. Its value is that a
reader can trust it — a subsystem recorded as done and found hollow costs
more than one recorded as absent.

---

## Reporting your work

State plainly what you did, what you verified and how. If tests fail, say so
and show the output. If you skipped part of the scope, say which part and
why — scaling the work down is the user's decision, not yours.

Do not claim a thing works because it compiles. This tree's history is mostly
cases where it compiled, reported success, and was wrong: a sysroot that
passed `pkg-config` and could not link, a preflight check that was a coin
flip, a negotiation whose compatibility guarantee had no implementation
behind it. **Run it.**
