//! Boot a guest and stream its own virtio-gpu scanout to console clients.
//!
//! This is the whole chain in one program: a PVH kernel is loaded and run,
//! the guest's virtio-gpu driver binds a PCI device this VMM emulates, and
//! the frames it flushes are handed to the §7 media plane, encoded once,
//! and served over RTSPS to however many clients connect.
//!
//! Nothing here draws anything. Every pixel is the guest's.
//!
//! ```sh
//! scripts/extract-vmlinux /boot/vmlinuz-$(uname -r) > /tmp/vmlinux
//! cargo run -p custom-vmm --example gpu_boot -- /tmp/vmlinux 60
//! # then, from anywhere:
//! cargo run -p vmm-console-client -- console --addr '[::1]:8554' --insecure
//! ```

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};

use libvmm_core::devices::{DeviceModel, SerialLog};
use libvmm_core::memory::{self, GuestMemoryMap};
use libvmm_core::pci::{Bdf, PciBus, PciFunction};
use libvmm_core::pvh::{BootInfo, MemmapEntry, MemmapType};
use libvmm_core::vcpu::{self, RunState};
use libvmm_virtio::gpu_pci::{SharedScanout, VirtioGpuPci};
use libvmm_virtio::mem::{GuestRam, Region};
use libvmm_virtio::pci_cap;
use vmm_codec_sys::{PackedFormat, PackedFrame};

/// Where the guest will find the virtio-gpu BAR.
///
/// Pre-assigned rather than left for the guest to allocate. Linux keeps a
/// firmware-assigned BAR that sits inside a window the ACPI `_CRS`
/// advertises, and pre-assigning means the device is reachable from the
/// first config read rather than only after resource assignment.
const GPU_BAR_BASE: u64 = memory::PCI_MMIO_BASE;

/// Feeds the media plane whatever the guest last flushed.
struct GpuScanout {
    shared: Arc<SharedScanout>,
    /// The last frame handed on, repeated while the guest is idle.
    ///
    /// An idle guest sends no virtio-gpu commands at all — the driver's
    /// damage merge returns early when nothing changed — so without this
    /// the encoder would starve rather than see a still picture.
    last: Option<PackedFrame>,
    frames: Arc<AtomicU64>,
}

impl libvmm_media::plane::CaptureSource for GpuScanout {
    fn next_frame(&mut self) -> libvmm_core::VmmResult<Option<PackedFrame>> {
        if let Ok(mut slot) = self.shared.frame.lock() {
            if let Some(frame) = slot.take() {
                self.frames.fetch_add(1, Ordering::Relaxed);
                let packed = PackedFrame {
                    width: frame.width,
                    height: frame.height,
                    // virtio-gpu 2D resources are tightly packed.
                    stride: frame.width as usize * 4,
                    pixels: frame.pixels,
                    // B8G8R8X8_UNORM: B, G, R, then padding.
                    format: PackedFormat::Bgra,
                };
                self.last = Some(packed.clone());
                return Ok(Some(packed));
            }
        }
        Ok(self.last.clone())
    }

    fn next_audio(&mut self) -> libvmm_core::VmmResult<Vec<i16>> {
        // No virtio-snd yet. Silence, so the audio track exists and the
        // client's demux has something to lock onto.
        Ok(vec![0i16; 960])
    }
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let mut args = std::env::args().skip(1);
    let kernel_path = args.next().ok_or("usage: gpu_boot <vmlinux> [seconds]")?;
    let seconds: u64 = args.next().and_then(|s| s.parse().ok()).unwrap_or(60);
    let image = std::fs::read(&kernel_path)?;

    // ---- the machine ----------------------------------------------------
    let mut cfg = libvmm_config::MachineConfig::from_toml_str(include_str!(
        "../../../config/reference-vm.toml"
    ))?;
    cfg.compute.vcpus = 1;
    cfg.memory.size_mb = 2048;
    cfg.memory.low_ram_mb = 2048;
    cfg.memory.high_ram_mb = 0;
    cfg.memory.hugepages_1gb = false;

    // The display geometry is the machine's, not this program's: the
    // encoder is opened from the same config, and a scanout that disagrees
    // with it is refused (§7, error 5010).
    let (width, height) = (cfg.display.width, cfg.display.height);

    let map = GuestMemoryMap::new(&cfg.memory)?;
    let mut machine = libvmm_core::kvm::Machine::bringup(&cfg, map)?;

    // ---- ACPI, so the guest finds its LAPIC and the ECAM window ---------
    let acpi = libvmm_core::acpi::builder::build(&cfg, machine.map_ref(), &|p| std::fs::read(p))?;
    machine.load_acpi(&acpi)?;

    // ---- the kernel ------------------------------------------------------
    let boot = BootInfo {
        // Both consoles, deliberately. Naming any `console=` stops Linux
        // adding `tty0` by default, so without the second one the guest's
        // messages go to the serial port and the framebuffer shows only
        // the boot logo — which looks exactly like a broken scanout.
        cmdline: std::env::var("VMM_CMDLINE").unwrap_or_else(|_| {
            "earlyprintk=serial,ttyS0,115200,keep console=ttyS0,115200 console=tty0".to_string()
        }),
        memmap: vec![
            MemmapEntry {
                addr: 0,
                size: 0x9FC00,
                kind: MemmapType::Ram,
            },
            MemmapEntry {
                addr: 0x10_0000,
                size: cfg.memory.low_ram_mb * memory::MIB - 0x10_0000,
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
    libvmm_core::kvm::configure_pvh_entry(&machine.vcpus[0], 0, &state)?;

    // ---- the virtio-gpu device ------------------------------------------
    let ram = GuestRam::new(
        machine
            .ram_regions()
            .into_iter()
            .map(|(gpa, host, len)| Region { gpa, host, len })
            .collect(),
    );
    let scanout = Arc::new(SharedScanout::default());
    let msi = Arc::new(libvmm_core::kvm::KvmMsiSender::new(Arc::clone(&machine.vm)));
    let mut gpu = VirtioGpuPci::new(width, height, ram, msi, Arc::clone(&scanout));
    gpu.set_bar_base(GPU_BAR_BASE);
    let bar_size = gpu.bar_size();
    let layout = pci_cap::BarLayout::new(
        libvmm_virtio::gpu::NUM_QUEUES,
        libvmm_virtio::gpu::CONFIG_LEN,
    );

    let mut bus = PciBus::new();
    bus.insert(PciFunction::new(
        Bdf::new(0, 0, 0),
        0x8086,
        0x29C0,
        0x00_06_00_00,
        0,
    ));
    let mut gpu_fn = PciFunction::new(
        Bdf::new(0, 1, 0),
        pci_cap::VIRTIO_VENDOR_ID,
        pci_cap::modern_device_id(pci_cap::VIRTIO_ID_GPU),
        // Class 03, subclass 80 (display / other). Not DISPLAY_VGA: §5.7.7
        // reserves that for the VGA-compatible variant, which this is not.
        0x00_03_80_00,
        pci_cap::MODERN_SUBSYSTEM_ID,
    );
    gpu_fn.set_bar64(0, GPU_BAR_BASE, bar_size);
    gpu_fn.add_virtio_cap(
        pci_cap::VIRTIO_PCI_CAP_COMMON_CFG,
        0,
        layout.common_offset,
        layout.common_length,
        None,
    );
    gpu_fn.add_virtio_cap(
        pci_cap::VIRTIO_PCI_CAP_NOTIFY_CFG,
        0,
        layout.notify_offset,
        layout.notify_length,
        Some(layout.notify_off_multiplier),
    );
    gpu_fn.add_virtio_cap(
        pci_cap::VIRTIO_PCI_CAP_ISR_CFG,
        0,
        layout.isr_offset,
        layout.isr_length,
        None,
    );
    gpu_fn.add_virtio_cap(
        pci_cap::VIRTIO_PCI_CAP_DEVICE_CFG,
        0,
        layout.device_offset,
        layout.device_length,
        None,
    );
    gpu_fn.add_msix_cap(
        libvmm_virtio::gpu::NUM_QUEUES + 1,
        0,
        layout.msix_table_offset,
        layout.msix_pba_offset,
    );
    // Memory decoding on, so the BAR is live from the first probe.
    gpu_fn.write(0x04, 2, 0x0002);
    log::info!(
        "virtio-gpu at 00:01.0, BAR0 {GPU_BAR_BASE:#x}..{:#x} ({} KiB)",
        GPU_BAR_BASE + bar_size,
        bar_size / 1024
    );
    bus.insert(gpu_fn);

    let log = SerialLog::new();
    let mut devices = DeviceModel::new(
        bus,
        cfg.memory.low_ram_mb * memory::MIB,
        cfg.memory.high_ram_mb * memory::MIB,
        Arc::clone(&log),
    );
    devices.mmio_devices.push(Box::new(gpu));
    devices.set_wall_clock(machine.wall_clock());

    // ---- the media plane -------------------------------------------------
    let frames = Arc::new(AtomicU64::new(0));
    let plane = libvmm_media::MediaPlane::new(cfg.clone(), None)?;
    plane.start(Box::new(GpuScanout {
        shared: Arc::clone(&scanout),
        last: None,
        frames: Arc::clone(&frames),
    }))?;

    let identity = libvmm_control::tls::SelfSignedIdentity::generate(&cfg.vm.name)?;
    let tls = libvmm_control::tls::server_config(&identity)?;
    let server = Arc::new(libvmm_media::server::RtspServer::bind(
        &format!("[::]:{}", cfg.display.rtsps.port),
        tls,
        libvmm_media::server::Credentials {
            username: cfg.display.rtsps.username.clone(),
            password: cfg.display.rtsps.password.clone(),
        },
        Arc::clone(&plane),
    )?);
    let bound = server
        .local_addr()
        .map(|a| a.to_string())
        .unwrap_or_default();
    let serving = Arc::clone(&server);
    std::thread::Builder::new()
        .name("rtsp-listener".to_string())
        .spawn(move || serving.accept_loop())?;
    log::info!(
        "RTSPS on rtsps://{bound}{} — connect a console client now",
        cfg.display.rtsps.stream_path
    );

    // ---- run -------------------------------------------------------------
    let running = vcpu::spawn(machine.take_vcpus(), RunState::new(devices))?;
    let deadline = Instant::now() + Duration::from_secs(seconds);
    let mut reported = 0u64;
    while Instant::now() < deadline && running.finished().is_none() {
        std::thread::sleep(Duration::from_secs(2));
        let flushed = scanout.flushed.load(Ordering::Relaxed);
        if flushed != reported {
            log::info!(
                "guest has flushed {flushed} frames; {} encoded and sent",
                frames.load(Ordering::Relaxed)
            );
            reported = flushed;
        }
    }
    // Optionally dump the raw scanout — the exact BGRX the guest handed
    // us, before any encoding. Useful for telling "the guest drew nothing"
    // apart from "the encoder or the client lost it".
    if let Ok(path) = std::env::var("VMM_DUMP_SCANOUT") {
        if let Ok(slot) = scanout.frame.lock() {
            match slot.as_ref() {
                Some(f) => {
                    let mut ppm = format!("P6\n{} {}\n255\n", f.width, f.height).into_bytes();
                    for px in f.pixels.chunks_exact(4) {
                        // BGRX in memory -> RGB on disk.
                        ppm.extend_from_slice(&[px[2], px[1], px[0]]);
                    }
                    std::fs::write(&path, ppm)?;
                    log::info!("wrote the raw guest scanout to {path}");
                }
                // An empty slot usually means the capture thread got
                // there first, not that the guest drew nothing — the
                // flush counter distinguishes them.
                None if scanout.flushed.load(Ordering::Relaxed) > 0 => {
                    log::info!("nothing to dump: the capture thread already took the last frame")
                }
                None => log::warn!("no scanout to dump: the guest flushed nothing"),
            }
        }
    }

    let text = log.text();
    let outcome = running.shutdown();
    println!("{text}");
    eprintln!(
        "=== outcome: {outcome} · {} serial lines · {} guest flushes · {} captured ===",
        text.lines().count(),
        scanout.flushed.load(Ordering::Relaxed),
        frames.load(Ordering::Relaxed)
    );
    Ok(())
}
