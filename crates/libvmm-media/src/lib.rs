//! `libvmm-media` — virtio-gpu/snd capture, negotiated encode, RTSPS (§7).
//!
//! [`plane`] is the shape of §7: one `media-capture` thread, one
//! `media-encode` thread, and `0..n` `rtsp-session` threads that frame and
//! write what the encoder produced. [`server`] is the RTSPS listener that
//! spawns those sessions.

// §0.1: datapath code MUST NOT unwrap, expect or panic. Test code may.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
#![cfg_attr(test, allow(clippy::unwrap_used, clippy::expect_used, clippy::panic))]

pub mod depacketize;
pub mod encoder;
pub mod negotiate;
pub mod packetize;
pub mod pipeline;
pub mod plane;
pub mod rtp;
pub mod rtsp;
pub mod server;

pub use depacketize::RtpPacket;
pub use encoder::{AudioEncoderParams, ScanoutFormat, VideoEncoderParams};
pub use negotiate::{Answer, Offer, Selection};
pub use packetize::Packet;
pub use pipeline::{AudioPipeline, VideoOutput, VideoPipeline};
pub use plane::{CaptureSource, MediaPlane, PlaneStats, SessionStream, StreamUnit};
pub use rtsp::{Method, RequestBuilder, Response, Session, SessionState};
