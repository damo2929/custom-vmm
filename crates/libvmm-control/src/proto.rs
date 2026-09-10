//! Control protocol v1 — §8.5.
//!
//! UTF-8 JSON text frames. Every frame carries `"v":1`. The client sends a
//! monotonic `seq`; the server echoes it in `ack`/`error`.
//!
//! Robustness rule (§8.5): a bad frame yields an error frame and MUST NOT
//! close the socket.

use libvmm_core::ControlError;
use serde::{Deserialize, Serialize};

/// The protocol version. Independently versioned from the document (§0.1).
pub const PROTOCOL_VERSION: u32 = 1;

/// Tablet coordinates are 0..32767, mapped to the 1920x1080 scanout (§8.5).
pub const ABS_MAX: i64 = 32767;

// ---------------------------------------------------------------------------
// Client -> Server
// ---------------------------------------------------------------------------

/// A frame from a control client.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(tag = "action", rename_all = "lowercase")]
pub enum ClientFrame {
    Input(InputFrame),
    /// ACPI Power Button SCI.
    Powerdown {
        v: u32,
        seq: u64,
    },
    /// Pulse FADT.RESET_REG.
    Reboot {
        v: u32,
        seq: u64,
    },
    Backup {
        v: u32,
        seq: u64,
        path: String,
    },
}

/// `{"action":"input", ...}` — keyboard or tablet.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(tag = "device", rename_all = "lowercase")]
pub enum InputFrame {
    /// EV_KEY: `code` is a Linux keycode, `value` 0=release 1=press 2=repeat.
    Keyboard {
        v: u32,
        seq: u64,
        #[serde(rename = "type")]
        event_type: String,
        code: i64,
        value: i64,
    },
    /// EV_ABS: `x`,`y` in 0..32767 mapped to the scanout.
    Tablet {
        v: u32,
        seq: u64,
        #[serde(rename = "type")]
        event_type: String,
        x: i64,
        y: i64,
        #[serde(default)]
        buttons: Buttons,
    },
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, Serialize)]
pub struct Buttons {
    #[serde(default)]
    pub left: bool,
    #[serde(default)]
    pub right: bool,
    #[serde(default)]
    pub middle: bool,
}

impl InputFrame {
    pub const fn seq(&self) -> u64 {
        match self {
            InputFrame::Keyboard { seq, .. } | InputFrame::Tablet { seq, .. } => *seq,
        }
    }
    pub const fn version(&self) -> u32 {
        match self {
            InputFrame::Keyboard { v, .. } | InputFrame::Tablet { v, .. } => *v,
        }
    }
    /// Which PCI function the event is routed to (§8.5).
    pub const fn target_bdf(&self) -> (u8, u8, u8) {
        match self {
            // keyboard -> 02:00.0
            InputFrame::Keyboard { .. } => (0x02, 0x00, 0),
            // tablet -> 02:01.0
            InputFrame::Tablet { .. } => (0x02, 0x01, 0),
        }
    }
}

impl ClientFrame {
    pub const fn seq(&self) -> u64 {
        match self {
            ClientFrame::Input(i) => i.seq(),
            ClientFrame::Powerdown { seq, .. }
            | ClientFrame::Reboot { seq, .. }
            | ClientFrame::Backup { seq, .. } => *seq,
        }
    }

    pub const fn version(&self) -> u32 {
        match self {
            ClientFrame::Input(i) => i.version(),
            ClientFrame::Powerdown { v, .. }
            | ClientFrame::Reboot { v, .. }
            | ClientFrame::Backup { v, .. } => *v,
        }
    }

    pub const fn action(&self) -> &'static str {
        match self {
            ClientFrame::Input(_) => "input",
            ClientFrame::Powerdown { .. } => "powerdown",
            ClientFrame::Reboot { .. } => "reboot",
            ClientFrame::Backup { .. } => "backup",
        }
    }
}

// ---------------------------------------------------------------------------
// Server -> Client
// ---------------------------------------------------------------------------

/// A frame to a control client (§8.5).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ServerFrame {
    Ack {
        v: u32,
        seq: u64,
    },
    Error {
        v: u32,
        seq: u64,
        code: u32,
        message: String,
    },
    Progress {
        v: u32,
        action: String,
        percent: u32,
        bytes: u64,
        total_bytes: u64,
    },
    Complete {
        v: u32,
        action: String,
        path: String,
        sha256: String,
    },
}

impl ServerFrame {
    pub const fn ack(seq: u64) -> Self {
        ServerFrame::Ack {
            v: PROTOCOL_VERSION,
            seq,
        }
    }

    /// Build an error frame from a control error, carrying its Appendix A code.
    pub fn error(seq: u64, e: &ControlError) -> Self {
        ServerFrame::Error {
            v: PROTOCOL_VERSION,
            seq,
            code: e.code(),
            message: e.to_string(),
        }
    }

    pub fn error_with(seq: u64, code: u32, message: impl Into<String>) -> Self {
        ServerFrame::Error {
            v: PROTOCOL_VERSION,
            seq,
            code,
            message: message.into(),
        }
    }

    pub fn backup_progress(bytes: u64, total_bytes: u64) -> Self {
        let percent = (bytes.min(total_bytes) * 100)
            .checked_div(total_bytes)
            .unwrap_or(0) as u32;
        ServerFrame::Progress {
            v: PROTOCOL_VERSION,
            action: "backup".to_string(),
            percent,
            bytes,
            total_bytes,
        }
    }

    pub fn backup_complete(path: impl Into<String>, sha256: impl Into<String>) -> Self {
        ServerFrame::Complete {
            v: PROTOCOL_VERSION,
            action: "backup".to_string(),
            path: path.into(),
            sha256: sha256.into(),
        }
    }

    pub fn to_json(&self) -> String {
        // The shape is a closed enum of plain fields, so serialisation cannot
        // fail; the fallback keeps this total on the control path.
        serde_json::to_string(self).unwrap_or_else(|_| {
            r#"{"v":1,"type":"error","seq":0,"code":6500,"message":"frame serialisation failed"}"#.to_string()
        })
    }
}

/// Parse and validate a client frame.
///
/// Malformed JSON, an unknown action, or a mismatched `v` all yield 6400
/// (§8.5). Out-of-range input payloads yield 6422.
pub fn parse_client_frame(text: &str) -> Result<ClientFrame, (u64, ControlError)> {
    // Recover the seq even from a frame we cannot fully parse, so the error
    // frame can still echo it.
    let seq = serde_json::from_str::<serde_json::Value>(text)
        .ok()
        .and_then(|v| v.get("seq").and_then(|s| s.as_u64()))
        .unwrap_or(0);

    let frame: ClientFrame =
        serde_json::from_str(text).map_err(|e| (seq, ControlError::BadFrame(e.to_string())))?;

    if frame.version() != PROTOCOL_VERSION {
        return Err((
            seq,
            ControlError::BadFrame(format!(
                "protocol version {} is not supported (this server speaks v{PROTOCOL_VERSION})",
                frame.version()
            )),
        ));
    }

    validate_input(&frame).map_err(|e| (seq, e))?;
    Ok(frame)
}

/// Range-check an input payload (§8.5, error 6422).
fn validate_input(frame: &ClientFrame) -> Result<(), ControlError> {
    let ClientFrame::Input(input) = frame else {
        return Ok(());
    };
    match input {
        InputFrame::Keyboard {
            event_type,
            code,
            value,
            ..
        } => {
            if event_type != "EV_KEY" {
                return Err(ControlError::BadFrame(format!(
                    "keyboard frames must carry type EV_KEY, got {event_type}"
                )));
            }
            // Linux keycodes are u16.
            if !(0..=0xFFFF).contains(code) {
                return Err(ControlError::InputRange(format!(
                    "keycode {code} is outside 0..65535"
                )));
            }
            // 0=release, 1=press, 2=repeat.
            if !(0..=2).contains(value) {
                return Err(ControlError::InputRange(format!(
                    "key value {value} is not 0 (release), 1 (press) or 2 (repeat)"
                )));
            }
            Ok(())
        }
        InputFrame::Tablet {
            event_type, x, y, ..
        } => {
            if event_type != "EV_ABS" {
                return Err(ControlError::BadFrame(format!(
                    "tablet frames must carry type EV_ABS, got {event_type}"
                )));
            }
            if !(0..=ABS_MAX).contains(x) || !(0..=ABS_MAX).contains(y) {
                return Err(ControlError::InputRange(format!(
                    "coordinate out of range: ({x},{y}) is outside 0..{ABS_MAX}"
                )));
            }
            Ok(())
        }
    }
}

/// Map a tablet coordinate onto the scanout (§8.5).
pub const fn map_abs(value: i64, extent: u32) -> u32 {
    ((value * (extent as i64 - 1)) / ABS_MAX) as u32
}

/// Serialise a client frame, for tools that generate protocol-v1 traffic.
pub fn to_json(frame: &ClientFrame) -> Option<String> {
    serde_json::to_string(frame).ok()
}
