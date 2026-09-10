//! `libvmm-net` — virtio-net over a pure-Rust AF_XDP datapath (§1.1, §11).
//!
//! The `rust_af_xdp` engine is a pure-Rust reimplementation, not a binding to
//! libbpf or libxdp: §1.1 forbids C linkage, and CI enforces it.
//!
//! Implementation status: the device model, feature set and thread taxonomy
//! below are complete; the AF_XDP ring datapath itself is not implemented in
//! this first cut. See `IMPLEMENTATION-STATUS.md`.

// §0.1: datapath code MUST NOT unwrap, expect or panic. Test code may.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

use libvmm_config::Network;
use libvmm_virtio::features;

/// virtio-net feature bits this device offers, on top of the transport set.
pub const VIRTIO_NET_F_MAC: u64 = 1 << 5;
pub const VIRTIO_NET_F_MRG_RXBUF: u64 = 1 << 15;
pub const VIRTIO_NET_F_STATUS: u64 = 1 << 16;
pub const VIRTIO_NET_F_MQ: u64 = 1 << 22;
pub const VIRTIO_NET_F_CTRL_VQ: u64 = 1 << 17;

/// The full offer for a virtio-net function.
pub const fn offered_features() -> u64 {
    features::COMMON_OFFER
        | VIRTIO_NET_F_MAC
        | VIRTIO_NET_F_MRG_RXBUF
        | VIRTIO_NET_F_STATUS
        | VIRTIO_NET_F_MQ
        | VIRTIO_NET_F_CTRL_VQ
}

/// `struct virtio_net_config` — the device-specific configuration space.
#[derive(Debug, Clone, Copy)]
pub struct NetConfig {
    pub mac: [u8; 6],
    pub status: u16,
    pub max_virtqueue_pairs: u16,
    pub mtu: u16,
}

impl NetConfig {
    pub const LEN: usize = 12;
    /// VIRTIO_NET_S_LINK_UP.
    pub const LINK_UP: u16 = 1;

    pub fn new(mac: [u8; 6], queue_pairs: u16) -> Self {
        NetConfig {
            mac,
            status: Self::LINK_UP,
            max_virtqueue_pairs: queue_pairs,
            mtu: 1500,
        }
    }

    /// Serialise little-endian, as all guest-facing virtio fields are (§0.1).
    pub fn to_bytes(&self) -> [u8; Self::LEN] {
        let mut b = [0u8; Self::LEN];
        b[0..6].copy_from_slice(&self.mac);
        b[6..8].copy_from_slice(&self.status.to_le_bytes());
        b[8..10].copy_from_slice(&self.max_virtqueue_pairs.to_le_bytes());
        b[10..12].copy_from_slice(&self.mtu.to_le_bytes());
        b
    }
}

/// Parse a `52:54:00:12:34:56` MAC. The config layer has already validated
/// the syntax; this is the conversion.
pub fn parse_mac(s: &str) -> Option<[u8; 6]> {
    let mut out = [0u8; 6];
    let mut parts = s.split(':');
    for slot in out.iter_mut() {
        *slot = u8::from_str_radix(parts.next()?, 16).ok()?;
    }
    parts.next().is_none().then_some(out)
}

/// The AF_XDP thread taxonomy from §1.2: `net-rx` and `net-tx`, two per
/// queue pair, on housekeeping cores.
pub fn thread_names(net: &Network) -> Vec<String> {
    let mut names = Vec::with_capacity(net.num_queue_pairs as usize * 2);
    for qp in 0..net.num_queue_pairs {
        names.push(format!("net-rx-{qp}"));
        names.push(format!("net-tx-{qp}"));
    }
    names
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mac_parses_and_serialises_little_endian() {
        let mac = parse_mac("52:54:00:12:34:56").unwrap();
        assert_eq!(mac, [0x52, 0x54, 0x00, 0x12, 0x34, 0x56]);
        let c = NetConfig::new(mac, 2);
        let b = c.to_bytes();
        assert_eq!(&b[0..6], &mac);
        assert_eq!(u16::from_le_bytes([b[6], b[7]]), NetConfig::LINK_UP);
        assert_eq!(u16::from_le_bytes([b[8], b[9]]), 2);
    }

    #[test]
    fn a_malformed_mac_is_rejected() {
        assert!(parse_mac("52:54:00:12:34").is_none());
        assert!(parse_mac("52:54:00:12:34:56:78").is_none());
        assert!(parse_mac("zz:54:00:12:34:56").is_none());
    }

    #[test]
    fn one_rx_and_tx_thread_per_queue_pair() {
        let cfg = libvmm_config::MachineConfig::from_toml_str(include_str!(
            "../../../config/reference-vm.toml"
        ))
        .unwrap();
        let names = thread_names(&cfg.network);
        assert_eq!(names.len(), cfg.network.num_queue_pairs as usize * 2);
        assert!(names.contains(&"net-rx-0".to_string()));
        assert!(names.contains(&"net-tx-1".to_string()));
    }
}
