//! The platform devices a vCPU exit lands in (§1.4, §2.1).
//!
//! §1.4 puts the LAPIC in the kernel and the I/O APIC in userspace, and §2.1
//! puts PCI config space behind ECAM. Both of those mean the vCPU thread
//! leaves the kernel and something here has to answer. So does every port
//! the firmware touches on its way up — the UART it prints on, the CMOS it
//! sizes memory from, the POST code it stamps its progress into.
//!
//! # Scope
//!
//! This is the *platform*, not the virtio devices. Nothing here is a §5–§9
//! datapath; those hang off the ECAM functions this module dispatches to.
//! What it has to be is complete enough that firmware never faults on a
//! device it reasonably expects, because a missing device does not announce
//! itself — the guest simply reads all-ones, believes it, and wanders off.
//! Every access that reaches no device is therefore *counted*, and the
//! counts are printed at shutdown: an unexplained hang has somewhere to
//! start.

use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

use crate::memory;
use crate::pci::PciBus;

/// Where firmware and guest output goes.
///
/// Both the 16550 at `0x3F8` and edk2's debug port at `0x402` feed this, and
/// deliberately so: which one a given OVMF build uses is a compile-time
/// choice of that build, not something the platform can know. Capturing both
/// costs one branch and removes an entire class of "it boots but prints
/// nothing" investigation.
#[derive(Default)]
pub struct SerialLog {
    inner: Mutex<SerialInner>,
}

#[derive(Default)]
struct SerialInner {
    /// Everything written, for tests and for the console.
    all: Vec<u8>,
    /// The current partial line, so the log emits whole lines.
    line: Vec<u8>,
    truncated: u64,
}

/// Cap on retained output. Firmware is chatty and a guest can be chattier;
/// the log is a diagnostic, not a transcript store.
const SERIAL_CAPACITY: usize = 1 << 20;

impl SerialLog {
    pub fn new() -> Arc<Self> {
        Arc::new(SerialLog::default())
    }

    pub fn push(&self, byte: u8) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        if inner.all.len() < SERIAL_CAPACITY {
            inner.all.push(byte);
        } else {
            inner.truncated += 1;
        }
        match byte {
            b'\n' => {
                let line = String::from_utf8_lossy(&inner.line).trim_end().to_string();
                inner.line.clear();
                if !line.is_empty() {
                    log::info!("guest: {line}");
                }
            }
            b'\r' => {}
            _ => inner.line.push(byte),
        }
    }

    /// Everything written so far.
    pub fn contents(&self) -> Vec<u8> {
        self.inner.lock().map(|i| i.all.clone()).unwrap_or_default()
    }

    pub fn text(&self) -> String {
        String::from_utf8_lossy(&self.contents()).into_owned()
    }

    pub fn len(&self) -> usize {
        self.inner.lock().map(|i| i.all.len()).unwrap_or(0)
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Flush a trailing partial line, at shutdown.
    pub fn flush(&self) {
        let Ok(mut inner) = self.inner.lock() else {
            return;
        };
        if !inner.line.is_empty() {
            let line = String::from_utf8_lossy(&inner.line).trim_end().to_string();
            inner.line.clear();
            if !line.is_empty() {
                log::info!("guest: {line}");
            }
        }
        if inner.truncated > 0 {
            log::warn!(
                "guest output exceeded {SERIAL_CAPACITY} bytes; {} byte(s) not retained",
                inner.truncated
            );
        }
    }
}

// ---------------------------------------------------------------------------
// 16550A UART
// ---------------------------------------------------------------------------

/// The console UART at `0x3F8` (COM1).
///
/// Transmit only, in the sense that there is nothing to receive from yet —
/// but the *register file* is complete, because firmware does not merely
/// write characters. It sets the divisor latch, reads back LCR, checks the
/// scratch register to decide whether a UART is present at all, and above
/// all polls LSR waiting for the transmitter to drain. A UART that answers
/// the write but not the poll is worse than no UART: the firmware spins
/// forever in a loop that looks, from outside, exactly like a hung guest.
pub struct Uart16550 {
    log: Arc<SerialLog>,
    divisor: u16,
    interrupt_enable: u8,
    line_control: u8,
    modem_control: u8,
    scratch: u8,
    bytes: u64,
}

/// Line Status Register bits. `THRE | TEMT` is the pair that matters: both
/// set says "the transmitter is idle, send another", which is the answer
/// that keeps firmware moving.
#[cfg_attr(not(test), allow(dead_code))]
const LSR_DATA_READY: u8 = 0x01;
const LSR_THR_EMPTY: u8 = 0x20;
const LSR_TRANSMITTER_EMPTY: u8 = 0x40;
/// Divisor Latch Access Bit, LCR bit 7.
const LCR_DLAB: u8 = 0x80;

impl Uart16550 {
    pub const BASE: u16 = 0x3F8;
    pub const LEN: u16 = 8;
    /// Register 0 with DLAB clear: the transmit holding register. This is
    /// the only one a guest ever writes repeatedly in a single exit.
    pub const TRANSMIT: u16 = 0;

    pub fn new(log: Arc<SerialLog>) -> Self {
        Uart16550 {
            log,
            // 115200 baud, which is what the divisor 1 means at the 16550's
            // 1.8432 MHz reference clock. Nothing here is timed, but
            // firmware reads the value back and a zero would be nonsense.
            divisor: 1,
            interrupt_enable: 0,
            line_control: 0x03, // 8N1
            modem_control: 0,
            scratch: 0,
            bytes: 0,
        }
    }

    pub fn bytes_written(&self) -> u64 {
        self.bytes
    }

    fn dlab(&self) -> bool {
        self.line_control & LCR_DLAB != 0
    }

    fn read(&mut self, offset: u16) -> u8 {
        match (offset, self.dlab()) {
            (0, true) => self.divisor as u8,
            (1, true) => (self.divisor >> 8) as u8,
            // Nothing ever arrives: there is no input device behind this
            // yet, and LSR says so by never setting DATA_READY.
            (0, false) => 0,
            (1, false) => self.interrupt_enable,
            // IIR: no interrupt pending, FIFOs enabled.
            (2, _) => 0xC1,
            (3, _) => self.line_control,
            (4, _) => self.modem_control,
            // The transmitter is always idle, because the write already
            // completed by the time the guest resumes.
            (5, _) => LSR_THR_EMPTY | LSR_TRANSMITTER_EMPTY,
            // MSR: DSR, CTS and DCD asserted. Firmware that honours flow
            // control will not transmit without them.
            (6, _) => 0xB0,
            (7, _) => self.scratch,
            _ => 0,
        }
    }

    fn write(&mut self, offset: u16, value: u8) {
        match (offset, self.dlab()) {
            (0, true) => self.divisor = (self.divisor & 0xFF00) | u16::from(value),
            (1, true) => self.divisor = (self.divisor & 0x00FF) | (u16::from(value) << 8),
            (0, false) => {
                self.log.push(value);
                self.bytes += 1;
            }
            (1, false) => self.interrupt_enable = value,
            // FCR: the FIFO is imaginary and always empty, so enabling or
            // clearing it changes nothing observable.
            (2, _) => {}
            (3, _) => self.line_control = value,
            (4, _) => self.modem_control = value,
            (5 | 6, _) => {} // LSR and MSR are read-only.
            (7, _) => self.scratch = value,
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// CMOS / RTC
// ---------------------------------------------------------------------------

/// The MC146818 CMOS at `0x70`/`0x71`.
///
/// This exists for one reason that is not obvious until firmware refuses to
/// boot without it: **edk2's OVMF sizes guest RAM from CMOS** when it cannot
/// find QEMU's `fw_cfg`. `OvmfPkg/.../MemDetect.c` reads registers `0x34` and
/// `0x35` for memory between 16 MiB and 4 GiB, and `0x5B`–`0x5D` for memory
/// above 4 GiB, both in 64 KiB units. A machine that answers zero to those is
/// a machine with 16 MiB of RAM as far as the firmware is concerned, and it
/// will not get far on that.
///
/// So these registers are not decoration: they are how §1.3's memory map is
/// communicated to §3.1's firmware, in the absence of the `fw_cfg` channel
/// this hypervisor deliberately does not implement.
pub struct Cmos {
    index: u8,
    /// Non-volatile RAM, including the memory-sizing registers.
    ram: [u8; 128],
    /// Set when the guest selects an index with NMI disabled (bit 7).
    nmi_disabled: bool,
}

impl Cmos {
    pub const INDEX_PORT: u16 = 0x70;
    pub const DATA_PORT: u16 = 0x71;

    /// Build the CMOS for a machine with `below_4g` and `above_4g` bytes of
    /// RAM.
    pub fn new(below_4g: u64, above_4g: u64) -> Self {
        let mut ram = [0u8; 128];

        // 0x34/0x35: (below 4 GiB - 16 MiB) in 64 KiB units, little-endian.
        // The 16 MiB is not a fudge — MemDetect.c adds SIZE_16MB back, so
        // subtracting it here is what makes the round trip exact.
        let below = below_4g.saturating_sub(16 * memory::MIB) >> 16;
        ram[0x34] = below as u8;
        ram[0x35] = (below >> 8) as u8;

        // 0x5B..0x5D: above 4 GiB, also in 64 KiB units, 24 bits.
        let above = above_4g >> 16;
        ram[0x5B] = above as u8;
        ram[0x5C] = (above >> 8) as u8;
        ram[0x5D] = (above >> 16) as u8;

        // Status registers. B: 24-hour mode, binary (not BCD) values — the
        // simpler of the two encodings and the one the A/B pair advertises.
        ram[0x0A] = 0x26; // divider on, 1024 Hz rate
        ram[0x0B] = 0x02 | 0x04; // 24-hour, binary
        ram[0x0C] = 0x00; // no interrupt pending
        ram[0x0D] = 0x80; // battery good
                          // 0x0F, the shutdown status byte: 0 means a normal power-on, which
                          // is what stops firmware from taking a resume path.
        ram[0x0F] = 0x00;
        // Equipment byte: no floppy, 80-column display.
        ram[0x14] = 0x05;

        Cmos {
            index: 0,
            ram,
            nmi_disabled: false,
        }
    }

    /// Fill in the real-time clock registers from the host clock.
    ///
    /// Computed on read rather than stored, because a guest that reads the
    /// time twice a second apart should see it advance.
    fn time_register(&self, index: u8) -> Option<u8> {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let (days, rem) = (now / 86_400, now % 86_400);
        let (hour, minute, second) = (rem / 3600, (rem % 3600) / 60, rem % 60);
        // Civil date from a Unix day number, Howard Hinnant's algorithm.
        let z = days as i64 + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z.rem_euclid(146_097);
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let y = yoe + era * 400;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        let y = if m <= 2 { y + 1 } else { y };

        Some(match index {
            0x00 => second as u8,
            0x02 => minute as u8,
            0x04 => hour as u8,
            0x06 => (days % 7 + 1) as u8,
            0x07 => d as u8,
            0x08 => m as u8,
            0x09 => (y % 100) as u8,
            0x32 => (y / 100) as u8,
            _ => return None,
        })
    }

    fn read_data(&mut self) -> u8 {
        let index = self.index & 0x7F;
        if let Some(value) = self.time_register(index) {
            return value;
        }
        self.ram.get(index as usize).copied().unwrap_or(0)
    }

    fn write_data(&mut self, value: u8) {
        let index = (self.index & 0x7F) as usize;
        // The time registers are derived from the host clock, so a guest
        // setting them is accepted and ignored rather than corrupting the
        // table it will read back.
        if matches!(index, 0x00 | 0x02 | 0x04 | 0x06 | 0x07 | 0x08 | 0x09 | 0x32) {
            return;
        }
        if let Some(slot) = self.ram.get_mut(index) {
            *slot = value;
        }
    }
}

// ---------------------------------------------------------------------------
// I/O APIC
// ---------------------------------------------------------------------------

/// The userspace I/O APIC §1.4 requires.
///
/// `KVM_CAP_SPLIT_IRQCHIP` puts the LAPIC in the kernel and leaves this one
/// out here, so its MMIO window traps to us. Nothing routes interrupts
/// through it yet — every §1.4 interrupt is MSI-X, which goes straight to
/// the LAPIC — but the register file must exist, because firmware reads the
/// version register to count redirection entries and writes the table to
/// mask them. A window that faults would stop the boot before any device
/// needed an interrupt at all.
pub struct IoApic {
    select: u32,
    id: u32,
    /// 24 redirection entries, two 32-bit halves each.
    redirection: [u32; 48],
}

impl Default for IoApic {
    fn default() -> Self {
        IoApic {
            select: 0,
            id: 0,
            // Masked, which is the architectural reset state.
            redirection: [0x0001_0000; 48],
        }
    }
}

impl IoApic {
    pub const BASE: u64 = memory::IOAPIC_BASE;
    pub const LEN: u64 = memory::IOAPIC_SIZE;
    /// §1.4: 24 GSIs.
    const ENTRIES: u32 = 24;

    fn read(&mut self, offset: u64) -> u32 {
        match offset {
            0x00 => self.select,
            0x10 => match self.select {
                0x00 => self.id,
                // Version 0x11, and the count of entries *minus one* in
                // bits 23:16 — an off-by-one the specification really does
                // require.
                0x01 => 0x11 | ((Self::ENTRIES - 1) << 16),
                0x02 => self.id,
                reg @ 0x10..=0x3F => self
                    .redirection
                    .get((reg - 0x10) as usize)
                    .copied()
                    .unwrap_or(0),
                _ => 0,
            },
            _ => 0,
        }
    }

    fn write(&mut self, offset: u64, value: u32) {
        match offset {
            0x00 => self.select = value & 0xFF,
            0x10 => match self.select {
                0x00 | 0x02 => self.id = value & 0x0F00_0000,
                reg @ 0x10..=0x3F => {
                    if let Some(slot) = self.redirection.get_mut((reg - 0x10) as usize) {
                        *slot = value;
                    }
                }
                _ => {}
            },
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// The dispatcher
// ---------------------------------------------------------------------------

/// Ports that are answered deliberately rather than by the catch-all.
mod port {
    /// edk2's debug console. A build compiled with `DEBUG_ON_SERIAL_PORT`
    /// off sends its `DEBUG()` output here instead of to the UART.
    pub const EDK2_DEBUG: u16 = 0x402;
    /// What edk2 expects to read back from `EDK2_DEBUG` to conclude the
    /// debug port is present.
    pub const EDK2_DEBUG_READBACK: u8 = 0xE9;
    /// POST code. Firmware stamps its progress here; on a hang it is the
    /// single most useful byte in the machine.
    pub const POST: u16 = 0x80;
    /// PCI configuration address and data (the pre-ECAM mechanism). OVMF
    /// uses this before it has an ECAM window, so §2.1's ECAM is necessary
    /// but not sufficient.
    pub const PCI_CONFIG_ADDRESS: u16 = 0xCF8;
    pub const PCI_CONFIG_DATA: u16 = 0xCFC;
    /// QEMU's `fw_cfg`. Deliberately absent — see [`DeviceModel`] — but
    /// named so the probe is recognised rather than counted as a mystery.
    pub const FW_CFG_SELECTOR: u16 = 0x510;
    pub const FW_CFG_DATA: u16 = 0x511;
}

/// Everything a vCPU exit can land in.
/// Somewhere to send an MSI-X message.
///
/// The message is whatever the guest wrote into the device's MSI-X table;
/// nothing here interprets it. Delivering it is the platform's job because
/// only the platform holds the VM handle.
pub trait MsiSender: Send + Sync {
    fn signal(&self, address: u64, data: u32);
}

/// An MMIO device the platform model routes to but does not know about.
///
/// `DeviceModel` lives in this crate and virtio devices live above it, so
/// the platform cannot name them. This is the seam: a device claims an
/// address range and answers reads and writes in it.
pub trait MmioDevice: Send {
    /// A short name, for the unhandled-access report.
    fn name(&self) -> &'static str;
    /// Does this device answer for `addr`?
    fn claims(&self, addr: u64) -> bool;
    fn read(&mut self, addr: u64, data: &mut [u8]);
    fn write(&mut self, addr: u64, data: &[u8]);
}

pub struct DeviceModel {
    pub pci: PciBus,
    pub serial: Uart16550,
    pub cmos: Cmos,
    pub ioapic: IoApic,
    log: Arc<SerialLog>,
    /// Devices claiming their own MMIO ranges, consulted before the
    /// platform's own regions.
    pub mmio_devices: Vec<Box<dyn MmioDevice>>,
    pci_config_address: u32,
    post_code: u8,
    /// Accesses that reached no device, by address, with a count. Firmware
    /// hangs are usually a device that is not there; this is the evidence.
    unhandled_io: BTreeMap<u16, u64>,
    unhandled_mmio: BTreeMap<u64, u64>,
}

impl DeviceModel {
    pub fn new(pci: PciBus, below_4g: u64, above_4g: u64, log: Arc<SerialLog>) -> Self {
        DeviceModel {
            pci,
            serial: Uart16550::new(Arc::clone(&log)),
            cmos: Cmos::new(below_4g, above_4g),
            ioapic: IoApic::default(),
            log,
            mmio_devices: Vec::new(),
            pci_config_address: 0,
            post_code: 0,
            unhandled_io: BTreeMap::new(),
            unhandled_mmio: BTreeMap::new(),
        }
    }

    pub fn post_code(&self) -> u8 {
        self.post_code
    }

    /// The ECAM offset the legacy `0xCF8` address register currently selects,
    /// or `None` if the enable bit is clear.
    fn cf8_target(&self, byte_offset: u16) -> Option<u64> {
        if self.pci_config_address & 0x8000_0000 == 0 {
            return None;
        }
        let address = u64::from(self.pci_config_address);
        let bus = (address >> 16) & 0xFF;
        let device = (address >> 11) & 0x1F;
        let function = (address >> 8) & 0x07;
        let register = (address & 0xFC) | u64::from(byte_offset);
        Some((bus << 20) | (device << 15) | (function << 12) | register)
    }

    pub fn io_read(&mut self, port: u16, data: &mut [u8]) {
        let len = data.len();
        match port {
            p if (Uart16550::BASE..Uart16550::BASE + Uart16550::LEN).contains(&p) => {
                data[0] = self.serial.read(p - Uart16550::BASE);
                return;
            }
            Cmos::INDEX_PORT => {
                // The index register reads back the NMI bit only; real
                // hardware does not return the selected index.
                data[0] = if self.cmos.nmi_disabled { 0x80 } else { 0x00 };
                return;
            }
            Cmos::DATA_PORT => {
                data[0] = self.cmos.read_data();
                return;
            }
            port::POST => {
                data[0] = self.post_code;
                return;
            }
            // edk2's PlatformDebugLibIoPort reads this port and only emits
            // debug output if it reads back 0xE9. Answering 0xFF — the
            // bus's "nobody home" — silently disables firmware logging on
            // the one port that would tell us why a boot failed.
            port::EDK2_DEBUG => {
                data[0] = port::EDK2_DEBUG_READBACK;
                return;
            }
            port::PCI_CONFIG_ADDRESS => {
                fill(data, u64::from(self.pci_config_address));
                return;
            }
            p if (port::PCI_CONFIG_DATA..port::PCI_CONFIG_DATA + 4).contains(&p) => {
                let value = match self.cf8_target(p - port::PCI_CONFIG_DATA) {
                    Some(offset) => self.pci.config_rw(offset, len, None),
                    // No function selected reads as all-ones, the same
                    // answer an absent device gives.
                    None => u64::MAX,
                };
                fill(data, value);
                return;
            }
            // fw_cfg is not implemented. Answering all-ones is what makes
            // OVMF's signature check fail cleanly and fall back to CMOS,
            // which is the path this platform actually supports.
            port::FW_CFG_SELECTOR | port::FW_CFG_DATA => {
                data.fill(0xFF);
                return;
            }
            _ => {}
        }
        *self.unhandled_io.entry(port).or_insert(0) += 1;
        // All-ones is the bus's answer for "nothing responded", and is what
        // lets a probing guest conclude the device is absent.
        data.fill(0xFF);
    }

    pub fn io_write(&mut self, port: u16, data: &[u8]) {
        let value = value_of(data);
        match port {
            p if (Uart16550::BASE..Uart16550::BASE + Uart16550::LEN).contains(&p) => {
                let register = p - Uart16550::BASE;
                if register == Uart16550::TRANSMIT {
                    // KVM flattens a repeated string instruction into one
                    // exit: `kvm_run.io` carries `count * size` bytes and
                    // kvm-ioctls hands them over as a single slice. A
                    // `rep outsb` to the transmit register is therefore
                    // several characters, and taking only `data[0]` drops
                    // the rest of the line.
                    for byte in data {
                        self.serial.write(register, *byte);
                    }
                } else {
                    // Every other UART register is one byte wide; a wider
                    // access is the guest's mistake, not ours to invent.
                    self.serial.write(register, data[0]);
                }
                return;
            }
            Cmos::INDEX_PORT => {
                self.cmos.nmi_disabled = data[0] & 0x80 != 0;
                self.cmos.index = data[0] & 0x7F;
                return;
            }
            Cmos::DATA_PORT => {
                self.cmos.write_data(data[0]);
                return;
            }
            port::EDK2_DEBUG => {
                for byte in data {
                    self.log.push(*byte);
                }
                return;
            }
            port::POST => {
                self.post_code = data[0];
                log::trace!("POST {:#04x}", data[0]);
                return;
            }
            port::PCI_CONFIG_ADDRESS => {
                self.pci_config_address = value as u32;
                return;
            }
            p if (port::PCI_CONFIG_DATA..port::PCI_CONFIG_DATA + 4).contains(&p) => {
                if let Some(offset) = self.cf8_target(p - port::PCI_CONFIG_DATA) {
                    self.pci.config_rw(offset, data.len(), Some(value));
                }
                return;
            }
            port::FW_CFG_SELECTOR | port::FW_CFG_DATA => return,
            _ => {}
        }
        *self.unhandled_io.entry(port).or_insert(0) += 1;
    }

    pub fn mmio_read(&mut self, addr: u64, data: &mut [u8]) {
        if let Some(d) = self.mmio_devices.iter_mut().find(|d| d.claims(addr)) {
            d.read(addr, data);
            return;
        }
        if (memory::ECAM_BASE..memory::ECAM_BASE + memory::ECAM_SIZE).contains(&addr) {
            let value = self
                .pci
                .config_rw(addr - memory::ECAM_BASE, data.len(), None);
            fill(data, value);
            return;
        }
        if (IoApic::BASE..IoApic::BASE + IoApic::LEN).contains(&addr) {
            fill(data, u64::from(self.ioapic.read(addr - IoApic::BASE)));
            return;
        }
        *self.unhandled_mmio.entry(addr).or_insert(0) += 1;
        // Zero, not all-ones: an MMIO region with no device is more often a
        // gap in a BAR window than a probe, and a guest reading all-ones
        // from a device it believes in tends to interpret it as a value.
        data.fill(0);
    }

    pub fn mmio_write(&mut self, addr: u64, data: &[u8]) {
        if let Some(d) = self.mmio_devices.iter_mut().find(|d| d.claims(addr)) {
            d.write(addr, data);
            return;
        }
        if (memory::ECAM_BASE..memory::ECAM_BASE + memory::ECAM_SIZE).contains(&addr) {
            self.pci
                .config_rw(addr - memory::ECAM_BASE, data.len(), Some(value_of(data)));
            return;
        }
        if (IoApic::BASE..IoApic::BASE + IoApic::LEN).contains(&addr) {
            self.ioapic
                .write(addr - IoApic::BASE, value_of(data) as u32);
            return;
        }
        *self.unhandled_mmio.entry(addr).or_insert(0) += 1;
    }

    /// What the guest touched that nothing answered, worst first.
    ///
    /// Printed at shutdown. A boot that stops for no visible reason has
    /// almost always just read all-ones from something it needed.
    pub fn unhandled_report(&self) -> Vec<String> {
        let mut io: Vec<_> = self.unhandled_io.iter().collect();
        io.sort_by_key(|(_, count)| std::cmp::Reverse(**count));
        let mut mmio: Vec<_> = self.unhandled_mmio.iter().collect();
        mmio.sort_by_key(|(_, count)| std::cmp::Reverse(**count));

        let mut out = Vec::new();
        for (port, count) in io.iter().take(12) {
            out.push(format!("port {port:#06x} x{count}"));
        }
        for (addr, count) in mmio.iter().take(12) {
            out.push(format!("mmio {addr:#012x} x{count}"));
        }
        out
    }
}

/// Little-endian read of an exit's data buffer.
fn value_of(data: &[u8]) -> u64 {
    let mut bytes = [0u8; 8];
    let n = data.len().min(8);
    bytes[..n].copy_from_slice(&data[..n]);
    u64::from_le_bytes(bytes)
}

/// Little-endian write into an exit's data buffer.
fn fill(data: &mut [u8], value: u64) {
    let bytes = value.to_le_bytes();
    let n = data.len().min(8);
    data[..n].copy_from_slice(&bytes[..n]);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_uart_reports_an_idle_transmitter_so_firmware_does_not_spin() {
        // Firmware polls LSR until THRE and TEMT are both set. A UART that
        // never sets them is indistinguishable from a hung guest.
        let mut uart = Uart16550::new(SerialLog::new());
        let lsr = uart.read(5);
        assert_ne!(lsr & LSR_THR_EMPTY, 0, "THRE must be set");
        assert_ne!(lsr & LSR_TRANSMITTER_EMPTY, 0, "TEMT must be set");
        assert_eq!(lsr & LSR_DATA_READY, 0, "nothing has been received");
    }

    #[test]
    fn a_byte_written_to_the_uart_reaches_the_log() {
        let log = SerialLog::new();
        let mut uart = Uart16550::new(Arc::clone(&log));
        for byte in b"hi\n" {
            uart.write(0, *byte);
        }
        assert_eq!(log.text(), "hi\n");
        assert_eq!(uart.bytes_written(), 3);
    }

    #[test]
    fn the_divisor_latch_hides_the_transmit_register() {
        // With DLAB set, offset 0 is the divisor low byte, not the
        // transmitter — writing a character there must not print it.
        let log = SerialLog::new();
        let mut uart = Uart16550::new(Arc::clone(&log));
        uart.write(3, LCR_DLAB);
        uart.write(0, 0x0C);
        uart.write(1, 0x00);
        assert_eq!(uart.divisor, 0x000C);
        assert!(log.is_empty(), "a divisor write is not console output");
        uart.write(3, 0x03);
        uart.write(0, b'x');
        assert_eq!(log.text(), "x");
    }

    #[test]
    fn cmos_reports_low_memory_the_way_ovmf_reads_it() {
        // OvmfPkg MemDetect.c: ((0x35 << 8 | 0x34) << 16) + 16 MiB.
        let below = 1024 * memory::MIB;
        let mut cmos = Cmos::new(below, 0);
        cmos.index = 0x34;
        let low = cmos.read_data();
        cmos.index = 0x35;
        let high = cmos.read_data();
        let decoded = ((u64::from(high) << 8 | u64::from(low)) << 16) + 16 * memory::MIB;
        assert_eq!(
            decoded, below,
            "firmware must read back exactly the RAM below 4 GiB"
        );
    }

    #[test]
    fn cmos_reports_high_memory_the_way_ovmf_reads_it() {
        // MemDetect.c: (0x5d << 16 | 0x5c << 8 | 0x5b) << 16.
        let above = 6 * 1024 * memory::MIB;
        let mut cmos = Cmos::new(1024 * memory::MIB, above);
        let mut byte = |index: u8| {
            cmos.index = index;
            u64::from(cmos.read_data())
        };
        let decoded = ((byte(0x5D) << 16) | (byte(0x5C) << 8) | byte(0x5B)) << 16;
        assert_eq!(decoded, above);
    }

    #[test]
    fn the_ioapic_advertises_the_twenty_four_gsis_of_1_4() {
        let mut ioapic = IoApic::default();
        ioapic.write(0x00, 0x01); // select the version register
        let version = ioapic.read(0x10);
        assert_eq!(version & 0xFF, 0x11, "I/O APIC version");
        assert_eq!(
            ((version >> 16) & 0xFF) + 1,
            crate::kvm::SPLIT_IRQCHIP_GSI_COUNT,
            "the entry count is stored minus one"
        );
    }

    #[test]
    fn legacy_cf8_config_reaches_the_same_function_as_ecam() {
        use crate::pci::{Bdf, PciFunction};
        let mut bus = PciBus::new();
        bus.insert(PciFunction::new(
            Bdf::new(0, 0, 0),
            0x8086,
            0x29C0,
            0x00_06_00_00,
            0,
        ));
        let mut devices = DeviceModel::new(bus, 1024 * memory::MIB, 0, SerialLog::new());

        // 0x8000_0000 | bus 0 | device 0 | function 0 | register 0.
        devices.io_write(port::PCI_CONFIG_ADDRESS, &0x8000_0000u32.to_le_bytes());
        let mut data = [0u8; 4];
        devices.io_read(port::PCI_CONFIG_DATA, &mut data);
        assert_eq!(
            u32::from_le_bytes(data),
            0x29C0_8086,
            "vendor and device ID through the legacy port"
        );

        // The same register through ECAM must agree.
        let mut ecam = [0u8; 4];
        devices.mmio_read(memory::ECAM_BASE, &mut ecam);
        assert_eq!(ecam, data, "ECAM and CF8 must describe one config space");
    }

    #[test]
    fn an_absent_function_reads_all_ones_through_the_legacy_port() {
        let mut devices = DeviceModel::new(PciBus::new(), 1024 * memory::MIB, 0, SerialLog::new());
        devices.io_write(port::PCI_CONFIG_ADDRESS, &0x8000_F800u32.to_le_bytes());
        let mut data = [0u8; 4];
        devices.io_read(port::PCI_CONFIG_DATA, &mut data);
        assert_eq!(u32::from_le_bytes(data), 0xFFFF_FFFF);
    }

    #[test]
    fn an_access_that_reaches_no_device_is_counted() {
        let mut devices = DeviceModel::new(PciBus::new(), 1024 * memory::MIB, 0, SerialLog::new());
        let mut data = [0u8; 1];
        devices.io_read(0x1234, &mut data);
        devices.io_read(0x1234, &mut data);
        assert_eq!(data[0], 0xFF);
        let report = devices.unhandled_report();
        assert!(
            report
                .iter()
                .any(|line| line.contains("0x1234") && line.contains("x2")),
            "an unanswered port must be counted for the shutdown report: {report:?}"
        );
    }
}
