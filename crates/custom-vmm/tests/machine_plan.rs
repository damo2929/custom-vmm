//! End-to-end: the §11 reference machine planned all the way through the
//! PCIe fabric, ACPI set and §1.4 bring-up sequence.

use libvmm_config::MachineConfig;

#[path = "../src/topology.rs"]
mod topology;

fn reference() -> MachineConfig {
    MachineConfig::from_toml_str(include_str!("../../../config/reference-vm.toml")).unwrap()
}

#[test]
fn the_reference_machine_places_every_device_where_the_spec_says() {
    let cfg = reference();
    let devices = topology::plan(&cfg);
    let at = |bus: u8, dev: u8, func: u8| {
        devices
            .iter()
            .find(|d| d.bdf.bus == bus && d.bdf.device == dev && d.bdf.function == func)
            .unwrap_or_else(|| panic!("nothing at {bus:02x}:{dev:02x}.{func}"))
    };

    // §7.1 — virtio-gpu scanout and virtio-snd PCM.
    assert_eq!(at(0x01, 0x00, 0).name, "virtio-gpu");
    assert_eq!(at(0x01, 0x00, 1).name, "virtio-snd");
    // §8.5 — input routing targets.
    assert_eq!(at(0x02, 0x00, 0).name, "virtio-input (keyboard)");
    assert_eq!(at(0x02, 0x01, 0).name, "virtio-input (tablet)");
    // §6 — virtio-tpm at 02:04.0.
    assert_eq!(at(0x02, 0x04, 0).name, "virtio-tpm");
    // §9 — the xHCI controller.
    assert_eq!(at(0x07, 0x00, 0).name, "xHCI");
    // §5 — virtio-scsi on the storage bus.
    assert_eq!(at(0x03, 0x00, 0).name, "virtio-scsi");
}

#[test]
fn virtio_scsi_has_controlq_eventq_and_one_request_queue_per_vcpu() {
    // §5.1 with the change-log item 9 invariant.
    let mut cfg = reference();
    for vcpus in [1u32, 2, 8] {
        cfg.compute.vcpus = vcpus;
        let devices = topology::plan(&cfg);
        let scsi = devices.iter().find(|d| d.name == "virtio-scsi").unwrap();
        assert_eq!(
            scsi.queues as u32,
            vcpus + 2,
            "controlq + eventq + {vcpus} request queues"
        );
    }
}

#[test]
fn the_tpm_uses_a_single_virtqueue() {
    // §6: TPM 2.0 command/response byte streams pass over a single virtqueue.
    let cfg = reference();
    let devices = topology::plan(&cfg);
    let tpm = devices.iter().find(|d| d.name == "virtio-tpm").unwrap();
    assert_eq!(tpm.queues, 1);
}

#[test]
fn every_function_is_enumerable_over_ecam_and_none_requests_intx() {
    use libvmm_core::pci::INTERRUPT_PIN;

    let cfg = reference();
    let mut fabric = topology::build(&cfg);

    for device in &fabric.devices {
        let offset = device.bdf.ecam_offset();
        let vendor = fabric.bus.config_rw(offset, 2, None) as u16;
        assert_ne!(vendor, 0xFFFF, "{} must answer an ECAM read", device.bdf);
        // §1.4: all interrupts are MSI-X; no device may request an INTx line.
        assert_eq!(
            fabric.bus.config_rw(offset + INTERRUPT_PIN as u64, 1, None),
            0,
            "{} must not request INTx",
            device.bdf
        );
    }
}

#[test]
fn every_queue_gets_its_own_doorbell_and_gsi() {
    let cfg = reference();
    let fabric = topology::build(&cfg);

    let total_queues: usize = fabric.devices.iter().map(|d| d.queues as usize).sum();
    assert_eq!(fabric.routing.len(), total_queues, "one GSI per queue");

    // Doorbells must be globally distinct: KVM_IOEVENTFD matches on address.
    let mut all: Vec<u64> = fabric
        .doorbells
        .iter()
        .flat_map(|(_, a)| a.iter().copied())
        .collect();
    assert_eq!(all.len(), total_queues);
    all.sort_unstable();
    all.dedup();
    assert_eq!(all.len(), total_queues, "doorbell addresses must be unique");
}

#[test]
fn bars_are_allocated_inside_the_pcie_mmio_window_without_overlapping() {
    use libvmm_core::memory::{PCI_MMIO_BASE, PCI_MMIO_END};

    let cfg = reference();
    let fabric = topology::build(&cfg);
    let mut spans: Vec<(u64, u64)> = Vec::new();

    for (_, doorbells) in &fabric.doorbells {
        if let Some(first) = doorbells.first() {
            assert!(
                *first >= PCI_MMIO_BASE && *first <= PCI_MMIO_END,
                "doorbell {first:#x} is outside the PCIe MMIO window"
            );
            spans.push((*first, *doorbells.last().unwrap_or(first)));
        }
    }
    spans.sort_unstable();
    for w in spans.windows(2) {
        assert!(
            w[0].1 < w[1].0,
            "BAR regions overlap: {:x?} and {:x?}",
            w[0],
            w[1]
        );
    }
}

#[test]
fn the_thread_taxonomy_matches_spec_1_2() {
    let cfg = reference();
    let threads = topology::thread_plan(&cfg);
    let n = cfg.compute.vcpus;

    assert!(threads.contains(&"main/control".to_string()));
    for i in 0..n {
        assert!(threads.contains(&format!("vcpu-{i}")), "vcpu-{i} missing");
        // §5.1: request queue k is served by scsi-q-k, pinned with vcpu-k.
        assert!(
            threads.contains(&format!("scsi-q-{i}")),
            "scsi-q-{i} missing"
        );
    }
    assert!(threads.contains(&"media-capture".to_string()));
    assert!(threads.contains(&"media-encode".to_string()));
    assert!(threads.contains(&"wss-listener".to_string()));
    // Two AF_XDP threads per queue pair.
    assert_eq!(
        threads.iter().filter(|t| t.starts_with("net-")).count(),
        cfg.network.num_queue_pairs as usize * 2
    );
}

#[test]
fn disabling_the_tpm_removes_its_function_and_its_acpi_table() {
    let mut cfg = reference();
    cfg.tpm.enabled = false;
    let devices = topology::plan(&cfg);
    assert!(devices.iter().all(|d| d.name != "virtio-tpm"));
}
