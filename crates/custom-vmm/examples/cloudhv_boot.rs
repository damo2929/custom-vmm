//! Boot the CloudHv UEFI firmware on a machine with no chipset.
//!
//! Revision D.2 option B. The machine this brings up is a PCIe root complex
//! and nothing else: one host bridge at 00:00.0 carrying the device ID
//! `0x0d57`, hardware-reduced ACPI at two fixed I/O addresses, a 16550, and
//! the ECAM window. No LPC bridge, no PMBASE, no `fw_cfg`, no A20 gate, no
//! PIC, no PIT.
//!
//! The firmware is an **ELF**, not a flash image — `OvmfPkg/CloudHv` builds
//! with `OvmfPkg/XenResetVector`, which carries an
//! `XEN_ELFNOTE_PHYS32_ENTRY` note — so it is loaded by exactly the same PVH
//! loader that loads a Linux kernel, and entered in 32-bit protected mode
//! with `ebx` pointing at an `hvm_start_info`. The firmware then takes its
//! memory map from that structure's memmap and its ACPI tables from the XSDT
//! behind its `rsdp_paddr`, both of which this tree already builds.
//!
//! Build the firmware first:
//!
//! ```sh
//! scripts/build-cloudhv-firmware.sh
//! cargo run --release -p custom-vmm --example cloudhv_boot -- firmware/CLOUDHV.fd
//! ```

use std::sync::Arc;
use std::time::{Duration, Instant};

use libvmm_core::devices::{DeviceModel, SerialLog};
use libvmm_core::memory::{self, GuestMemoryMap};
use libvmm_core::pci::PciBus;
use libvmm_core::pvh::{BootInfo, MemmapEntry, MemmapType};
use libvmm_core::vcpu::{self, RunState};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let mut args = std::env::args().skip(1);
    let fd_path = args
        .next()
        .unwrap_or_else(|| "firmware/CLOUDHV.fd".to_string());
    let seconds: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(25);

    let image = std::fs::read(&fd_path)
        .map_err(|e| format!("{fd_path}: {e} — build it with scripts/build-cloudhv-firmware.sh"))?;
    if image.first_chunk_is_elf() {
        log::info!("{fd_path}: {} bytes, ELF", image.len());
    }

    let mut cfg = libvmm_config::MachineConfig::from_toml_str(include_str!(
        "../../../config/reference-vm.toml"
    ))?;
    cfg.compute.vcpus = 1;
    // 3 GiB of low RAM, filling the map right up to the bottom of the §1.3
    // MMIO hole. edk2's CloudHv path takes `Uc32Base` from
    // `CLOUDHV_MMIO_HOLE_ADDRESS`, which is 0xC0000000 — the same boundary.
    cfg.memory.size_mb = 3072;
    cfg.memory.low_ram_mb = 3072;
    cfg.memory.high_ram_mb = 0;
    cfg.memory.hugepages_1gb = false;

    let map = GuestMemoryMap::new(&cfg.memory)?;
    let mut machine = libvmm_core::kvm::Machine::bringup(&cfg, map)?;

    // ACPI. On this platform it is not optional and it is not decorative:
    // `InstallCloudHvTables` walks the XSDT reached through
    // `hvm_start_info.rsdp_paddr` and installs every table it finds. There
    // is no fw_cfg fallback behind it.
    let acpi = libvmm_core::acpi::builder::build(&cfg, machine.map_ref(), &|p| std::fs::read(p))?;
    machine.load_acpi(&acpi)?;
    log::info!(
        "acpi: rsdp at {:#x}, {} tables, checksums {}",
        acpi.rsdp_gpa,
        acpi.tables.len(),
        if acpi.all_checksums_valid() {
            "ok"
        } else {
            "BAD"
        }
    );

    let low = cfg.memory.low_ram_mb * memory::MIB;
    let boot = BootInfo {
        // The firmware ignores a command line; this is here so the block is
        // well-formed rather than because anything reads it.
        cmdline: String::new(),
        memmap: vec![
            MemmapEntry {
                addr: 0,
                size: 0x9FC00,
                kind: MemmapType::Ram,
            },
            MemmapEntry {
                addr: 0x10_0000,
                size: low - 0x10_0000,
                kind: MemmapType::Ram,
            },
            MemmapEntry {
                addr: memory::ECAM_BASE,
                size: memory::ECAM_SIZE,
                kind: MemmapType::Reserved,
            },
        ],
        rsdp: Some(acpi.rsdp_gpa),
        initramfs: None,
    };

    let state = machine.load_pvh_kernel(&image, &boot)?;
    log::info!(
        "PVH entry {:#x}, start_info {:#x}",
        state.entry,
        state.start_info
    );
    libvmm_core::kvm::configure_pvh_entry(&machine.vcpus[0], 0, &state)?;

    // The entire chipset.
    let mut bus = PciBus::new();
    bus.insert(libvmm_core::cloudhv::host_bridge());

    let log = SerialLog::new();
    let mut devices = DeviceModel::new(
        bus,
        low,
        cfg.memory.high_ram_mb * memory::MIB,
        Arc::clone(&log),
    );
    devices.present_cloudhv_platform();
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

/// Tiny helper so the ELF check reads as a sentence at the call site.
trait ElfCheck {
    fn first_chunk_is_elf(&self) -> bool;
}

impl ElfCheck for Vec<u8> {
    fn first_chunk_is_elf(&self) -> bool {
        self.starts_with(b"\x7fELF")
    }
}
