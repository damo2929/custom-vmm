//! Feature negotiation — §2.3.
//!
//! `VIRTIO_F_VERSION_1` and `VIRTIO_F_RING_PACKED` MUST be offered;
//! `VIRTIO_F_IN_ORDER` and `VIRTIO_F_NOTIFICATION_DATA` SHOULD be. Split
//! rings are accepted as a fallback for guests that reject packed rings.

/// Transport feature bits (virtio 1.2 §6).
pub const VIRTIO_F_INDIRECT_DESC: u64 = 1 << 28;
pub const VIRTIO_F_EVENT_IDX: u64 = 1 << 29;
pub const VIRTIO_F_VERSION_1: u64 = 1 << 32;
pub const VIRTIO_F_ACCESS_PLATFORM: u64 = 1 << 33;
pub const VIRTIO_F_RING_PACKED: u64 = 1 << 34;
pub const VIRTIO_F_IN_ORDER: u64 = 1 << 35;
pub const VIRTIO_F_ORDER_PLATFORM: u64 = 1 << 36;
pub const VIRTIO_F_NOTIFICATION_DATA: u64 = 1 << 38;

/// Bits every device in this VMM MUST offer (§2.3).
pub const MUST_OFFER: u64 = VIRTIO_F_VERSION_1 | VIRTIO_F_RING_PACKED;

/// Bits every device SHOULD offer (§2.3).
pub const SHOULD_OFFER: u64 = VIRTIO_F_IN_ORDER | VIRTIO_F_NOTIFICATION_DATA;

/// The transport feature set common to all devices here.
pub const COMMON_OFFER: u64 =
    MUST_OFFER | SHOULD_OFFER | VIRTIO_F_INDIRECT_DESC | VIRTIO_F_EVENT_IDX;

/// Does this offer satisfy the §2.3 MUST?
pub const fn offer_is_conformant(offered: u64) -> bool {
    offered & MUST_OFFER == MUST_OFFER
}

/// Which ring layout the negotiated set selects.
///
/// Packed is used when the guest acked `VIRTIO_F_RING_PACKED`; otherwise the
/// device falls back to split rings, which §2.3 explicitly permits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RingLayout {
    Split,
    Packed,
}

pub const fn ring_layout(acked: u64) -> RingLayout {
    if acked & VIRTIO_F_RING_PACKED != 0 {
        RingLayout::Packed
    } else {
        RingLayout::Split
    }
}

/// Human-readable feature names, for the negotiation log line.
pub fn describe(bits: u64) -> Vec<&'static str> {
    let mut v = Vec::new();
    for (bit, name) in [
        (VIRTIO_F_INDIRECT_DESC, "INDIRECT_DESC"),
        (VIRTIO_F_EVENT_IDX, "EVENT_IDX"),
        (VIRTIO_F_VERSION_1, "VERSION_1"),
        (VIRTIO_F_ACCESS_PLATFORM, "ACCESS_PLATFORM"),
        (VIRTIO_F_RING_PACKED, "RING_PACKED"),
        (VIRTIO_F_IN_ORDER, "IN_ORDER"),
        (VIRTIO_F_ORDER_PLATFORM, "ORDER_PLATFORM"),
        (VIRTIO_F_NOTIFICATION_DATA, "NOTIFICATION_DATA"),
    ] {
        if bits & bit != 0 {
            v.push(name);
        }
    }
    v
}
