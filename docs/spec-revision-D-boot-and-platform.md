# Revision D — the boot entry and the platform identity

**Date:** 2026-09-10 · **Status:** proposed, D.2 needs a decision

Written after reading Firecracker, Cloud Hypervisor, crosvm, libkrun, kvmtool,
bhyve and Xen against §3. Four numbered requirements are wrong or
under-specified, and one of them is wrong in a way that would have cost
weeks. Each clause below says what §3 says, what the sources say, and what
the evidence is.

---

## D.1 — §3.1's reset vector is replaced by a PVH ELF entry

**§3.1 says** the firmware is an image at `0xFFC0_0000` entered through the
x86 reset vector at `0xFFFF_FFF0` in 16-bit real mode.

**Revised:** the machine's first guest image is a **PVH ELF**, loaded at the
`p_paddr` of its `PT_LOAD` segments and entered in **32-bit flat protected
mode** at the address in its `XEN_ELFNOTE_PHYS32_ENTRY` note. There is no
reset vector and no real mode in this path.

Entry state, from the PVH ABI:

| | value |
|---|---|
| `cr0` | `PE` set, every other writeable bit clear |
| `cr4` | 0 |
| `EFER`, `cr3` | untouched — **no paging, no page tables** |
| `cs` | 32-bit read/execute, base 0, limit `0xFFFFFFFF` |
| `ds/es/ss` | 32-bit read/write, base 0, limit `0xFFFFFFFF` |
| `tr` | 32-bit TSS (active), base 0, limit `0x67` |
| `ebx` | physical address of `hvm_start_info` |
| `rflags` | `0x2` — bit 1 is reserved-and-always-set |

Fixed addresses, identical across Cloud Hypervisor and Firecracker:
GDT `0x500` (limit 31), IDT `0x520` (limit 7), `hvm_start_info` `0x6000`,
memory map `0x7000`, cmdline `0x20000`. The GDT is four entries:
`[null, 0xc09b/0/0xffffffff, 0xc093/0/0xffffffff, 0x008b/0/0x67]`.

**Why this and not stock OVMF.** No surveyed VMM boots an unmodified
OVMF. Stock OVMF learns its memory map, CPU count and ACPI tables from
QEMU's `fw_cfg` and expects an i440FX or Q35 host bridge; bhyve's answer was
to fork edk2 into `OvmfPkg/Bhyve` with its own `PlatformPei`, and even then
its `pci_emul.c` carries the comment *"OVMF always uses 0xc0000000 as base
address for 32 bit PCI MMIO. Don't change this address without changing it
in OVMF."* The VMM and its firmware are one artefact, not two. Building that
is 3–6 weeks of chipset we have said we do not want.

PVH costs ~150 lines and needs only the 16550 that already exists. It is
also not a detour: Cloud Hypervisor's `--firmware` and `--kernel` are the
same loader, because `CLOUDHV.fd` is itself a PVH ELF. The UEFI path, when
we want it, is this code plus a firmware build.

**Two traps, both verified against the kernel built for this machine
(linux-7.1.13, `CONFIG_PVH=y`):**

* **`e_entry` is not the PVH entry.** The ELF header says `0x35ea950`; the
  note says `0x35ea570`, which is `pvh_start_xen`. A loader that trusts
  `e_entry` lands 0x3E0 bytes into the wrong place and triple-faults with
  no diagnostic.
* **`p_memsz > p_filesz`.** The second `PT_LOAD` is `0xaf7000` on disk and
  `0xbc5000` in memory. The loader must zero the `0xCE000` difference —
  that is `.bss`, and a kernel whose `.bss` holds stale bytes fails
  arbitrarily far from the cause.

## D.2 — the host bridge is a bare PCIe root complex, not a Q35 MCH

**§3 presents** a Q35 MCH (`8086:29C0`) with an ICH9 LPC bridge
(`8086:2918`).

**Proposed:** a host bridge at 00:00.0 with class `06/00/00`, header type 0,
no capabilities beyond PCIe, and **no chipset behaviour behind it**.

The standing constraint is that this machine presents nothing legacy. ICH9's
LPC bridge is *precisely* the component that carries the PIC, PIT, RTC,
SMBus and SCI — the surface that constraint exists to delete. Presenting an
ICH9 and then not implementing it is the contradiction; a bare root complex
is the same intent stated honestly.

Nobody emulates a real chipset. Cloud Hypervisor and Firecracker's PCIe mode
both use `8086:0d57`; bhyve uses `0x1275:0x1275` (a NetApp ID for a device
NetApp never made) with an alternate `0x1022:0x7432` annotated **"made up"**
in the source. crosvm uses `8086:1237` — the i440FX identity with none of
the i440FX behaviour, which is worth noting because it shows the constraint
was always about behaviour rather than a number.

**One concrete hazard decides this.** Linux's
`pci_mmcfg_check_hostbridge()` matches 00:00.0 against a hardcoded list
(`arch/x86/pci/mmconfig-shared.c`) — E7520 MCH, `8086:2770`, AMD fam10h at
00:18.0 — and on a match reads a *chipset register we do not emulate* to
find the ECAM base. `8086:29C0` is not on that list, so Q35 is not actively
dangerous; but the list is the reason to choose an identity deliberately
rather than inherit one.

**This clause is a proposal, not a decision.** The alternative — keeping the
Q35 device IDs and recording "LPC bridge deliberately omitted" — is the same
machine with the specification left intact.

## D.3 — ACPI reaches the guest through `hvm_start_info`, not a memory scan

**§3.3 says** the RSDP and tables are staged in low memory for the firmware
to find.

**Revised:** under PVH the guest is handed `hvm_start_info.rsdp_paddr`
directly. Nothing scans. The §3.3 staging area is never consulted on this
path and the requirement should say so, because a table set that is written
but unreachable looks exactly like a table set that is wrong.

For the record, stock OVMF would not have found it either: it takes ACPI
only from `fw_cfg`'s `etc/table-loader`. The low-memory scan is bhyve's
mechanism (`BHYVE_ACPI_BASE 0xf2400`), and it works only because bhyve's
firmware is bhyve's.

## D.4 — the boot path and the framebuffer are coupled

This is not in §3 at all, and it needs to be, because it constrains the
order of the next two priorities.

**`hvm_start_info` has no `screen_info`.** Its fields are `magic`,
`version`, `flags`, `nr_modules`, `modlist_paddr`, `cmdline_paddr`,
`rsdp_paddr`, `memmap_paddr`, `memmap_entries` — and nothing else. A PVH
boot therefore cannot hand the guest a firmware framebuffer. The bzImage
zero page can, via `boot_params.screen_info`.

Measured on the actual target, Fedora's `7.1.13-200.fc44.x86_64`:

| driver | Fedora | our test kernel |
|---|---|---|
| `DRM_SIMPLEDRM` | **`=y`** | not set |
| `SYSFB_SIMPLEFB` | **`=y`** | not set |
| `DRM_BOCHS` | `=m` | not set |
| `DRM_VIRTIO_GPU` | `=m` | `=y` |
| `VIRTIO_INPUT` | `=m` | `=y` |

So for a **stock guest with no initramfs**, the only display that appears is
`simpledrm` over `screen_info` — everything else is a module that has to be
loaded first. That makes the ordering explicit:

* **Priority 1 (a VM boots)** — PVH, serial console. No framebuffer needed,
  no zero page, nothing above. This is the current target.
* **Priority 2 (a picture)** — needs *either* the bzImage/zero-page loader
  with `screen_info`, *or* a guest that can load a module. Both loaders
  share D.1's entry state exactly; the increment is the zero page, not a
  second boot architecture. That is why D.1 specifies the register state
  rather than the boot protocol.

There are two candidates for the framebuffer itself and they fail in
opposite directions:

* **`bochs-display`** (`1234:1111`, class `0x0380`, LFB in BAR0, DISPI
  registers at BAR2+0x500, ID register returning `0xB0C5`) has **no
  handoff gap** — stock `QemuVideoDxe` drives it, and it is accepted with
  class `DISPLAY_OTHER`, so no VGA BAR and no option ROM are needed. But
  the VMM learns nothing about damage and must poll a dirty log.
* **virtio-gpu 2D** pushes a damage rectangle with every `RESOURCE_FLUSH`
  and sends *nothing at all* when the guest is idle, which is exactly what
  the encoder wants. But `VirtioGpuDxe` publishes `PixelBltOnly`, so it has
  no linear framebuffer address; Linux's EFI stub skips such a GOP
  (`libstub/gop.c`), and the screen is blank from `ExitBootServices` until
  `drm/virtio` loads.

Note for §7.1, which currently says `PackedFormat::Bgra` is the
`B8G8R8A8_UNORM` scanout: the **scanout** format Linux asks for is
`B8G8R8X8_UNORM` (2). `B8G8R8A8_UNORM` (1) is the *cursor* format. The
memory layout is the same four bytes — B, G, R, then padding — so the
encoder input is unaffected, but the text names the wrong constant.

---

## Consequences

* §3.1's reset-vector text is superseded for the PVH path. It stays valid
  as a description of what a firmware boot would need, and is where the
  UEFI work resumes.
* §3.3's staging area is not on the PVH path.
* The `SLOT_OVMF_CODE` mapping is still where a firmware image goes, but
  the PVH loader writes to RAM at `p_paddr` instead, so `load_firmware`'s
  end-of-region placement is not the mechanism here.
* D.2 needs a yes or no before the host bridge is built for real.

---

# D.5 — the ECAM base moves to `0xE000_0000`

**§2.1 said** ECAM/MMCONFIG occupies 256 MiB at `0xC000_0000`, the bottom of
the §1.3 MMIO hole, with the PCIe BAR window above it at `0xD000_0000`.

**It now reads:** ECAM occupies 256 MiB at `0xE000_0000`, and the 32-bit BAR
window is the space below it, `0xC000_0000`–`0xDFFF_FFFF`.

Two independent authorities give the same address, and neither of them is
negotiable from this side.

**The firmware.** Every edk2 Q35 build fixes
`gEfiMdePkgTokenSpaceGuid.PcdPciExpressBaseAddress` at `0xE0000000`
(`OvmfPkg/OvmfPkgX64.dsc`). `PlatformInitLib` does not *read* a base from the
host bridge — it **writes** that constant into `PCIEXBAR` and proceeds. Its
own map, from `OvmfPkg/Library/PlatformInitLib/Platform.c`, is:

```text
  max(top, 2g)  PCI MMIO  0xE0000000 - max(top, 2g)  (q35)
  0xE0000000    MMCONFIG                     256 MB  (q35)
  0xFC000000    gap                           44 MB
  0xFEC00000    IO-APIC                        4 KB
  0xFEE00000    LAPIC                          1 MB
```

**The host.** The reference machine's own firmware reports

```text
PCI: ECAM [mem 0xe0000000-0xefffffff] (base 0xe0000000) for domain 0000 [bus 00-ff]
```

— the same base, the same size, the same bus range. Mirroring the live system
and satisfying the firmware turn out to be the same requirement.

The 32-bit BAR window follows from edk2's aperture, `PciExBarBase -
Uc32Base`. With low RAM filled to the bottom of the hole, `Uc32Base` is
`0xC000_0000` and the aperture is exactly `0xC000_0000`–`0xDFFF_FFFF` — the
space ECAM vacated, and 512 MiB rather than the 236 MiB §2.1 gave it.

**How this was found.** With ECAM at `0xC000_0000`, PEI worked — it uses
`BasePciLibCf8`, and the `0xCF8`/`0xCFC` path was answered correctly — and
then DXE switched to `DxePciLibI440FxQ35`, went to ECAM, and read from an
address nothing decoded. The configuration read returned zero, so
`AcpiTimerLibConstructor` computed an ACPI timer at `(0 & ~1) + 8` = port
`0x0008` and spun there: **4,514,549 unanswered reads in twenty seconds**,
with no error message of any kind. The only visible symptom was a firmware
that had stopped, and the only evidence was the unhandled-access report
naming `mmio 0x00e00f8040` — the LPC bridge's `PMBASE`, at the base edk2
expected.

---

# D.6 — an ICH9 LPC bridge at 00:1f.0

**§1.4 says** no legacy chipset: no PIIX, no PIC, no PIT, no ISA bus, no
INTx. That stands. This adds one function to the bus and changes none of it.

Stock OVMF's `AcpiTimerLibConstructor` runs before the firmware emits a
single line of output. It reads the host bridge's device ID at 00:00.0 and,
for a Q35 MCH, goes straight to **00:1f.0 offset 0x44** (`ICH9_ACPI_CNTL`)
to ask whether the power-management I/O decode is enabled, and to **offset
0x40** (`ICH9_PMBASE`) for the base. With nothing at 00:1f.0 both reads
return all-ones, which the firmware reads as "already enabled, base
`0xFFFFFFFF`", and it computes

```text
ASSERT .../IoLibGcc.c(211): ((Port) & 3) == 0
```

The machine therefore presents a single ICH9 LPC function at 00:1f.0
carrying four configuration registers — `PMBASE`, `ACPI_CNTL`, `RCBA` and
the header — and the ACPI power-management block they point at:
`PM1_STS`/`PM1_EN`/`PM1_CNT`, the 3.579545 MHz ACPI timer, `GPE0` and
`SMI_EN`. Nothing is routed through the bridge; there is no LPC bus behind
it. It is a register file with a device ID, and it exists because the
firmware's first action is to read it.

One consequence is welcome rather than merely tolerable: `PM1_CNT` with
`SLP_EN` and `SLP_TYP = 5` is how an ACPI guest powers itself off, and KVM
never surfaces that as a system event because it is an ordinary `out`. The
machine now has somewhere for that write to land, so an orderly guest
shutdown ends the run instead of hanging it.

---

# D.7 — the RTC reads the clock KVM gives the guest

**§3.2 did not say** where the CMOS RTC gets the time. It took it from the
host's `CLOCK_REALTIME` via `gettimeofday`.

**It now reads:** the RTC takes its time from `KVM_GET_CLOCK` with
`KVM_CLOCK_REALTIME`, falling back to host `CLOCK_REALTIME` where the kernel
does not report one.

A guest running `ptp_kvm` gets its time from the `KVM_HC_CLOCK_PAIRING`
hypercall, which the kernel answers out of `ktime_get_snapshot()` — one
host-realtime reading taken against one TSC reading. `KVM_GET_CLOCK` with
`KVM_CLOCK_REALTIME` resolves to the same call. Reading the RTC from
anywhere else makes the guest's two views of the time two samples of two
clocks, free to drift apart by whatever the host's NTP discipline is doing.
Reading both from one place makes that structurally impossible rather than
merely unlikely.

The fallback is real, not defensive padding, and it is *load-bearing at
bring-up*: KVM only publishes a realtime pairing once its master clock is
up, which needs a vCPU running kvmclock and a TSC host clocksource. Probed
before the first vCPU runs, it always reports nothing. The clock therefore
announces its source on first use and on any change, rather than once at
construction where the answer would always have been the wrong one.

---

# D.8 — CMOS status registers C and D are read-only

`PcRtcInit` opens by writing `PcdInitialValueRtcRegisterD`, which is
**`0x00`** — VRT included. A CMOS that stores what it is given then answers
the firmware's very next read with VRT clear, and `RtcWaitToUpdate` concludes
the RTC has lost its battery:

```text
ASSERT_EFI_ERROR (Status = Device Error)
ASSERT PcRtcEntry.c(259)
```

On an MC146818, register D bit 7 is driven by the battery sense circuit and
a write to it does nothing; register C's interrupt flags are set by the chip
and cleared by *reading*, never by writing; register A's `UIP` bit is driven
by the update cycle. The emulation now matches: C and D ignore writes, C
clears on read, and `UIP` always reads clear — which it must, because the
time registers here are computed at the instant they are read and can never
be caught half-updated.

---

# D.2 — revisited, not yet decided

D.2 asked whether the machine should present a Q35 MCH or a bare PCIe root
complex. Two things learned since narrow it without closing it.

**Stock OVMF recognises exactly four host bridges.** 45 files in `OvmfPkg`
branch on `PcdOvmfHostBridgePciDevId`, and **nine of them `ASSERT(FALSE)`
and return `RETURN_UNSUPPORTED` on anything they do not know** —
`AcpiTimerLib` (three variants), `PlatformInitLib`, `PlatformBootManagerLib`,
`XenPlatformPei`. The four are i440FX (`0x1237`, excluded by §1.4), Q35 MCH
(`0x29C0`), CloudHv (`0x0d57`) and bhyve (`0x1275`). Presenting the reference
host's real root complex — AMD Krackan, `1022:1122` — makes stock OVMF stop
before it prints anything.

**There is a third option neither branch of D.2 anticipated.** `OvmfPkg`
contains a complete CloudHv path: `CLOUDHV_DEVICE_ID = 0x0d57`,
`CLOUDHV_ACPI_TIMER_IO_ADDRESS = 0x0608`, and 21 branches across 11 files
including `PlatformScanE820Pvh` — which takes the memory map from a PVH
`hvm_start_info` rather than `fw_cfg` — and `InstallCloudHvTables`, which
installs ACPI without `fw_cfg`. That firmware needs **no chipset at all**:
no ICH9, no i440FX, no `fw_cfg`. It would make D.6 unnecessary and it fits
the PVH machinery this tree already has.

It is not free. Fedora ships no `CLOUDHV.fd`, so it has to be built from
edk2 (`nasm` and `acpica-tools`), and the resulting firmware is not the one
the distribution tests, patches or signs.

So the question is now three-way, and it is a question about what the
product is rather than about what is implementable:

| | identity at 00:00.0 | firmware | chipset needed |
|---|---|---|---|
| **A** | Q35 MCH `8086:29C0` | the distro's own OVMF, unmodified | ICH9 LPC stub (D.6) |
| **B** | CloudHv `8086:0d57` | built from edk2 by us | none |
| **C** | AMD Krackan `1022:1122` | ours, or a patched edk2 | none, but nine edk2 sites to patch |

**A is implemented and boots. B is now the chosen option, and it also
boots** — see D.9. C remains open and is not currently pursued.

---

# D.9 — decided: option B, the machine with no chipset

**Decision, 2026-09-10:** the machine presents a bare PCIe root complex with
the CloudHv host bridge identity `8086:0d57`, and boots firmware built from
`OvmfPkg/CloudHv/CloudHvX64.dsc`. There is no LPC bridge, no PMBASE, no
`fw_cfg`, no A20 gate, no PIC and no PIT.

This resolves D.2 in the direction §1.4 always pointed. The ICH9 stub of D.6
was a concession to a firmware, and the concession is no longer needed.

## What the machine is

One PCI function, at 00:00.0. Two I/O registers, at fixed addresses that no
guest programs and no chipset carries:

| | address | what it is |
|---|---|---|
| host bridge | 00:00.0 | `8086:0d57`, class 06/00, no capability list |
| `SLEEP_CONTROL_REG` | `0x0600` | `SLP_TYP` in bits 4:2, `SLP_EN` in bit 5 |
| `SLEEP_STATUS_REG` | `0x0601` | write-one-to-clear |
| ACPI PM timer | `0x0608` | 24 bits, 3.579545 MHz |

Plus the 16550 at `0x3F8`, the ECAM window at `0xE000_0000` (D.5) and the
CMOS at `0x70`/`0x71` (D.7, D.8), all of which predate this revision.

The power management is **hardware-reduced ACPI**, not a cut-down ICH9 PM
block, and the distinction is not cosmetic. ACPI 5.0 replaces the PM1 event
and control registers with two byte-wide ones and puts the sleep type in
different bits. edk2 writes `5 << 2 | 1 << 5` to `0x0600`; in an ICH9
`PM1_CNT` those bits mean nothing, and offset 0 of an ICH9 PM block is
`PM1_STS`, which is write-one-to-clear. The same address, the same write, and
an entirely different meaning — so the two are separate device models
(`crate::cloudhv` and `crate::ich9`) and presenting both at once is a hard
assertion failure rather than a subtle misbehaviour.

## The firmware is an ELF

`OvmfPkg/CloudHv` builds with `OvmfPkg/XenResetVector`, which carries an
`XEN_ELFNOTE_PHYS32_ENTRY` note. `CLOUDHV.fd` is therefore an ELF, not a
flash image, and is loaded by **the same PVH loader that loads a Linux
kernel** — `crates/libvmm-core/src/pvh.rs`, unchanged. The firmware is
entered in 32-bit protected mode with `ebx` pointing at an `hvm_start_info`,
exactly as D.1 describes for a kernel.

One detail worth recording because the module header asserted the opposite:
the note's `n_descsz` is **4** here, not 8. Linux's `_ASM_PTR` is `.quad` on
x86_64 so a kernel's is always 8; this firmware is 32-bit at entry and writes
a 32-bit pointer. The loader already handled both.

Everything else the firmware needs, it takes from that same structure:

* `PlatformScanE820Pvh` reads the memory map from `memmap_paddr` /
  `memmap_entries` — not from `fw_cfg`.
* `InstallCloudHvTables` walks the XSDT reached through `rsdp_paddr` and
  installs every table it finds — not `InstallQemuFwCfgTables`.

Both of those are structures §3.3 and D.1 already build. Nothing was added to
the VMM to feed this firmware; it consumes what the PVH path already
produced.

## Evidence

A `DEBUG` build says so in its own words:

```text
PlatformMiscInitialization: Cloud Hypervisor is done.
```

— the early return in the `CLOUDHV_DEVICE_ID` branch, before the A20 write
and the PM base programming that the other platforms perform. Then:

```text
PciHostBridgeUtilityInitRootBridge: populated root bus 0, with room for 255 subordinate bus(es)
RootBridge: PciRoot(0x0)
PciBus: Discovered PCI @ [00|00|00]  [VID = 0x8086, DID = 0x0D57]
...
  Boot0000: BootManagerMenuApp
  Boot0001: EFI Firmware Setup
  Boot0002: EFI Internal Shell
BdsDxe: No bootable option or device was found.
```

The entire unhandled-access report for a complete boot is two entries:

```text
port 0x0021 x1
port 0x00a1 x1
```

— the 8259 mask registers, written once and never read, by a firmware
masking a PIC that is not there. That is the whole cost of having no
chipset, and it is fewer unanswered accesses than the Q35 path incurs.

## Known issues

* `PciHostBridge driver failed to set EFI_MEMORY_UC to MMIO aperture - Out
  of Resources.` Non-fatal — resource allocation succeeds and the boot
  completes — but it is a real message and not yet explained.
* edk2's CloudHv 32-bit aperture is `CLOUDHV_MMIO_HOLE_ADDRESS` `0xc0000000`
  for `CLOUDHV_MMIO_HOLE_SIZE` `0x38000000`, which **overlaps the D.5 ECAM
  window at `0xE000_0000`**. Cloud Hypervisor's own layout puts its device
  window at `0xC000_0000..0xDFFF_FFFF`, below ECAM, exactly as D.5 does, so
  the aperture edk2 computes is larger than the space actually available.
  With the current device count BAR assignment starts at the bottom and
  never reaches ECAM, so nothing has gone wrong yet. It will when there are
  enough devices, and the fix is to shrink the aperture the firmware is
  given rather than to move ECAM.
* **SMBIOS is not presented, and its address collides with §3.3's ACPI
  staging area.** CloudHv's `SmbiosPlatformDxe` looks for an
  `SMBIOS_TABLE_3_0_ENTRY_POINT` — anchor `_SM3_` — at
  `CLOUDHV_SMBIOS_ADDRESS`, which is **`0x000F_0000`**. That is exactly
  `memory::ACPI_STAGING_BASE`. Today the driver returns `Not Found` (visible
  in the boot log as `Error: Image at ... start failed: Not Found`), which is
  correct behaviour and harmless — our ACPI bytes do not happen to begin
  `_SM3_`. But §3.2's SMBIOS builder cannot simply be staged at that address
  without moving the ACPI tables first, and a future change that stages it
  there anyway would corrupt the ACPI table set rather than fail visibly.
  Whoever wires SMBIOS in must move one of the two.

## The cost

Fedora ships no `CLOUDHV.fd` and neither does any other distribution, so this
firmware is built rather than installed:

```sh
scripts/build-cloudhv-firmware.sh
```

That script needs no root — where `nasm` and `iasl` are absent it downloads
the RPMs as an ordinary user and unpacks them into its own work directory,
the same technique `setup-local-sysroot.sh` uses for the C libraries. It
pins the edk2 revision, verifies the output is an ELF before installing it,
and takes under a minute on a warm tree. The firmware itself is a build
artifact and is not tracked; `firmware/README.md` is.

The real cost is not the build. It is that this firmware is not the one the
distribution tests, patches or signs, and Secure Boot in particular would
need a key story that option A gets for free. That is the trade this decision
accepts.

---

## D.10 — the machine has a linear framebuffer, because a GOP that only does `Blt` disappears at `ExitBootServices`

**§4 says** the console is virtio-gpu, captured from the guest's scanout.

**Revised:** the machine presents **two** display devices — virtio-gpu, and
a Bochs VBE linear framebuffer at `1234:1111` — and the console capture
prefers whichever is being painted.

**Why.** virtio-gpu is the better device and it is not sufficient on its
own. edk2's `OvmfPkg/VirtioGpuDxe/Gop.c` sets

```c
  VgpuGop->GopModeInfo.PixelFormat = PixelBltOnly;
```

and never assigns `FrameBufferBase`, with the comment *"No direct
framebuffer access is supported, only Blt() is."* That is correct for
virtio-gpu — the device has no linear aperture, only a resource the driver
transfers into — but it has a consequence:

* While firmware is running, `Blt` works, because `VirtioGpuDxe` is there to
  turn each one into `TRANSFER_TO_HOST_2D` + `RESOURCE_FLUSH`.
* After `ExitBootServices`, `VirtioGpuDxe` is gone. An operating system with
  no virtio-gpu driver of its own has a graphics protocol whose framebuffer
  address is zero and no way to reach the screen at all.

Windows 11 reaches exactly that point on this machine: it loads
`bootmgfw.efi` from our virtio-scsi DVD, reads 516 MiB of `boot.wim` into a
RAM disk, calls `ExitBootServices`, programs the I/O APIC — and then has
nowhere to draw. Windows has no inbox virtio-gpu driver, and neither does a
Linux kernel built without `CONFIG_DRM_VIRTIO_GPU`.

This was measured, not assumed. `VirtioGpu::scanout_backing` publishes the
guest pages behind the scanout and `gpu::frame_from_backing` reads them
without the guest's cooperation; after sixty seconds of Windows those pages
still held the firmware's last console frame.

**What is added.** `crates/libvmm-core/src/display.rs`: a PCI display-class
function with two memory BARs — BAR 0 the framebuffer, BAR 2 a 4 KiB
register window holding the VBE dispi registers at `0x500 + (index << 1)`
and the VGA register file as memory at `0x400 + (port - 0x3c0)`. edk2's
stock `QemuVideoDxe` binds it and reports a real `FrameBufferBase`, so every
guest can paint with no driver at all.

**Why not `ramfb`**, which is the smaller device: it is configured over
`fw_cfg`, and D.9 deliberately has no `fw_cfg`.

**Why this is not a retreat from §1.4.** The device has no VGA I/O port, no
option ROM, no real-mode BIOS interface and no chipset attachment. Its class
code is `0x038000`, "display controller, other" — deliberately *not*
`0x030000`, "VGA compatible controller", which `QemuVideoDxe` refuses on a
bridge that cannot forward VGA I/O. It is a PCI function with two BARs.

**The one structural consequence.** BAR 0 cannot be a trapping MMIO region.
A guest clearing an 800x600 screen writes 480,000 pixels, and an exit per
pixel is not a display. It is registered with KVM as guest RAM in
`SLOT_FRAMEBUFFER`, which means the slot has to follow the BAR wherever
firmware puts it. Two rules fall out, both in the device:

1. The slot is published only while the command register's memory-space bit
   is set. Firmware sizes a BAR by writing all-ones into it, and a slot that
   followed *that* would be mapped over the top of the address space.
2. `MmioDevice::set_bar_base` takes a BAR index. A device with more than one
   window needs to know which one moved, and the platform now decodes that
   from the register offset rather than assuming BAR 0.

---

## D.11 — the FADT must describe *this* machine

**§3.3 says** the table set includes a FADT.

**Revised:** the FADT is **hardware-reduced**, and every register it names is
one this machine decodes.

**What was wrong.** `build_fadt` cleared `HW_REDUCED_ACPI` and then supplied
none of what that implies:

| field | was | why that is wrong |
|---|---|---|
| `FIRMWARE_CTRL` | 0 | a FACS is mandatory when `HW_REDUCED_ACPI` is clear |
| `PM_TMR_BLK`, `PM_TMR_LEN` | 0 | the machine has the 3.579545 MHz timer at `0x0608` |
| `PM1a_EVT_BLK` | `0x05FC` | nothing decodes it |
| `PM1a_CNT_BLK` | `0x0600` | that is `SLEEP_CONTROL_REG`; a `PM1_CNT` write there means something else entirely |
| `SLEEP_CONTROL_REG`, `SLEEP_STATUS_REG` | absent | they are the whole power management of this platform |
| `RESET_REG_SUP` | set | nothing decoded `0x0CF9`, so a guest rebooting through ACPI wrote to a port nothing answered and then waited |
| FADT minor version | 6 | that field is the minor version — 3, for ACPI 6.3 |

Linux tolerated all of it, which is why it survived this long.

**What it says now**, following Cloud Hypervisor's `vmm/src/acpi.rs`, which
describes the same platform to the same firmware:

* Flags `HW_REDUCED_ACPI | RESET_REG_SUP | TMR_VAL_EXT`.
* `SLEEP_CONTROL_REG` and `SLEEP_STATUS_REG` as byte-wide I/O GAS at
  `0x0600` and `0x0601`.
* `X_PM_TMR_BLK` at `0x0608`, 32 bits — and `CloudHvPm` now uses
  `PmTimer::wide()`, because `TMR_VAL_EXT` is a claim about the hardware.
  An operating system told the counter is 32 bits wide, watching a 24-bit
  one, sees it stop dead every 4.7 seconds.
* `FIRMWARE_CTRL` and `X_FIRMWARE_CTRL` zero, which is what
  `HW_REDUCED_ACPI` requires.
* `RESET_REG` at `0x0CF9`, **and `CloudHvPm` decodes it**: bit 2, `RST_CPU`,
  raises `PowerEvent::Reset`.

**The one deliberate inconsistency.** A hardware-reduced platform has no PM1
event or control block, and this FADT declares both, at `0x060C` and
`0x0610`, which `CloudHvPm` answers. The reason is in Cloud Hypervisor's
source:

> Windows' nested-Hyper-V hvloader rejects a HW-reduced FADT whose PM1a GAS
> is zero; point the blocks at unused ACPI I/O ports (conforming guests
> ignore them).

**What this did not fix.** Windows 11 still reaches `ExitBootServices` and
stops without drawing. The FADT was wrong and is now right; it was not the
reason.
