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
