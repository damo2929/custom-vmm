# Legacy-Free Native Rust KVM Hypervisor & Client Suite

First implementation of the *Detailed Design Specification* (revision
`implementation-ready draft`, 2026-09-09).

A single Cargo workspace, edition 2021, `x86_64-unknown-linux-gnu`. Two
binaries and eight libraries, matching the §1.1 topology exactly.

```
custom-vmm            (bin) hypervisor entrypoint, KVM owner, control plane
vmm-console-client    (bin) remote console + embedded USB/IP server
libvmm-config         (lib) serde/TOML schema, validation (deny_unknown_fields)
libvmm-core           (lib) KVM setup, memory, ACPI/SMBIOS, PCIe, lifecycle
libvmm-virtio         (lib) virtio-pci transport, virtqueue engine, MSI-X
libvmm-storage        (lib) virtio-scsi + the four unified engines, vhost-user
libvmm-net            (lib) virtio-net + rust_af_xdp
libvmm-media          (lib) virtio-gpu/snd capture, H.264/VP9/AV1 + Opus/Vorbis, RTSPS
libvmm-control        (lib) WSS listener, JSON protocol v1, auth + lockout
libvmm-usbip          (lib) USB/IP client bridge + server
```

**What works today and what does not is recorded honestly in
[IMPLEMENTATION-STATUS.md](IMPLEMENTATION-STATUS.md).** Read it before
assuming a subsystem is finished. In short: the machine constructs for real
against `/dev/kvm`, its control plane is live, and its §7 console streams —
several clients at once connect over TLS 1.3, negotiate a codec, and decode
a real picture from one hardware encoder. What that picture shows is a test
pattern, because no guest code executes yet.

## Before you start

The suite needs things from the host that `cargo build` cannot provide:
1 GiB hugepages, `KVM_CAP_SPLIT_IRQCHIP`, an OVMF image, a reflink-capable
filesystem, and device nodes for USB/VFIO/VA-API.
**[HOST-REQUIREMENTS.md](HOST-REQUIREMENTS.md)** lists all of it, including
the C libraries the codecs and Ceph need. On Fedora:

```sh
sudo dnf install ffmpeg-devel libva-devel libvorbis-devel \
                 librados-devel librbd-devel x264-devel clang-libs
```

No root? `./scripts/setup-local-sysroot.sh` stages the same packages into a
scratch directory and points the build at them.

Check a host — and optionally a specific machine — against it:

```sh
./scripts/preflight.sh
./scripts/preflight.sh config/reference-vm.toml
```

## Build and test

```sh
cargo build --workspace
cargo test  --workspace          # 367 tests
cargo clippy --workspace --all-targets
./scripts/c-dependency-inventory.sh   # what C is linked, and why
```

## Try it

Validate a machine and print its full plan without touching `/dev/kvm`:

```sh
cargo run -p custom-vmm -- --config config/reference-vm.toml --check
```

That prints the §1.3 memory map, the PCIe fabric with per-queue doorbell
addresses, the ACPI table set with staging addresses, the SMBIOS Type 1
serial, the §1.2 thread taxonomy, and every warning the machine has earned.

Boot for real (needs `/dev/kvm` access and a provisioned drive image):

```sh
truncate -s 32G /var/lib/vmm/disks/boot_os.raw
cargo run -p custom-vmm -- --config /path/to/machine.toml
```

```
CONFIG_LOAD -> MEM_ALLOC -> KVM_SETUP -> DEVICE_INIT -> FIRMWARE_MAP -> LISTENERS_UP -> VCPUS_RUN
```

The machine holds at `RUNNING` serving control clients until one asks it to
stop, or `--run-for SECONDS` elapses. Take a backup without booting with
`--backup PATH`.

## Drive it with the client

With the hypervisor running, `vmm-console-client` speaks all three protocols.

**Control channel (§8)** — connect, forward keystrokes, disconnect with Ctrl-]:

```sh
cargo run -p vmm-console-client -- connect --addr '[::1]:8080' --insecure
```

Or send one action and read the reply:

```sh
vmm-console-client send --addr '[::1]:8080' --insecure reboot
vmm-console-client send --addr '[::1]:8080' --insecure backup --path /var/backups/vm.vmbk
vmm-console-client send --addr '[::1]:8080' --insecure key --code 30 --value 1
```

`--insecure` accepts the hypervisor's boot-time self-signed certificate,
which is the §8.2 situation until a cluster CA exists.

**Console (§7)** — view and drive the machine. This is the client's whole
point, so a window opens by default:

```sh
vmm-console-client console --addr '[::1]:8554' --insecure --seconds 30
```

Keyboard and pointer go back to the guest over the §8 control channel
(`--control-addr`, default `[::1]:8080`) as §8.5 input frames. `--view-only`
watches without typing; if the control endpoint cannot be reached the session
says so and continues watch-only rather than refusing to open.

To record instead of watching, `--no-display` and a file:

```sh
vmm-console-client console --addr '[::1]:8554' --insecure --no-display \
    --video-out /tmp/console.video --audio-out /tmp/console.audio --seconds 30
```

Which codecs those hold depends on what was negotiated — the client logs the
selection on connect. When H.264 is chosen the video file is a valid Annex-B
stream any player will take.

The window is `xdg-shell` + `wl_shm` and links no libwayland — closing it
ends the session. Or record alongside it, raw BGRA frames or a single still:

```sh
vmm-console-client console --addr '[::1]:8554' --insecure \
    --decode-to /tmp/console.bgra --seconds 30
vmm-console-client console --addr '[::1]:8554' --insecure \
    --snapshot /tmp/console.ppm --seconds 5
```

Those run *with* the window, not instead of it — asking for a recording
should not cost you the picture.

`custom-vmm` serves this: the RTSPS listener binds `:8554` and accepts as
many concurrent sessions as you open. What you will see is a generated test
pattern rather than a guest, because no vCPU runs yet — see
[IMPLEMENTATION-STATUS.md](IMPLEMENTATION-STATUS.md).

**USB/IP server (§9)** — export a real host device to the guest:

```sh
vmm-console-client devices                    # list bus IDs
vmm-console-client usbip --allow 1-2          # serve only that device
```

A bus ID outside `--allow` is neither listed nor importable (§9.2). Exporting
a device needs write access to its `/dev/bus/usb` node.

**Frame generation** — protocol-v1 JSON without connecting, for scripting:

```sh
vmm-console-client frame input-key --seq 42 --code 30 --value 1
vmm-console-client frame backup --seq 46
```

## C dependencies

§1.1's blanket no-C rule was **lifted**: it blocked TLS, the video and audio
codecs, zstd and RADOS, and only two of the six had a pure-Rust path. C is now permitted
from an allow-list in `deny.toml` — an accidental `-sys` crate arriving
transitively still fails the build.

All six are implemented:

| buys | library |
|---|---|
| TLS 1.3 on every listener and client | `ring`, statically linked |
| the `.vmbk` stream | `zstd-sys`, statically linked |
| H.264 / VP9 / AV1 encode | `libva` + `libavcodec`'s `*_vaapi` encoders, falling back to `libx264`, libvpx and SVT-AV1 |
| Opus encode, and Vorbis as the fallback | `libopus` via `libavcodec`; `libvorbisenc` direct |
| video decode in the client | `libavcodec` + `libswscale`, with `libdav1d` for AV1 |
| the `rust_ceph_rbd` engine and its snapshots | `librados` + `librbd` |

The whole C surface lives in two crates, `vmm-codec-sys` and `vmm-rbd-sys`,
so `unsafe` does not leak past them.

```sh
./scripts/c-dependency-inventory.sh
```

The hardware encode path is chosen by a real capability probe, not by
configuration: `hardware_accelerator = "vaapi"` is a request, and the probe
asks the driver, per codec, whether a profile carries an encode entrypoint.
See [HOST-REQUIREMENTS.md §6](HOST-REQUIREMENTS.md).

## Codec negotiation

The codecs are **negotiated, not configured** — see [Revision B of the
specification](docs/spec-revision-B-media-codecs.md). The client advertises
what it can decode on DESCRIBE:

```
X-Codec-Capabilities: video=av1,vp9,h264;audio=opus,vorbis
```

and the server picks the cheapest pair both ends support, preferring a
fixed-function encoder over a CPU one and Opus over Vorbis. It says what it
chose and why, in the boot log and in `X-Codec-Selected`:

```
can encode: video ["h264/sw", "vp9/sw", "av1/hw"], audio ["opus", "vorbis"]
codec selection: av1 (hardware) + opus — av1 chosen for hardware encode
(score 55); next best h264 at 140
```

A client that sends no header at all is assumed to want H.264 and Vorbis,
which is exactly the pre-negotiation behaviour, so older clients are
unaffected. When the two ends share no codec the DESCRIBE fails with error
5011 naming both sets, rather than negotiating down to something silent.

**Negotiation binds the stream, not the session**
([Amendment B.1](docs/spec-revision-B-media-codecs.md#amendment-b1--one-encoder-bound-by-the-first-session)).
There is one video and one audio encoder per machine, however many clients
watch it. The first DESCRIBE chooses; a later one is answered with the codec
already running:

```
media plane: session 2 inherits av1 + opus — bound by an earlier session,
not selected for this one (§7.6.4)
```

A client that cannot decode what is running is refused 5011 **naming it**,
so it is told what it would have to accept. The binding releases when the
last session tears down, and the next DESCRIBE negotiates afresh.

## Documentation

| document | what it answers |
|---|---|
| [IMPLEMENTATION-STATUS.md](IMPLEMENTATION-STATUS.md) | what actually works, per spec section, and what does not |
| [HOST-REQUIREMENTS.md](HOST-REQUIREMENTS.md) | everything needed outside Rust — kernel, privileges, C libraries |
| [docs/spec-revision-B-media-codecs.md](docs/spec-revision-B-media-codecs.md) | the media §7 specification as implemented: codecs, negotiation, RTP, and Amendment B.1's one-encoder rule |
| [docs/spec-revision-C-rtsp-session-state.md](docs/spec-revision-C-rtsp-session-state.md) | §7.2's session state table under a shared encoder |
| [docs/error-codes.md](docs/error-codes.md) | all 89 Appendix A codes, and what to do about the common ones |
| [AGENTS.md](AGENTS.md) | conventions to follow when changing this tree |
| [CHECKPOINT.md](CHECKPOINT.md) | where the work stands and what comes next |
| [docs/design-review-media-plane.md](docs/design-review-media-plane.md) | why the §7 plane was rebuilt from §1.2/§7.1, and what each finding cost |

## Layout

| path | what it holds |
|---|---|
| `config/reference-vm.toml` | the §11 reference machine, verbatim |
| `crates/*/src` | the implementation, annotated with the section it realises |
| `crates/*/tests` | spec-conformance tests, one per stated MUST where possible |
| `crates/vmm-console-client/tests/end_to_end.rs` | a real client against a real listener over TLS 1.3 |
| `crates/libvmm-media/tests/pipeline.rs` | a real frame through convert → encode → RTP → decode, compared against the original |
| `crates/libvmm-media/tests/media_plane.rs` | one encoder fanned out to many sessions: the codec binding, the 5011 refusal, the keyframe a late joiner starts on |
| `crates/vmm-codec-sys`, `crates/vmm-rbd-sys` | the entire C surface: codecs and Ceph |
| `crates/vmm-sysdeps` | build-script support that locates those libraries |
| `deny.toml`, `scripts/c-dependency-inventory.sh` | the dependency allow-list, and what C is linked |
| `HOST-REQUIREMENTS.md`, `scripts/preflight.sh` | everything needed outside Rust, and a check for it |
| `scripts/setup-local-sysroot.sh` | stages the `-devel` packages without root, for hosts where installing them is not an option |

Every module header names the specification section it implements, and the
tests are written to pin *stated requirements* rather than incidental
behaviour — `a_third_client_is_rejected_with_503_before_the_upgrade`,
`volatile_tpm_engine_aborts_with_1020`, `queues_always_equal_vcpus`, and so
on. If a test name does not read like a sentence from the spec, it is
probably testing the wrong thing.
