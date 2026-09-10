//! `vmm-console-client` — remote console and embedded USB/IP server (§1.1).
//!
//! Three jobs, matching the three protocols the hypervisor exposes:
//!
//! * [`wss`] — the WSS control channel (§8), protocol v1
//! * [`rtsp_client`] — the RTSPS media stream (§7), reassembled to elementary
//!   streams, and [`decode`] to turn those back into pictures
//! * [`usbip_server`] — the embedded USB/IP server (§9), relaying real host
//!   devices over [`usbdev`]
//!
//! The binary in `main.rs` is a thin CLI over these.

#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod decode;
pub mod input;
pub mod rtsp_client;
pub mod transport;
pub mod usbdev;
pub mod usbip_server;
pub mod wayland;
pub mod wss;
