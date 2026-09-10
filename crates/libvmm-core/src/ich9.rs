//! §3.1 / Revision D.2 — the ICH9 LPC bridge and its ACPI power-management
//! block.
//!
//! This is the one piece of "chipset" the machine presents, and it exists
//! for a single reason: stock OVMF will not start without it.
//!
//! `AcpiTimerLibConstructor` runs before almost anything else in the
//! firmware. It reads the host bridge's device ID at 00:00.0, and for a Q35
//! MCH it then goes to **00:1f.0 offset 0x44** to ask whether the power
//! management I/O base is already decoded, and to **offset 0x40** for the
//! base itself. With no function at 00:1f.0 those reads return all-ones,
//! which the firmware reads as "ACPI_EN is set, PMBASE is 0xFFFFFFFF", and
//! it computes an ACPI timer port of `0xFFFFFFFE + 8`. That is not
//! DWORD-aligned, so the next thing that happens is
//!
//! ```text
//! ASSERT .../IoLibGcc.c(211): ((Port) & 3) == 0
//! ```
//!
//! and the machine stops in PEI, before a single line of firmware output
//! that would explain it. Providing the bridge is what turns that into a
//! boot.
//!
//! Note what this is *not*. §1.4 and the standing platform constraint forbid
//! i440FX and everything behind it: there is no PIIX, no PIC, no PIT, no
//! ISA bus and no INTx. The LPC bridge here is a config-space stub carrying
//! four registers plus the ACPI PM block they point at. Nothing is routed
//! through it.
//!
//! Register numbers are from edk2's `OvmfPkg/Include/IndustryStandard/
//! Q35MchIch9.h` and `OvmfPlatforms.h`, which is the definition that
//! actually matters here — the firmware's, not the silicon's.

use crate::devices::{PmTimer, PowerEvent};
use crate::pci::{Bdf, PciBus, PciFunction};

/// Where the firmware looks. Not configurable: `POWER_MGMT_REGISTER_Q35`
/// hard-codes 0/0x1f/0.
pub const LPC_DEVICE: u8 = 0x1F;
pub const LPC_FUNCTION: u8 = 0;

/// Intel 82801IB (ICH9) LPC Interface Controller — the ID QEMU's `q35`
/// machine presents, and therefore the one OVMF has been tested against.
pub const LPC_VENDOR_ID: u16 = 0x8086;
pub const LPC_DEVICE_ID: u16 = 0x2918;
/// Base 06 (bridge), sub 01 (ISA), prog-if 00.
pub const LPC_CLASS: u32 = 0x00_06_01_00;

// --- configuration-space registers -----------------------------------------

/// Power Management Base Address. Bits 15:7 are the I/O base; bit 0 reads as
/// 1 to mark it an I/O range.
pub const PMBASE: usize = 0x40;
/// The writable bits of `PMBASE` (`ICH9_PMBASE_MASK`).
pub const PMBASE_MASK: u32 = 0x0000_FF80;
/// What OVMF programs when it finds the base disabled (`ICH9_PMBASE_VALUE`).
pub const PMBASE_DEFAULT: u32 = 0x0000_0600;
/// ACPI Control. Bit 7 enables the PMBASE I/O decode.
pub const ACPI_CNTL: usize = 0x44;
pub const ACPI_CNTL_EN: u8 = 0x80;
/// Root Complex Base Address.
pub const RCBA: usize = 0xF0;

// --- I/O registers, as offsets from PMBASE ---------------------------------

pub const PM1_STS: u16 = 0x00;
pub const PM1_EN: u16 = 0x02;
pub const PM1_CNT: u16 = 0x04;
/// The reason this module exists. 24 bits, 3.579545 MHz.
pub const PM_TMR: u16 = 0x08;
pub const GPE0_BASE: u16 = 0x20;
pub const GPE0_LEN: u16 = 0x10;
pub const SMI_EN: u16 = 0x30;
pub const SMI_STS: u16 = 0x34;
/// How much I/O space the block decodes. QEMU's ICH9 claims 0x80.
pub const BLOCK_LEN: u16 = 0x80;

/// `PM1_CNT` sleep-enable, and the sleep type it applies to.
const SLP_EN: u16 = 1 << 13;
const SLP_TYP_SHIFT: u16 = 10;
const SLP_TYP_MASK: u16 = 0b111;
/// S5: soft off.
const SLP_TYP_S5: u16 = 5;

/// The 00:1f.0 function.
///
/// `PMBASE` reads back with bit 0 set because that is how software tells an
/// I/O base from a memory one; OVMF masks it off with `PMBA_RTE` before use.
pub fn lpc_bridge() -> PciFunction {
    let mut f = PciFunction::new(
        Bdf::new(0, LPC_DEVICE, LPC_FUNCTION),
        LPC_VENDOR_ID,
        LPC_DEVICE_ID,
        LPC_CLASS,
        0,
    )
    // 0x40 and 0x44 are PMBASE and ACPI_CNTL on this function, which is
    // exactly where a capability chain would otherwise start. It cannot
    // have both.
    .without_capabilities()
    // Bit 7 marks a multi-function device. Firmware that finds function 0
    // without it will not probe 1..7, and while nothing else lives at
    // device 0x1f today, lying about it would be a trap for whoever adds
    // something.
    .with_header_type(0x80);
    f.write_config_u32(PMBASE, 0x0000_0001);
    f
}

/// The ACPI power-management register block that `PMBASE` points at.
///
/// The base address is deliberately *not* stored here: it lives in the
/// bridge's configuration space, which the firmware writes, and duplicating
/// it would create two answers to one question. [`pmbase`] reads it back.
pub struct AcpiPm {
    timer: PmTimer,
    pm1_sts: u16,
    pm1_en: u16,
    pm1_cnt: u16,
    smi_en: u32,
    smi_sts: u32,
    gpe0: [u8; GPE0_LEN as usize],
    power: Option<PowerEvent>,
}

impl Default for AcpiPm {
    fn default() -> Self {
        Self::new()
    }
}

impl AcpiPm {
    pub fn new() -> Self {
        AcpiPm {
            timer: PmTimer::new(),
            pm1_sts: 0,
            pm1_en: 0,
            pm1_cnt: 0,
            smi_en: 0,
            smi_sts: 0,
            gpe0: [0; GPE0_LEN as usize],
            power: None,
        }
    }

    /// The 24-bit ACPI timer count. See [`PmTimer`].
    pub fn timer(&self) -> u32 {
        self.timer.ticks()
    }

    /// Take the pending power request, if any.
    pub fn take_power_event(&mut self) -> Option<PowerEvent> {
        self.power.take()
    }

    pub fn read(&self, offset: u16, data: &mut [u8]) {
        let value: u32 = match offset {
            PM1_STS => u32::from(self.pm1_sts),
            PM1_EN => u32::from(self.pm1_en),
            PM1_CNT => u32::from(self.pm1_cnt),
            PM_TMR => self.timer(),
            SMI_EN => self.smi_en,
            SMI_STS => self.smi_sts,
            o if (GPE0_BASE..GPE0_BASE + GPE0_LEN).contains(&o) => {
                let at = (o - GPE0_BASE) as usize;
                let mut v = 0u32;
                for i in 0..data.len().min(4) {
                    v |= u32::from(self.gpe0.get(at + i).copied().unwrap_or(0)) << (i * 8);
                }
                v
            }
            // Reads of an unimplemented register in a decoded range are
            // zero, not all-ones: all-ones is what "nothing is here" looks
            // like, and this block *is* here.
            _ => 0,
        };
        for (i, byte) in data.iter_mut().enumerate() {
            *byte = ((value >> (i * 8)) & 0xFF) as u8;
        }
    }

    pub fn write(&mut self, offset: u16, data: &[u8]) {
        let mut value = 0u32;
        for (i, byte) in data.iter().enumerate().take(4) {
            value |= u32::from(*byte) << (i * 8);
        }
        match offset {
            // Status bits are write-one-to-clear, not write-to-set. Storing
            // the written value would latch every event the guest just
            // acknowledged and it would never see another.
            PM1_STS => self.pm1_sts &= !(value as u16),
            PM1_EN => self.pm1_en = value as u16,
            PM1_CNT => {
                self.pm1_cnt = value as u16;
                if value as u16 & SLP_EN != 0 {
                    let typ = (value as u16 >> SLP_TYP_SHIFT) & SLP_TYP_MASK;
                    // SLP_EN is a one-shot: it is not a state the guest can
                    // read back, so clear it here rather than leave the
                    // machine looking permanently asleep.
                    self.pm1_cnt &= !SLP_EN;
                    if typ == SLP_TYP_S5 {
                        log::info!("guest wrote S5 to PM1_CNT: powering off");
                        self.power = Some(PowerEvent::Off);
                    } else {
                        log::warn!("guest requested sleep state S{typ}, which is not implemented");
                    }
                }
            }
            SMI_EN => self.smi_en = value,
            SMI_STS => self.smi_sts &= !value,
            o if (GPE0_BASE..GPE0_BASE + GPE0_LEN).contains(&o) => {
                let at = (o - GPE0_BASE) as usize;
                for (i, byte) in data.iter().take(4).enumerate() {
                    if let Some(slot) = self.gpe0.get_mut(at + i) {
                        // The GPE status half is write-one-to-clear too.
                        *slot &= !byte;
                    }
                }
            }
            _ => {}
        }
    }
}

/// The I/O base the bridge currently decodes, or `None` if `ACPI_CNTL`'s
/// enable bit is clear or the bridge is absent.
pub fn pmbase(pci: &PciBus) -> Option<u16> {
    let f = pci.get(Bdf::new(0, LPC_DEVICE, LPC_FUNCTION))?;
    if f.read(ACPI_CNTL, 1) as u8 & ACPI_CNTL_EN == 0 {
        return None;
    }
    let base = f.read(PMBASE, 4) as u32 & PMBASE_MASK;
    if base == 0 {
        return None;
    }
    Some(base as u16)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reproduces the exact sequence `AcpiTimerLibConstructor` performs, and
    /// asserts the port it computes is the one the ACPI timer answers on.
    #[test]
    fn ovmf_programs_pmbase_and_finds_an_aligned_timer_port() {
        let mut pci = PciBus::new();
        pci.insert(lpc_bridge());

        // Before the firmware runs, the decode is off — which is what makes
        // OVMF program it rather than trust a stale value.
        assert_eq!(pmbase(&pci), None);

        let bdf = Bdf::new(0, LPC_DEVICE, LPC_FUNCTION);
        let f = pci.get_mut(bdf).expect("the bridge must be present");
        // `if ((PciRead8 (AcpiCtlReg) & AcpiEnBit) == 0)`
        assert_eq!(f.read(ACPI_CNTL, 1) as u8 & ACPI_CNTL_EN, 0);
        // `PciAndThenOr32 (Pmba, ~ICH9_PMBASE_MASK, ICH9_PMBASE_VALUE)`
        let pmba = f.read(PMBASE, 4) as u32;
        f.write(PMBASE, 4, u64::from((pmba & !PMBASE_MASK) | PMBASE_DEFAULT));
        // `PciOr8 (AcpiCtlReg, AcpiEnBit)`
        let cntl = f.read(ACPI_CNTL, 1) as u8;
        f.write(ACPI_CNTL, 1, u64::from(cntl | ACPI_CNTL_EN));

        // `mAcpiTimerIoAddr = (PciRead32 (Pmba) & ~PMBA_RTE) + ACPI_TIMER_OFFSET`
        let timer_port = (f.read(PMBASE, 4) as u32 & !1) + 8;
        assert_eq!(
            timer_port & 3,
            0,
            "IoLibGcc.c asserts the timer port is DWORD-aligned; this is the \
             assert that stopped OVMF in PEI"
        );
        assert_eq!(timer_port, 0x0608);
        assert_eq!(pmbase(&pci), Some(0x0600));
        assert_eq!(
            u32::from(pmbase(&pci).unwrap()) + u32::from(PM_TMR),
            timer_port
        );
    }

    #[test]
    fn the_acpi_timer_advances_at_roughly_three_and_a_half_megahertz() {
        let pm = AcpiPm::new();
        let first = pm.timer();
        std::thread::sleep(std::time::Duration::from_millis(50));
        let second = pm.timer();
        // 50 ms is ~179_000 ticks, comfortably short of the 24-bit wrap at
        // ~4.7 s, so the counter cannot have gone backwards.
        let elapsed = second.wrapping_sub(first);
        assert!(
            (100_000..400_000).contains(&elapsed),
            "50 ms should be about 179000 ticks, got {elapsed}; a timer that \
             does not advance hangs every firmware delay loop"
        );
    }

    #[test]
    fn writing_s5_to_pm1_cnt_requests_power_off() {
        let mut pm = AcpiPm::new();
        assert_eq!(pm.take_power_event(), None);
        // SLP_TYP = 5, SLP_EN — what an ACPI `_S5` shutdown writes.
        let cnt = (SLP_TYP_S5 << SLP_TYP_SHIFT) | SLP_EN;
        pm.write(PM1_CNT, &cnt.to_le_bytes());
        assert_eq!(pm.take_power_event(), Some(PowerEvent::Off));
        assert_eq!(pm.take_power_event(), None, "the event is taken once");
    }

    #[test]
    fn status_bits_are_write_one_to_clear() {
        let mut pm = AcpiPm::new();
        pm.pm1_sts = 0xFFFF;
        pm.write(PM1_STS, &0x0001u16.to_le_bytes());
        let mut back = [0u8; 2];
        pm.read(PM1_STS, &mut back);
        assert_eq!(
            u16::from_le_bytes(back),
            0xFFFE,
            "a write must clear the bits it sets, not store the value"
        );
    }
}
