//! Priority 1: a guest kernel boots and says so.
//!
//! Everything else in this tree can be checked against a buffer or a plan.
//! This cannot. The only evidence that the PVH loader is right is a real
//! Linux kernel running on a real vCPU and printing its own banner through
//! a UART this VMM emulates.
//!
//! The kernel is not in the repository — it is 52 MB. Build one with
//! `scratchpad/kernel/build.sh`, or extract one from any distribution's
//! bzImage, which keeps the PVH note:
//!
//! ```sh
//! scripts/extract-vmlinux /boot/vmlinuz-$(uname -r) > vmlinux
//! VMM_TEST_KERNEL=$PWD/vmlinux cargo test -p libvmm-core --test pvh_boot
//! ```
//!
//! Absent a kernel or a `/dev/kvm`, these skip: neither absence says
//! anything about the code (AGENTS.md).

#![cfg(target_os = "linux")]

use std::time::{Duration, Instant};

use libvmm_core::devices::{DeviceModel, SerialLog};
use libvmm_core::memory::{self, GuestMemoryMap};
use libvmm_core::pci::{Bdf, PciBus, PciFunction};
use libvmm_core::pvh::{BootInfo, MemmapEntry, MemmapType};
use libvmm_core::vcpu::{self, RunState};

/// Where to find a PVH-capable `vmlinux`, if there is one.
fn kernel_path() -> Option<std::path::PathBuf> {
    if let Ok(p) = std::env::var("VMM_TEST_KERNEL") {
        let p = std::path::PathBuf::from(p);
        return p.exists().then_some(p);
    }
    None
}

fn kvm_available() -> bool {
    std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open("/dev/kvm")
        .is_ok()
}

/// A machine with enough RAM for a distribution kernel.
fn config(ram_mb: u64) -> libvmm_config::MachineConfig {
    let mut cfg = libvmm_config::MachineConfig::from_toml_str(include_str!(
        "../../../config/reference-vm.toml"
    ))
    .expect("the reference config must load");
    cfg.compute.vcpus = 1;
    cfg.memory.size_mb = ram_mb;
    cfg.memory.low_ram_mb = ram_mb;
    cfg.memory.high_ram_mb = 0;
    cfg.memory.hugepages_1gb = false;
    cfg
}

/// The guest's view of its own memory.
///
/// The ISA hole is deliberately absent: Linux appends it itself, as
/// `E820_TYPE_RESERVED`, in `init_pvh_bootparams`.
fn memmap(low_ram_mb: u64) -> Vec<MemmapEntry> {
    vec![
        // Conventional memory below the EBDA.
        MemmapEntry {
            addr: 0,
            size: 0x9FC00,
            kind: MemmapType::Ram,
        },
        // Everything from 1 MiB up.
        MemmapEntry {
            addr: 0x10_0000,
            size: low_ram_mb * memory::MIB - 0x10_0000,
            kind: MemmapType::Ram,
        },
        // The ECAM window. Reserving it is not decoration: with no SMBIOS
        // the kernel's early mmconfig check requires the range to be
        // reserved before it will trust the window.
        MemmapEntry {
            addr: memory::ECAM_BASE,
            size: memory::ECAM_SIZE,
            kind: MemmapType::Reserved,
        },
    ]
}

#[test]
fn a_pvh_kernel_boots_and_prints_its_banner() {
    let Some(path) = kernel_path() else {
        eprintln!("skipping: set VMM_TEST_KERNEL to a PVH-capable vmlinux");
        return;
    };
    if !kvm_available() {
        eprintln!("skipping: no usable /dev/kvm");
        return;
    }

    let image = std::fs::read(&path).expect("read the kernel");
    let ram_mb = 1024;
    let cfg = config(ram_mb);
    let map = GuestMemoryMap::new(&cfg.memory).expect("memory map");
    let mut machine = libvmm_core::kvm::Machine::bringup(&cfg, map).expect("KVM bring-up");

    let boot = BootInfo {
        // `earlyprintk` is the point of the exercise: it writes straight to
        // the UART from `parse_early_param`, long before the driver model,
        // ACPI or the timer exist. `keep` stops it being unregistered when
        // the real 8250 driver takes over.
        cmdline: "earlyprintk=serial,ttyS0,115200,keep console=ttyS0,115200".to_string(),
        memmap: memmap(ram_mb),
        rsdp: None,
        initramfs: None,
    };

    let state = machine
        .load_pvh_kernel(&image, &boot)
        .expect("load the PVH kernel");
    eprintln!(
        "loaded: entry {:#x}, start_info {:#x}",
        state.entry, state.start_info
    );

    libvmm_core::kvm::configure_pvh_entry(&machine.vcpus[0], 0, &state)
        .expect("configure the boot vCPU for PVH");

    let mut bus = PciBus::new();
    bus.insert(PciFunction::new(
        Bdf::new(0, 0, 0),
        0x8086,
        0x29C0,
        0x00_06_00_00,
        0,
    ));
    let log = SerialLog::new();
    let devices = DeviceModel::new(
        bus,
        cfg.memory.low_ram_mb * memory::MIB,
        cfg.memory.high_ram_mb * memory::MIB,
        std::sync::Arc::clone(&log),
    );
    let running = vcpu::spawn(machine.take_vcpus(), RunState::new(devices)).expect("vcpu threads");

    // "Linux version" is the first thing the kernel prints, from
    // `setup_arch`. Reaching it means the loader placed the image
    // correctly, the note's entry was the right one, the segments and .bss
    // are intact, `hvm_start_info` passed the guest's own validation, and
    // the entry register state was accepted.
    let deadline = Instant::now() + Duration::from_secs(30);
    let mut found = false;
    while Instant::now() < deadline {
        if log.text().contains("Linux version") {
            found = true;
            break;
        }
        if running.finished().is_some() {
            break;
        }
        std::thread::sleep(Duration::from_millis(20));
    }

    let text = log.text();
    let outcome = running.shutdown();
    eprintln!("--- guest serial output ---\n{text}\n--- end ---");

    assert!(
        found,
        "the guest kernel must reach its own banner. The run ended {outcome}. Serial output \
         was {text:?}"
    );
}
