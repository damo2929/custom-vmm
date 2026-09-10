# Checkpoint — 2026-09-10

Resume point. Written to be read cold: it assumes only
[README.md](README.md) and [AGENTS.md](AGENTS.md).

**Tree state: green.** `cargo build --workspace` clean, `cargo clippy
--workspace --all-targets` zero warnings, **465 tests passing**. Built with
`-C target-cpu=x86-64-v3` (checked in at `.cargo/config.toml`; see
[HOST-REQUIREMENTS.md](HOST-REQUIREMENTS.md)).

---

## There is a linear framebuffer now, and a real installer draws on it

The live command:

```sh
cargo build --release --example install_boot
RUST_LOG=info ./target/release/examples/install_boot firmware/CLOUDHV.fd \
  /home/damien/Downloads/proxmox-ve_9.2-1.iso 60 /tmp/shot.ppm
```

That produces the Proxmox VE 9.2 installer's GRUB menu in colour at
1024x768 — the guest set the mode itself through the VBE dispi registers and
painted straight into the framebuffer, and the host read it back out of the
mapping. `RUST_LOG=info,libvmm_core::vcpu=trace` logs every port and MMIO
exit with its data, which is the instrument that found everything below.

**The device** is `crates/libvmm-core/src/display.rs`, Revision D.10: the
Bochs VBE display at `1234:1111`, bound by edk2's stock `QemuVideoDxe`.
BAR 0 is the framebuffer and is a **KVM memory slot**, not a trapping MMIO
region — an exit per pixel is not a display. The slot follows the BAR and is
published only while the command register's memory-space bit is set.

`VirtioGpuDxe` is deliberately **not** in the firmware any more. Its GOP is
`PixelBltOnly` with no `FrameBufferBase`, so it stops working at
`ExitBootServices`; and with both drivers present the firmware publishes two
graphics protocols and the guest picks one. virtio-gpu is still on the bus
for guests that drive it themselves.

## Windows 11 boots off our DVD and then stops before it draws

**Where it gets to.** BDS loads and starts `bootmgfw.efi` from
`PciRoot(0x0)/Pci(0x2,0x0)/Scsi(0x0,0x0)`, Windows reads **516 MiB** off the
DVD through our virtio-scsi — that is `boot.wim` going into a RAM disk,
which is why no Windows storage driver is needed afterwards — calls
`ExitBootServices`, programs all 24 I/O APIC redirection entries, re-walks
ECAM, and then does nothing but poll the ACPI timer at `0x0608`. It never
paints, on either surface.

**Not the display.** The same firmware and the same framebuffer render the
Proxmox installer above. Windows reaches `ExitBootServices` and stops for
another reason.

**What it is doing while it does nothing.** `RUST_LOG=libvmm_core::vcpu=trace`
now logs the instruction pointer at every exit. Windows sits at
`rip 0xfffff802bdb239d5` — kernel space — in a four-instruction loop reading
the ACPI timer. The timer itself is fine: the values are monotonic and
advance about 34 ticks per read, which is 9.5 µs at 3.579545 MHz, the cost
of the exit. So the guest is running, has a working clock, and is waiting
for something that never comes.

**The FADT was wrong, has been fixed (D.11), and was not the cause.** It
declared itself not hardware-reduced and then supplied none of what that
implies: no FACS, no PM timer, a `PM1a_EVT_BLK` at `0x05FC` that nothing
decoded, a `PM1a_CNT_BLK` pointing at `0x0600` — which on this platform is
`SLEEP_CONTROL_REG` and means something else — and `RESET_REG_SUP` set with
nothing decoding `0x0CF9`. It now says `HW_REDUCED_ACPI | RESET_REG_SUP |
TMR_VAL_EXT` with the sleep pair, a 32-bit `X_PM_TMR_BLK`, and PM1 blocks at
`0x060C`/`0x0610` that `CloudHvPm` answers, following Cloud Hypervisor,
which boots Windows on this same firmware. Windows behaves identically.

**What has not been tried yet**, roughly in order of what a Windows bugcheck
this early usually means:

1. **CPUID.** Windows 11 refuses a processor missing what it requires and
   bugchecks `0x5D UNSUPPORTED_PROCESSOR` before it has a screen. Check what
   `KVM_SET_CPUID2` actually leaves the guest — NX, SSE4.2, POPCNT,
   CMPXCHG16B, PrefetchW, LAHF/SAHF in long mode, and the invariant-TSC bit
   in `0x8000_0007`.
2. **The bugcheck is invisible, and that is fixable.** Windows paints a
   bugcheck through bootvid into the framebuffer it was given. Nothing has
   painted, so either it crashed before display init or it never got a
   framebuffer. Confirm which by checking whether `winload` was handed a
   `PixelBlueGreenRedReserved8BitPerColor` GOP at all.
3. **Windows' own debug output.** The install media's BCD can be edited on a
   writable copy of the ISO to turn on `bootdebug`/`bootlog`, which would
   say what it is waiting for rather than leaving it to inference.

**Fixed on the way here**, all with regression tests:

* **The RTC answered in the wrong encoding.** `PcRtcInit` writes register B
  with `Dm = 0` — BCD — and `Cmos::time_register` kept answering in binary,
  so Windows read 2020-09-10 12:10:35 instead of 2026-09-10 18:16:53 and
  `BlInitializeLibrary` failed with `0xc0000185`, which is
  `STATUS_IO_DEVICE_ERROR`, which is what bootlib maps `EFI_DEVICE_ERROR`
  to. That string is in `efi/boot/bootx64.efi` on the ISO as UTF-16 and
  nowhere in `CLOUDHV.fd`, so it was Windows' own loader saying it.
* **A BAR's type bits are read-only and we were letting the guest clear
  them.** Firmware writes the bare address — `reg 0x10 <- 0x00008000` — and
  storing that verbatim turned a 64-bit BAR into something that read back as
  32-bit. The following write to `0x14` was then attributed to a BAR that
  does not exist, and the device never learned it had moved to
  `0x1_0000_8000`. Symptom: `mmio 0x0100008014 x64` unanswered, and a
  virtio-scsi the firmware enumerates and will not talk to.
* **The upper half of a 64-bit BAR sized as an unimplemented BAR**, reading
  back zero instead of the top of the mask, which for any region under 4 GiB
  is all-ones.

**What is proven working on the storage path**, and should not be re-tested
from scratch:

* Booting from optical media. The UEFI Shell ISO reaches an interactive
  `Shell>` at `PciRoot(0x0)/Pci(0x2,0x0)/Scsi(0x0,0x0)/CDROM(0x0)`, and
  Proxmox VE reaches its installer menu.
* One virtio-scsi HBA per drive, a request queue per vCPU, a worker thread
  per queue named `{controller}-q{k}`.
* CD/DVD/BD-ROM media (peripheral type 0x05, 2048-byte blocks, the MMC
  command set, writes refused with DATA PROTECT) and SSD media (type 0x00,
  VPD 0xB1 rotation rate 1, TRIM/UNMAP).
* A megabyte read spanning many descriptors, byte-compared against the ISO.

**One number worth knowing.** A guest sitting at a boot menu takes about
200,000 exits a second, essentially all of them reads of `0x03FD`, the
16550 line-status register: GRUB polling for a keystroke that cannot arrive,
because there is no input device. That is priority 3 arriving early.

## The machine has no chipset, and UEFI boots on it anyway

**Revision D.2 is decided: option B.** The machine is a PCIe root complex and
nothing else — one PCI function at 00:00.0 with the CloudHv host bridge ID
`8086:0d57`, two fixed I/O registers, and no LPC bridge, no PMBASE, no
`fw_cfg`, no A20 gate, no PIC and no PIT.

```sh
scripts/build-cloudhv-firmware.sh          # once; no root needed
cargo run --release -p custom-vmm --example cloudhv_boot -- firmware/CLOUDHV.fd
```

A `DEBUG` build of that firmware confirms which path it took, in its own
words, and then enumerates our bus and reaches BDS:

```text
PlatformMiscInitialization: Cloud Hypervisor is done.
PciHostBridgeUtilityInitRootBridge: populated root bus 0, with room for 255 subordinate bus(es)
PciBus: Discovered PCI @ [00|00|00]  [VID = 0x8086, DID = 0x0D57]
  Boot0000: BootManagerMenuApp
  Boot0001: EFI Firmware Setup
  Boot0002: EFI Internal Shell
BdsDxe: No bootable option or device was found.
```

The entire unhandled-access report for a complete UEFI boot is **two
entries** — `port 0x0021 x1` and `port 0x00a1 x1`, the 8259 masks, written
once by a firmware masking a PIC that is not there. That is the whole cost of
having no chipset, and it is *fewer* unanswered accesses than the Q35 path.

Nothing was added to the VMM to feed this firmware. `CLOUDHV.fd` is an
**ELF** — `OvmfPkg/CloudHv` builds with `OvmfPkg/XenResetVector` and carries
an `XEN_ELFNOTE_PHYS32_ENTRY` note — so it loads through the *same PVH
loader that loads a Linux kernel*, unchanged, and then takes its memory map
from `hvm_start_info.memmap_paddr` and its ACPI tables from the XSDT behind
`rsdp_paddr`. Both are structures this tree already built.

Written up as [Revision D.9](docs/spec-revision-D-boot-and-platform.md).
Three known issues are recorded there, none fatal:

* a non-fatal `EFI_MEMORY_UC` complaint from `PciHostBridgeDxe`;
* edk2's CloudHv 32-bit aperture overlapping the D.5 ECAM window — harmless
  at the current device count, and the fix is to shrink the aperture, not to
  move ECAM;
* **SMBIOS is not presented, and `CLOUDHV_SMBIOS_ADDRESS` is `0x000F_0000` —
  the same address as §3.3's ACPI staging area.** Harmless today (the driver
  correctly reports `Not Found`), but §3.2's SMBIOS builder cannot be staged
  there without moving the ACPI tables first, and doing it anyway would
  corrupt them silently.

### Regenerating the firmware

No distribution ships `CLOUDHV.fd`, so it is built. `scripts/build-cloudhv-firmware.sh`
needs no root: where `nasm` and `iasl` are missing it downloads the RPMs as
an ordinary user and unpacks them into its own work directory. It pins the
edk2 revision, verifies the output is an ELF before installing it, and takes
under a minute on a warm tree. See [firmware/README.md](firmware/README.md).
The `.fd` is a build artifact and is not tracked; the recipe is.

---

## Stock Fedora OVMF also boots to the UEFI Boot Manager

This is **Revision D.2 option A**, kept working as the alternative to the
decision above: the distribution's own firmware, unmodified. Not a build of
ours, not a patched image — `/usr/share/edk2/ovmf/OVMF_CODE.fd` as `dnf`
installed it. It needs the ICH9 LPC stub (D.6) that option B does without.

```sh
cargo run --release -p custom-vmm --example ovmf_boot -- \
    /usr/share/edk2/ovmf/OVMF_CODE.fd /usr/share/edk2/ovmf/OVMF_VARS.fd 25
```

It reaches the end of BDS and says so:

```text
[Bds] Expand \EFI\BOOT\BOOTX64.EFI -> <null string>
[Bds] Unable to boot!
BdsDxe: No bootable option or device was found.
BdsDxe: Press any key to enter the Boot Manager Menu.
```

That is the correct ending for a machine with no disk. Everything before it
— SEC, PEI, the DXE dispatcher, ~40 drivers, the variable store, the GCD,
MpInitLib, the serial and RTC drivers, BDS — ran.

What nothing answered, at the end of a full boot:

```text
port 0x02ff x54     port 0x0064 x22    port 0x0060 x11
port 0x0092 x2      port 0x0021 x1     port 0x00a1 x1
mmio 0x00ffe00010 x2   mmio 0x00ffe20010 x2
```

0x60/0x64 is the i8042; §1.4 forbids it and virtio-input replaces it.
0x21/0xa1 is the 8259 mask, written once and never read. The two MMIO
addresses are writes into the flash variable store, which is mapped
read-only — the reason the boot order is not persisted between runs.

### Four things had to be right, and each was found by running it

Written up in full as [Revision D.5–D.8](docs/spec-revision-D-boot-and-platform.md);
in short:

1. **ECAM moved to `0xE000_0000`** (D.5). edk2 does not read the MMCONFIG
   base from the host bridge, it *writes* `PcdPciExpressBaseAddress` —
   fixed at `0xE0000000` in every Q35 build — and proceeds. The reference
   host's own firmware puts its ECAM at exactly the same address, so
   mirroring the live machine and satisfying the firmware are one change.
   Symptom before the fix: PEI fine (it uses `0xCF8`/`0xCFC`), then DXE
   switched to ECAM, read zeros, computed an ACPI timer at port `0x0008`,
   and spun — **4,514,549 unanswered reads in twenty seconds, no message.**
2. **An ICH9 LPC bridge at 00:1f.0** (D.6), in `libvmm-core/src/ich9.rs`.
   `AcpiTimerLibConstructor` reads `PMBASE` and `ACPI_CNTL` from that
   function before the firmware emits a single line; absent, both read
   all-ones and it asserts on a misaligned timer port. This is a register
   file with a device ID — no LPC bus, no PIC, no PIT, no INTx.
3. **CMOS registers C and D are read-only** (D.8). `PcRtcInit` opens by
   writing register D = `0x00`, VRT included; a CMOS that stores that
   answers the next read with VRT clear and the firmware declares the RTC
   dead (`ASSERT PcRtcEntry.c(259)`). On real silicon that bit is driven
   by the battery sense circuit and the write does nothing.
4. **The RTC reads the clock KVM gives the guest** (D.7). `KVM_GET_CLOCK`
   with `KVM_CLOCK_REALTIME` is the same `ktime_get_snapshot()` pairing
   `ptp_kvm` answers `KVM_HC_CLOCK_PAIRING` from, so the guest's RTC and
   its PTP source cannot drift apart. Confirmed live:

   ```text
   INFO libvmm_core::kvm::live] RTC clock source: KVM_GET_CLOCK realtime
       — the same ktime_get_snapshot() pairing the guest's ptp_kvm reads
   ```

   The fallback matters: KVM publishes no realtime pairing until its master
   clock is up, so a probe at bring-up always reports the fallback. The
   source is announced on first use and on change, never at construction.

### What is left before an OS boots

* **A disk.** virtio-blk on the transport in `libvmm-virtio/src/transport.rs`.
  This is now the single thing between here and installing an OS.
* **A writable variable store**, so the boot order survives a reboot.
* **virtio-input** (keyboard, tablet) — priority 3, and what makes the boot
  manager usable rather than merely visible.

### The host bridge identity — decided

This section previously left the question open. It is now **option B**; the
reasoning and the evidence are in D.9, and what follows is kept because the
three options are still the right frame for revisiting it.


Revision D.2 is now a three-way question, restated at the end of that
document with the evidence. Briefly: 45 files in `OvmfPkg` branch on the
host bridge device ID and **nine of them `ASSERT(FALSE)` on one they do not
recognise**. Stock OVMF knows four: i440FX (excluded by §1.4), Q35 MCH,
CloudHv and bhyve. **A** — Q35 MCH plus the D.6 LPC stub — boots, and is what
this section describes. **B** — the CloudHv identity `8086:0d57`, no chipset
at all — is **chosen, implemented and booting**; see the top of this file.
**C** — the reference host's real root complex, AMD Krackan `1022:1122` —
needs either our own firmware or nine patched edk2 sites, and is not
currently pursued.

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
PCI: ECAM [mem 0xe0000000-0xefffffff] reserved as E820 entry
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
