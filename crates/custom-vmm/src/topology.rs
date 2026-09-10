//! PCIe device topology.
//!
//! Each subsystem owns a bus from `[display] [peripherals] [storage]
//! [network] [usb]`, and every function is a modern virtio-pci device with
//! MSI-X. The placements the spec names explicitly are pinned here:
//!
//! * `01:00.0` virtio-gpu scanout, `01:00.1` virtio-snd PCM (§7.1)
//! * `02:00.0` keyboard, `02:01.0` tablet (§8.5)
//! * `02:04.0` virtio-tpm, `VIRTIO_ID_TPM = 45` (§6)
//! * `07:00.0` xHCI controller (§9)

use libvmm_config::MachineConfig;
use libvmm_core::memory::PCI_MMIO_BASE;
use libvmm_core::pci::{Bdf, PciBus, PciFunction};
use libvmm_virtio::msix::MsiRoutingTable;
use libvmm_virtio::pci_cap::{self, BarLayout};

/// One planned device: where it sits and what it is.
#[derive(Debug, Clone)]
pub struct Device {
    pub bdf: Bdf,
    pub name: &'static str,
    pub virtio_id: Option<u16>,
    /// Number of virtqueues, including any control/event queues.
    pub queues: u16,
    pub device_cfg_len: u32,
}

/// PCI class codes, written as `0x00_<base>_<sub>_<prog-if>`.
const CLASS_DISPLAY: u32 = 0x00_03_00_00;
const CLASS_MULTIMEDIA_AUDIO: u32 = 0x00_04_03_00;
const CLASS_INPUT: u32 = 0x00_09_00_00;
const CLASS_STORAGE_SCSI: u32 = 0x00_01_00_00;
const CLASS_NETWORK: u32 = 0x00_02_00_00;
const CLASS_SERIAL_USB_XHCI: u32 = 0x00_0C_03_30;
const CLASS_OTHER: u32 = 0x00_FF_00_00;

/// Plan the whole fabric for a machine.
///
/// The queue counts follow §5.1's 1:1 invariant: virtio-scsi gets
/// `controlq + eventq + vcpus` request queues, and there is no override.
pub fn plan(cfg: &MachineConfig) -> Vec<Device> {
    let mut devices = Vec::new();
    let vcpus = cfg.compute.vcpus;

    if cfg.display.enabled {
        devices.push(Device {
            bdf: Bdf::new(cfg.display.bus, 0x00, 0),
            name: "virtio-gpu",
            virtio_id: Some(pci_cap::VIRTIO_ID_GPU),
            queues: 2, // controlq + cursorq
            device_cfg_len: 16,
        });
        if cfg.display.virtio_sound_enabled {
            devices.push(Device {
                bdf: Bdf::new(cfg.display.bus, 0x00, 1),
                name: "virtio-snd",
                virtio_id: Some(pci_cap::VIRTIO_ID_SOUND),
                queues: 4, // control, event, tx, rx
                device_cfg_len: 8,
            });
        }
    }

    let p = &cfg.peripherals;
    devices.push(Device {
        bdf: Bdf::new(p.bus, p.keyboard_slot, 0),
        name: "virtio-input (keyboard)",
        virtio_id: Some(pci_cap::VIRTIO_ID_INPUT),
        queues: 2,
        device_cfg_len: 136,
    });
    devices.push(Device {
        bdf: Bdf::new(p.bus, p.tablet_slot, 0),
        name: "virtio-input (tablet)",
        virtio_id: Some(pci_cap::VIRTIO_ID_INPUT),
        queues: 2,
        device_cfg_len: 136,
    });
    devices.push(Device {
        bdf: Bdf::new(p.bus, p.rng_slot, 0),
        name: "virtio-rng",
        virtio_id: Some(pci_cap::VIRTIO_ID_RNG),
        queues: 1,
        device_cfg_len: 0,
    });
    devices.push(Device {
        bdf: Bdf::new(p.bus, p.serial_slot, 0),
        name: "virtio-serial",
        virtio_id: Some(pci_cap::VIRTIO_ID_CONSOLE),
        queues: 4,
        device_cfg_len: 12,
    });
    if cfg.tpm.enabled {
        devices.push(Device {
            bdf: Bdf::new(p.bus, p.tpm_slot, 0),
            name: "virtio-tpm",
            virtio_id: Some(pci_cap::VIRTIO_ID_TPM),
            // §6: TPM 2.0 command/response streams pass over a single queue.
            queues: 1,
            device_cfg_len: 4,
        });
    }

    // §5.1 / Revision E: one virtio-scsi controller **per drive**, each with
    // one request queue per vCPU and one worker thread per request queue.
    //
    // A single HBA carrying every drive as a target would also be valid SCSI
    // and would use fewer PCI slots, but it would make every drive share one
    // set of queues and one set of workers. Per-drive controllers mean an
    // optical drive being polled cannot delay the SSD an installer is
    // writing to, and that a slow backing store is isolated to its own
    // drive. The slot is the drive id, so a drive keeps its address across
    // reboots and across configuration changes to the other drives.
    for drive in &cfg.storage.drives {
        devices.push(Device {
            bdf: Bdf::new(cfg.storage.bus, drive.drive_id as u8, 0),
            name: "virtio-scsi",
            virtio_id: Some(pci_cap::VIRTIO_ID_SCSI),
            queues: libvmm_storage::total_queues(vcpus),
            device_cfg_len: 36,
        });
    }

    for card in &cfg.network.cards {
        devices.push(Device {
            bdf: Bdf::new(cfg.network.bus, card.card_id as u8, 0),
            name: "virtio-net",
            virtio_id: Some(pci_cap::VIRTIO_ID_NET),
            // rx/tx per queue pair, plus the control queue.
            queues: (cfg.network.num_queue_pairs as u16) * 2 + 1,
            device_cfg_len: libvmm_net::NetConfig::LEN as u32,
        });
    }

    devices.push(Device {
        bdf: Bdf::new(cfg.usb.bus, 0x00, 0),
        name: "xHCI",
        virtio_id: None,
        queues: 0,
        device_cfg_len: 0,
    });

    devices
}

fn class_of(d: &Device) -> u32 {
    match d.name {
        "virtio-gpu" => CLASS_DISPLAY,
        "virtio-snd" => CLASS_MULTIMEDIA_AUDIO,
        n if n.starts_with("virtio-input") => CLASS_INPUT,
        "virtio-scsi" => CLASS_STORAGE_SCSI,
        "virtio-net" => CLASS_NETWORK,
        "xHCI" => CLASS_SERIAL_USB_XHCI,
        // virtio-rng, virtio-serial, virtio-tpm.
        _ => CLASS_OTHER,
    }
}

/// Realise the plan: build the config spaces, allocate BARs out of the PCIe
/// MMIO window, and reserve one MSI-X vector (and GSI) per queue.
pub struct Fabric {
    pub bus: PciBus,
    pub routing: MsiRoutingTable,
    /// Doorbell addresses per device, in queue order — these are what get
    /// registered with `KVM_IOEVENTFD` (§2.2).
    pub doorbells: Vec<(Bdf, Vec<u64>)>,
    pub devices: Vec<Device>,
}

pub fn build(cfg: &MachineConfig) -> Fabric {
    let devices = plan(cfg);
    let mut bus = PciBus::new();
    let mut routing = MsiRoutingTable::new();
    let mut doorbells = Vec::new();
    let mut bar_cursor = PCI_MMIO_BASE;

    for d in &devices {
        let (vendor, device_id) = match d.virtio_id {
            Some(id) => (pci_cap::VIRTIO_VENDOR_ID, pci_cap::modern_device_id(id)),
            // A generic xHCI controller: Intel's class-compliant ID is what
            // guests expect to bind their xhci driver to.
            None => (0x8086u16, 0x1E31u16),
        };
        let mut f = PciFunction::new(
            d.bdf,
            vendor,
            device_id,
            class_of(d),
            d.virtio_id.unwrap_or(0),
        );

        let layout = BarLayout::new(d.queues.max(1), d.device_cfg_len);
        // Align the BAR to its own size, as PCI requires.
        let base = (bar_cursor + layout.bar_size - 1) & !(layout.bar_size - 1);
        f.set_bar64(0, base, layout.bar_size);
        bar_cursor = base + layout.bar_size;

        // One MSI-X vector per queue, plus one for configuration change.
        f.add_msix_cap(
            d.queues + 1,
            0,
            layout.msix_table_offset,
            layout.msix_pba_offset,
        );

        if d.virtio_id.is_some() {
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
        }

        let mut device_doorbells = Vec::with_capacity(d.queues as usize);
        for q in 0..d.queues {
            device_doorbells.push(layout.doorbell_address(base, q));
            routing.allocate();
        }
        doorbells.push((d.bdf, device_doorbells));
        bus.insert(f);
    }

    Fabric {
        bus,
        routing,
        doorbells,
        devices,
    }
}

/// The §1.2 thread taxonomy for a machine, for the boot log and `--check`.
pub fn thread_plan(cfg: &MachineConfig) -> Vec<String> {
    let n = cfg.compute.vcpus;
    let mut threads = vec!["main/control".to_string()];
    for i in 0..n {
        threads.push(format!("vcpu-{i}"));
    }
    // §5.1 / Revision E: request queue k of drive d is served by
    // `scsi{d}-q{k}`, pinned with vcpu-k. One worker per queue per drive.
    for drive in &cfg.storage.drives {
        for i in 0..n {
            threads.push(format!("scsi{}-q{i}", drive.drive_id));
        }
    }
    threads.extend(libvmm_net::thread_names(&cfg.network));
    if cfg.display.enabled {
        threads.push("media-capture".to_string());
        threads.push("media-encode".to_string());
    }
    if cfg.control_wss.enabled {
        threads.push("wss-listener".to_string());
    }
    threads
}
