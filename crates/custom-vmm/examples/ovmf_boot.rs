//! Run a stock QEMU-style OVMF flash image and report what it wants.
//!
//! Unlike the PVH path, this enters at the x86 reset vector in real mode —
//! the §3.1 machinery that already exists. The point of this program is
//! diagnostic: stock OVMF is built for a QEMU i440FX/Q35 machine with
//! `fw_cfg`, and rather than guess which of that we must provide, this runs
//! it and prints every port and address it touched that nothing answered.
//!
//! ```sh
//! cargo run -p custom-vmm --example ovmf_boot -- \
//!     /usr/share/edk2/ovmf/OVMF_CODE.fd /usr/share/edk2/ovmf/OVMF_VARS.fd
//! ```

use std::sync::Arc;
use std::time::{Duration, Instant};

use libvmm_core::devices::{DeviceModel, SerialLog};
use libvmm_core::memory::{self, GuestMemoryMap};
use libvmm_core::pci::{Bdf, PciBus, PciFunction};
use libvmm_core::vcpu::{self, RunState};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let mut args = std::env::args().skip(1);
    let code_path = args
        .next()
        .ok_or("usage: ovmf_boot <OVMF_CODE.fd> [OVMF_VARS.fd] [secs]")?;
    let vars_path = args.next();
    let seconds: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(15);

    // The two halves are contiguous at the top of 4 GiB: VARS first, then
    // CODE, with CODE's last byte at 0xFFFFFFFF so the reset vector lands
    // in it. Concatenating them and loading at the end of the ROM region
    // puts each exactly where its build expects.
    let code = std::fs::read(&code_path)?;
    let mut image = Vec::new();
    if let Some(p) = &vars_path {
        let vars = std::fs::read(p)?;
        log::info!(
            "OVMF_VARS {} bytes -> {:#x}",
            vars.len(),
            0x1_0000_0000u64 - (vars.len() + code.len()) as u64
        );
        image.extend_from_slice(&vars);
    }
    log::info!(
        "OVMF_CODE {} bytes -> {:#x}",
        code.len(),
        0x1_0000_0000u64 - code.len() as u64
    );
    image.extend_from_slice(&code);

    let mut cfg = libvmm_config::MachineConfig::from_toml_str(include_str!(
        "../../../config/reference-vm.toml"
    ))?;
    cfg.compute.vcpus = 1;
    // 3 GiB of low RAM, which is not arbitrary: edk2 sets the 32-bit PCI
    // aperture to `[max(top_of_low_ram, 2G) .. PcdPciExpressBaseAddress]`,
    // so filling low RAM to the bottom of the MMIO hole is what makes the
    // firmware's aperture and §1.3's hole the same range.
    cfg.memory.size_mb = 3072;
    cfg.memory.low_ram_mb = 3072;
    cfg.memory.high_ram_mb = 0;
    cfg.memory.hugepages_1gb = false;

    let map = GuestMemoryMap::new(&cfg.memory)?;
    let mut machine = libvmm_core::kvm::Machine::bringup(&cfg, map)?;

    // OVMF finds ACPI through fw_cfg, not through a staged RSDP, so these
    // tables are here only so the region is populated rather than absent.
    let acpi = libvmm_core::acpi::builder::build(&cfg, machine.map_ref(), &|p| std::fs::read(p))?;
    machine.load_acpi(&acpi)?;
    machine.load_firmware(&image)?;

    // Q35 MCH at 00:00.0 — what stock OVMF probes for — and its ICH9 LPC
    // companion at 00:1f.0. The companion is not optional: OVMF's
    // AcpiTimerLibConstructor reads PMBASE and ACPI_CNTL from it before it
    // has emitted a single line of output, and without it the firmware
    // asserts in PEI on a misaligned timer port. See `libvmm_core::ich9`.
    let mut bus = PciBus::new();
    bus.insert(PciFunction::new(
        Bdf::new(0, 0, 0),
        0x8086,
        0x29C0,
        0x00_06_00_00,
        0,
    ));
    bus.insert(libvmm_core::ich9::lpc_bridge());
    let log = SerialLog::new();
    let mut devices = DeviceModel::new(
        bus,
        cfg.memory.low_ram_mb * memory::MIB,
        cfg.memory.high_ram_mb * memory::MIB,
        Arc::clone(&log),
    );
    // The RTC reads the same clock the guest's own ptp_kvm would.
    devices.set_wall_clock(machine.wall_clock());

    let running = vcpu::spawn(machine.take_vcpus(), RunState::new(devices))?;
    let deadline = Instant::now() + Duration::from_secs(seconds);
    while Instant::now() < deadline && running.finished().is_none() {
        std::thread::sleep(Duration::from_millis(250));
    }

    let text = log.text();
    let state = Arc::clone(running.state());
    let outcome = running.shutdown();

    println!("--- firmware output ({} bytes) ---", text.len());
    println!("{text}");
    println!("--- what nothing answered ---");
    if let Ok(devices) = state.devices.lock() {
        for line in devices.unhandled_report() {
            println!("  {line}");
        }
    }
    eprintln!("=== outcome: {outcome} ===");
    eprintln!("=== exits: {} ===", state.exits.summary());
    Ok(())
}
