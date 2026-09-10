//! Revision D.2 option B: the machine with no chipset.
//!
//! These pin the *absence* of things, which is unusual and deliberate. The
//! whole claim of this platform is that a stock UEFI firmware boots on a bus
//! carrying one function, and a claim about what is not there cannot be
//! tested by exercising what is.

#![cfg(target_os = "linux")]

use libvmm_core::cloudhv;
use libvmm_core::devices::{DeviceModel, PowerEvent, SerialLog};
use libvmm_core::ich9;
use libvmm_core::memory;
use libvmm_core::pci::{Bdf, PciBus};

fn cloudhv_machine() -> DeviceModel {
    let mut bus = PciBus::new();
    bus.insert(cloudhv::host_bridge());
    let mut devices = DeviceModel::new(bus, 3072 * memory::MIB, 0, SerialLog::new());
    devices.present_cloudhv_platform();
    devices
}

#[test]
fn the_whole_chipset_is_one_host_bridge() {
    let devices = cloudhv_machine();
    let bdfs: Vec<_> = devices.pci.bdfs().copied().collect();
    assert_eq!(
        bdfs,
        vec![Bdf::new(0, 0, 0)],
        "option B is a PCIe root complex and nothing else; anything at \
         00:1f.0 means an LPC bridge crept back in"
    );
}

#[test]
fn there_is_no_lpc_bridge_and_therefore_no_pmbase() {
    let devices = cloudhv_machine();
    assert_eq!(
        ich9::pmbase(&devices.pci),
        None,
        "an enabled ICH9 PM block would decode 0x0600, which on this \
         platform is SLEEP_CONTROL_REG and means something else entirely"
    );
}

/// `AcpiTimerLibConstructor`'s CloudHv branch does no PCI access at all: it
/// assigns the constant and returns. So the timer has to be at exactly that
/// address, and it has to count.
#[test]
fn the_acpi_timer_answers_at_0x608_without_any_chipset_to_program_it() {
    let mut devices = cloudhv_machine();

    let mut first = [0u8; 4];
    devices.io_read(cloudhv::ACPI_TIMER_IO_ADDRESS, &mut first);
    std::thread::sleep(std::time::Duration::from_millis(30));
    let mut second = [0u8; 4];
    devices.io_read(cloudhv::ACPI_TIMER_IO_ADDRESS, &mut second);

    let elapsed = u32::from_le_bytes(second).wrapping_sub(u32::from_le_bytes(first));
    assert!(
        (50_000..250_000).contains(&elapsed),
        "30 ms is about 107000 ticks at 3.579545 MHz, got {elapsed}"
    );
    assert!(
        devices.unhandled_report().is_empty(),
        "the timer must be answered, not counted as unhandled: {:?}",
        devices.unhandled_report()
    );
}

/// The exact write from `OvmfPkg/Library/ResetSystemLib/BaseResetShutdown.c`.
#[test]
fn the_firmwares_shutdown_write_reaches_the_run_loop_as_a_power_off() {
    let mut devices = cloudhv_machine();
    assert_eq!(devices.take_power_event(), None);
    devices.io_write(cloudhv::ACPI_SHUTDOWN_IO_ADDRESS, &[5 << 2 | 1 << 5]);
    assert_eq!(
        devices.take_power_event(),
        Some(PowerEvent::Off),
        "KVM never surfaces an ACPI shutdown as a system event — it is an \
         ordinary `out` — so the device model is the only place it can be seen"
    );
}

/// `PlatformInitLib` reads this one number and every later phase branches on
/// it. Get it wrong and stock edk2 `ASSERT(FALSE)`s in nine places.
#[test]
fn the_host_bridge_device_id_is_the_one_edk2_selects_the_cloudhv_path_on() {
    let devices = cloudhv_machine();
    let f = devices
        .pci
        .get(Bdf::new(0, 0, 0))
        .expect("a host bridge at 00:00.0");
    assert_eq!(f.read(libvmm_core::pci::DEVICE_ID, 2) as u16, 0x0D57);
}

/// The two platforms disagree about what address 0x0600 is, so presenting
/// both at once is a bug that would otherwise show up as a firmware that
/// shuts the machine down while trying to clear a status bit.
#[test]
#[should_panic(expected = "0x0600")]
fn the_cloudhv_platform_and_an_enabled_ich9_pm_block_are_mutually_exclusive() {
    let mut bus = PciBus::new();
    bus.insert(cloudhv::host_bridge());
    bus.insert(ich9::lpc_bridge());
    // Program PMBASE and enable the decode, exactly as OVMF's Q35 path does.
    let f = bus
        .get_mut(Bdf::new(0, ich9::LPC_DEVICE, ich9::LPC_FUNCTION))
        .unwrap();
    f.write(ich9::PMBASE, 4, u64::from(ich9::PMBASE_DEFAULT | 1));
    f.write(ich9::ACPI_CNTL, 1, u64::from(ich9::ACPI_CNTL_EN));

    let mut devices = DeviceModel::new(bus, 3072 * memory::MIB, 0, SerialLog::new());
    devices.present_cloudhv_platform();
}
