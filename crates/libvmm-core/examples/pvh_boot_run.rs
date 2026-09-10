//! Boot a PVH kernel and dump everything it says, for as long as asked.
fn main() {
    let path = std::env::args()
        .nth(1)
        .expect("usage: pvh_boot_run <vmlinux> [secs]");
    let secs: u64 = std::env::args()
        .nth(2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(15);
    let image = std::fs::read(&path).expect("read kernel");

    let mut cfg = libvmm_config::MachineConfig::from_toml_str(include_str!(
        "../../../config/reference-vm.toml"
    ))
    .expect("config");
    cfg.compute.vcpus = 1;
    cfg.memory.size_mb = 1024;
    cfg.memory.low_ram_mb = 1024;
    cfg.memory.high_ram_mb = 0;
    cfg.memory.hugepages_1gb = false;

    use libvmm_core::memory::{self, GuestMemoryMap};
    use libvmm_core::pvh::{BootInfo, MemmapEntry, MemmapType};
    let map = GuestMemoryMap::new(&cfg.memory).expect("map");
    let mut machine = libvmm_core::kvm::Machine::bringup(&cfg, map).expect("bringup");

    let boot = BootInfo {
        cmdline: std::env::var("VMM_CMDLINE").unwrap_or_else(|_| {
            "earlyprintk=serial,ttyS0,115200,keep console=ttyS0,115200".to_string()
        }),
        memmap: vec![
            MemmapEntry {
                addr: 0,
                size: 0x9FC00,
                kind: MemmapType::Ram,
            },
            MemmapEntry {
                addr: 0x10_0000,
                size: 1024 * memory::MIB - 0x10_0000,
                kind: MemmapType::Ram,
            },
            MemmapEntry {
                addr: memory::ECAM_BASE,
                size: memory::ECAM_SIZE,
                kind: MemmapType::Reserved,
            },
        ],
        rsdp: None,
        initramfs: None,
    };

    // ACPI. Without it the guest finds no MADT, concludes there is no
    // LAPIC configuration, falls back to virtual-wire mode, and then has
    // no timer interrupt at all because there is no PIT either.
    let mut boot = boot;
    if std::env::var("VMM_NO_ACPI").is_err() {
        let acpi =
            libvmm_core::acpi::builder::build(&cfg, machine.map_ref(), &|p| std::fs::read(p))
                .expect("build ACPI tables");
        machine.load_acpi(&acpi).expect("load ACPI tables");
        eprintln!(
            "acpi: rsdp at {:#x}, {} tables, checksums {}",
            acpi.rsdp_gpa,
            acpi.tables.len(),
            if acpi.all_checksums_valid() {
                "ok"
            } else {
                "BAD"
            }
        );
        boot.rsdp = Some(acpi.rsdp_gpa);
    }

    let state = machine.load_pvh_kernel(&image, &boot).expect("load");
    eprintln!(
        "entry {:#x} start_info {:#x}",
        state.entry, state.start_info
    );
    libvmm_core::kvm::configure_pvh_entry(&machine.vcpus[0], 0, &state).expect("regs");

    use libvmm_core::devices::{DeviceModel, SerialLog};
    use libvmm_core::pci::{Bdf, PciBus, PciFunction};
    let mut bus = PciBus::new();
    bus.insert(PciFunction::new(
        Bdf::new(0, 0, 0),
        0x8086,
        0x29C0,
        0x00_06_00_00,
        0,
    ));
    let log = SerialLog::new();
    let devices = DeviceModel::new(bus, 1024 * memory::MIB, 0, std::sync::Arc::clone(&log));
    let running = libvmm_core::vcpu::spawn(
        machine.take_vcpus(),
        libvmm_core::vcpu::RunState::new(devices),
    )
    .expect("spawn");

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs);
    while std::time::Instant::now() < deadline && running.finished().is_none() {
        std::thread::sleep(std::time::Duration::from_millis(200));
    }
    let text = log.text();
    let outcome = running.shutdown();
    println!("{text}");
    eprintln!(
        "=== outcome: {outcome} · {} lines ===",
        text.lines().count()
    );
}
