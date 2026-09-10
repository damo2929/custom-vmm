# Checkpoint — 2026-09-10

Resume point. Written to be read cold: it assumes only
[README.md](README.md) and [AGENTS.md](AGENTS.md).

**Tree state: green.** `cargo build --workspace` clean, `cargo clippy
--workspace --all-targets` zero warnings, **427 tests passing**.

---

## The guest draws its own screen, and a console client watches it live

Priorities 1 and 2 are done. A Linux kernel boots, binds a virtio-gpu
device this VMM emulates, renders its console into a scanout we own, and
that scanout is encoded once and streamed over RTSPS to any number of
console clients.

```sh
scripts/extract-vmlinux /boot/vmlinuz-$(uname -r) > /tmp/vmlinux
cargo run -p custom-vmm --example gpu_boot -- /tmp/vmlinux 60
# then, from anywhere:
cargo run -p vmm-console-client -- console --addr '[::1]:8554' --insecure
```

Measured: **446 frames decoded at 1920x1082** by `vmm-console-client` over
AV1 on `av1_vaapi`, from the guest's own framebuffer.

The proof that it is really the guest's picture is not the frame count.
The raw scanout was dumped before encoding (`VMM_DUMP_SCANOUT=path`) and
read back through the kernel's own `lib/fonts/font_8x16.c`, which fbcon
rendered it with. It decodes to the guest's kernel log, pixel-exact,
including the guest announcing that it had found us:

```
[drm] pci: virtio-gpu-pci detected at 0000:00:01.0
[drm] Initialized virtio_gpu 0.1.0 for 0000:00:01.0 on minor 0
Console: switching to colour frame buffer device 240x67
virtio-pci 0000:00:01.0: [drm] fb0: virtio_gpudrmfb frame buffer device
```

### What was built for it

| | where |
|---|---|
| virtio-pci modern transport | `libvmm-virtio/src/transport.rs` |
| virtio-gpu 2D, the eight commands a real driver sends | `libvmm-virtio/src/gpu.rs` |
| MSI-X table, PBA and queue running | `libvmm-virtio/src/gpu_pci.rs` |
| Guest RAM for device models | `libvmm-virtio/src/mem.rs` |
| `MmioDevice`/`MsiSender` seams, MMIO routing | `libvmm-core/src/devices.rs` |
| `KVM_SIGNAL_MSI` delivery | `libvmm-core/src/kvm.rs` |
| End-to-end demo | `custom-vmm/examples/gpu_boot.rs` |

Interrupts use `KVM_SIGNAL_MSI`, not irqfd, because it carries the message
the guest wrote into its own MSI-X table and so needs no GSI and no
routing table — and GSI routing is still unwritten here. Queues are
serviced inline on the vCPU thread that took the exit; `KVM_IOEVENTFD` on
the doorbell is the fix when that starts to matter, and
`BarLayout::doorbell_address` already exists for it.

### Three real bugs, all found by a running guest

None of these were caught by 409 passing tests:

1. **A feature-word read past 64 bits panicked the vCPU thread.** Linux
   walks the feature selector upwards; `offered >> 64` overflows. The
   thread died mid-MMIO and the guest hung forever on a read nobody would
   answer — the actual error printed on a thread nobody was watching.
   Fixed, and `with_devices` now catches a device panic and stops the
   machine with the reason attached rather than wedging it.
2. **Packed rings ignored `VIRTQ_DESC_F_INDIRECT`**, handing the
   descriptor table to the device as though it were the request. The
   guest's first command arrived as `0x4684498`. A packed indirect table
   is not shaped like a split one — `id` sits where split keeps `flags`,
   and entries are consumed in order with no `next`.
3. **The FADT never pointed at the DSDT** (see below).

---

## A Linux kernel boots. Priority 1 is done.

An unmodified `vmlinux` runs on this hypervisor, through the PVH loader in
`crates/libvmm-core/src/pvh.rs`, and prints to a UART this VMM emulates. It
gets from the ELF note to `VFS: Unable to mount root fs` — which is the
correct end for a machine with no disk attached.

Reproduce it:

```sh
# Any distribution's bzImage will do; the PVH note survives extraction.
scripts/extract-vmlinux /boot/vmlinuz-$(uname -r) > /tmp/vmlinux
cargo run -q -p libvmm-core --example pvh_boot_run -- /tmp/vmlinux 20
```

`cargo test -p libvmm-core --test pvh_boot` is the same thing as an
assertion; it skips unless `VMM_TEST_KERNEL` points at a kernel.

What the guest reports, in its own words:

```
Linux version 7.1.13 ...
Hypervisor detected: KVM
ACPI: RSDP 0x00000000000E0000 000024 (v02 RUSTVM)
ACPI: Interpreter enabled
PCI: ECAM [mem 0xc0000000-0xcfffffff] reserved as E820 entry
PCI host bridge to bus 0000:00
APIC: Switch to symmetric I/O mode setup
APIC: Switched APIC routing to: physical x2apic
input: Power Button as /devices/platform/PNP0C0C:00/input/input0
Kernel panic - not syncing: VFS: Unable to mount root fs
```

Every line of that is load-bearing. The ECAM window is found and reserved
because the memmap marks it `Reserved`. The APIC reaches symmetric I/O mode
because the MADT is present with `PCAT_COMPAT` clear. The power button is
our own DSDT's AML being interpreted.

**One real bug was found by booting and by nothing else.** The FADT's
`DSDT` and `X_DSDT` fields were left at zero with a comment saying "patched
at link time" — but there is no link step. The table set passed every
checksum and every existing test; the guest reported `Could not acquire
table length at 0000000000000000` and then oopsed in
`acpi_tb_load_namespace`. The DSDT was also wrongly listed in the XSDT,
which is reached only through the FADT. Both are fixed in
`acpi/builder.rs::link_fadt_to_dsdt` and pinned by
`the_fadt_points_at_the_dsdt_and_the_xsdt_does_not`.

### What is not done

* **Nothing is wired into `boot.rs`.** The loader and run loop are
  exercised by tests and the example, not by `custom-vmm`.
* **No root filesystem.** virtio-blk over MSI-X, or an initramfs, is what
  turns this from "the kernel runs" into "the machine is usable".
* **No `KVM_EXIT_MMIO` dispatch to devices**, no ioeventfd doorbells, no
  irqfd injection. The guest saw a PCI bus with one host bridge on it
  because config space answers; nothing behind it does.
* Priorities 2 and 3 — a picture and input — are untouched. See D.4 below
  for the constraint that shapes them.

---

## Background: the run loop

`KVM_RUN` executes guest instructions and their exits reach the VMM. That
is proven by `crates/libvmm-core/tests/vcpu_run.rs`, five tests on a real
vCPU: a guest's own `out dx, al` reaches the UART, execution begins at the
reset vector, four vCPUs all stop when asked, the exit counters match what
the guest did, and a triple fault is reported rather than swallowed.

**What is not done: nothing loads a kernel.** The run loop is not wired
into `boot.rs` at `VCPUS_RUN`, and the PVH loader is unwritten. That is
the next work and it is what stands between here and priority 1.

### The kernel to boot is built

`linux-7.1.13` (an exact match for the host kernel) with `CONFIG_PVH=y`,
built to an uncompressed `vmlinux` because the PVH note lives in a
`PT_NOTE` segment a bzImage does not carry. Verified:

```
Xen  0x00000008  note type 0x12    (18 = XEN_ELFNOTE_PHYS32_ENTRY)
  description data: 70 a5 5e 03 00 00 00 00      -> 0x035ea570
nm vmlinux | grep pvh_start_xen
  ffffffff835ea570 T pvh_start_xen                -> 0x035ea570  (match)
```

Load map the loader has to honour:

| segment | file offset | `p_paddr` | filesz | memsz |
|---|---|---|---|---|
| LOAD 1 | 0x200000 | 0x01000000 | 0x1d01994 | 0x1d01994 |
| LOAD 2 | 0x2000000 | 0x02e00000 | 0xaf7000 | **0xbc5000** |

Two traps recorded in [Revision D](docs/spec-revision-D-boot-and-platform.md):
`e_entry` (0x35ea950) is **not** the PVH entry (0x35ea570), and LOAD 2's
`memsz > filesz` means 0xCE000 of `.bss` the loader must zero itself.

Build script: `scratchpad/kernel/build.sh` (scratch dirs do not survive;
re-run it).

### What the research settled

Nine threads across Firecracker, Cloud Hypervisor, crosvm, libkrun,
kvmtool, bhyve, Xen and rust-vmm. Written up as
[Revision D](docs/spec-revision-D-boot-and-platform.md); the findings that
changed the plan:

* **No surveyed VMM boots stock OVMF**, and bhyve — the one that tried
  hardest — forked edk2 and hardcoded its VMM's memory map into the
  firmware. PVH is ~150 lines and needs only the 16550 that exists.
* **Enter in 32-bit flat protected mode with no page tables.** Cloud
  Hypervisor never enters long mode at all (`cr0 = PE`, `cr4 = 0`, EFER
  and CR3 untouched). Firecracker's 64-bit path costs a 4-level paging
  bootstrap for nothing.
* **D.2, needs a decision:** Q35 MCH / ICH9 LPC should become a bare PCIe
  root complex. ICH9's LPC bridge is exactly the PIC/PIT/RTC/SCI surface
  the no-legacy rule exists to delete.
* **D.4:** `hvm_start_info` has no `screen_info`, and on Fedora
  `DRM_SIMPLEDRM`/`SYSFB_SIMPLEFB` are built in while `DRM_BOCHS`,
  `DRM_VIRTIO_GPU` and `VIRTIO_INPUT` are all modules. So priority 2 will
  need the bzImage/zero-page loader too, or a guest that can load a
  module. Both loaders share the same entry state.
* **Input is virtio-input, ~400-700 lines**, not xHCI (~4100). bhyve never
  wrote a USB keyboard at all — its keyboard is a PS/2 i8042, which this
  machine cannot have.

### Corrections the research got wrong for this machine

Worth keeping, because both were stated confidently:

* `set_tss_address`/`set_identity_map_address` were called "my first
  suspect for an OVMF that won't get off the reset vector". They are VMX
  fixups for real mode without `unrestricted_guest`; **this host is AMD**,
  where SVM runs real mode natively. Harmless to set, but not a blocker.
* `KVM_MAX_CPUID_ENTRIES = 80` in kvm-bindings 0.9.1 was called a latent
  `E2BIG`. Measured on this host: **67 needed, 80 available.** Real, but
  not what is in the way.

### Known gaps in the run loop, ranked

1. `kvm-ioctls` 0.18 → 0.25 and `kvm-bindings` 0.9.1 → 0.14 (needs
   `rust-version` 1.85; rustc here is 1.98.1, and the workspace has no
   `vmm_sys_util` references, so the bump is free). The reason that
   matters is `set_gsi_routing`, which takes a raw flexible-array struct
   in 0.18 and the checked `KvmIrqRouting` wrapper from 0.21 — GSI
   routing is unwritten, so write it once against the safe API.
2. `KVM_CAP_EXIT_ON_EMULATION_FAILURE` is **1 on this host** and
   kvm-ioctls discards its payload. Decoding
   `kvm_run.emulation_failure` by hand is the difference between "it
   broke" and "it broke on these instruction bytes at this RIP". Do this
   *before* debugging the PVH boot, not after.
3. No `Cap::ReadonlyMem` check before `KVM_MEM_READONLY` is set.
4. `set_mp_state` for APs, and the boot MSR set, are still unwritten.

---

## The §7 media-plane refactor is done

All eight findings of
[docs/design-review-media-plane.md](docs/design-review-media-plane.md) are
closed; that document's "Disposition" table says where each one went. The
plane now is what §1.2 and §7.1 describe:

```
 virtio-gpu scanout ─┐
                     ├─► media-capture (1) ─► handoff ─► media-encode (1)
 virtio-snd PCM   ───┘                                        │
                                             StreamUnits, broadcast
                        ┌────────────────────┬────────────────┴──────┐
                   rtsp-session 1       rtsp-session 2          rtsp-session n
                   frame + write        frame + write           frame + write
```

New code: `crates/libvmm-media/src/plane.rs`. `server.rs` was rewritten to
consume it and now only accepts sockets and runs sessions.
`boot.rs::open_media()` is gone.

**Verified live, not just compiled.** Two `vmm-console-client` processes
connected concurrently to `custom-vmm` over TLS 1.3 and both decoded a real
picture — 288 and 169 frames at ~29 fps — from **one** `av1_vaapi` context.
Session 1 negotiated, session 2 was logged as inheriting, and the binding
released on the last teardown. Reproduce it with the recipe at the foot of
this file.

Two specification revisions were written as part of it:

* **[Amendment B.1](docs/spec-revision-B-media-codecs.md)** gained a
  *Consequences for §1.5* section: DEVICE_INIT can no longer prove the
  negotiated encoder opens, because no codec is chosen until the first
  DESCRIBE. It now proves the machine can encode *something* and fails 5001
  if not.
* **[Revision C](docs/spec-revision-C-rtsp-session-state.md)** is new, and
  closes Finding 7. C.1 widens §7.2's SETUP arm to `INIT|READY` (one SETUP
  per media section). C.2 restates PLAY/PAUSE/TEARDOWN: they act on the
  session's subscription, not on an encoder, which is why `Action`'s
  variants were renamed `StartStreaming`/`PauseStreaming`/`ReleaseSession`.

---

## Next step — a root filesystem, and wire it into `boot.rs`

The kernel boots and then panics for want of somewhere to mount. Two
things close that, in this order:

1. **`boot.rs` at `VCPUS_RUN`** should call `load_pvh_kernel` +
   `configure_pvh_entry` + `vcpu::spawn`, so `custom-vmm` boots a machine
   rather than a test doing it.
2. **virtio-blk over MSI-X**, which needs the three pieces named above:
   MMIO exits dispatched to devices, ioeventfd doorbells, irqfd injection.
   An initramfs is the cheaper intermediate — `BootInfo::initramfs` is
   already implemented and tested, and needs only a file to point at.

What exists underneath is done and tested: the memory map, config-space
emulation in `PciBus::config_rw`, the virtqueue engine, the GSI table, the
device model, the run loop, and a media plane that will take a scanout the
moment there is one. Still missing beyond the loader: `KVM_EXIT_MMIO`
dispatch to devices, ioeventfd doorbell binding and irqfd injection.

The seam into §7 is already cut: `boot.rs` hands
`console_source::DemoScanout` to `MediaPlane::start` as a
`libvmm_media::plane::CaptureSource`. Substituting the virtio-gpu scanout is
a change to that one call, which is what the trait is for. Delete
`console_source.rs` when it happens.

## Also outstanding

* **The keyframe wait.** A session joining mid-GOP waits up to §7.1's two
  seconds for its first picture, because Revision C.2 forbids it from
  forcing a keyframe on everyone else's behalf. Right default; if joins turn
  out to be frequent, an on-demand IDR request is a *specification* question
  first, not a quiet addition.
* **Known defect, pre-existing**: `vmm-console-client`'s `transport.rs` is
  shared by the WSS, RTSP and USB/IP clients but raises `ControlError::
  Connect` (6005) unconditionally, so an unreachable *RTSP* endpoint reports
  in the Control domain. Still reproducible; recorded in
  IMPLEMENTATION-STATUS.
* **The web client** the user wants is gated on a live VM. The RTSP half of
  that gate is now cleared — what remains is that the picture is a test
  pattern rather than a guest. See IMPLEMENTATION-STATUS, "A browser-based
  console client".

---

## Reproducing the live session

```sh
export PATH="$HOME/.cargo/bin:$PATH"
# .cargo/config.toml points VMM_SYSROOT at a scratch directory. If it is
# gone — they do not survive — regenerate with:
#   ./scripts/setup-local-sysroot.sh <some scratch dir>

# A runnable machine: the reference config with small non-huge memory, one
# io_uring drive, and paths under a scratch directory.
cargo run -q -p custom-vmm -- --config <scratch>/run/machine.toml --run-for 26 &
sleep 8
cargo run -q -p vmm-console-client -- console --addr '[::1]:8554' --insecure \
    --no-display --snapshot /tmp/a.ppm --seconds 10 &
sleep 3
cargo run -q -p vmm-console-client -- console --addr '[::1]:8554' --insecure \
    --no-display --snapshot /tmp/b.ppm --seconds 6 &
wait
```

Both clients must decode frames, and the VMM log must show `session 2
inherits` — not a second `negotiated`. Two `negotiated` lines would mean two
encoders, which is the defect Amendment B.1 exists to prevent.

## Ground rules

From [AGENTS.md](AGENTS.md), and the reason the previous checkpoint existed:

**Work from the specification, not from the symptom in front of you.** The
refactor now finished was triggered by a run of reactive fixes — SETUP
state, transport echo, a blocking read, a swallowed flush, hardcoded
depacketisers — each defensible alone and collectively the wrong
architecture. When something fails, check what the numbered section requires
before changing code, and when the specification is wrong, write the
revision down. Revision C is what that looks like.
