# Implementation status

An honest ledger against the *Detailed Design Specification* (2026-09-09).
Section numbers are the spec's.

Legend: **done** — implemented and covered by tests · **partial** — the
contract and structure are complete, some behaviour is not · **not started**.

## Summary

The machine **constructs for real** against `/dev/kvm` and **the control
plane is live**: a client connects over TLS 1.3, authenticates, sends
protocol-v1 frames, and its `reboot` and `powerdown` drive the real §1.5
runtime transitions.

```
CONFIG_LOAD -> MEM_ALLOC -> KVM_SETUP -> DEVICE_INIT -> FIRMWARE_MAP -> LISTENERS_UP -> VCPUS_RUN -> RUNNING
   done         done          done         done           done            done         no vCPUs      done
                                                                                            |
                                        RUNNING --(reboot)--> RESET --> VCPUS_RUN --> RUNNING     done
                                        RUNNING --(powerdown)--> GUEST_SHUTDOWN --> TEARDOWN --> EXIT   done
```

What is missing is guest *execution*: the vCPU run loop and the device
datapaths behind it. No guest code runs, so `VCPUS_RUN` starts no vCPU — but
every other phase does its real work, and the machine holds at `RUNNING`
serving control clients. Everything that can be specified and tested without
a running guest is implemented and pinned by tests.

## By section

| § | Subsystem | Status | Notes |
|---|---|---|---|
| 1.1 | Crate topology | **done** | Topology as specified. The no-C rule was **lifted** — `deny.toml` is now an allow-list, `scripts/c-dependency-inventory.sh` reports what is linked |
| 1.2 | Thread taxonomy | **partial** | The plan is computed and asserted; `wss-listener` and its per-client threads are spawned and named, the rest are not |
| 1.3 | Guest memory map | **done** | Exact §1.3 table, invariants enforced, 9 tests |
| 1.4 | KVM bring-up | **done** | Real ioctls; split irqchip mandatory, no PIC/PIT; CPUID/MSR/SREGS |
| 1.5 | Boot lifecycle | **done** | Transition table enforces the ordering MUSTs; 7 tests |
| 1.6 | Error model | **done** | `VmmError` over 8 domains, stable Appendix A codes throughout |
| 2.1 | ECAM config space | **done** | Address decode, BAR sizing probe, absent-function all-ones. Base moved to `0xE000_0000` (Revision D.5) to match both edk2's `PcdPciExpressBaseAddress` and the reference host's own ECAM |
| 2.2 | virtio capability layout | **done** | All four caps chained, per-queue doorbell addresses |
| 2.3 | Feature negotiation | **done** | MUST/SHOULD sets, packed+split, FEATURES_OK rejection |
| 3.1 | OVMF load | **done** | Read-only slot at `0xFFC0_0000`, image tail-aligned to the reset vector. **Stock Fedora `OVMF_CODE.fd` boots to the UEFI Boot Manager** — SEC, PEI, DXE, BDS. Needs the ICH9 LPC stub (D.6) and the D.5 ECAM base |
| 3.1b | CloudHv platform (no chipset) | **done** | `libvmm-core/src/cloudhv.rs` (Revision D.9, the chosen option): host bridge `8086:0d57` at 00:00.0 and hardware-reduced ACPI at `0x0600`/`0x0608`. **Firmware built from `OvmfPkg/CloudHv` boots to the UEFI Boot Manager with two unanswered I/O accesses in the whole boot.** Loads through the existing PVH loader — the firmware is an ELF |
| 3.1a | ICH9 LPC bridge + ACPI PM block | **done** | `libvmm-core/src/ich9.rs` (Revision D.6): 00:1f.0 with `PMBASE`/`ACPI_CNTL`/`RCBA`, the 3.579545 MHz ACPI timer, PM1/GPE0/SMI registers, and S5 → `Outcome::PowerOff`. A register file, not an LPC bus — no PIC, no PIT, no INTx |
| 3.2a | CMOS RTC | **done** | Reads `KVM_GET_CLOCK` realtime — the `ktime_get_snapshot()` pairing `ptp_kvm` answers from (D.7), so the RTC and the guest's PTP clock cannot drift. Status registers C and D are read-only, `UIP` always clear (D.8). The time registers are encoded as **register B says** — BCD or binary, 12- or 24-hour. `PcRtcInit` writes `Dm = 0`, and answering in binary anyway is what made Windows' boot loader fail with `STATUS_IO_DEVICE_ERROR` |
| 3.2 | SMBIOS Type 1 | **done** | Serial is `vm.name` byte-for-byte, verified through serialisation |
| 3.3 | ACPI table set | **done** | RSDP/XSDT/MCFG/MADT/SRAT/SLIT/DSDT/FADT/TPM2 + MSDM/SLIC injection, checksums enforced. The FADT is **hardware-reduced** (Revision D.11) and every register it names is one `cloudhv::CloudHvPm` decodes: `SLEEP_CONTROL_REG`/`SLEEP_STATUS_REG` at `0x0600`/`0x0601`, a 32-bit `X_PM_TMR_BLK` at `0x0608`, `RESET_REG` at `0x0CF9`, and PM1 blocks at `0x060C`/`0x0610` that exist only because Windows' hvloader refuses a hardware-reduced FADT whose PM1a GAS is zero |
| 3.4 | Display | **done** | Two devices (Revision D.10). virtio-gpu, which the firmware draws to; and a **Bochs VBE linear framebuffer** at `1234:1111` bound by stock `QemuVideoDxe`, which is the one a guest with no virtio-gpu driver can use — `VirtioGpuDxe`'s GOP is `PixelBltOnly` and stops working at `ExitBootServices`. Its framebuffer BAR is a KVM memory slot that follows the BAR, published only while the command register's memory-space bit is set |
| 4 | EFI NVRAM | **partial** | Engine binding, persistence policy and the §4.2 warning are done; the OVMF variable protocol is not wired |
| 5.1–5.3 | virtio-scsi + CDBs | **done** | INQUIRY/VPD/READ CAPACITY/READ/WRITE/SYNC/UNMAP/REPORT LUNS/MODE SENSE. **One HBA per drive, a request queue per vCPU, a worker thread per queue** (`{controller}-q{k}`). Two media: SSD (peripheral type 0x00, VPD 0xB1 rotation rate 1, TRIM/UNMAP) and **CD/DVD/BD-ROM** (type 0x05, removable, 2048-byte blocks, the MMC command set in `mmc.rs`, writes refused with DATA PROTECT). Booting from optical media is verified live: the UEFI Shell ISO reaches an interactive prompt at `PciRoot(0x0)/Pci(0x2,0x0)/Scsi(0x0,0x0)/CDROM(0x0)`, and Windows 11 reads 516 MiB of `boot.wim` off it |
| 5.4 | Unified engine trait | **done** | One trait; capability matrix is the single source of truth |
| 5.5 | io_uring datapath | **partial** | Submit/poll shape and a working file datapath; **io_uring SQPOLL with registered fixed buffers is not implemented** — see below |
| 5.6 | vhost-user front-end | **partial** | Message set, mem-table isolation and crash-isolation semantics are done and tested; the AF_UNIX transport is not |
| 6 | virtio-tpm | **partial** | Placement, TPM2 ACPI table, single queue, and the §6.2 volatile-engine rejection are done; **no TPM 2.0 command engine** |
| 7.1 | Capture + encode | **done** | Real encode for **H.264, VP9 and AV1**, each with a VA-API path (`h264_vaapi`/`vp9_vaapi`/`av1_vaapi`, chosen by a libva entrypoint probe) and a software fallback (libx264 shim, libvpx, SVT-AV1). Real **Opus** encode with **Vorbis** as the fallback. Constrained VBR with the 2000 kbps ceiling enforced over a VBV window; a keyframe at least every 2 s, held on the wall clock as well as the GOP |
| 7.6 | Codec negotiation | **done** | Revision B, as amended by [B.1](docs/spec-revision-B-media-codecs.md#amendment-b1--one-encoder-bound-by-the-first-session): negotiation binds the **stream**, not the session. `X-Codec-Capabilities` on DESCRIBE, `X-Codec-Selected` in reply; ordinal cost model over encode placement, decode cost and bitrate efficiency. The first session negotiates and its choice becomes the machine's codec; a later one inherits it, or is refused 5011 **naming the codec being served**. The binding releases with the last session |
| 7.2 | RTSP state machine + listener | **done** | The RTSPS listener binds `:8554` and serves `0..n` concurrent sessions, one `rtsp-session` thread each. Full OPTIONS → DESCRIBE → SETUP ×2 → PLAY → TEARDOWN over TLS 1.3, verified live against `vmm-console-client`. The state table is amended by [Revision C](docs/spec-revision-C-rtsp-session-state.md): SETUP is accepted per media section, and PLAY/TEARDOWN act on the session's subscription rather than on an encoder |
| 1.2 / 7.1 | Media plane threading | **done** | `media-capture` (1) and `media-encode` (1) exist as the specification's own threads and run for the machine's life, fanning one encoded stream out to every session. Previously the whole plane ran inline on the accepting thread — one client at a time, one encoder per client, capture driven by a client's session clock. See [the design review](docs/design-review-media-plane.md) |
| 7.3 | Interleaved framing | **done** | `$`-framing, big-endian length, channel assignment. RTP **packetisation** and depacketisation for all five codecs — RFC 6184 (H.264 STAP-A/FU-A), draft-ietf-payload-vp9, AOM AV1, RFC 7587 (Opus), RFC 5215 (Vorbis) — round-tripped against each other. Payload types 96/97/98/99/100 |
| 7.4 | RTSPS auth | **done** | Basic Auth on every request, 401 before any allocation |
| 8.1–8.3 | WSS handshake | **done** | Live listener and live client. 101/401/503/429/400/404, client cap, last-write-wins arbitration — all verified end to end over a real socket |
| 8.2 | TLS provisioning | **done** | Self-signed at boot, TLS 1.3 only, server and client, on WSS, RTSPS and USB/IP `:3241` |
| 8.4 | Lockout | **done** | Per-source-IP, 10 attempts, expiry resets, no credential check while locked |
| 8.5 | Control protocol v1 | **done** | Every frame round-trips client→server and back; 6400/6422/6423 pinned live |
| 9.1 | USB/IP wire | **done** | All structures, big-endian, round-tripped |
| 9.2 | Import + relay | **partial** | Real TCP server, sysfs enumeration, allow-set isolation, usbdevfs URB relay with kernel-driver detach, xHCI port model, unlink handling. **No TLS on :3241, no xHCI register emulation on the hypervisor side** |
| 10.1–10.2 | Backup, end to end | **done** | Preflight (8001), quiesce with the 8005 warning, per-engine snapshot, stream, progress and `complete` frames, staging cleanup. Both the CLI and WSS triggers work |
| 10.3 | `.vmbk` stream | **done** | Real tar + zstd with frame checksums and per-member SHA-256; the full §10.3 layout, readable by stock `zstd`/`tar` |
| 11 | TOML schema | **done** | Complete, `deny_unknown_fields`, all §11 defaults, 25 tests |

## The four storage engines (§5.4)

| engine | status |
|---|---|
| `pure_rust_io_uring` | **partial** — working file datapath (pread/pwrite, `FALLOC_FL_PUNCH_HOLE` discard, `FICLONE` snapshot with full-copy fallback). io_uring SQPOLL is not wired. |
| `rust_hugepage_file` | **done** — hugepage mmap, memcpy datapath, zeroing discard, correctly refuses snapshot |
| `rust_nvme` | **contract only** — capability contract is authoritative and consumed by the backup preflight; `open` returns `Storage(EngineOpen)` 4002. A user-space VFIO NVMe driver is a substantial separate effort. |
| `rust_ceph_rbd` | **done** — real librados/librbd: async `rbd_aio_*` submit/poll, native RBD discard for UNMAP, and constant-time RBD snapshots for §10.2. Needs a reachable cluster; without one `open` fails at `DEVICE_INIT` with a message naming the cause. |

`rust_nvme` fails loudly at `DEVICE_INIT` rather than pretending to work, so
a machine configured for it cannot silently run on a stub.

## The client (`vmm-console-client`)

| job | status |
|---|---|
| WSS control client (§8) | **done** — connects over TLS 1.3, authenticates, sends every protocol-v1 action, handles ack/error/progress/complete, answers pings, distinguishes 401/503/429 |
| Local input capture (§8.5) | **done** — raw-mode terminal, ASCII and ANSI navigation keys mapped to Linux keycodes with correct Shift bracketing; Ctrl-] disconnects |
| RTSPS client (§7) | **done** — full §7.2 method sequence over TLS 1.3, SDP parsing, §7.3 interleaved demux, reassembly and real decode to BGRA frames for **H.264, VP9 and AV1**, plus Opus and Vorbis audio. Advertises its own decode capability on DESCRIBE. A **window opens by default**; `--decode-to`, `--snapshot` and `--video-out` record alongside it |
| Wayland console | **done** — `xdg-shell` toplevel, `wl_shm` double-buffered, with keyboard and pointer captured from `wl_seat` and forwarded to the guest as §8.5 input frames. Verified against a real GNOME/Mutter session with real decoded H.264. Pure Rust: no libwayland is linked. Not yet exercised against a live server, because nothing serves RTSPS on this build |
| USB/IP server (§9) | **partial** — real listener, sysfs enumeration, allow-set isolation, URB relay via usbdevfs. **Cleartext :3240 only** |
| Frame generator | **done** — emits protocol-v1 frames for scripting, round-tripped through the server's own parser |

### Viewing and driving the machine

The console client exists to **view and interact with** a VM, and to host the
USB/IP server the guest dials into. So the window is not an option: `console`
opens one by default, and `--no-display` is the explicit opt-out for headless
capture. `--video-out` implies it, because storing the elementary stream does
not decode at all.

Input travels the other way. The window captures keyboard and pointer from
`wl_seat` and sends them over the §8 control channel as §8.5 frames —
keyboard as Linux keycodes, pointer as an absolute tablet on the 0..32767
grid. Two details that are silent when wrong:

* **No `+8` on keycodes.** `wayland.xml` says clients must add 8 to reach the
  *xkb* keycode, so the wire value is already the evdev code — which is what
  §8.5 wants. Adding 8 would give a console where every key types the wrong
  character.
* **Keys are released on focus loss.** Wayland sends the release to whoever
  has focus now, so a key held while Alt-Tabbing away would stick in the
  guest forever. The window tracks what is down and releases it on `leave`.

Because the media stream is one-way, input needs a second connection
(`--control-addr`). If it cannot be reached the session downgrades to
watch-only with the reason logged, rather than refusing to open — a console
you can see but not type into still beats no console.

`--decode-to`, `--snapshot` and `--video-out` all record *alongside* the
window rather than replacing it. They are one `FrameHandler` fanout, so the
decode path does not branch.

Decode is libavcodec's software decoder for whichever codec was negotiated —
`h264`, `libvpx-vp9` or `libdav1d` — deliberately: at §7.1's 1080p/2 Mbps
each costs a fraction of a core, and a hardware decode path would add a
surface-import dependency on the client's GPU for no measurable gain.

The window is **`wl_shm`, not dmabuf**, for the same reason: a GPU buffer
path would make the console depend on importing surfaces from the client's
graphics stack, and the frame already sits in a `Vec<u8>` on the CPU because
that is where the decoder put it. A 1080p blit is one memcpy per frame.

It links **no libwayland** — `wayland-client`'s default backend speaks the
wire protocol over the compositor socket in Rust, which `ldd` confirms. The
display path therefore adds nothing to the C inventory.

The tests in `crates/vmm-console-client/tests/wayland.rs` run against a real
compositor and assert on `wl_buffer::release`, which the compositor only
sends after reading the pixels — the difference between "the protocol was
accepted" and "the frames were displayed". One of them drives the whole
pipeline, encoding H.264 and painting what the decoder returns, which is
what would catch a pixel-format mismatch between the two halves. Where there
is no `WAYLAND_DISPLAY` they skip, because the absence of a compositor says
nothing about the code.

**A live session has now been run.** Two `vmm-console-client` processes
connected concurrently to `custom-vmm` over TLS 1.3, negotiated AV1 on the
host's VA-API encoder plus Opus, and decoded a real picture: 353 and 203
frames respectively at ~29 fps, from **one** hardware encode context. The
second client's DESCRIBE was answered with the codec the first had bound,
and the binding released when the last of them tore down. `--snapshot`
wrote a non-black 1920x1082 frame from each.

The untested span is now only the guest itself: the picture is
`console_source`'s generated pattern, not a virtio-gpu scanout, because no
vCPU runs on this build.

## Known gaps, with reasons

Host-side prerequisites — kernel features, firmware, privileges, and the C
libraries §1.1 forbids — are enumerated in
[HOST-REQUIREMENTS.md](HOST-REQUIREMENTS.md), with `scripts/preflight.sh` to
check a host against them.

### §1.1's no-C rule was lifted

The rule blocked six subsystems — TLS, video encode, audio encode, video
decode, zstd and RADOS — and only two of the six had a pure-Rust path. The
spec owner lifted it.

What replaced it is an allow-list in `deny.toml`: C is permitted, but only
the dependencies chosen on purpose. An accidental `-sys` crate arriving
transitively still fails, and `scripts/c-dependency-inventory.sh` reports the
inventory with a reason for each entry.

All six are now unblocked and implemented: **TLS 1.3** everywhere (`ring`),
**the `.vmbk` stream** (`zstd-sys`), **video encode** for H.264/VP9/AV1
(libva entrypoint probe plus `h264_vaapi`/`vp9_vaapi`/`av1_vaapi`, falling
back to libx264, libvpx and SVT-AV1), **audio encode** for Opus and Vorbis
(`libopus` via libavcodec, `libvorbisenc` direct), **video decode** in the
client (`libavcodec` + `libswscale`, with `libdav1d` for AV1), and **RADOS**
(`librados`/`librbd`). Two new crates hold the whole C surface —
`vmm-codec-sys` and `vmm-rbd-sys` — so `unsafe` does not leak past them.

One wrinkle worth recording: **libx264 is reached through a small C shim**
rather than generated bindings, because bindgen cannot represent
`x264_param_t`. `x264.h` declares `x264_zone_t` first, holding a
`struct x264_param_t *` back-pointer, and bindgen materialises the forward
declaration as an opaque one-byte struct that it never upgrades when the real
definition arrives. Reproduced on bindgen 0.70, 0.71 and 0.72. The other four
libraries bind cleanly. See
[HOST-REQUIREMENTS.md §6](HOST-REQUIREMENTS.md).

### Why the codecs are negotiated (Revision B)

The single biggest thing the implementation learned is that **the right
codec is a property of the host, and the two ends disagree about it.** That
is what [Revision B of the
specification](docs/spec-revision-B-media-codecs.md) records.

Worth recording, because the symptom is misleading: **Fedora's stock
`mesa-dri-drivers` is built without H.264 and HEVC** for patent reasons. On
that build the VA-API probe reports JPEG, VP9 and AV1 only — no H.264
entrypoint at all, encode *or* decode — which looks exactly like a GPU that
cannot do it. It is a packaging choice, not a hardware limit. RPM Fusion's
`mesa-va-drivers-freeworld` restores H.264/HEVC into
`/usr/lib64/dri-freeworld`, which libva already searches ahead of the stock
path:

```sh
sudo dnf install mesa-va-drivers-freeworld
```

Following the original spec literally on the stock build meant encoding
H.264 on the CPU while a perfectly good fixed-function AV1 encoder sat idle.
Negotiation is what fixes that, and the boot log shows it choosing:

```
can encode: video ["h264/sw", "vp9/sw", "av1/hw"], audio ["opus", "vorbis"]
codec selection: av1 (hardware) + opus — av1 chosen for hardware encode
(score 55); next best h264 at 140
```

With freeworld installed the offer becomes `["h264/hw", "vp9/sw", "av1/hw"]`
and H.264 rises to 65 — still behind AV1's 55, because among equal placement
the more efficient codec wins. Both driver builds are verified on this
machine, and the full suite passes against each.

Audio needed no cost model: Opus is cheaper, lower-delay (20 ms against
Vorbis' 46 ms) and needs no out-of-band configuration, so it wins on every
axis at once. Vorbis stays fully implemented as the fallback rather than
being removed, because a client that cannot decode Opus would otherwise have
no audio at all.

The backends differ in one way callers must handle: **libx264 returns every
frame on the call that submitted it** (it is tuned `zerolatency`), while
**the VA-API encoders have one frame of pipeline delay**. `async_depth`
is pinned to 1 to hold that at its floor — the libavcodec default of 2 would
double it, and on an interactive console that is visible input lag.
`push_scanout` returning no packets is therefore normal, not an error.

### The vCPU run loop (§1.4, §2)

`KVM_RUN`, `KVM_EXIT_MMIO` dispatch into `PciBus::config_rw`, ioeventfd
doorbell binding and irqfd injection are the next body of work. The pieces
they need — the memory map, the config-space emulation, the virtqueue engine,
the GSI table — are implemented and tested underneath.

Its absence is what makes two otherwise-complete paths inert: a control
client's keystrokes are validated, arbitrated and routed to `02:00.0`, but
there is no virtio-input datapath to inject them into; and the `media-capture`
thread runs, but what it captures is a generated pattern rather than a
virtio-gpu scanout, because no guest has drawn one.

Everything downstream of that seam is done and has been run: capture,
encode, packetisation, the RTSPS listener, and concurrent clients decoding
the result. Substituting virtio-gpu for `console_source::DemoScanout` is a
one-line change in `boot.rs` — the `CaptureSource` trait exists precisely so
that it is.

## Interpretations made where the spec was ambiguous

Recorded so they can be corrected rather than discovered:

1. **§3.3, unreadable injection path.** A configured `msdm_path`/`slic_path`
   that cannot be read is treated like an absent path: the table is skipped
   with a warning and boot continues. A table that *is* read but fails its
   checksum, length or signature check aborts with 1010. Reading it the other
   way — hard-failing a missing file — would contradict "injection is
   optional… boot continues".

2. **Empty drive images.** §11 has no per-drive size field, so a drive image
   must be provisioned before boot. A zero-length image is rejected at
   `DEVICE_INIT` rather than presenting a zero-block LUN.

3. **A control client's keystrokes are not an error.** With no virtio-input
   datapath, `on_input` counts and routes the event and returns `Ok`. Making
   it an error would turn every keystroke into an error frame; the boot log
   says plainly that the datapath is absent instead.

4. **`pure_rust_io_uring` snapshot on a non-CoW filesystem.** §10.2 declares
   the engine snapshot-capable via reflink. Where `FICLONE` is unavailable the
   engine falls back to a full copy and records the method in the manifest,
   rather than failing a backup the spec says should succeed.

5. **Exactly one bootable drive.** §11's reference machine has one; the
   validator requires exactly one when any drive is configured.

## Known defects

* **A connect failure always reports in the Control domain.**
  `vmm-console-client`'s `transport.rs` is shared by the WSS, RTSP and USB/IP
  clients but raises `ControlError::Connect` (6005) unconditionally, so an
  unreachable *RTSP* endpoint is reported as `[Control 6005] control:
  connecting to …`. The message names the right address, but the domain and
  code point at the wrong subsystem — which for a client that now opens
  three connections is actively misleading. Fixing it means threading the
  caller's domain through the shared connect helper.

## Planned, not started

### A browser-based console client

A **web client** connecting to the VMM console is wanted alongside the native
`vmm-console-client`, and is deliberately sequenced *after* the native client
has been proven against a live machine. Two things block that, and neither is
about the web client:

* ~~**No RTSP listener (§7.2).**~~ Cleared: the listener binds and serves
  `0..n` concurrent sessions, verified live.
* **No vCPU run loop (§1.4).** No guest executes, so what the media plane
  captures is a generated pattern rather than a virtio-gpu scanout.

The remaining blocker is the one that matters for a *console*: there is a
live stream, but nothing of a guest in it. A web client written now would be
debugged against a test pattern. The native client stays the reference — it
exercises DESCRIBE with capability negotiation, the interleaved demux,
decode for all three video codecs, and a real window, and it has now been
run against a real server.

Worth noting for whoever picks it up: a browser cannot take the RTSP
interleaved transport of §7.3 directly, so this is not a port of the native
client's transport. It needs either WebRTC or WebSocket-framed media, which
is a **specification question first** — a new revision under `docs/`, in the
manner of [Revision B](docs/spec-revision-B-media-codecs.md) — not an
implementation detail to settle in code.

## Open items the spec itself flags (Appendix B)

* §8.5 — whether WSS input frames should be acknowledged, or only errors.
  Implemented as **errors only**, per the §8.5 note about keeping the
  datapath light. Changing this is a one-line change in the frame handler.
* §10.2 — whether a non-snapshot drive can be excluded rather than failing
  the whole backup. Implemented as **fails the whole backup** (8001), which is
  the stated behaviour; per-drive exclusion is not implemented.
* §8.2 — cluster TLS authority remains out of scope, as specified.
