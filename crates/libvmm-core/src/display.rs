//! The display: a linear framebuffer every guest can paint into without a
//! driver.
//!
//! # Why this exists at all
//!
//! virtio-gpu is the better device and it is not enough. edk2's
//! `OvmfPkg/VirtioGpuDxe/Gop.c` reports `PixelBltOnly` and never assigns
//! `FrameBufferBase` — *"No direct framebuffer access is supported, only
//! Blt() is."* An operating system that has no virtio-gpu driver of its own
//! is therefore handed a graphics protocol with nowhere to paint, and once
//! it calls `ExitBootServices` the firmware driver that used to forward
//! `Blt` is gone. Windows 11 boots to exactly that point and then has no
//! screen. Linux without `virtio_gpu` is in the same position.
//!
//! So the machine also has a display whose framebuffer is plain memory at a
//! known address: the Bochs VBE interface as QEMU's `bochs-display`
//! presents it, which edk2's stock `QemuVideoDxe` binds and reports with a
//! real `FrameBufferBase`. Nothing about it is legacy in the sense §1.4
//! forbids — there is no VGA I/O port, no option ROM and no real-mode BIOS
//! interface. It is a PCI display-class function with two memory BARs.
//!
//! `ramfb` would be the smaller device and is ruled out: it is configured
//! over `fw_cfg`, which Revision D.9 deliberately does not have.
//!
//! # The shape of the device
//!
//! | BAR | contents |
//! |---|---|
//! | 0 | the framebuffer, a real KVM memory slot |
//! | 2 | 4 KiB of registers |
//!
//! Inside BAR 2, at the offsets `QemuVideoDxe` uses:
//!
//! | offset | contents |
//! |---|---|
//! | `0x000` | EDID blob, if there is one |
//! | `0x400 + (port - 0x3c0)` | the VGA register file, as memory |
//! | `0x500 + (index << 1)` | the VBE dispi registers, 16 bits each |
//!
//! BAR 0 is not an MMIO region. A guest clearing an 800x600 screen writes
//! 480,000 pixels, and an exit per pixel is not a display. It is registered
//! with KVM as guest RAM, which means the slot has to follow the BAR
//! wherever firmware puts it — see [`BochsDisplay::set_bar_base`] and
//! [`BochsDisplay::set_memory_decode`].

use std::sync::{Arc, Mutex};

use crate::devices::{GuestRamMapper, MmioDevice};
use crate::kvm::HostMapping;
use crate::pci::Bdf;

/// What `QemuVideoDxe` looks for in configuration space.
pub const VENDOR_ID: u16 = 0x1234;
pub const DEVICE_ID: u16 = 0x1111;
/// Display controller, "other" — deliberately *not* 0x030000, which is
/// "VGA compatible controller". `QemuVideoDxe` refuses a VGA-compatible
/// function whose bridge cannot forward VGA I/O, and this machine has no
/// VGA I/O to forward.
pub const CLASS: u32 = 0x00_03_80_00;

/// BAR 0. 16 MiB covers 1920x1200 at 32 bits per pixel twice over, and a
/// power of two is what a BAR can express.
pub const FRAMEBUFFER_BAR_SIZE: u64 = 16 * 1024 * 1024;
/// BAR 2.
pub const REGISTER_BAR_SIZE: u64 = 4096;

/// Offsets inside BAR 2, from `OvmfPkg/QemuVideoDxe/Driver.c`.
mod bar2 {
    /// `QemuVideoBochsEdid` reads 128 bytes from here.
    pub const EDID: u64 = 0x000;
    pub const EDID_LEN: u64 = 128;
    /// `VgaOutb` writes VGA register `r` at `0x400 - 0x3c0 + r`.
    pub const VGA: u64 = 0x400;
    pub const VGA_LEN: u64 = 0x20;
    /// `BochsWrite` writes dispi register `r` at `0x500 + (r << 1)`.
    pub const DISPI: u64 = 0x500;
    pub const DISPI_COUNT: u64 = 11;
}

/// The VBE dispi register file. Public because it is the interface:
/// a reader chasing what a guest wrote wants the whole map, not the
/// subset this device treats specially.
pub mod dispi {
    pub const ID: usize = 0x0;
    pub const XRES: usize = 0x1;
    pub const YRES: usize = 0x2;
    pub const BPP: usize = 0x3;
    pub const ENABLE: usize = 0x4;
    pub const BANK: usize = 0x5;
    pub const VIRT_WIDTH: usize = 0x6;
    pub const VIRT_HEIGHT: usize = 0x7;
    pub const X_OFFSET: usize = 0x8;
    pub const Y_OFFSET: usize = 0x9;
    pub const VIDEO_MEMORY_64K: usize = 0xA;
    pub const COUNT: usize = 0xB;

    /// `QemuVideoDxe` checks `(id & 0xFFF0) == 0xB0C0` and refuses the
    /// device otherwise, so this is not decoration.
    pub const ID5: u16 = 0xB0C5;
    pub const ENABLED: u16 = 0x01;
    pub const LFB_ENABLED: u16 = 0x40;
    pub const NOCLEARMEM: u16 = 0x80;
}

/// The mode the guest has programmed, as the capture side needs it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DisplayMode {
    pub width: u32,
    pub height: u32,
    pub bits_per_pixel: u16,
    /// Bytes per row, which is the *virtual* width, not the visible one.
    pub stride: usize,
    /// Where the visible rectangle starts inside the framebuffer.
    pub offset: usize,
    /// Clear until the guest sets `VBE_DISPI_ENABLED`. A capture must not
    /// show a framebuffer whose mode has not been programmed: the contents
    /// are whatever was there before.
    pub enabled: bool,
}

impl DisplayMode {
    const BLANK: DisplayMode = DisplayMode {
        width: 0,
        height: 0,
        bits_per_pixel: 0,
        stride: 0,
        offset: 0,
        enabled: false,
    };

    /// Bytes the visible rectangle occupies.
    pub const fn visible_len(&self) -> usize {
        self.stride * self.height as usize
    }
}

/// The framebuffer, shared between the vCPU that programs it and whoever is
/// watching.
///
/// There is no frame queue and no copy on the device side, unlike
/// virtio-gpu: the guest writes pixels straight into this memory and never
/// tells anyone. A capture reads it when it wants a picture.
pub struct Framebuffer {
    memory: HostMapping,
    mode: Mutex<DisplayMode>,
}

impl Framebuffer {
    /// The mode the guest has programmed.
    pub fn mode(&self) -> DisplayMode {
        self.mode.lock().map_or(DisplayMode::BLANK, |m| *m)
    }

    /// Copy the visible rectangle out, if there is one.
    ///
    /// Returns `None` before the guest has programmed a mode. The copy is
    /// deliberate: the guest keeps writing while we read, and a caller that
    /// held a borrow of live guest memory would be reading a moving target
    /// with no way to say when it stopped.
    pub fn snapshot(&self) -> Option<(DisplayMode, Vec<u8>)> {
        let mode = self.mode();
        if !mode.enabled || mode.width == 0 || mode.height == 0 {
            return None;
        }
        let len = mode.visible_len();
        let end = mode.offset.checked_add(len)?;
        if end > self.memory.len() {
            return None;
        }
        let mut out = vec![0u8; len];
        // SAFETY: `memory` is a live mapping of `self.memory.len()` bytes
        // and `offset + len` is inside it. The guest may be writing to it
        // concurrently, which is what a framebuffer is: the result is a
        // torn frame, never an invalid read.
        unsafe {
            std::ptr::copy_nonoverlapping(
                (self.memory.host_addr() as *const u8).add(mode.offset),
                out.as_mut_ptr(),
                len,
            );
        }
        Some((mode, out))
    }
}

/// The Bochs VBE display as a PCI function.
pub struct BochsDisplay {
    bdf: Option<Bdf>,
    framebuffer: Arc<Framebuffer>,
    mapper: Arc<dyn GuestRamMapper>,
    registers: [u16; dispi::COUNT],
    /// The VGA register file, answered as memory. Nothing reads it back
    /// that matters; it exists so `VgaOutb` does not fall off the end of
    /// the device.
    vga: [u8; bar2::VGA_LEN as usize],
    /// Where firmware put each BAR, and whether the guest has enabled
    /// decoding. The framebuffer slot is published only when both are
    /// settled — see [`Self::republish`].
    framebuffer_base: u64,
    register_base: u64,
    decoding: bool,
    published_at: Option<u64>,
}

impl BochsDisplay {
    /// `framebuffer_base` and `register_base` are where the platform puts
    /// the BARs before firmware has an opinion.
    pub fn new(
        mapper: Arc<dyn GuestRamMapper>,
        framebuffer_base: u64,
        register_base: u64,
    ) -> crate::VmmResult<Self> {
        let memory = HostMapping::anonymous(
            FRAMEBUFFER_BAR_SIZE as usize,
            framebuffer_base,
            crate::memory::SLOT_FRAMEBUFFER,
            false,
            false,
        )?;

        let mut registers = [0u16; dispi::COUNT];
        registers[dispi::ID] = dispi::ID5;
        registers[dispi::VIDEO_MEMORY_64K] = (FRAMEBUFFER_BAR_SIZE / (64 * 1024)) as u16;

        let mut display = BochsDisplay {
            bdf: None,
            framebuffer: Arc::new(Framebuffer {
                memory,
                mode: Mutex::new(DisplayMode::BLANK),
            }),
            mapper,
            registers,
            vga: [0; bar2::VGA_LEN as usize],
            framebuffer_base,
            register_base,
            decoding: false,
            published_at: None,
        };
        display.republish();
        Ok(display)
    }

    /// The framebuffer, for whoever is showing it.
    pub fn framebuffer(&self) -> Arc<Framebuffer> {
        Arc::clone(&self.framebuffer)
    }

    pub fn set_bdf(&mut self, bdf: Bdf) {
        self.bdf = Some(bdf);
    }

    /// Put the framebuffer slot where the BAR now says it is.
    ///
    /// Called on every BAR and command-register write, and idempotent, so
    /// the sizing dance — write all-ones, read the mask back, write the
    /// real address — costs at most one wasted registration.
    fn republish(&mut self) {
        let wanted = (self.framebuffer_base != 0 && self.decoding).then_some(self.framebuffer_base);
        if wanted == self.published_at {
            return;
        }
        // Moving a slot means dropping it first: KVM will not re-register a
        // live slot at a different guest address. Dropping one that was
        // never registered is an error, though, so this is conditional on
        // having published one.
        if self.published_at.is_some() {
            if let Err(e) = self.mapper.unmap(crate::memory::SLOT_FRAMEBUFFER) {
                log::warn!("display: framebuffer could not be withdrawn: {e}");
                return;
            }
            self.published_at = None;
        }
        let Some(gpa) = wanted else { return };
        let result = self.mapper.remap(
            crate::memory::SLOT_FRAMEBUFFER,
            gpa,
            self.framebuffer.memory.host_addr(),
            FRAMEBUFFER_BAR_SIZE,
        );
        match result {
            Ok(()) => {
                log::info!("display: framebuffer mapped at {gpa:#x}");
                self.published_at = wanted;
            }
            // Not fatal, and not silent. The guest will find a framebuffer
            // that does not answer, which is a blank screen rather than a
            // dead machine.
            Err(e) => log::warn!("display: framebuffer could not be mapped: {e}"),
        }
    }

    /// Recompute the mode from the dispi registers.
    fn apply_mode(&mut self) {
        let r = &self.registers;
        let enabled = r[dispi::ENABLE] & dispi::ENABLED != 0;
        let bpp = r[dispi::BPP];
        // A virtual width of zero means the guest never set one, in which
        // case the visible width is the whole row.
        let virt_width = if r[dispi::VIRT_WIDTH] == 0 {
            r[dispi::XRES]
        } else {
            r[dispi::VIRT_WIDTH]
        };
        let bytes_per_pixel = (bpp as usize).div_ceil(8);
        let stride = virt_width as usize * bytes_per_pixel;
        let mode = DisplayMode {
            width: u32::from(r[dispi::XRES]),
            height: u32::from(r[dispi::YRES]),
            bits_per_pixel: bpp,
            stride,
            offset: u32::from(r[dispi::Y_OFFSET]) as usize * stride
                + u32::from(r[dispi::X_OFFSET]) as usize * bytes_per_pixel,
            enabled,
        };
        if let Ok(mut slot) = self.framebuffer.mode.lock() {
            if *slot != mode {
                if mode.enabled {
                    log::info!(
                        "display: {}x{} at {} bpp, {} bytes per row",
                        mode.width,
                        mode.height,
                        mode.bits_per_pixel,
                        mode.stride
                    );
                }
                *slot = mode;
            }
        }
    }

    /// Clearing memory on mode set is part of the interface: the Bochs VBE
    /// interface zeroes the framebuffer when `VBE_DISPI_ENABLED` is set
    /// unless `VBE_DISPI_NOCLEARMEM` says otherwise.
    fn clear(&mut self) {
        // SAFETY: a live mapping of exactly this length, and no guest is
        // running in this device's critical section.
        unsafe {
            std::ptr::write_bytes(
                self.framebuffer.memory.host_addr() as *mut u8,
                0,
                FRAMEBUFFER_BAR_SIZE as usize,
            );
        }
    }

    fn read_register(&self, offset: u64, data: &mut [u8]) {
        let value: u64 = if (bar2::EDID..bar2::EDID + bar2::EDID_LEN).contains(&offset) {
            // No EDID. `QemuVideoBochsEdid` checks for 0x00 0xFF and gives
            // up quietly, leaving the built-in mode list — which already
            // contains every mode this machine offers.
            0
        } else if (bar2::VGA..bar2::VGA + bar2::VGA_LEN).contains(&offset) {
            u64::from(self.vga[(offset - bar2::VGA) as usize])
        } else if (bar2::DISPI..bar2::DISPI + bar2::DISPI_COUNT * 2).contains(&offset) {
            let index = ((offset - bar2::DISPI) / 2) as usize;
            u64::from(self.registers[index])
        } else {
            0
        };
        let bytes = value.to_le_bytes();
        let n = data.len().min(8);
        data[..n].copy_from_slice(&bytes[..n]);
        data[n..].fill(0);
    }

    fn write_register(&mut self, offset: u64, data: &[u8]) {
        if (bar2::VGA..bar2::VGA + bar2::VGA_LEN).contains(&offset) {
            if let Some(&byte) = data.first() {
                self.vga[(offset - bar2::VGA) as usize] = byte;
            }
            return;
        }
        if !(bar2::DISPI..bar2::DISPI + bar2::DISPI_COUNT * 2).contains(&offset) {
            return;
        }
        let index = ((offset - bar2::DISPI) / 2) as usize;
        let mut value = [0u8; 2];
        let n = data.len().min(2);
        value[..n].copy_from_slice(&data[..n]);
        let value = u16::from_le_bytes(value);

        match index {
            // The ID register and the memory size are the device
            // describing itself. A guest writing to them is telling us
            // what it wishes were true.
            dispi::ID | dispi::VIDEO_MEMORY_64K => {}
            dispi::ENABLE => {
                let turning_on = value & dispi::ENABLED != 0
                    && self.registers[dispi::ENABLE] & dispi::ENABLED == 0;
                self.registers[dispi::ENABLE] = value;
                if turning_on && value & dispi::NOCLEARMEM == 0 {
                    self.clear();
                }
                self.apply_mode();
            }
            _ => {
                self.registers[index] = value;
                self.apply_mode();
            }
        }
    }
}

impl MmioDevice for BochsDisplay {
    fn name(&self) -> &'static str {
        "display"
    }

    /// Only BAR 2. BAR 0 is guest RAM and never reaches a device model —
    /// if it ever does, the slot is not registered and the picture is
    /// gone, so it is worth saying out loud that the absence of exits here
    /// is the device working.
    fn claims(&self, addr: u64) -> bool {
        (self.register_base..self.register_base + REGISTER_BAR_SIZE).contains(&addr)
    }

    fn read(&mut self, addr: u64, data: &mut [u8]) {
        self.read_register(addr - self.register_base, data);
    }

    fn write(&mut self, addr: u64, data: &[u8]) {
        let offset = addr - self.register_base;
        self.write_register(offset, data);
    }

    fn bdf(&self) -> Option<Bdf> {
        self.bdf
    }

    fn set_bar_base(&mut self, bar: usize, base: u64) {
        match bar {
            0 => {
                self.framebuffer_base = base;
                self.republish();
            }
            2 => self.register_base = base,
            _ => {}
        }
    }

    fn set_memory_decode(&mut self, enabled: bool) {
        self.decoding = enabled;
        self.republish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    #[derive(Default)]
    struct RecordingMapper {
        mapped_at: AtomicU64,
        remaps: AtomicU64,
    }

    impl GuestRamMapper for RecordingMapper {
        fn remap(&self, _slot: u32, gpa: u64, _host: u64, _len: u64) -> crate::VmmResult<()> {
            self.mapped_at.store(gpa, Ordering::Relaxed);
            self.remaps.fetch_add(1, Ordering::Relaxed);
            Ok(())
        }
        fn unmap(&self, _slot: u32) -> crate::VmmResult<()> {
            self.mapped_at.store(0, Ordering::Relaxed);
            Ok(())
        }
    }

    fn display(mapper: Arc<RecordingMapper>) -> BochsDisplay {
        let mut d = BochsDisplay::new(mapper, 0xC000_0000, 0xC100_0000).expect("build the display");
        d.set_bdf(Bdf::new(0, 3, 0));
        d
    }

    fn dispi_write(d: &mut BochsDisplay, index: usize, value: u16) {
        let addr = 0xC100_0000 + bar2::DISPI + (index as u64) * 2;
        d.write(addr, &value.to_le_bytes());
    }

    #[test]
    fn the_id_register_is_what_qemuvideodxe_refuses_the_device_without() {
        let mut d = display(Arc::default());
        let mut data = [0u8; 2];
        d.read(0xC100_0000 + bar2::DISPI, &mut data);
        let id = u16::from_le_bytes(data);
        assert_eq!(
            id & 0xFFF0,
            0xB0C0,
            "QemuVideoDxe checks exactly this and gives up with EFI_DEVICE_ERROR otherwise"
        );
    }

    #[test]
    fn the_framebuffer_slot_is_published_only_once_decoding_is_on() {
        let mapper = Arc::new(RecordingMapper::default());
        let mut d = display(Arc::clone(&mapper));
        assert_eq!(
            mapper.mapped_at.load(Ordering::Relaxed),
            0,
            "nothing is mapped before the guest enables memory decoding"
        );

        // Firmware sizes the BAR by writing all-ones and reading the mask
        // back. Following *that* would map the framebuffer over the top of
        // the address space, so it must not be published yet.
        d.set_bar_base(0, 0xFFFF_FFFF & !0xF);
        assert_eq!(mapper.mapped_at.load(Ordering::Relaxed), 0);

        d.set_bar_base(0, 0xD000_0000);
        d.set_memory_decode(true);
        assert_eq!(mapper.mapped_at.load(Ordering::Relaxed), 0xD000_0000);

        // Idempotent: the command register is written more than once.
        let before = mapper.remaps.load(Ordering::Relaxed);
        d.set_memory_decode(true);
        assert_eq!(mapper.remaps.load(Ordering::Relaxed), before);

        d.set_memory_decode(false);
        assert_eq!(
            mapper.mapped_at.load(Ordering::Relaxed),
            0,
            "the slot is withdrawn when the guest stops decoding"
        );
    }

    #[test]
    fn a_mode_set_is_visible_to_the_capture_side_only_when_enabled() {
        let mut d = display(Arc::default());
        let fb = d.framebuffer();
        assert!(fb.snapshot().is_none(), "no mode has been programmed");

        // The order QemuVideoDxe writes them in.
        dispi_write(&mut d, dispi::ENABLE, 0);
        dispi_write(&mut d, dispi::BANK, 0);
        dispi_write(&mut d, dispi::X_OFFSET, 0);
        dispi_write(&mut d, dispi::Y_OFFSET, 0);
        dispi_write(&mut d, dispi::BPP, 32);
        dispi_write(&mut d, dispi::XRES, 800);
        dispi_write(&mut d, dispi::VIRT_WIDTH, 800);
        dispi_write(&mut d, dispi::YRES, 600);
        dispi_write(&mut d, dispi::VIRT_HEIGHT, 600);
        assert!(
            fb.snapshot().is_none(),
            "the mode is not live until VBE_DISPI_ENABLED is set"
        );

        dispi_write(
            &mut d,
            dispi::ENABLE,
            dispi::ENABLED | dispi::LFB_ENABLED | dispi::NOCLEARMEM,
        );
        let (mode, pixels) = fb.snapshot().expect("a mode is programmed");
        assert_eq!(
            (mode.width, mode.height, mode.bits_per_pixel),
            (800, 600, 32)
        );
        assert_eq!(mode.stride, 800 * 4);
        assert_eq!(pixels.len(), 800 * 600 * 4);
    }

    #[test]
    fn what_the_guest_writes_is_what_the_capture_reads() {
        let mut d = display(Arc::default());
        let fb = d.framebuffer();
        dispi_write(&mut d, dispi::BPP, 32);
        dispi_write(&mut d, dispi::XRES, 4);
        dispi_write(&mut d, dispi::VIRT_WIDTH, 4);
        dispi_write(&mut d, dispi::YRES, 2);
        dispi_write(&mut d, dispi::ENABLE, dispi::ENABLED | dispi::LFB_ENABLED);

        // The guest writes into the mapping directly; that is the whole
        // point of the device, so the test does the same.
        // SAFETY: eight pixels at the base of a 16 MiB mapping.
        unsafe {
            let p = fb.memory.host_addr() as *mut u8;
            for i in 0..4 * 2 * 4 {
                *p.add(i) = i as u8;
            }
        }
        let (_, pixels) = fb.snapshot().expect("a mode is programmed");
        assert_eq!(pixels[..8], [0, 1, 2, 3, 4, 5, 6, 7]);
    }

    #[test]
    fn enabling_a_mode_clears_the_framebuffer_unless_told_not_to() {
        let mut d = display(Arc::default());
        let fb = d.framebuffer();
        // SAFETY: one pixel at the base of a 16 MiB mapping.
        unsafe {
            std::ptr::write_bytes(fb.memory.host_addr() as *mut u8, 0xAB, 16);
        }
        dispi_write(&mut d, dispi::BPP, 32);
        dispi_write(&mut d, dispi::XRES, 2);
        dispi_write(&mut d, dispi::VIRT_WIDTH, 2);
        dispi_write(&mut d, dispi::YRES, 2);
        dispi_write(&mut d, dispi::ENABLE, dispi::ENABLED);
        let (_, pixels) = fb.snapshot().expect("a mode is programmed");
        assert!(pixels.iter().all(|&b| b == 0), "the mode set cleared it");
    }
}
