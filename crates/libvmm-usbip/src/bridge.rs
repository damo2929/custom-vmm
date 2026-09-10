//! xHCI bridge and import sequence — §9.2.
//!
//! ```text
//! 1. bridge --OP_REQ_DEVLIST--> server; server returns attached devices
//! 2. bridge --OP_REQ_IMPORT(busid)--> server
//! 3. server: unbind local kernel driver, bind usbip-host, reply OP_REP_IMPORT
//! 4. bridge attaches device to a virtual xHCI port -> guest enumerates it
//! 5. guest URB -> xHCI TRB -> USBIP_CMD_SUBMIT(seqnum) --> server --> device
//! 6. completion --> USBIP_RET_SUBMIT(seqnum) --> xHCI event ring --> guest
//!    cancellation: guest stop -> USBIP_CMD_UNLINK -> USBIP_RET_UNLINK
//! ```

use crate::wire::*;
use libvmm_core::{UsbipError, VmmResult};
use std::collections::HashMap;

/// The two USB/IP ports (§9). `use_tls` selects 3241.
pub const PORT_CLEARTEXT: u16 = 3240;
pub const PORT_TLS: u16 = 3241;

pub const fn port_for(use_tls: bool) -> u16 {
    if use_tls {
        PORT_TLS
    } else {
        PORT_CLEARTEXT
    }
}

/// State of one virtual xHCI port.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PortState {
    Empty,
    Attached {
        busid: String,
        devid: u32,
    },
    /// The socket dropped: the guest is signalled with a port-disconnect
    /// event and the port returns to Empty (§9.2).
    Disconnected {
        busid: String,
    },
}

/// The hypervisor-side xHCI controller bridging guest URBs to a remote
/// device (§9).
pub struct XhciBridge {
    ports: Vec<PortState>,
    /// Monotonic seqnum for outgoing submissions.
    next_seqnum: u32,
    /// In-flight submissions, so a completion can be matched back and an
    /// unlink can name the right seqnum.
    inflight: HashMap<u32, InFlight>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InFlight {
    pub port: usize,
    pub devid: u32,
    pub ep: u32,
    pub direction: u32,
}

impl XhciBridge {
    pub fn new(ports: u8) -> Self {
        XhciBridge {
            ports: vec![PortState::Empty; ports as usize],
            next_seqnum: 1,
            inflight: HashMap::new(),
        }
    }

    pub fn port_count(&self) -> usize {
        self.ports.len()
    }

    pub fn port(&self, index: usize) -> Option<&PortState> {
        self.ports.get(index)
    }

    /// §9.2 step 4 — attach an imported device to a free virtual port.
    pub fn attach(&mut self, busid: &str, devid: u32) -> VmmResult<usize> {
        let index = self
            .ports
            .iter()
            .position(|p| *p == PortState::Empty)
            .ok_or(UsbipError::NoFreePort {
                ports: self.ports.len() as u8,
            })?;
        self.ports[index] = PortState::Attached {
            busid: busid.to_string(),
            devid,
        };
        Ok(index)
    }

    /// A dropped socket detaches the device and signals the guest with an
    /// xHCI port-disconnect event (§9.2).
    ///
    /// Returns the in-flight seqnums that must be completed with an error, so
    /// the guest's URBs do not hang.
    pub fn on_peer_reset(&mut self, port: usize) -> Vec<u32> {
        let busid = match self.ports.get(port) {
            Some(PortState::Attached { busid, .. }) => busid.clone(),
            _ => return Vec::new(),
        };
        self.ports[port] = PortState::Disconnected { busid };
        let orphaned: Vec<u32> = self
            .inflight
            .iter()
            .filter(|(_, f)| f.port == port)
            .map(|(seq, _)| *seq)
            .collect();
        for seq in &orphaned {
            self.inflight.remove(seq);
        }
        orphaned
    }

    /// Acknowledge the disconnect event and free the port.
    pub fn clear_disconnect(&mut self, port: usize) {
        if matches!(self.ports.get(port), Some(PortState::Disconnected { .. })) {
            self.ports[port] = PortState::Empty;
        }
    }

    /// §9.2 step 5 — turn a guest URB into a `USBIP_CMD_SUBMIT`.
    pub fn submit(
        &mut self,
        port: usize,
        ep: u32,
        direction: u32,
        transfer_buffer_length: i32,
        setup: [u8; 8],
        interval: i32,
    ) -> VmmResult<CmdSubmit> {
        let devid = match self.ports.get(port) {
            Some(PortState::Attached { devid, .. }) => *devid,
            _ => {
                return Err(
                    UsbipError::PeerReset(format!("port {port} has no attached device")).into(),
                )
            }
        };

        let seqnum = self.next_seqnum;
        self.next_seqnum = self.next_seqnum.wrapping_add(1).max(1);
        self.inflight.insert(
            seqnum,
            InFlight {
                port,
                devid,
                ep,
                direction,
            },
        );

        Ok(CmdSubmit {
            header: HeaderBasic {
                command: USBIP_CMD_SUBMIT,
                seqnum,
                devid,
                direction,
                ep,
            },
            transfer_flags: 0,
            transfer_buffer_length,
            start_frame: 0,
            number_of_packets: -1,
            interval,
            setup,
        })
    }

    /// §9.2 step 6 — match a completion back to its submission.
    pub fn complete(&mut self, ret: &RetSubmit) -> VmmResult<InFlight> {
        self.inflight.remove(&ret.header.seqnum).ok_or_else(|| {
            UsbipError::BadWire {
                structure: "usbip_ret_submit",
                detail: format!(
                    "seqnum {} does not match any in-flight URB",
                    ret.header.seqnum
                ),
            }
            .into()
        })
    }

    /// Guest-initiated cancellation.
    pub fn unlink(&mut self, seqnum: u32) -> VmmResult<CmdUnlink> {
        let flight =
            self.inflight
                .get(&seqnum)
                .copied()
                .ok_or_else(|| -> libvmm_core::VmmError {
                    UsbipError::BadWire {
                        structure: "usbip_cmd_unlink",
                        detail: format!("seqnum {seqnum} is not in flight"),
                    }
                    .into()
                })?;
        let unlink_seq = self.next_seqnum;
        self.next_seqnum = self.next_seqnum.wrapping_add(1).max(1);
        Ok(CmdUnlink {
            header: HeaderBasic {
                command: USBIP_CMD_UNLINK,
                seqnum: unlink_seq,
                devid: flight.devid,
                direction: DIRECTION_OUT,
                ep: 0,
            },
            unlink_seqnum: seqnum,
        })
    }

    pub fn inflight_count(&self) -> usize {
        self.inflight.len()
    }
}

/// Server-side import policy — §9.2 isolation.
///
/// "Only the imported device is bridged; the server MUST reject import of a
/// busid not in its allow-set."
#[derive(Debug, Default)]
pub struct ImportPolicy {
    allow: Vec<String>,
}

impl ImportPolicy {
    pub fn new(allow: impl IntoIterator<Item = String>) -> Self {
        ImportPolicy {
            allow: allow.into_iter().collect(),
        }
    }

    pub fn permits(&self, busid: &str) -> bool {
        self.allow.iter().any(|a| a == busid)
    }

    /// Answer an `OP_REQ_IMPORT`. A busid outside the allow-set is refused
    /// with `Usbip(ImportDenied)` 7001.
    pub fn authorize(&self, busid: &str) -> VmmResult<()> {
        if !self.permits(busid) {
            return Err(UsbipError::ImportDenied {
                busid: busid.to_string(),
            }
            .into());
        }
        Ok(())
    }

    /// The `op_common` status for an import decision.
    pub fn status_for(&self, busid: &str) -> u32 {
        if self.permits(busid) {
            ST_OK
        } else {
            ST_NA
        }
    }
}
