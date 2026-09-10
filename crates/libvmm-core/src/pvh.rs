//! The PVH boot path — Xen's "x86/HVM direct boot ABI".
//!
//! This is how a kernel is loaded and entered, and it replaces §3.1's
//! reset-vector firmware for the direct-boot path. The reasoning is in
//! [Revision D](../../../docs/spec-revision-D-boot-and-platform.md); the
//! short version is that no surveyed VMM boots an unmodified OVMF, that
//! Cloud Hypervisor's `--firmware` and `--kernel` are the same loader
//! because `CLOUDHV.fd` is itself a PVH ELF, and that PVH needs nothing
//! from the platform but RAM and a 16550.
//!
//! The guest is entered in **32-bit flat protected mode with paging off**.
//! There is no long mode here and no page table: the kernel builds its own
//! within a few dozen instructions. That is the single largest reason to
//! prefer this over the 64-bit Linux boot protocol, whose four-level paging
//! bootstrap fails as an undiagnosable triple fault when it is wrong.
//!
//! The authority is Xen's `docs/misc/pvh.pandoc`, which is byte-identical
//! across every stable branch — the ABI is frozen. Three requirements are
//! *not* in that document and are recorded here because each one fails
//! silently:
//!
//! * **`hvm_start_info` must be in writable RAM.** Linux's `pvh_start_xen`
//!   has no stack when it is entered, so it stashes `magic` in `%eax` and
//!   uses `%ebx + 4` as a one-slot stack for a `call`/`pop` to discover
//!   where it is running, then writes `magic` back. A read-only start_info
//!   faults on the second instruction.
//! * **`version` must be 1 and `memmap_entries` must be non-zero.** For a
//!   guest that finds no Xen CPUID leaves, `init_pvh_bootparams` has no
//!   hypercall to fall back on and calls `BUG()`.
//! * **The guest cannot tell us any of this.** Every diagnostic in
//!   `xen_prepare_pvh` goes through `xen_raw_printk`, which writes to port
//!   0xE9 only `if (xen_cpuid_base())` — a silent no-op here. And it runs
//!   before `setup_arch`, so `earlyprintk` does not exist yet either. A
//!   malformed start_info is a hang with no output, which is why
//!   [`BootInfo::validate`] checks on this side of the boundary.

use crate::error::{KvmError, VmmResult};

// ---------------------------------------------------------------------------
// Guest-physical layout
// ---------------------------------------------------------------------------
//
// These addresses are not in the ABI — the specification only says "below
// 4 GiB, not zero, naturally aligned". They are the values Cloud Hypervisor
// and Firecracker both use, and matching them means a kernel that boots
// under one of those boots here, which is worth more than any reason to
// differ.

/// The four-entry boot GDT.
pub const GDT_START: u64 = 0x500;
/// A single zero descriptor. The guest replaces the IDT immediately.
pub const IDT_START: u64 = 0x520;
/// `hvm_start_info`. Must be writable — see the module header.
pub const START_INFO_START: u64 = 0x6000;
/// `hvm_modlist_entry[]`, immediately after the start info's 56 bytes.
pub const MODLIST_START: u64 = 0x6040;
/// `hvm_memmap_table_entry[]`.
pub const MEMMAP_START: u64 = 0x7000;
/// The kernel command line, NUL-terminated ASCII.
pub const CMDLINE_START: u64 = 0x20000;
/// How much room the command line has before it would run into anything.
pub const CMDLINE_MAX: usize = 0x10000;

/// `XEN_HVM_START_MAGIC_VALUE` — "xEn3" with the 0x80 bit of the E set.
pub const START_MAGIC: u32 = 0x336E_C578;

/// The only version a non-Xen VMM may emit.
pub const START_INFO_VERSION: u32 = 1;

/// `XEN_ELFNOTE_PHYS32_ENTRY`.
const NOTE_PHYS32_ENTRY: u32 = 18;

/// The note's name field, including its NUL.
const NOTE_NAME: &[u8] = b"Xen\0";

/// Linux's zero page holds `E820_MAX_ENTRIES_ZEROPAGE` entries and appends
/// one more for the ISA hole, so this is the ceiling on what we may pass.
pub const MAX_MEMMAP_ENTRIES: usize = 127;

/// Memory-map entry types. The values are the ACPI address-range types, and
/// Xen asserts the correspondence with its own e820 constants at build
/// time; Linux copies them into the zero page without translating.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u32)]
pub enum MemmapType {
    Ram = 1,
    Reserved = 2,
    Acpi = 3,
    Nvs = 4,
    Unusable = 5,
    Disabled = 6,
    Pmem = 7,
}

// ---------------------------------------------------------------------------
// Writing into guest memory
// ---------------------------------------------------------------------------

/// Somewhere the loader can put bytes at a guest-physical address.
///
/// A trait rather than a concrete type so the layout this module produces
/// can be tested against a plain buffer. Getting `hvm_start_info` wrong is
/// invisible from inside the guest, so it has to be visible from a test.
pub trait GuestWrite {
    fn write_gpa(&self, gpa: u64, data: &[u8]) -> VmmResult<()>;
}

// ---------------------------------------------------------------------------
// The ELF image
// ---------------------------------------------------------------------------

/// One `PT_LOAD` segment, as the loader needs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Segment {
    pub file_offset: u64,
    pub paddr: u64,
    pub filesz: u64,
    pub memsz: u64,
}

impl Segment {
    /// The bytes beyond `filesz` that the loader must zero. This is `.bss`,
    /// and a kernel whose `.bss` holds whatever was in RAM fails arbitrarily
    /// far from the cause.
    pub const fn zero_fill(&self) -> u64 {
        self.memsz.saturating_sub(self.filesz)
    }
}

/// A parsed PVH-capable kernel.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PvhImage {
    /// From `XEN_ELFNOTE_PHYS32_ENTRY` — **not** the ELF header's `e_entry`,
    /// which on a Linux kernel is a different address entirely.
    pub entry: u64,
    pub segments: Vec<Segment>,
}

impl PvhImage {
    /// The highest guest-physical byte the image occupies.
    pub fn top(&self) -> u64 {
        self.segments
            .iter()
            .map(|s| s.paddr.saturating_add(s.memsz))
            .max()
            .unwrap_or(0)
    }

    /// The lowest guest-physical byte the image occupies.
    pub fn base(&self) -> u64 {
        self.segments.iter().map(|s| s.paddr).min().unwrap_or(0)
    }
}

fn u16_at(b: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(off..off + 2)?.try_into().ok()?))
}

fn u32_at(b: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(off..off + 4)?.try_into().ok()?))
}

fn u64_at(b: &[u8], off: usize) -> Option<u64> {
    Some(u64::from_le_bytes(b.get(off..off + 8)?.try_into().ok()?))
}

fn bad(detail: impl Into<String>) -> crate::error::VmmError {
    KvmError::FirmwareLoad {
        path: "<pvh kernel>".to_string(),
        detail: detail.into(),
    }
    .into()
}

const PT_LOAD: u32 = 1;
const PT_NOTE: u32 = 4;

/// Parse an ELF64 kernel and find its PVH entry point.
///
/// Loading is by `p_paddr`, not `p_vaddr` — a Linux `vmlinux` is linked at
/// `0xffffffff81000000` and loaded at `0x1000000`, so using the virtual
/// address would place it 16 exabytes away.
pub fn parse(image: &[u8]) -> VmmResult<PvhImage> {
    // e_ident
    if image.len() < 64 || &image[0..4] != b"\x7fELF" {
        return Err(bad("not an ELF file"));
    }
    if image[4] != 2 {
        return Err(bad("not ELF64; a 32-bit kernel is not supported here"));
    }
    if image[5] != 1 {
        return Err(bad("not little-endian"));
    }

    let e_phoff = u64_at(image, 0x20).ok_or_else(|| bad("truncated ELF header"))?;
    let e_phentsize = u16_at(image, 0x36).ok_or_else(|| bad("truncated ELF header"))? as usize;
    let e_phnum = u16_at(image, 0x38).ok_or_else(|| bad("truncated ELF header"))? as usize;

    if e_phentsize < 56 {
        return Err(bad(format!(
            "program header entry is {e_phentsize} bytes, need at least 56"
        )));
    }

    let mut segments = Vec::new();
    let mut entry = None;

    for i in 0..e_phnum {
        let off = usize::try_from(e_phoff)
            .ok()
            .and_then(|b| b.checked_add(i.checked_mul(e_phentsize)?))
            .ok_or_else(|| bad("program header table overflows"))?;

        let p_type = u32_at(image, off).ok_or_else(|| bad("truncated program header"))?;
        let p_flags = u32_at(image, off + 4).ok_or_else(|| bad("truncated program header"))?;
        let p_offset = u64_at(image, off + 8).ok_or_else(|| bad("truncated program header"))?;
        let p_paddr = u64_at(image, off + 24).ok_or_else(|| bad("truncated program header"))?;
        let p_filesz = u64_at(image, off + 32).ok_or_else(|| bad("truncated program header"))?;
        let p_memsz = u64_at(image, off + 40).ok_or_else(|| bad("truncated program header"))?;

        match p_type {
            // Xen's `elf_phdr_is_loadable`: PT_LOAD with at least one of
            // read, write or execute. A segment with no permissions is
            // metadata, not something to copy.
            PT_LOAD if p_flags & 0x7 != 0 => {
                if p_memsz < p_filesz {
                    return Err(bad("segment memsz is smaller than filesz"));
                }
                segments.push(Segment {
                    file_offset: p_offset,
                    paddr: p_paddr,
                    filesz: p_filesz,
                    memsz: p_memsz,
                });
            }
            // Some binutils versions leave p_offset zero on note segments,
            // which Xen's loader skips rather than misread.
            PT_NOTE if p_offset != 0 => {
                if let Some(found) = find_phys32_entry(image, p_offset, p_filesz)? {
                    entry = Some(found);
                }
            }
            _ => {}
        }
    }

    if segments.is_empty() {
        return Err(bad("no loadable segments"));
    }

    let entry = entry.ok_or_else(|| {
        bad(
            "no XEN_ELFNOTE_PHYS32_ENTRY note: this kernel was built without CONFIG_PVH and \
             cannot be booted this way",
        )
    })?;

    Ok(PvhImage { entry, segments })
}

/// Walk one `PT_NOTE` segment looking for `Xen`/`XEN_ELFNOTE_PHYS32_ENTRY`.
///
/// Note layout is a 12-byte header, then the name padded to 4 bytes, then
/// the descriptor padded to 4 bytes.
fn find_phys32_entry(image: &[u8], offset: u64, size: u64) -> VmmResult<Option<u64>> {
    let start = usize::try_from(offset).map_err(|_| bad("note segment offset overflows"))?;
    let len = usize::try_from(size).map_err(|_| bad("note segment size overflows"))?;
    let end = start
        .checked_add(len)
        .filter(|e| *e <= image.len())
        .ok_or_else(|| bad("note segment runs past the end of the file"))?;

    let mut cursor = start;
    while cursor + 12 <= end {
        let namesz = u32_at(image, cursor).ok_or_else(|| bad("truncated note"))? as usize;
        let descsz = u32_at(image, cursor + 4).ok_or_else(|| bad("truncated note"))? as usize;
        let ntype = u32_at(image, cursor + 8).ok_or_else(|| bad("truncated note"))?;

        let name_at = cursor + 12;
        let desc_at = name_at + namesz.next_multiple_of(4);
        let next = desc_at + descsz.next_multiple_of(4);
        if next > end || next <= cursor {
            break;
        }

        if ntype == NOTE_PHYS32_ENTRY
            && namesz == NOTE_NAME.len()
            && image.get(name_at..name_at + namesz) == Some(NOTE_NAME)
        {
            // The descriptor is 8 bytes on every x86_64 kernel, not 4: the
            // note is emitted with `_ASM_PTR`, which assembles to `.quad`.
            // A loader that insists on 4 rejects every distribution kernel
            // there is. Xen's own `elf_note_numeric` accepts 1, 2, 4 or 8.
            let value = match descsz {
                4 => u64::from(u32_at(image, desc_at).ok_or_else(|| bad("truncated note"))?),
                8 => u64_at(image, desc_at).ok_or_else(|| bad("truncated note"))?,
                other => return Err(bad(format!("PVH note descriptor is {other} bytes"))),
            };
            if value == 0 || value > u64::from(u32::MAX) {
                return Err(bad(format!(
                    "PVH entry {value:#x} is not a 32-bit physical address"
                )));
            }
            return Ok(Some(value));
        }

        cursor = next;
    }
    Ok(None)
}

// ---------------------------------------------------------------------------
// The boot information block
// ---------------------------------------------------------------------------

/// One entry of the guest's memory map.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MemmapEntry {
    pub addr: u64,
    pub size: u64,
    pub kind: MemmapType,
}

impl MemmapEntry {
    /// The 24-byte on-the-wire form.
    fn encode(&self) -> [u8; 24] {
        let mut b = [0u8; 24];
        b[0..8].copy_from_slice(&self.addr.to_le_bytes());
        b[8..16].copy_from_slice(&self.size.to_le_bytes());
        b[16..20].copy_from_slice(&(self.kind as u32).to_le_bytes());
        // b[20..24] is `reserved`, which must be zero for version 1.
        b
    }
}

/// What the loader hands the guest in `%ebx`.
#[derive(Debug, Clone, Default)]
pub struct BootInfo {
    pub cmdline: String,
    pub memmap: Vec<MemmapEntry>,
    /// Physical address of the ACPI RSDP, or `None`.
    ///
    /// Leaving this out is a bigger decision than it looks. With no RSDP the
    /// guest falls back to scanning the EBDA and `0xE0000..0xFFFFF` for the
    /// signature, finds nothing on this machine, and calls `disable_acpi()`.
    /// It then looks for MP tables, fails again, and settles on
    /// `APIC_VIRTUAL_WIRE_NO_CONFIG` — uniprocessor, no IOAPIC, and the PIT
    /// promoted to mandatory. Everything §1.3 says about this platform stops
    /// being true.
    pub rsdp: Option<u64>,
    /// `(address, size)` of an initramfs, if there is one.
    pub initramfs: Option<(u64, u64)>,
}

impl BootInfo {
    /// Reject anything the guest would fail on silently.
    ///
    /// This exists because the guest has no way to complain: see the module
    /// header on `xen_raw_printk`.
    pub fn validate(&self) -> VmmResult<()> {
        if self.memmap.is_empty() {
            return Err(bad(
                "the memory map is empty; a guest that finds no Xen CPUID leaves has no \
                 hypercall to fall back on and will BUG() in init_pvh_bootparams",
            ));
        }
        if self.memmap.len() > MAX_MEMMAP_ENTRIES {
            return Err(bad(format!(
                "{} memory map entries; the guest's zero page holds {MAX_MEMMAP_ENTRIES} plus \
                 the ISA hole it adds itself",
                self.memmap.len()
            )));
        }
        if self.cmdline.len() + 1 > CMDLINE_MAX {
            return Err(bad(format!(
                "command line is {} bytes, the region holds {CMDLINE_MAX}",
                self.cmdline.len() + 1
            )));
        }
        if self.cmdline.bytes().any(|b| b == 0) {
            return Err(bad("command line contains a NUL"));
        }
        if !self.cmdline.is_ascii() {
            return Err(bad("command line must be ASCII"));
        }
        Ok(())
    }

    /// The 56-byte `hvm_start_info`.
    fn encode_start_info(&self) -> [u8; 56] {
        let mut b = [0u8; 56];
        b[0..4].copy_from_slice(&START_MAGIC.to_le_bytes());
        b[4..8].copy_from_slice(&START_INFO_VERSION.to_le_bytes());
        // flags: SIF_* bits, meaningful only under Xen.
        b[8..12].copy_from_slice(&0u32.to_le_bytes());
        let modules: u32 = if self.initramfs.is_some() { 1 } else { 0 };
        b[12..16].copy_from_slice(&modules.to_le_bytes());
        let modlist = if modules > 0 { MODLIST_START } else { 0 };
        b[16..24].copy_from_slice(&modlist.to_le_bytes());
        b[24..32].copy_from_slice(&CMDLINE_START.to_le_bytes());
        b[32..40].copy_from_slice(&self.rsdp.unwrap_or(0).to_le_bytes());
        b[40..48].copy_from_slice(&MEMMAP_START.to_le_bytes());
        b[48..52].copy_from_slice(&(self.memmap.len() as u32).to_le_bytes());
        // b[52..56] is `reserved`, which must be zero.
        b
    }

    /// The 32-byte `hvm_modlist_entry` for the initramfs.
    fn encode_modlist(&self) -> Option<[u8; 32]> {
        let (addr, size) = self.initramfs?;
        let mut b = [0u8; 32];
        b[0..8].copy_from_slice(&addr.to_le_bytes());
        b[8..16].copy_from_slice(&size.to_le_bytes());
        // No separate command line for the module, and `reserved` is zero.
        Some(b)
    }
}

// ---------------------------------------------------------------------------
// Loading
// ---------------------------------------------------------------------------

/// Where the guest starts, once everything is in place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EntryState {
    /// The PVH note's address, to go in `rip`.
    pub entry: u64,
    /// What `%ebx` must hold: the address of `hvm_start_info`.
    pub start_info: u64,
}

/// Copy the kernel and its boot information into guest memory.
///
/// Returns where to start it. This does not touch a vCPU — see
/// [`entry_sregs`] and [`entry_regs`] for that half, which is separated so
/// the memory layout can be tested without a hypervisor.
pub fn load(
    mem: &dyn GuestWrite,
    image: &[u8],
    kernel: &PvhImage,
    boot: &BootInfo,
) -> VmmResult<EntryState> {
    boot.validate()?;

    // The kernel's PT_LOAD segments, at p_paddr.
    for seg in &kernel.segments {
        let start =
            usize::try_from(seg.file_offset).map_err(|_| bad("segment file offset overflows"))?;
        let len = usize::try_from(seg.filesz).map_err(|_| bad("segment filesz overflows"))?;
        let bytes = image
            .get(start..start.saturating_add(len))
            .ok_or_else(|| bad("segment runs past the end of the file"))?;
        mem.write_gpa(seg.paddr, bytes)?;

        // Zero .bss ourselves. Fresh guest RAM is already zero, but a
        // reboot's is not, and neither is a region another image used.
        let fill = usize::try_from(seg.zero_fill()).map_err(|_| bad("bss size overflows"))?;
        if fill > 0 {
            let zeros = vec![0u8; fill];
            mem.write_gpa(seg.paddr.saturating_add(seg.filesz), &zeros)?;
        }
    }

    // The command line, NUL-terminated.
    let mut cmdline = boot.cmdline.clone().into_bytes();
    cmdline.push(0);
    mem.write_gpa(CMDLINE_START, &cmdline)?;

    // The memory map.
    for (i, entry) in boot.memmap.iter().enumerate() {
        let at = MEMMAP_START + (i as u64) * 24;
        mem.write_gpa(at, &entry.encode())?;
    }

    // The module list, if there is an initramfs.
    if let Some(modlist) = boot.encode_modlist() {
        mem.write_gpa(MODLIST_START, &modlist)?;
    }

    // The start info last, so nothing above can have overwritten it.
    mem.write_gpa(START_INFO_START, &boot.encode_start_info())?;

    // The GDT the ABI requires, and an IDT the guest will replace.
    for (i, descriptor) in boot_gdt().iter().enumerate() {
        mem.write_gpa(GDT_START + (i as u64) * 8, &descriptor.to_le_bytes())?;
    }
    mem.write_gpa(IDT_START, &0u64.to_le_bytes())?;

    Ok(EntryState {
        entry: kernel.entry,
        start_info: START_INFO_START,
    })
}

/// The four descriptors of the boot GDT.
///
/// | selector | descriptor | what it is |
/// |---|---|---|
/// | 0x00 | `0x0000000000000000` | null |
/// | 0x08 | `0x00CF9B000000FFFF` | 32-bit code, base 0, limit 4 GiB |
/// | 0x10 | `0x00CF93000000FFFF` | 32-bit data, base 0, limit 4 GiB |
/// | 0x18 | `0x00008B0000000067` | 32-bit busy TSS, base 0, limit 0x67 |
pub const fn boot_gdt() -> [u64; 4] {
    [
        0x0000_0000_0000_0000,
        0x00CF_9B00_0000_FFFF,
        0x00CF_9300_0000_FFFF,
        0x0000_8B00_0000_0067,
    ]
}

/// Selectors matching [`boot_gdt`].
pub mod selector {
    pub const CODE: u16 = 0x08;
    pub const DATA: u16 = 0x10;
    pub const TSS: u16 = 0x18;
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    use std::collections::BTreeMap;

    /// A guest memory made of whatever was written to it.
    #[derive(Default)]
    struct FakeMemory {
        writes: RefCell<BTreeMap<u64, Vec<u8>>>,
    }

    impl GuestWrite for FakeMemory {
        fn write_gpa(&self, gpa: u64, data: &[u8]) -> VmmResult<()> {
            self.writes.borrow_mut().insert(gpa, data.to_vec());
            Ok(())
        }
    }

    impl FakeMemory {
        fn at(&self, gpa: u64) -> Vec<u8> {
            self.writes.borrow().get(&gpa).cloned().unwrap_or_default()
        }
    }

    /// Build an ELF64 with one PT_LOAD and a PT_NOTE carrying the PVH note.
    ///
    /// `descsz` is a parameter because real kernels use 8 and the
    /// specification's example implies 4, and both must work.
    fn synthetic_kernel(entry: u32, descsz: usize) -> Vec<u8> {
        let mut elf = vec![0u8; 0x1000];
        elf[0..4].copy_from_slice(b"\x7fELF");
        elf[4] = 2; // ELF64
        elf[5] = 1; // little-endian
        elf[0x10..0x12].copy_from_slice(&2u16.to_le_bytes()); // ET_EXEC
                                                              // e_entry deliberately differs from the note, as it does on a real
                                                              // kernel — a loader that uses it is wrong and this proves it.
        elf[0x18..0x20].copy_from_slice(&0xDEAD_BEEFu64.to_le_bytes());
        elf[0x20..0x28].copy_from_slice(&64u64.to_le_bytes()); // e_phoff
        elf[0x36..0x38].copy_from_slice(&56u16.to_le_bytes()); // e_phentsize
        elf[0x38..0x3A].copy_from_slice(&2u16.to_le_bytes()); // e_phnum

        // PT_LOAD: 16 bytes of payload at 0x100000, 32 bytes in memory.
        let ph = 64;
        elf[ph..ph + 4].copy_from_slice(&PT_LOAD.to_le_bytes());
        elf[ph + 4..ph + 8].copy_from_slice(&5u32.to_le_bytes()); // R+X
        elf[ph + 8..ph + 16].copy_from_slice(&0x800u64.to_le_bytes()); // p_offset
        elf[ph + 24..ph + 32].copy_from_slice(&0x100000u64.to_le_bytes()); // p_paddr
        elf[ph + 32..ph + 40].copy_from_slice(&16u64.to_le_bytes()); // p_filesz
        elf[ph + 40..ph + 48].copy_from_slice(&32u64.to_le_bytes()); // p_memsz
        for (i, b) in (0..16u8).enumerate() {
            elf[0x800 + i] = b;
        }

        // PT_NOTE at 0x900: a decoy note, then the Xen one.
        let note_off = 0x900usize;
        let mut cursor = note_off;
        let decoy = b"GNU\0";
        elf[cursor..cursor + 4].copy_from_slice(&(decoy.len() as u32).to_le_bytes());
        elf[cursor + 4..cursor + 8].copy_from_slice(&4u32.to_le_bytes());
        elf[cursor + 8..cursor + 12].copy_from_slice(&NOTE_PHYS32_ENTRY.to_le_bytes());
        elf[cursor + 12..cursor + 16].copy_from_slice(decoy);
        elf[cursor + 16..cursor + 20].copy_from_slice(&0xFFFF_FFFFu32.to_le_bytes());
        cursor += 20;

        elf[cursor..cursor + 4].copy_from_slice(&4u32.to_le_bytes()); // namesz
        elf[cursor + 4..cursor + 8].copy_from_slice(&(descsz as u32).to_le_bytes());
        elf[cursor + 8..cursor + 12].copy_from_slice(&NOTE_PHYS32_ENTRY.to_le_bytes());
        elf[cursor + 12..cursor + 16].copy_from_slice(NOTE_NAME);
        match descsz {
            4 => elf[cursor + 16..cursor + 20].copy_from_slice(&entry.to_le_bytes()),
            8 => elf[cursor + 16..cursor + 24].copy_from_slice(&u64::from(entry).to_le_bytes()),
            _ => unreachable!("test only builds 4- or 8-byte descriptors"),
        }
        let note_len = (cursor + 16 + descsz.next_multiple_of(4) - note_off) as u64;

        let ph = 64 + 56;
        elf[ph..ph + 4].copy_from_slice(&PT_NOTE.to_le_bytes());
        elf[ph + 4..ph + 8].copy_from_slice(&4u32.to_le_bytes()); // R
        elf[ph + 8..ph + 16].copy_from_slice(&(note_off as u64).to_le_bytes());
        elf[ph + 32..ph + 40].copy_from_slice(&note_len.to_le_bytes());
        elf
    }

    fn simple_boot() -> BootInfo {
        BootInfo {
            cmdline: "console=ttyS0".to_string(),
            memmap: vec![MemmapEntry {
                addr: 0,
                size: 128 * 1024 * 1024,
                kind: MemmapType::Ram,
            }],
            rsdp: Some(0xE_0000),
            initramfs: None,
        }
    }

    #[test]
    fn the_entry_point_comes_from_the_note_and_not_the_elf_header() {
        let elf = synthetic_kernel(0x0035_EA570, 8);
        let image = parse(&elf).expect("a PVH kernel must parse");
        assert_eq!(
            image.entry, 0x0035_EA570,
            "the entry must be the note's value, not e_entry (0xDEADBEEF)"
        );
    }

    #[test]
    fn a_note_descriptor_of_eight_bytes_is_accepted() {
        // Every x86_64 Linux kernel emits 8, because the note is written
        // with `_ASM_PTR`. A loader that only accepts 4 boots nothing.
        let image = parse(&synthetic_kernel(0x100000, 8)).expect("8-byte descriptor");
        assert_eq!(image.entry, 0x100000);
        let image = parse(&synthetic_kernel(0x100000, 4)).expect("4-byte descriptor");
        assert_eq!(image.entry, 0x100000);
    }

    #[test]
    fn a_note_named_something_other_than_xen_is_not_the_pvh_note() {
        // The decoy in the synthetic kernel has the right type but the name
        // "GNU", and its value is 0xFFFFFFFF. If that were taken the entry
        // would be wrong rather than absent, which is the dangerous failure.
        let image = parse(&synthetic_kernel(0x1234, 8)).expect("parse");
        assert_eq!(image.entry, 0x1234, "the GNU note must have been skipped");
    }

    #[test]
    fn a_kernel_without_the_note_is_refused_by_name() {
        let mut elf = synthetic_kernel(0x100000, 8);
        // Break the note's type so nothing matches.
        elf[0x900..0x960].fill(0);
        let err = parse(&elf).expect_err("a kernel with no PVH note cannot boot this way");
        let text = format!("{err}");
        assert!(
            text.contains("CONFIG_PVH"),
            "the error must say what is actually wrong with the kernel: {text}"
        );
    }

    #[test]
    fn bss_beyond_the_file_is_zeroed() {
        let elf = synthetic_kernel(0x100000, 8);
        let image = parse(&elf).expect("parse");
        let seg = image.segments[0];
        assert_eq!(seg.zero_fill(), 16, "32 bytes in memory, 16 in the file");

        let mem = FakeMemory::default();
        load(&mem, &elf, &image, &simple_boot()).expect("load");
        assert_eq!(
            mem.at(0x100000 + 16),
            vec![0u8; 16],
            "the .bss tail must be written as zeros, not left as whatever was there"
        );
    }

    #[test]
    fn the_start_info_is_what_the_guest_checks_for() {
        let elf = synthetic_kernel(0x100000, 8);
        let image = parse(&elf).expect("parse");
        let mem = FakeMemory::default();
        load(&mem, &elf, &image, &simple_boot()).expect("load");

        let si = mem.at(START_INFO_START);
        assert_eq!(si.len(), 56, "hvm_start_info is 56 bytes in version 1");
        assert_eq!(
            u32_at(&si, 0),
            Some(START_MAGIC),
            "magic — Linux BUG()s on it"
        );
        assert_eq!(
            u32_at(&si, 4),
            Some(1),
            "version must be 1 for a non-Xen VMM"
        );
        assert_eq!(u64_at(&si, 24), Some(CMDLINE_START));
        assert_eq!(u64_at(&si, 32), Some(0xE_0000), "rsdp_paddr");
        assert_eq!(u64_at(&si, 40), Some(MEMMAP_START));
        assert_eq!(u32_at(&si, 48), Some(1), "memmap_entries must be non-zero");
        assert_eq!(u32_at(&si, 52), Some(0), "reserved must be zero");
    }

    #[test]
    fn an_empty_memory_map_is_refused_rather_than_hung_on() {
        // The guest would BUG() here, and its BUG() is invisible: every
        // diagnostic on this path goes through xen_raw_printk, which does
        // nothing without Xen CPUID leaves.
        let boot = BootInfo {
            memmap: Vec::new(),
            ..simple_boot()
        };
        let err = boot
            .validate()
            .expect_err("an empty memmap must be refused");
        assert!(format!("{err}").contains("BUG()"));
    }

    #[test]
    fn the_memory_map_is_written_as_twenty_four_byte_entries() {
        let elf = synthetic_kernel(0x100000, 8);
        let image = parse(&elf).expect("parse");
        let boot = BootInfo {
            memmap: vec![
                MemmapEntry {
                    addr: 0,
                    size: 0x9FC00,
                    kind: MemmapType::Ram,
                },
                MemmapEntry {
                    addr: crate::memory::ECAM_BASE,
                    size: crate::memory::ECAM_SIZE,
                    kind: MemmapType::Reserved,
                },
            ],
            ..simple_boot()
        };
        let mem = FakeMemory::default();
        load(&mem, &elf, &image, &boot).expect("load");

        let first = mem.at(MEMMAP_START);
        assert_eq!(first.len(), 24);
        assert_eq!(u64_at(&first, 8), Some(0x9FC00));
        assert_eq!(u32_at(&first, 16), Some(MemmapType::Ram as u32));

        // The second entry must be exactly 24 bytes further on, or the guest
        // reads the table misaligned and every entry after it is nonsense.
        let second = mem.at(MEMMAP_START + 24);
        assert_eq!(u64_at(&second, 0), Some(crate::memory::ECAM_BASE));
        assert_eq!(u32_at(&second, 16), Some(MemmapType::Reserved as u32));
    }

    #[test]
    fn too_many_memory_map_entries_are_refused() {
        let boot = BootInfo {
            memmap: vec![
                MemmapEntry {
                    addr: 0,
                    size: 4096,
                    kind: MemmapType::Ram,
                };
                MAX_MEMMAP_ENTRIES + 1
            ],
            ..simple_boot()
        };
        assert!(
            boot.validate().is_err(),
            "the guest's zero page cannot hold them"
        );
    }

    #[test]
    fn the_command_line_is_nul_terminated() {
        let elf = synthetic_kernel(0x100000, 8);
        let image = parse(&elf).expect("parse");
        let mem = FakeMemory::default();
        load(&mem, &elf, &image, &simple_boot()).expect("load");
        assert_eq!(mem.at(CMDLINE_START), b"console=ttyS0\0");
    }

    #[test]
    fn an_initramfs_is_described_by_a_module_entry() {
        let elf = synthetic_kernel(0x100000, 8);
        let image = parse(&elf).expect("parse");
        let boot = BootInfo {
            initramfs: Some((0x400_0000, 0x20_0000)),
            ..simple_boot()
        };
        let mem = FakeMemory::default();
        load(&mem, &elf, &image, &boot).expect("load");

        let si = mem.at(START_INFO_START);
        assert_eq!(u32_at(&si, 12), Some(1), "nr_modules");
        assert_eq!(u64_at(&si, 16), Some(MODLIST_START));

        let m = mem.at(MODLIST_START);
        assert_eq!(m.len(), 32);
        assert_eq!(u64_at(&m, 0), Some(0x400_0000));
        assert_eq!(u64_at(&m, 8), Some(0x20_0000));
    }

    #[test]
    fn with_no_initramfs_the_module_list_is_not_advertised() {
        let elf = synthetic_kernel(0x100000, 8);
        let image = parse(&elf).expect("parse");
        let mem = FakeMemory::default();
        load(&mem, &elf, &image, &simple_boot()).expect("load");
        let si = mem.at(START_INFO_START);
        assert_eq!(u32_at(&si, 12), Some(0), "nr_modules");
        assert_eq!(
            u64_at(&si, 16),
            Some(0),
            "a zero address is how the ABI says 'not present'"
        );
    }

    #[test]
    fn the_boot_gdt_matches_the_abi() {
        // Base 0 and limit 0xFFFFFFFF on code and data; a 32-bit busy TSS
        // with limit 0x67. These exact values are what Xen, Cloud Hypervisor
        // and Firecracker all install.
        let gdt = boot_gdt();
        assert_eq!(gdt[0], 0);
        assert_eq!(gdt[1], 0x00CF_9B00_0000_FFFF, "32-bit code");
        assert_eq!(gdt[2], 0x00CF_9300_0000_FFFF, "32-bit data");
        assert_eq!(gdt[3], 0x0000_8B00_0000_0067, "32-bit busy TSS, limit 0x67");
    }

    #[test]
    fn the_gdt_and_idt_are_written_where_the_abi_expects() {
        let elf = synthetic_kernel(0x100000, 8);
        let image = parse(&elf).expect("parse");
        let mem = FakeMemory::default();
        load(&mem, &elf, &image, &simple_boot()).expect("load");
        for (i, descriptor) in boot_gdt().iter().enumerate() {
            assert_eq!(
                mem.at(GDT_START + (i as u64) * 8),
                descriptor.to_le_bytes().to_vec()
            );
        }
        assert_eq!(mem.at(IDT_START), vec![0u8; 8]);
    }

    #[test]
    fn a_command_line_that_would_not_fit_is_refused() {
        let boot = BootInfo {
            cmdline: "x".repeat(CMDLINE_MAX),
            ..simple_boot()
        };
        assert!(boot.validate().is_err());
    }

    #[test]
    fn a_thirty_two_bit_kernel_is_refused_clearly() {
        let mut elf = synthetic_kernel(0x100000, 8);
        elf[4] = 1; // ELFCLASS32
        let err = parse(&elf).expect_err("32-bit is not supported");
        assert!(format!("{err}").contains("32-bit"));
    }
}
