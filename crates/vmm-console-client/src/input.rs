//! Local input capture — the source of the §8.5 input frames.
//!
//! There is no window to capture from (see the rendering note in
//! `rtsp_client`), so keystrokes are read from the controlling terminal in
//! raw mode and translated to Linux keycodes, which is exactly what
//! `{"device":"keyboard","type":"EV_KEY","code":...}` carries.

use libvmm_control::proto::{ClientFrame, InputFrame, PROTOCOL_VERSION};
use std::io::Read;
use std::os::unix::io::AsRawFd;

/// Linux keycodes (`linux/input-event-codes.h`) for the keys a terminal can
/// unambiguously report.
pub mod keycode {
    pub const ESC: i64 = 1;
    pub const ENTER: i64 = 28;
    pub const BACKSPACE: i64 = 14;
    pub const TAB: i64 = 15;
    pub const SPACE: i64 = 57;
    pub const LEFTSHIFT: i64 = 42;
    pub const UP: i64 = 103;
    pub const LEFT: i64 = 105;
    pub const RIGHT: i64 = 106;
    pub const DOWN: i64 = 108;
    pub const DELETE: i64 = 111;
    pub const HOME: i64 = 102;
    pub const END: i64 = 107;
    pub const PAGEUP: i64 = 104;
    pub const PAGEDOWN: i64 = 109;
}

/// Press and release values (`EV_KEY`).
pub const RELEASE: i64 = 0;
pub const PRESS: i64 = 1;
pub const REPEAT: i64 = 2;

/// Map an ASCII character to its Linux keycode, and whether Shift is needed.
pub fn keycode_for(c: char) -> Option<(i64, bool)> {
    // Row order matters: these are scancode positions, not alphabetical.
    const ROW_NUMBERS: &str = "1234567890";
    const ROW_Q: &str = "qwertyuiop";
    const ROW_A: &str = "asdfghjkl";
    const ROW_Z: &str = "zxcvbnm";

    let lower = c.to_ascii_lowercase();
    let shifted = c.is_ascii_uppercase();

    if let Some(i) = ROW_NUMBERS.find(lower) {
        return Some((2 + i as i64, shifted));
    }
    if let Some(i) = ROW_Q.find(lower) {
        return Some((16 + i as i64, shifted));
    }
    if let Some(i) = ROW_A.find(lower) {
        return Some((30 + i as i64, shifted));
    }
    if let Some(i) = ROW_Z.find(lower) {
        return Some((44 + i as i64, shifted));
    }

    Some(match c {
        ' ' => (keycode::SPACE, false),
        '\r' | '\n' => (keycode::ENTER, false),
        '\t' => (keycode::TAB, false),
        '\x7f' | '\x08' => (keycode::BACKSPACE, false),
        '\x1b' => (keycode::ESC, false),
        '-' => (12, false),
        '_' => (12, true),
        '=' => (13, false),
        '+' => (13, true),
        '[' => (26, false),
        '{' => (26, true),
        ']' => (27, false),
        '}' => (27, true),
        ';' => (39, false),
        ':' => (39, true),
        '\'' => (40, false),
        '"' => (40, true),
        '`' => (41, false),
        '~' => (41, true),
        '\\' => (43, false),
        '|' => (43, true),
        ',' => (51, false),
        '<' => (51, true),
        '.' => (52, false),
        '>' => (52, true),
        '/' => (53, false),
        '?' => (53, true),
        '!' => (2, true),
        '@' => (3, true),
        '#' => (4, true),
        '$' => (5, true),
        '%' => (6, true),
        '^' => (7, true),
        '&' => (8, true),
        '*' => (9, true),
        '(' => (10, true),
        ')' => (11, true),
        _ => return None,
    })
}

/// Expand a character into the press/release frames a guest expects,
/// including the Shift press and release where the character needs one.
pub fn frames_for(c: char) -> Vec<(i64, i64)> {
    let Some((code, shifted)) = keycode_for(c) else {
        return Vec::new();
    };
    let mut events = Vec::with_capacity(4);
    if shifted {
        events.push((keycode::LEFTSHIFT, PRESS));
    }
    events.push((code, PRESS));
    events.push((code, RELEASE));
    if shifted {
        events.push((keycode::LEFTSHIFT, RELEASE));
    }
    events
}

/// Build a protocol-v1 keyboard frame.
pub fn key_frame(seq: u64, code: i64, value: i64) -> ClientFrame {
    ClientFrame::Input(InputFrame::Keyboard {
        v: PROTOCOL_VERSION,
        seq,
        event_type: "EV_KEY".to_string(),
        code,
        value,
    })
}

/// Build a protocol-v1 tablet frame. `x`/`y` are already in 0..32767.
pub fn tablet_frame(
    seq: u64,
    x: i64,
    y: i64,
    buttons: libvmm_control::proto::Buttons,
) -> ClientFrame {
    ClientFrame::Input(InputFrame::Tablet {
        v: PROTOCOL_VERSION,
        seq,
        event_type: "EV_ABS".to_string(),
        x,
        y,
        buttons,
    })
}

/// Puts the terminal in raw mode and restores it on drop, so an interrupted
/// session never leaves the user's shell unusable.
pub struct RawTerminal {
    fd: i32,
    original: libc::termios,
    restored: bool,
}

impl RawTerminal {
    pub fn acquire() -> Option<Self> {
        let stdin = std::io::stdin();
        let fd = stdin.as_raw_fd();
        // SAFETY: isatty on a valid fd.
        if unsafe { libc::isatty(fd) } != 1 {
            return None;
        }

        // SAFETY: tcgetattr fills a caller-owned termios.
        let mut original: libc::termios = unsafe { std::mem::zeroed() };
        if unsafe { libc::tcgetattr(fd, &mut original) } != 0 {
            return None;
        }

        let mut raw = original;
        // SAFETY: cfmakeraw mutates the caller-owned struct only.
        unsafe { libc::cfmakeraw(&mut raw) };
        // Return from read() every 100ms so the session stays responsive to
        // server frames even while nobody is typing.
        raw.c_cc[libc::VMIN] = 0;
        raw.c_cc[libc::VTIME] = 1;
        // SAFETY: applying a well-formed termios to the terminal.
        if unsafe { libc::tcsetattr(fd, libc::TCSANOW, &raw) } != 0 {
            return None;
        }

        Some(RawTerminal {
            fd,
            original,
            restored: false,
        })
    }

    /// Read whatever the user typed since the last call.
    pub fn read_available(&self) -> Vec<u8> {
        let mut buf = [0u8; 256];
        match std::io::stdin().read(&mut buf) {
            Ok(n) if n > 0 => buf[..n].to_vec(),
            _ => Vec::new(),
        }
    }

    pub fn restore(&mut self) {
        if !self.restored {
            // SAFETY: restoring the termios captured in `acquire`.
            unsafe { libc::tcsetattr(self.fd, libc::TCSANOW, &self.original) };
            self.restored = true;
        }
    }
}

impl Drop for RawTerminal {
    fn drop(&mut self) {
        self.restore();
    }
}

/// Decode a terminal byte stream into key events, translating the ANSI arrow
/// and navigation sequences into their Linux keycodes.
pub fn decode_terminal_input(bytes: &[u8]) -> Vec<(i64, i64)> {
    let mut events = Vec::new();
    let mut i = 0;
    while i < bytes.len() {
        // CSI sequences: ESC [ ...
        if bytes[i] == 0x1B && i + 2 < bytes.len() && bytes[i + 1] == b'[' {
            let (code, consumed) = match bytes[i + 2] {
                b'A' => (Some(keycode::UP), 3),
                b'B' => (Some(keycode::DOWN), 3),
                b'C' => (Some(keycode::RIGHT), 3),
                b'D' => (Some(keycode::LEFT), 3),
                b'H' => (Some(keycode::HOME), 3),
                b'F' => (Some(keycode::END), 3),
                b'3' if i + 3 < bytes.len() && bytes[i + 3] == b'~' => (Some(keycode::DELETE), 4),
                b'5' if i + 3 < bytes.len() && bytes[i + 3] == b'~' => (Some(keycode::PAGEUP), 4),
                b'6' if i + 3 < bytes.len() && bytes[i + 3] == b'~' => (Some(keycode::PAGEDOWN), 4),
                _ => (None, 3),
            };
            if let Some(code) = code {
                events.push((code, PRESS));
                events.push((code, RELEASE));
            }
            i += consumed;
            continue;
        }
        events.extend(frames_for(bytes[i] as char));
        i += 1;
    }
    events
}
