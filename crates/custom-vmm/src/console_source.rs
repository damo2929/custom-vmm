//! A stand-in for the guest's scanout and audio capture.
//!
//! **This is not a guest.** No vCPU runs on this build, so virtio-gpu has no
//! framebuffer to hand over and virtio-snd captures nothing. Rather than
//! serve a black screen — which is indistinguishable from a broken encoder,
//! a broken decoder, or a broken window — this draws a pattern that makes
//! the whole path observable end to end:
//!
//! * a sweeping band, so a stalled stream is obvious at a glance;
//! * a crosshair at the pointer, so §8.5 tablet events can be seen arriving;
//! * a bar and blocks driven by the last keycode, likewise for the keyboard;
//! * a tone, so the audio path is carrying something real.
//!
//! The pointer and keyboard readouts are the point: they close the loop from
//! the client's window, out over the §8 control channel, into the VMM, and
//! back down the §7 media stream. When a guest exists, `DemoScanout` is
//! replaced by the virtio-gpu scanout and this file goes away.
//!
//! It is the `media-capture` thread's [`CaptureSource`] (§7.1), so it is
//! polled by that thread and by nothing else — never by a session.

use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::Arc;

use libvmm_core::VmmResult;
use libvmm_media::plane::CaptureSource;
use vmm_codec_sys::{PackedFormat, PackedFrame};

/// Absolute coordinates arrive on §8.5's 0..32767 grid.
const ABS_MAX: i64 = 32767;

/// What the control channel has most recently been told, shared with the
/// scanout so the picture can reflect it.
#[derive(Default)]
pub struct SharedInput {
    pub pointer_x: AtomicI64,
    pub pointer_y: AtomicI64,
    pub buttons: AtomicU64,
    pub last_key: AtomicI64,
    pub key_events: AtomicU64,
    pub pointer_events: AtomicU64,
}

impl SharedInput {
    pub fn on_key(&self, code: i64, value: i64) {
        // Record presses only; a release would blank the readout the instant
        // the user let go, which makes it useless for confirming a keypress.
        if value != 0 {
            self.last_key.store(code, Ordering::Relaxed);
        }
        self.key_events.fetch_add(1, Ordering::Relaxed);
    }

    pub fn on_pointer(&self, x: i64, y: i64, left: bool, right: bool, middle: bool) {
        self.pointer_x.store(x, Ordering::Relaxed);
        self.pointer_y.store(y, Ordering::Relaxed);
        let bits = u64::from(left) | u64::from(right) << 1 | u64::from(middle) << 2;
        self.buttons.store(bits, Ordering::Relaxed);
        self.pointer_events.fetch_add(1, Ordering::Relaxed);
    }
}

pub struct DemoScanout {
    width: u32,
    height: u32,
    input: Arc<SharedInput>,
    tick: u32,
    /// Phase of the tone, kept across calls so the waveform is continuous —
    /// restarting it each frame would produce an audible click at 30 Hz.
    phase: f32,
    samples_per_frame: usize,
}

impl DemoScanout {
    pub fn new(width: u32, height: u32, framerate: u32, input: Arc<SharedInput>) -> Self {
        DemoScanout {
            width,
            height,
            input,
            tick: 0,
            phase: 0.0,
            samples_per_frame: 48_000 / framerate.max(1) as usize,
        }
    }

    fn scale(v: i64, max: u32) -> usize {
        if max == 0 {
            return 0;
        }
        let clamped = v.clamp(0, ABS_MAX) as f64 / ABS_MAX as f64;
        ((clamped * (max as f64 - 1.0)).round() as usize).min(max as usize - 1)
    }
}

impl CaptureSource for DemoScanout {
    fn next_frame(&mut self) -> VmmResult<Option<PackedFrame>> {
        let (w, h) = (self.width as usize, self.height as usize);
        let stride = w * 4;
        let mut pixels = vec![0u8; stride * h];

        let tick = self.tick;
        self.tick = self.tick.wrapping_add(1);

        // Background: a vertical gradient with a band sweeping across it.
        // Any freeze in the pipeline shows up as a band that stops moving.
        let band = ((tick as usize * 9) % (w + 240)).saturating_sub(120);
        for y in 0..h {
            let base_g = (y * 160 / h) as u8;
            let row = y * stride;
            for x in 0..w {
                let near = x.abs_diff(band);
                let glow = if near < 120 {
                    (120 - near) as u8 * 2
                } else {
                    0
                };
                let p = row + x * 4;
                pixels[p] = 32u8.saturating_add(glow); // B
                pixels[p + 1] = base_g.saturating_add(glow / 3); // G
                pixels[p + 2] = 24u8.saturating_add(glow / 2); // R
                pixels[p + 3] = 0xff;
            }
        }

        // Keyboard readout: a bar whose length tracks the last keycode, with
        // the code itself in blocks beneath it. Both change the moment a key
        // is pressed in the client's window.
        let last_key = self.input.last_key.load(Ordering::Relaxed).clamp(0, 255);
        let bar = (last_key as usize * w / 256).min(w);
        for y in (h / 12)..(h / 12 + h / 40).min(h) {
            let row = y * stride;
            for x in 0..bar {
                let p = row + x * 4;
                pixels[p] = 0x40;
                pixels[p + 1] = 0xe0;
                pixels[p + 2] = 0xff;
            }
        }
        // Eight blocks, one per bit of the keycode: an exact, readable value.
        let block = (w / 64).max(4);
        for bit in 0..8usize {
            if last_key >> (7 - bit) & 1 == 0 {
                continue;
            }
            let x0 = w / 24 + bit * block * 2;
            for y in (h / 7)..(h / 7 + block) {
                let row = y * stride;
                for x in x0..(x0 + block).min(w) {
                    let p = row + x * 4;
                    pixels[p] = 0xff;
                    pixels[p + 1] = 0xff;
                    pixels[p + 2] = 0xff;
                }
            }
        }

        // Pointer: a crosshair at the reported position, coloured by which
        // buttons are held.
        let px = Self::scale(self.input.pointer_x.load(Ordering::Relaxed), self.width);
        let py = Self::scale(self.input.pointer_y.load(Ordering::Relaxed), self.height);
        let buttons = self.input.buttons.load(Ordering::Relaxed);
        let (cb, cg, cr) = match buttons {
            0 => (0x30, 0xff, 0x30),               // idle: green
            b if b & 1 != 0 => (0x30, 0x30, 0xff), // left: red
            b if b & 2 != 0 => (0xff, 0x30, 0x30), // right: blue
            _ => (0x30, 0xff, 0xff),               // middle: yellow
        };
        let arm = (self.width as usize / 40).max(8);
        for x in px.saturating_sub(arm)..(px + arm).min(w) {
            let p = py * stride + x * 4;
            pixels[p] = cb;
            pixels[p + 1] = cg;
            pixels[p + 2] = cr;
        }
        for y in py.saturating_sub(arm)..(py + arm).min(h) {
            let p = y * stride + px * 4;
            pixels[p] = cb;
            pixels[p + 1] = cg;
            pixels[p + 2] = cr;
        }

        Ok(Some(PackedFrame {
            width: self.width,
            height: self.height,
            stride,
            pixels,
            format: PackedFormat::Bgra,
        }))
    }

    fn next_audio(&mut self) -> VmmResult<Vec<i16>> {
        // A quiet 440 Hz tone. Quiet because it plays for as long as the
        // console is open, and the point is only to prove the audio path
        // carries something a decoder can reconstruct.
        let mut pcm = Vec::with_capacity(self.samples_per_frame * 2);
        let step = std::f32::consts::TAU * 440.0 / 48_000.0;
        for _ in 0..self.samples_per_frame {
            let s = (self.phase.sin() * 2_000.0) as i16;
            self.phase = (self.phase + step) % std::f32::consts::TAU;
            pcm.push(s);
            pcm.push(s);
        }
        Ok(pcm)
    }
}
