//! Virtqueue engine: split and packed rings, feature negotiation, and the
//! §2.3 device-status handshake.

use libvmm_virtio::features::*;
use libvmm_virtio::pci_cap::BarLayout;
use libvmm_virtio::queue::*;
use libvmm_virtio::status::*;
use std::cell::RefCell;

/// A flat block of "guest RAM" starting at GPA 0.
struct FakeMemory {
    bytes: RefCell<Vec<u8>>,
}

impl FakeMemory {
    fn new(size: usize) -> Self {
        FakeMemory {
            bytes: RefCell::new(vec![0u8; size]),
        }
    }
    fn poke(&self, gpa: u64, data: &[u8]) {
        self.bytes.borrow_mut()[gpa as usize..gpa as usize + data.len()].copy_from_slice(data);
    }
    fn peek_u16(&self, gpa: u64) -> u16 {
        let b = self.bytes.borrow();
        u16::from_le_bytes([b[gpa as usize], b[gpa as usize + 1]])
    }
    fn peek_u32(&self, gpa: u64) -> u32 {
        let b = self.bytes.borrow();
        u32::from_le_bytes(b[gpa as usize..gpa as usize + 4].try_into().unwrap())
    }
}

impl GuestMemory for FakeMemory {
    fn read(&self, gpa: u64, out: &mut [u8]) -> libvmm_core::VmmResult<()> {
        let b = self.bytes.borrow();
        let end = gpa as usize + out.len();
        if end > b.len() {
            return Err(libvmm_core::VirtioError::BadDescriptor {
                queue: 0,
                detail: "oob read".into(),
            }
            .into());
        }
        out.copy_from_slice(&b[gpa as usize..end]);
        Ok(())
    }
    fn write(&self, gpa: u64, data: &[u8]) -> libvmm_core::VmmResult<()> {
        let mut b = self.bytes.borrow_mut();
        let end = gpa as usize + data.len();
        if end > b.len() {
            return Err(libvmm_core::VirtioError::BadDescriptor {
                queue: 0,
                detail: "oob write".into(),
            }
            .into());
        }
        b[gpa as usize..end].copy_from_slice(data);
        Ok(())
    }
    fn is_valid_range(&self, gpa: u64, len: u64) -> bool {
        (gpa + len) as usize <= self.bytes.borrow().len()
    }
}

// Split-ring layout used by the tests.
const QSIZE: u16 = 8;
const DESC: u64 = 0x1000;
const AVAIL: u64 = 0x2000;
const USED: u64 = 0x3000;
const DATA: u64 = 0x4000;

fn split_queue() -> Virtqueue {
    let mut q = Virtqueue::new(0, QSIZE, RingLayout::Split);
    q.addresses = RingAddresses {
        desc: DESC,
        driver: AVAIL,
        device: USED,
    };
    q.enable().unwrap();
    q
}

fn write_split_desc(mem: &FakeMemory, index: u16, addr: u64, len: u32, flags: u16, next: u16) {
    let base = DESC + index as u64 * 16;
    mem.poke(base, &addr.to_le_bytes());
    mem.poke(base + 8, &len.to_le_bytes());
    mem.poke(base + 12, &flags.to_le_bytes());
    mem.poke(base + 14, &next.to_le_bytes());
}

/// Publish descriptor chain `head` in the avail ring at position `slot`.
fn publish_avail(mem: &FakeMemory, slot: u16, head: u16, new_idx: u16) {
    mem.poke(AVAIL + 4 + slot as u64 * 2, &head.to_le_bytes());
    mem.poke(AVAIL + 2, &new_idx.to_le_bytes());
}

// -- §2.3 feature negotiation ------------------------------------------------

#[test]
fn devices_must_offer_version_1_and_ring_packed() {
    assert!(offer_is_conformant(COMMON_OFFER));
    assert_eq!(MUST_OFFER, VIRTIO_F_VERSION_1 | VIRTIO_F_RING_PACKED);
    assert!(
        !offer_is_conformant(VIRTIO_F_VERSION_1),
        "RING_PACKED is a MUST"
    );
    assert!(
        !offer_is_conformant(VIRTIO_F_RING_PACKED),
        "VERSION_1 is a MUST"
    );
}

#[test]
fn devices_should_offer_in_order_and_notification_data() {
    assert_eq!(COMMON_OFFER & SHOULD_OFFER, SHOULD_OFFER);
}

#[test]
fn split_rings_are_the_fallback_when_packed_is_not_acked() {
    assert_eq!(ring_layout(COMMON_OFFER), RingLayout::Packed);
    assert_eq!(ring_layout(VIRTIO_F_VERSION_1), RingLayout::Split);
}

// -- §2.3 device status handshake -------------------------------------------

#[test]
fn status_handshake_follows_ack_driver_features_ok_driver_ok() {
    let mut s = DeviceStatus::new();
    s.write("virtio-scsi", ACKNOWLEDGE).unwrap();
    s.write("virtio-scsi", ACKNOWLEDGE | DRIVER).unwrap();
    s.write("virtio-scsi", ACKNOWLEDGE | DRIVER | FEATURES_OK)
        .unwrap();
    assert!(!s.is_running(), "the device must not run before DRIVER_OK");
    s.write(
        "virtio-scsi",
        ACKNOWLEDGE | DRIVER | FEATURES_OK | DRIVER_OK,
    )
    .unwrap();
    assert!(s.is_running());
}

#[test]
fn out_of_order_status_bits_are_refused() {
    let mut s = DeviceStatus::new();
    // DRIVER without ACKNOWLEDGE.
    let e = s.write("virtio-scsi", DRIVER).unwrap_err();
    assert_eq!(e.code(), 3002);

    let mut s = DeviceStatus::new();
    s.write("virtio-scsi", ACKNOWLEDGE).unwrap();
    // DRIVER_OK skipping FEATURES_OK.
    let e = s.write("virtio-scsi", ACKNOWLEDGE | DRIVER_OK).unwrap_err();
    assert_eq!(e.code(), 3002);
}

#[test]
fn clearing_features_ok_makes_the_device_refuse_to_run() {
    // §2.3: "If the guest clears FEATURES_OK the device MUST refuse to run
    // and log Virtio(FeatureMismatch)."
    let mut s = DeviceStatus::new();
    s.write("virtio-scsi", ACKNOWLEDGE).unwrap();
    s.write("virtio-scsi", ACKNOWLEDGE | DRIVER).unwrap();
    s.write("virtio-scsi", ACKNOWLEDGE | DRIVER | FEATURES_OK)
        .unwrap();

    let e = s.write("virtio-scsi", ACKNOWLEDGE | DRIVER).unwrap_err();
    assert_eq!(e.code(), 3001, "must be Virtio(FeatureMismatch)");
    assert!(s.features_rejected());
    assert!(!s.is_running());
}

#[test]
fn writing_zero_resets_the_device() {
    let mut s = DeviceStatus::new();
    s.write("virtio-net", ACKNOWLEDGE).unwrap();
    s.write("virtio-net", 0).unwrap();
    assert_eq!(s.bits(), 0);
}

// -- split ring --------------------------------------------------------------

#[test]
fn split_ring_walks_a_read_write_chain() {
    let mem = FakeMemory::new(0x10000);
    let mut q = split_queue();
    let mut chain = DescriptorChain::new();

    // A typical virtio-scsi request: 64-byte request out, 108-byte response in.
    write_split_desc(&mem, 0, DATA, 64, VIRTQ_DESC_F_NEXT, 1);
    write_split_desc(&mem, 1, DATA + 64, 108, VIRTQ_DESC_F_WRITE, 0);
    publish_avail(&mem, 0, 0, 1);

    assert!(q.pop(&mem, &mut chain).unwrap());
    assert_eq!(chain.head, 0);
    assert_eq!(chain.len(), 2);
    assert_eq!(chain.readable_bytes(), 64);
    assert_eq!(chain.writable_bytes(), 108);
    assert_eq!(chain.readable().next().unwrap().addr, DATA);
    assert!(chain.writable().next().unwrap().writable);

    // The ring is now empty.
    assert!(!q.pop(&mem, &mut chain).unwrap());
}

#[test]
fn split_ring_completion_publishes_id_and_length_before_the_index() {
    let mem = FakeMemory::new(0x10000);
    let mut q = split_queue();
    let mut chain = DescriptorChain::new();

    write_split_desc(&mem, 3, DATA, 32, VIRTQ_DESC_F_WRITE, 0);
    publish_avail(&mem, 0, 3, 1);
    q.pop(&mem, &mut chain).unwrap();
    q.push(&mem, &chain, 32).unwrap();

    assert_eq!(
        mem.peek_u32(USED + 4),
        3,
        "used ring must carry the head index"
    );
    assert_eq!(
        mem.peek_u32(USED + 8),
        32,
        "used ring must carry the written length"
    );
    assert_eq!(mem.peek_u16(USED + 2), 1, "used idx advances once");
}

#[test]
fn split_ring_follows_indirect_descriptor_tables() {
    let mem = FakeMemory::new(0x10000);
    let mut q = split_queue();
    let mut chain = DescriptorChain::new();

    // Indirect table of two entries at 0x5000.
    let table = 0x5000u64;
    for (i, (addr, len, flags, next)) in [
        (DATA, 16u32, VIRTQ_DESC_F_NEXT, 1u16),
        (DATA + 16, 32u32, VIRTQ_DESC_F_WRITE, 0u16),
    ]
    .iter()
    .enumerate()
    {
        let base = table + i as u64 * 16;
        mem.poke(base, &addr.to_le_bytes());
        mem.poke(base + 8, &len.to_le_bytes());
        mem.poke(base + 12, &flags.to_le_bytes());
        mem.poke(base + 14, &next.to_le_bytes());
    }
    write_split_desc(&mem, 0, table, 32, VIRTQ_DESC_F_INDIRECT, 0);
    publish_avail(&mem, 0, 0, 1);

    assert!(q.pop(&mem, &mut chain).unwrap());
    assert_eq!(chain.len(), 2);
    assert_eq!(chain.readable_bytes(), 16);
    assert_eq!(chain.writable_bytes(), 32);
}

#[test]
fn a_descriptor_loop_is_rejected_not_followed_forever() {
    let mem = FakeMemory::new(0x10000);
    let mut q = split_queue();
    let mut chain = DescriptorChain::new();

    // 0 -> 1 -> 0 -> ...
    write_split_desc(&mem, 0, DATA, 8, VIRTQ_DESC_F_NEXT, 1);
    write_split_desc(&mem, 1, DATA + 8, 8, VIRTQ_DESC_F_NEXT, 0);
    publish_avail(&mem, 0, 0, 1);

    let e = q.pop(&mem, &mut chain).unwrap_err();
    assert_eq!(e.code(), 3005, "must be Virtio(BadDescriptor)");
}

#[test]
fn a_buffer_outside_guest_ram_is_rejected() {
    let mem = FakeMemory::new(0x10000);
    let mut q = split_queue();
    let mut chain = DescriptorChain::new();

    write_split_desc(&mem, 0, 0xFFFF_0000, 4096, VIRTQ_DESC_F_WRITE, 0);
    publish_avail(&mem, 0, 0, 1);

    let e = q.pop(&mem, &mut chain).unwrap_err();
    assert_eq!(e.code(), 3005);
}

#[test]
fn a_head_index_beyond_the_ring_is_rejected() {
    let mem = FakeMemory::new(0x10000);
    let mut q = split_queue();
    let mut chain = DescriptorChain::new();
    publish_avail(&mem, 0, QSIZE + 5, 1);
    assert_eq!(q.pop(&mem, &mut chain).unwrap_err().code(), 3005);
}

// -- packed ring -------------------------------------------------------------

fn write_packed_desc(mem: &FakeMemory, slot: u16, addr: u64, len: u32, id: u16, flags: u16) {
    let base = DESC + slot as u64 * 16;
    mem.poke(base, &addr.to_le_bytes());
    mem.poke(base + 8, &len.to_le_bytes());
    mem.poke(base + 12, &id.to_le_bytes());
    mem.poke(base + 14, &flags.to_le_bytes());
}

#[test]
fn packed_ring_walks_a_chain_and_completes_it_in_place() {
    let mem = FakeMemory::new(0x10000);
    let mut q = Virtqueue::new(0, QSIZE, RingLayout::Packed);
    q.addresses = RingAddresses {
        desc: DESC,
        driver: AVAIL,
        device: USED,
    };
    q.enable().unwrap();
    let mut chain = DescriptorChain::new();

    // Two descriptors, buffer id 7 on the last one. AVAIL set, USED clear.
    write_packed_desc(&mem, 0, DATA, 64, 0, VIRTQ_DESC_F_AVAIL | VIRTQ_DESC_F_NEXT);
    write_packed_desc(
        &mem,
        1,
        DATA + 64,
        108,
        7,
        VIRTQ_DESC_F_AVAIL | VIRTQ_DESC_F_WRITE,
    );

    assert!(q.pop(&mem, &mut chain).unwrap());
    assert_eq!(
        chain.head, 7,
        "the chain is identified by the last buffer id"
    );
    assert_eq!(chain.len(), 2);
    assert_eq!(chain.writable_bytes(), 108);

    q.push(&mem, &chain, 108).unwrap();
    assert_eq!(
        mem.peek_u16(DESC + 12),
        7,
        "completion carries the buffer id"
    );
    assert_eq!(mem.peek_u32(DESC + 8), 108);
    let flags = mem.peek_u16(DESC + 14);
    assert_ne!(
        flags & VIRTQ_DESC_F_USED,
        0,
        "USED must be set on completion"
    );
}

#[test]
fn packed_ring_reports_empty_when_the_wrap_counter_does_not_match() {
    let mem = FakeMemory::new(0x10000);
    let mut q = Virtqueue::new(0, QSIZE, RingLayout::Packed);
    q.addresses = RingAddresses {
        desc: DESC,
        driver: AVAIL,
        device: USED,
    };
    q.enable().unwrap();
    let mut chain = DescriptorChain::new();

    // AVAIL clear: not yet made available by the driver.
    write_packed_desc(&mem, 0, DATA, 64, 0, 0);
    assert!(!q.pop(&mem, &mut chain).unwrap());
}

// -- queue configuration -----------------------------------------------------

#[test]
fn a_queue_cannot_be_enabled_before_its_rings_are_programmed() {
    let mut q = Virtqueue::new(0, QSIZE, RingLayout::Split);
    assert_eq!(q.enable().unwrap_err().code(), 3008);
}

#[test]
fn a_non_power_of_two_queue_size_is_refused() {
    let mut q = Virtqueue::new(0, 100, RingLayout::Split);
    q.addresses = RingAddresses {
        desc: DESC,
        driver: AVAIL,
        device: USED,
    };
    assert_eq!(q.enable().unwrap_err().code(), 3007);
}

// -- §2.2 BAR layout ---------------------------------------------------------

#[test]
fn doorbells_are_spaced_by_the_notify_multiplier() {
    let l = BarLayout::new(4, 36);
    let base = 0xD000_0000;
    for q in 0..4u16 {
        assert_eq!(
            l.doorbell_address(base, q),
            base + l.notify_offset as u64 + q as u64 * l.notify_off_multiplier as u64
        );
    }
    // Distinct doorbells are what make per-queue KVM_IOEVENTFD data matching
    // possible (§2.2).
    assert_ne!(l.doorbell_address(base, 0), l.doorbell_address(base, 1));
    assert!(l.bar_size.is_power_of_two());
}
