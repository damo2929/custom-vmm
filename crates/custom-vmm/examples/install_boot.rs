//! Boot the CloudHv firmware with a virtio-gpu and an optical drive, and
//! photograph the screen.
//!
//! This is the whole machine: a chipset-free PCIe root complex (Revision
//! D.9), UEFI firmware loaded through the PVH path, a virtio-gpu the
//! firmware draws to, and a virtio-scsi controller carrying a DVD-ROM backed
//! by an ISO file. It is what "boot any OS" means in practice.
//!
//! ```sh
//! scripts/build-cloudhv-firmware.sh
//! cargo run --release -p custom-vmm --example install_boot -- \
//!     firmware/CLOUDHV.fd /home/damien/Documents/win11.iso 120 shot.ppm
//! ```
//!
//! The last argument is where the raw scanout is written when the run ends —
//! the guest's own framebuffer, before any encoding, which is the only
//! evidence that distinguishes "the guest drew nothing" from "the encoder
//! lost it".

use std::sync::Arc;
use std::time::{Duration, Instant};

use libvmm_config::DriveMedium;
use libvmm_core::devices::{DeviceModel, SerialLog};
use libvmm_core::memory::{self, GuestMemoryMap};
use libvmm_core::pci::{Bdf, PciBus, PciFunction};
use libvmm_core::pvh::{BootInfo, MemmapEntry, MemmapType};
use libvmm_core::vcpu::{self, RunState};
use libvmm_storage::engine::StorageEngine;
use libvmm_storage::engines::file::FileEngine;
use libvmm_storage::scsi_pci::{ScsiTarget, VirtioScsiPci};
use libvmm_virtio::gpu_pci::{SharedScanout, VirtioGpuPci};
use libvmm_virtio::mem::{GuestRam, Region};
use libvmm_virtio::pci_cap;

/// BAR windows, pre-assigned. The firmware would assign them itself, but
/// pre-assigning means each device is reachable from the first config read.
const GPU_BAR_BASE: u64 = memory::PCI_MMIO_BASE;
const SCSI_BAR_BASE: u64 = memory::PCI_MMIO_BASE + 0x0010_0000;
/// The display's framebuffer is 16 MiB and must be 16 MiB-aligned, so it
/// goes at the top of the window rather than beside the others.
const DISPLAY_FB_BASE: u64 = memory::PCI_MMIO_BASE + 0x0100_0000;
const DISPLAY_REG_BASE: u64 = memory::PCI_MMIO_BASE + 0x0020_0000;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let mut args = std::env::args().skip(1);
    let fd_path = args
        .next()
        .unwrap_or_else(|| "firmware/CLOUDHV.fd".to_string());
    let iso_path = args
        .next()
        .ok_or("usage: install_boot <CLOUDHV.fd> <image.iso> [seconds] [screenshot.ppm]")?;
    let seconds: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(120);
    let shot_path = args.next().unwrap_or_else(|| "screenshot.ppm".to_string());

    let firmware = std::fs::read(&fd_path)
        .map_err(|e| format!("{fd_path}: {e} — build it with scripts/build-cloudhv-firmware.sh"))?;

    let mut cfg = libvmm_config::MachineConfig::from_toml_str(include_str!(
        "../../../config/reference-vm.toml"
    ))?;
    cfg.compute.vcpus = 4;
    cfg.memory.size_mb = 3072;
    cfg.memory.low_ram_mb = 3072;
    cfg.memory.high_ram_mb = 0;
    cfg.memory.hugepages_1gb = false;
    let (width, height) = (cfg.display.width, cfg.display.height);

    let map = GuestMemoryMap::new(&cfg.memory)?;
    let mut machine = libvmm_core::kvm::Machine::bringup(&cfg, map)?;

    let acpi = libvmm_core::acpi::builder::build(&cfg, machine.map_ref(), &|p| std::fs::read(p))?;
    machine.load_acpi(&acpi)?;

    let low = cfg.memory.low_ram_mb * memory::MIB;
    let boot = BootInfo {
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
    let entry = machine.load_pvh_kernel(&firmware, &boot)?;
    log::info!("firmware PVH entry {:#x}", entry.entry);
    libvmm_core::kvm::configure_pvh_entry(&machine.vcpus[0], 0, &entry)?;

    let ram = GuestRam::new(
        machine
            .ram_regions()
            .into_iter()
            .map(|(gpa, host, len)| Region { gpa, host, len })
            .collect(),
    );
    let msi = Arc::new(libvmm_core::kvm::KvmMsiSender::new(Arc::clone(&machine.vm)));

    // ---- the bus: a root complex, a GPU and one controller per drive ----
    let mut bus = PciBus::new();
    bus.insert(libvmm_core::cloudhv::host_bridge());

    // virtio-gpu at 00:01.0.
    let scanout = Arc::new(SharedScanout::default());
    let mut gpu = VirtioGpuPci::new(
        width,
        height,
        ram.clone(),
        Arc::clone(&msi) as Arc<dyn libvmm_core::devices::MsiSender>,
        Arc::clone(&scanout),
    );
    gpu.set_bar_base(GPU_BAR_BASE);
    gpu.set_bdf(Bdf::new(0, 1, 0));
    let gpu_bar = gpu.bar_size();
    let gpu_layout = pci_cap::BarLayout::new(
        libvmm_virtio::gpu::NUM_QUEUES,
        libvmm_virtio::gpu::CONFIG_LEN,
    );
    let mut gpu_fn = PciFunction::new(
        Bdf::new(0, 1, 0),
        pci_cap::VIRTIO_VENDOR_ID,
        pci_cap::modern_device_id(pci_cap::VIRTIO_ID_GPU),
        0x00_03_80_00,
        pci_cap::MODERN_SUBSYSTEM_ID,
    );
    gpu_fn.set_bar64(0, GPU_BAR_BASE, gpu_bar);
    add_virtio_caps(&mut gpu_fn, &gpu_layout, libvmm_virtio::gpu::NUM_QUEUES + 1);
    gpu_fn.write(0x04, 2, 0x0002);
    bus.insert(gpu_fn);
    log::info!("virtio-gpu at 00:01.0, BAR0 {GPU_BAR_BASE:#x}, {width}x{height}");

    // virtio-scsi at 00:02.0, carrying the optical drive.
    //
    // §5.1's 1:1 invariant: one request queue per vCPU, each with its own
    // worker thread, on its own controller.
    let request_queues = libvmm_storage::queue_count(cfg.compute.vcpus);
    let total_queues = libvmm_storage::total_queues(cfg.compute.vcpus);
    let iso = FileEngine::open_read_only(
        std::path::Path::new(&iso_path),
        DriveMedium::DvdRom.block_size(),
        "dvdrom",
    )?;
    log::info!(
        "DVD-ROM: {iso_path}, {} sectors of {} bytes",
        iso.capacity() / u64::from(DriveMedium::DvdRom.block_size()),
        DriveMedium::DvdRom.block_size()
    );
    let scsi_layout = pci_cap::BarLayout::new(total_queues, 36);
    let mut transport = libvmm_virtio::transport::VirtioTransport::new(
        "virtio-scsi",
        total_queues,
        256,
        36,
        1u64 << 32, // VIRTIO_F_VERSION_1
    );
    transport.bar_base = Some(SCSI_BAR_BASE);
    let mut scsi = VirtioScsiPci::new(
        "scsi0",
        transport,
        ram.clone(),
        vec![ScsiTarget::new(0, Box::new(iso), DriveMedium::DvdRom)],
        Arc::clone(&msi) as Arc<dyn libvmm_core::devices::MsiSender>,
        request_queues,
        total_queues as usize + 1,
        u64::from(scsi_layout.msix_table_offset),
        u64::from(scsi_layout.msix_pba_offset),
    )?;
    scsi.set_bar_base(SCSI_BAR_BASE);
    scsi.set_bdf(Bdf::new(0, 2, 0));
    let scsi_bar = scsi.bar_size();
    let mut scsi_fn = PciFunction::new(
        Bdf::new(0, 2, 0),
        pci_cap::VIRTIO_VENDOR_ID,
        pci_cap::modern_device_id(pci_cap::VIRTIO_ID_SCSI),
        // Class 01, subclass 00: mass storage, SCSI.
        0x00_01_00_00,
        pci_cap::MODERN_SUBSYSTEM_ID,
    );
    scsi_fn.set_bar64(0, SCSI_BAR_BASE, scsi_bar);
    add_virtio_caps(&mut scsi_fn, &scsi_layout, total_queues + 1);
    scsi_fn.write(0x04, 2, 0x0002);
    bus.insert(scsi_fn);
    log::info!(
        "virtio-scsi at 00:02.0, BAR0 {SCSI_BAR_BASE:#x}, {request_queues} request queue(s), \
         one worker each"
    );

    // VMM_NO_DISPLAY leaves the display off the bus, for A/B runs against
    // a firmware change.
    let with_display = std::env::var_os("VMM_NO_DISPLAY").is_none();
    // The display at 00:03.0: a linear framebuffer, for the guests that
    // have no virtio-gpu driver — which is most of them, Windows included.
    // See crates/libvmm-core/src/display.rs.
    let mapper = Arc::new(libvmm_core::kvm::KvmRamMapper::new(Arc::clone(&machine.vm)));
    let mut display = libvmm_core::display::BochsDisplay::new(
        mapper as Arc<dyn libvmm_core::devices::GuestRamMapper>,
        DISPLAY_FB_BASE,
        DISPLAY_REG_BASE,
    )?;
    display.set_bdf(Bdf::new(0, 3, 0));
    let framebuffer = display.framebuffer();
    let mut display_fn = PciFunction::new(
        Bdf::new(0, 3, 0),
        libvmm_core::display::VENDOR_ID,
        libvmm_core::display::DEVICE_ID,
        libvmm_core::display::CLASS,
        0x1100,
    );
    display_fn.set_bar32(
        0,
        DISPLAY_FB_BASE,
        libvmm_core::display::FRAMEBUFFER_BAR_SIZE,
    );
    display_fn.set_bar32(2, DISPLAY_REG_BASE, libvmm_core::display::REGISTER_BAR_SIZE);
    // Memory space enabled: the framebuffer slot is published on this bit,
    // so the device has to be told the same thing the function says.
    display_fn.write(0x04, 2, 0x0002);
    libvmm_core::devices::MmioDevice::set_memory_decode(&mut display, true);
    if with_display {
        bus.insert(display_fn);
    }
    log::info!(
        "display at 00:03.0, framebuffer {DISPLAY_FB_BASE:#x}, registers {DISPLAY_REG_BASE:#x}"
    );

    // ---- the machine -----------------------------------------------------
    let log = SerialLog::new();
    let mut devices = DeviceModel::new(bus, low, 0, Arc::clone(&log));
    devices.present_cloudhv_platform();
    devices.set_wall_clock(machine.wall_clock());
    devices.mmio_devices.push(Box::new(gpu));
    if with_display {
        devices.mmio_devices.push(Box::new(display));
    }
    devices.mmio_devices.push(Box::new(scsi));

    let running = vcpu::spawn(machine.take_vcpus(), RunState::new(devices))?;
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let mut last_report = Instant::now();
    while Instant::now() < deadline && running.finished().is_none() {
        std::thread::sleep(Duration::from_millis(500));
        if last_report.elapsed() >= Duration::from_secs(15) {
            last_report = Instant::now();
            log::info!(
                "{}s elapsed, {} frame(s) flushed by the guest",
                seconds
                    .saturating_sub(deadline.saturating_duration_since(Instant::now()).as_secs()),
                scanout.flushed.load(std::sync::atomic::Ordering::Relaxed)
            );
        }
    }

    // ---- the screenshot --------------------------------------------------
    let flushed = scanout.flushed.load(std::sync::atomic::Ordering::Relaxed);
    // Three places a picture can come from, in the order they are
    // trustworthy:
    //
    // 1. The linear framebuffer. A guest with no virtio-gpu driver paints
    //    there, and after `ExitBootServices` it is the only surface still
    //    being updated by anyone.
    // 2. The virtio-gpu scanout's backing pages, read by us. That covers a
    //    guest whose GOP is virtio-gpu's Blt-only one and which therefore
    //    never flushes.
    // 3. The last frame the guest actually flushed.
    let shot = framebuffer
        .snapshot()
        .map(|(mode, pixels)| (mode.width, mode.height, pixels, "the linear framebuffer"))
        .or_else(|| {
            let backing = scanout.backing.lock().ok().and_then(|b| b.clone())?;
            let f = libvmm_virtio::gpu::frame_from_backing(&ram, &backing)?;
            Some((f.width, f.height, f.pixels, "the virtio-gpu backing"))
        })
        .or_else(|| {
            let f = scanout.frame.lock().ok().and_then(|mut f| f.take())?;
            Some((f.width, f.height, f.pixels, "a flushed virtio-gpu frame"))
        });
    match shot {
        Some((width, height, pixels, source)) => {
            write_ppm(&shot_path, width, height, &pixels)?;
            println!("screenshot: {shot_path} ({width}x{height}, from {source})");
        }
        None if flushed > 0 => {
            println!(
                "the guest flushed {flushed} frame(s) but the last one was already \
                 collected; nothing to write"
            );
        }
        None => println!("the guest never drew anything: no scanout to write"),
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
    eprintln!("=== frames flushed by the guest: {flushed} ===");
    eprintln!("=== outcome: {outcome} ===");
    eprintln!("=== exits: {} ===", state.exits.summary());
    Ok(())
}

/// Emit the four virtio PCI capabilities and the MSI-X one.
fn add_virtio_caps(f: &mut PciFunction, layout: &pci_cap::BarLayout, vectors: u16) {
    f.add_virtio_cap(
        pci_cap::VIRTIO_PCI_CAP_COMMON_CFG,
        0,
        layout.common_offset,
        layout.common_length,
        None,
    );
    f.add_virtio_cap(
        pci_cap::VIRTIO_PCI_CAP_NOTIFY_CFG,
        0,
        layout.notify_offset,
        layout.notify_length,
        Some(layout.notify_off_multiplier),
    );
    f.add_virtio_cap(
        pci_cap::VIRTIO_PCI_CAP_ISR_CFG,
        0,
        layout.isr_offset,
        layout.isr_length,
        None,
    );
    f.add_virtio_cap(
        pci_cap::VIRTIO_PCI_CAP_DEVICE_CFG,
        0,
        layout.device_offset,
        layout.device_length,
        None,
    );
    f.add_msix_cap(vectors, 0, layout.msix_table_offset, layout.msix_pba_offset);
}

/// Write the scanout as a binary PPM.
///
/// The scanout is BGRX — B, G, R, then a padding byte — which is the format
/// virtio-gpu's `B8G8R8X8_UNORM` names. PPM is RGB, so the two colour
/// channels are swapped on the way out. Getting that backwards produces a
/// picture that is obviously wrong rather than subtly so, which is the point
/// of using a format a human can open.
fn write_ppm(path: &str, width: u32, height: u32, bgrx: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let mut out = std::io::BufWriter::new(std::fs::File::create(path)?);
    write!(out, "P6\n{width} {height}\n255\n")?;
    let mut rgb = Vec::with_capacity(width as usize * height as usize * 3);
    for px in bgrx.chunks_exact(4) {
        rgb.extend_from_slice(&[px[2], px[1], px[0]]);
    }
    out.write_all(&rgb)?;
    out.flush()
}
