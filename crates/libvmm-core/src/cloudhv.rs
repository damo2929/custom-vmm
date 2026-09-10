//! Revision D.2 option B — the CloudHv platform: a PCIe root complex and
//! nothing else.
//!
//! This is the machine with no chipset. There is no LPC bridge, no PMBASE
//! register to program, no PIIX, no ICH9, no `fw_cfg`, and no A20 gate —
//! stock edk2 skips all of it. What remains is a host bridge with a device
//! ID the firmware recognises, and two I/O registers at fixed addresses.
//!
//! ## Why stock firmware accepts this
//!
//! `OvmfPkg` carries a complete CloudHv path — 21 branches across 11 files —
//! that exists because Cloud Hypervisor presents no chipset either. The
//! branches that matter here:
//!
//! * `AcpiTimerLibConstructor` sets `mAcpiTimerIoAddr` to the constant
//!   [`ACPI_TIMER_IO_ADDRESS`] and returns, instead of reading a PMBASE out
//!   of a bridge at 00:1f.0.
//! * `PlatformMiscInitialization` returns early — no A20 write to port
//!   0x92, no PM base programming.
//! * `PlatformScanE820` calls `PlatformScanE820Pvh`, which takes the memory
//!   map from the PVH `hvm_start_info` this tree already builds, rather than
//!   from `fw_cfg`.
//! * `AcpiPlatformDxe` calls `InstallCloudHvTables`, which walks the XSDT
//!   reached through `hvm_start_info.rsdp_paddr` — again, tables we already
//!   build — instead of `InstallQemuFwCfgTables`.
//! * `ResetSystemLib` writes the sleep request to [`ACPI_SHUTDOWN_IO_ADDRESS`].
//!
//! The firmware selects all of that on one number: the device ID at 00:00.0.
//! It is read once, in `PlatformInitLib`, and saved into a HOB that every
//! later phase reads back.
//!
//! ## Hardware-reduced ACPI
//!
//! The power management here is **not** a cut-down ICH9 PM block, and
//! reusing [`crate::ich9::AcpiPm`] for it would be wrong in a way that
//! happens to compile. ACPI 5.0's hardware-reduced profile replaces the PM1
//! event and control registers with two byte-wide ones — `SLEEP_CONTROL_REG`
//! and `SLEEP_STATUS_REG` — and the sleep type sits in different bits. edk2
//! writes `5 << 2 | 1 << 5` to `0x0600`, which is `SLP_TYP = 5` in bits 4:2
//! and `SLP_EN` in bit 5; in an ICH9 `PM1_CNT` those same bits mean nothing
//! at all, and offset 0 of an ICH9 PM block is `PM1_STS`, which is
//! write-one-to-clear. The same address, the same write, and a completely
//! different meaning.
//!
//! Constants are from `OvmfPkg/Include/IndustryStandard/CloudHv.h`.

use crate::devices::{PmTimer, PowerEvent};
use crate::pci::{Bdf, PciFunction};

/// The device ID that selects the CloudHv path in stock edk2
/// (`CLOUDHV_DEVICE_ID`). The vendor is not checked by the firmware —
/// `OVMF_HOSTBRIDGE_DID` reads the device ID alone — but Intel's is what
/// Cloud Hypervisor presents, and matching it keeps a guest's PCI ID
/// database from reporting something that does not exist.
pub const HOST_BRIDGE_DEVICE_ID: u16 = 0x0D57;
pub const HOST_BRIDGE_VENDOR_ID: u16 = 0x8086;
/// Base 06 (bridge), sub 00 (host), prog-if 00.
pub const HOST_BRIDGE_CLASS: u32 = 0x00_06_00_00;

/// `CLOUDHV_ACPI_SHUTDOWN_IO_ADDRESS`. `SLEEP_CONTROL_REG`, byte wide.
pub const ACPI_SHUTDOWN_IO_ADDRESS: u16 = 0x0600;
/// `SLEEP_STATUS_REG`, immediately after it, as ACPI 5.0 §4.8.3.7 pairs them.
pub const ACPI_SLEEP_STATUS_IO_ADDRESS: u16 = 0x0601;
/// `CLOUDHV_ACPI_TIMER_IO_ADDRESS`. 32 bits at 3.579545 MHz — the width the
/// FADT's `TMR_VAL_EXT` flag declares.
pub const ACPI_TIMER_IO_ADDRESS: u16 = 0x0608;

/// `PM1a_EVT_BLK`: `PM1_STS` then `PM1_EN`, two bytes each.
///
/// A hardware-reduced platform has no PM1 event block — the sleep pair
/// replaces it — and this one exists anyway, for one reason:
///
/// > Windows' nested-Hyper-V hvloader rejects a HW-reduced FADT whose PM1a
/// > GAS is zero; point the blocks at unused ACPI I/O ports (conforming
/// > guests ignore them).
///
/// — Cloud Hypervisor, `vmm/src/acpi.rs`. A conforming guest never looks
/// here, so the registers exist to be *described*, and answering them is
/// cheaper than reasoning about which guest reads what.
pub const PM1A_EVT_IO_ADDRESS: u16 = 0x060C;
/// `PM1a_CNT_BLK`, two bytes. Same reason.
pub const PM1A_CNT_IO_ADDRESS: u16 = 0x0610;

/// The reset register the FADT points `RESET_REG` at.
///
/// Not part of the CloudHv constants — it is the conventional PC reset
/// control port, and the FADT has claimed `RESET_REG_SUP` since Revision
/// D.2 without anything decoding it. A guest that reboots through ACPI
/// wrote to a port nothing answered and then waited forever.
pub const RESET_IO_ADDRESS: u16 = 0x0CF9;
/// Bit 2, `RST_CPU`: the write that actually resets.
const RESET_CPU: u8 = 1 << 2;

/// The whole I/O footprint of this platform: the sleep pair, the timer, and
/// the two PM1 blocks. Decoded as one range so a stray access inside it is
/// answered rather than counted as a mystery.
pub const IO_BASE: u16 = ACPI_SHUTDOWN_IO_ADDRESS;
pub const IO_LEN: u16 = 0x12;

/// `PM1_CNT`: sleep type in bits 12:10, sleep enable in bit 13. Note that
/// these are *not* the `SLEEP_CONTROL_REG` bits — same idea, different
/// place, which is exactly the confusion this module exists to prevent.
const PM1_CNT_SLP_TYP_SHIFT: u16 = 10;
const PM1_CNT_SLP_TYP_MASK: u16 = 0b111;
const PM1_CNT_SLP_EN: u16 = 1 << 13;

/// `SLEEP_CONTROL_REG`: sleep type in bits 4:2, sleep enable in bit 5.
const SLP_TYP_SHIFT: u8 = 2;
const SLP_TYP_MASK: u8 = 0b111;
const SLP_EN: u8 = 1 << 5;
/// S5: soft off.
const SLP_TYP_S5: u8 = 5;

/// The host bridge at 00:00.0, and the entire chipset.
///
/// Capabilities are suppressed for the same reason as on the LPC bridge: a
/// host bridge has no capability list on real hardware, and advertising one
/// would point firmware at 0x40, where there is nothing.
pub fn host_bridge() -> PciFunction {
    PciFunction::new(
        Bdf::new(0, 0, 0),
        HOST_BRIDGE_VENDOR_ID,
        HOST_BRIDGE_DEVICE_ID,
        HOST_BRIDGE_CLASS,
        0,
    )
    .without_capabilities()
}

/// The hardware-reduced ACPI registers at [`IO_BASE`].
pub struct CloudHvPm {
    timer: PmTimer,
    sleep_status: u8,
    pm1_sts: u16,
    pm1_en: u16,
    pm1_cnt: u16,
    power: Option<PowerEvent>,
}

impl Default for CloudHvPm {
    fn default() -> Self {
        Self::new()
    }
}

impl CloudHvPm {
    pub fn new() -> Self {
        CloudHvPm {
            timer: PmTimer::wide(),
            sleep_status: 0,
            pm1_sts: 0,
            pm1_en: 0,
            pm1_cnt: 0,
            power: None,
        }
    }

    /// Does this device answer for `port`?
    pub fn claims(port: u16) -> bool {
        (IO_BASE..IO_BASE + IO_LEN).contains(&port) || port == RESET_IO_ADDRESS
    }

    /// Take the pending power request, if any.
    pub fn take_power_event(&mut self) -> Option<PowerEvent> {
        self.power.take()
    }

    pub fn read(&self, port: u16, data: &mut [u8]) {
        let value: u32 = match port {
            ACPI_SHUTDOWN_IO_ADDRESS => 0,
            ACPI_SLEEP_STATUS_IO_ADDRESS => u32::from(self.sleep_status),
            ACPI_TIMER_IO_ADDRESS => self.timer.ticks(),
            PM1A_EVT_IO_ADDRESS => u32::from(self.pm1_sts) | (u32::from(self.pm1_en) << 16),
            PM1A_CNT_IO_ADDRESS => u32::from(self.pm1_cnt),
            // Reading the reset register back gives what was last written;
            // real hardware keeps the other bits of it.
            RESET_IO_ADDRESS => 0,
            // Inside the decoded range but not a register. Zero, not
            // all-ones: all-ones is what "nothing is here" looks like, and
            // this platform *is* here.
            _ => 0,
        };
        for (i, byte) in data.iter_mut().enumerate() {
            *byte = ((value >> (i * 8)) & 0xFF) as u8;
        }
    }

    pub fn write(&mut self, port: u16, data: &[u8]) {
        let Some(&value) = data.first() else {
            return;
        };
        match port {
            ACPI_SHUTDOWN_IO_ADDRESS => {
                if value & SLP_EN == 0 {
                    // Setting the sleep type without the enable bit is
                    // legal and does nothing; ACPI writes both together.
                    return;
                }
                let typ = (value >> SLP_TYP_SHIFT) & SLP_TYP_MASK;
                if typ == SLP_TYP_S5 {
                    log::info!("guest requested S5 through SLEEP_CONTROL_REG: powering off");
                    self.power = Some(PowerEvent::Off);
                } else {
                    log::warn!("guest requested sleep state S{typ}, which is not implemented");
                }
                // ACPI 5.0 §4.8.3.7: entering a sleep state sets the
                // corresponding status bit. Nothing here ever wakes back up
                // to read it, but a guest that polls for the transition it
                // just asked for should see it happen.
                self.sleep_status |= SLP_EN;
            }
            // Write-one-to-clear, like every ACPI status register.
            ACPI_SLEEP_STATUS_IO_ADDRESS => self.sleep_status &= !value,
            PM1A_EVT_IO_ADDRESS => {
                let word = word_of(data);
                self.pm1_sts &= !word;
            }
            PM1A_CNT_IO_ADDRESS => {
                let word = word_of(data);
                self.pm1_cnt = word;
                if word & PM1_CNT_SLP_EN != 0 {
                    let typ = (word >> PM1_CNT_SLP_TYP_SHIFT) & PM1_CNT_SLP_TYP_MASK;
                    if typ == u16::from(SLP_TYP_S5) {
                        log::info!("guest requested S5 through PM1a_CNT: powering off");
                        self.power = Some(PowerEvent::Off);
                    } else {
                        log::warn!("guest requested sleep state S{typ}, which is not implemented");
                    }
                }
            }
            RESET_IO_ADDRESS if value & RESET_CPU != 0 => {
                log::info!("guest wrote RST_CPU to the reset register: resetting");
                self.power = Some(PowerEvent::Reset);
            }
            // The timer is read-only; so is everything else in the range.
            _ => {}
        }
    }
}

/// The low 16 bits of a write, whatever width the guest used.
fn word_of(data: &[u8]) -> u16 {
    let mut v = [0u8; 2];
    let n = data.len().min(2);
    v[..n].copy_from_slice(&data[..n]);
    u16::from_le_bytes(v)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The firmware selects its entire platform path on this one number.
    #[test]
    fn the_host_bridge_carries_the_device_id_edk2_switches_on() {
        let f = host_bridge();
        // `OVMF_HOSTBRIDGE_DID` is PCI_LIB_ADDRESS(0, 0, 0, 2) — the device
        // ID at offset 2, read as 16 bits.
        assert_eq!(f.read(crate::pci::DEVICE_ID, 2) as u16, 0x0D57);
        // A host bridge advertises no capability list. If it did, firmware
        // would follow the pointer to 0x40 and find nothing.
        assert_eq!(f.read(crate::pci::CAPABILITY_LIST, 1), 0);
    }

    /// Reproduces `BaseResetShutdown.c`:
    /// `IoWrite8 (CLOUDHV_ACPI_SHUTDOWN_IO_ADDRESS, 5 << 2 | 1 << 5)`.
    #[test]
    fn the_firmwares_own_shutdown_write_powers_the_machine_off() {
        let mut pm = CloudHvPm::new();
        assert_eq!(pm.take_power_event(), None);
        pm.write(ACPI_SHUTDOWN_IO_ADDRESS, &[5 << 2 | 1 << 5]);
        assert_eq!(pm.take_power_event(), Some(PowerEvent::Off));
        assert_eq!(pm.take_power_event(), None, "the event is taken once");
    }

    /// The bit layout is the whole difference between this and an ICH9 PM
    /// block, so it is pinned rather than assumed.
    #[test]
    fn a_sleep_type_without_the_enable_bit_does_nothing() {
        let mut pm = CloudHvPm::new();
        pm.write(ACPI_SHUTDOWN_IO_ADDRESS, &[5 << 2]);
        assert_eq!(
            pm.take_power_event(),
            None,
            "SLP_TYP alone must not power the machine off; ACPI writes \
             SLP_TYP and SLP_EN together"
        );
    }

    #[test]
    fn the_acpi_timer_answers_at_the_address_edk2_hard_codes() {
        let pm = CloudHvPm::new();
        assert!(CloudHvPm::claims(ACPI_TIMER_IO_ADDRESS));
        assert_eq!(ACPI_TIMER_IO_ADDRESS, 0x0608);

        let mut first = [0u8; 4];
        pm.read(ACPI_TIMER_IO_ADDRESS, &mut first);
        std::thread::sleep(std::time::Duration::from_millis(50));
        let mut second = [0u8; 4];
        pm.read(ACPI_TIMER_IO_ADDRESS, &mut second);

        let elapsed = u32::from_le_bytes(second).wrapping_sub(u32::from_le_bytes(first));
        // 50 ms is ~179_000 ticks, well short of the 24-bit wrap at ~4.7 s.
        assert!(
            (100_000..400_000).contains(&elapsed),
            "50 ms should be about 179000 ticks, got {elapsed}; a timer that \
             does not advance hangs every firmware delay loop"
        );
        assert_eq!(
            u32::from_le_bytes(second) & !PmTimer::MASK,
            0,
            "the ACPI timer is 24 bits wide"
        );
    }
}
