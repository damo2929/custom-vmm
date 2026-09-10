//! Client-side video decode and framebuffer rendering.
//!
//! [`FileSink`](crate::rtsp_client::FileSink) writes the elementary streams
//! straight to disk, which is right for capture but leaves the caller with a
//! file rather than a picture. This sink decodes instead: each reassembled
//! frame goes through libavcodec — H.264, VP9 or AV1, whichever the session
//! negotiated — and is converted to BGRA for the caller's surface.
//!
//! The decoder is created lazily on the first access unit, because the
//! stream's geometry is only known once a frame has come back — the SDP
//! advertises a framesize, but the encoder is the authority.

use libvmm_core::{MediaError, VmmError, VmmResult};
use libvmm_media::depacketize::{AudioPacket, CodedUnit};
use std::path::{Path, PathBuf};
use vmm_codec_sys::{PackedFormat, PackedFrame, Scaler, VideoCodec, VideoDecoder};

fn decode_error(detail: String) -> VmmError {
    MediaError::Encode {
        stream: "video",
        detail,
    }
    .into()
}

/// What to do with each decoded frame.
pub trait FrameHandler {
    /// One decoded frame, BGRA, tightly packed.
    fn on_frame(&mut self, frame: &PackedFrame) -> VmmResult<()>;
}

/// Keeps only the most recent frame, for a caller that wants a snapshot.
#[derive(Default)]
pub struct LatestFrame {
    pub frame: Option<PackedFrame>,
    pub count: u64,
}

impl FrameHandler for LatestFrame {
    fn on_frame(&mut self, frame: &PackedFrame) -> VmmResult<()> {
        self.count += 1;
        self.frame = Some(frame.clone());
        Ok(())
    }
}

impl LatestFrame {
    /// Write the held frame as a binary PPM, which every image viewer reads
    /// and which needs no encoder to produce.
    pub fn write_ppm(&self, path: &Path) -> VmmResult<()> {
        let frame = self
            .frame
            .as_ref()
            .ok_or_else(|| decode_error("no frame has been decoded yet".to_string()))?;

        let mut out = Vec::with_capacity(frame.pixels.len() / 4 * 3 + 32);
        out.extend_from_slice(format!("P6\n{} {}\n255\n", frame.width, frame.height).as_bytes());
        for row in 0..frame.height as usize {
            let start = row * frame.stride;
            for col in 0..frame.width as usize {
                let at = start + col * 4;
                // BGRA in memory; PPM wants R, G, B.
                out.push(frame.pixels[at + 2]);
                out.push(frame.pixels[at + 1]);
                out.push(frame.pixels[at]);
            }
        }
        std::fs::write(path, out)
            .map_err(|e| decode_error(format!("writing {}: {e}", path.display())))
    }
}

/// Writes every decoded frame to a raw BGRA stream, for piping to a player.
pub struct RawBgraWriter {
    file: std::fs::File,
    pub path: PathBuf,
    pub frames: u64,
}

impl RawBgraWriter {
    pub fn create(path: &Path) -> VmmResult<Self> {
        let file = std::fs::File::create(path)
            .map_err(|e| decode_error(format!("creating {}: {e}", path.display())))?;
        Ok(RawBgraWriter {
            file,
            path: path.to_path_buf(),
            frames: 0,
        })
    }
}

impl FrameHandler for RawBgraWriter {
    fn on_frame(&mut self, frame: &PackedFrame) -> VmmResult<()> {
        use std::io::Write;
        // Write row by row so a padded stride does not leak into the file.
        for row in 0..frame.height as usize {
            let start = row * frame.stride;
            let end = start + frame.width as usize * 4;
            self.file
                .write_all(&frame.pixels[start..end])
                .map_err(|e| decode_error(format!("writing {}: {e}", self.path.display())))?;
        }
        self.frames += 1;
        Ok(())
    }
}

/// A [`MediaSink`](crate::rtsp_client::MediaSink) that decodes video.
pub struct DecodingSink<H: FrameHandler> {
    decoder: VideoDecoder,
    /// Built on the first frame, once the geometry is known.
    scaler: Option<Scaler>,
    surface: Option<PackedFrame>,
    handler: H,
    /// Audio is passed through, since the client has nothing to play it on.
    audio: Option<std::fs::File>,

    pub video_units: u64,
    pub frames_decoded: u64,
    pub audio_packets: u64,
    pub audio_bytes: u64,
    first_keyframe_seen: bool,
    /// The last decode error, kept rather than raised. A client must not
    /// abandon a session over one corrupt frame.
    pub last_error: Option<String>,
}

impl<H: FrameHandler> DecodingSink<H> {
    /// Open a sink decoding `codec`, which the session negotiated.
    pub fn new(codec: VideoCodec, handler: H, audio_path: Option<&Path>) -> VmmResult<Self> {
        let decoder = VideoDecoder::open(codec).map_err(|e| MediaError::EncoderInit {
            accelerator: "software",
            detail: format!("opening the {} decoder: {e}", codec.as_str()),
        })?;

        let audio = audio_path
            .map(std::fs::File::create)
            .transpose()
            .map_err(|e| decode_error(format!("creating the audio output: {e}")))?;

        Ok(DecodingSink {
            decoder,
            scaler: None,
            surface: None,
            handler,
            audio,
            video_units: 0,
            frames_decoded: 0,
            audio_packets: 0,
            audio_bytes: 0,
            first_keyframe_seen: false,
            last_error: None,
        })
    }

    pub fn handler(&self) -> &H {
        &self.handler
    }

    /// The handler, mutably — the console needs this to drain the window's
    /// input between pump slices.
    pub fn handler_mut(&mut self) -> &mut H {
        &mut self.handler
    }

    /// Access units libavcodec could not parse — normal at a mid-GOP join.
    pub fn undecodable_units(&self) -> u64 {
        self.decoder.undecodable_units()
    }

    pub fn geometry(&self) -> Option<(u32, u32)> {
        self.decoder.geometry()
    }

    /// The codec this sink decodes.
    pub fn codec(&self) -> VideoCodec {
        self.decoder.codec()
    }

    /// Decode one access unit and hand every frame to the handler.
    fn decode_unit(&mut self, data: &[u8]) -> VmmResult<()> {
        let frames = self
            .decoder
            .decode(data)
            .map_err(|e| decode_error(e.to_string()))?;

        for frame in frames {
            // Build the scaler and surface once the real geometry is known.
            if self.scaler.is_none() {
                self.scaler = Some(
                    Scaler::from_i420(frame.width, frame.height, PackedFormat::Bgra)
                        .map_err(|e| decode_error(e.to_string()))?,
                );
                self.surface = Some(
                    PackedFrame::packed(
                        frame.width,
                        frame.height,
                        vec![0u8; (frame.width * frame.height * 4) as usize],
                        PackedFormat::Bgra,
                    )
                    .map_err(|e| decode_error(e.to_string()))?,
                );
            }

            let (Some(scaler), Some(surface)) = (self.scaler.as_mut(), self.surface.as_mut())
            else {
                return Err(decode_error(
                    "the scaler was not built for this frame".to_string(),
                ));
            };

            if surface.width != frame.width || surface.height != frame.height {
                return Err(decode_error(format!(
                    "the stream changed geometry mid-session, {}x{} to {}x{}",
                    surface.width, surface.height, frame.width, frame.height
                )));
            }

            scaler
                .convert_from_i420(&frame, surface)
                .map_err(|e| decode_error(e.to_string()))?;
            self.frames_decoded += 1;
            self.handler.on_frame(surface)?;
        }
        Ok(())
    }
}

impl<H: FrameHandler> crate::rtsp_client::MediaSink for DecodingSink<H> {
    fn on_video(&mut self, unit: &CodedUnit) -> std::io::Result<()> {
        // Decoding before the first keyframe wastes work and fills the log
        // with "non-existing PPS" for every slice of the partial GOP.
        if !self.first_keyframe_seen {
            if !unit.keyframe {
                return Ok(());
            }
            self.first_keyframe_seen = true;
        }

        self.video_units += 1;
        if let Err(e) = self.decode_unit(&unit.data) {
            // One bad access unit must not end the session: packet loss is
            // ordinary, and the next keyframe recovers.
            log::debug!("decode of access unit {} failed: {e}", self.video_units);
            self.last_error = Some(e.to_string());
        }
        Ok(())
    }

    fn on_audio(&mut self, packet: &AudioPacket) -> std::io::Result<()> {
        use std::io::Write;
        self.audio_packets += 1;
        self.audio_bytes += packet.data.len() as u64;
        match &mut self.audio {
            // Length-prefixed, so packet boundaries survive.
            Some(f) => {
                f.write_all(&(packet.data.len() as u32).to_be_bytes())?;
                f.write_all(&packet.data)
            }
            None => Ok(()),
        }
    }
}

/// Send every decoded frame to more than one place.
///
/// The window is the console's reason to exist, so it is never an
/// alternative to writing a file — asking for a recording should not cost
/// you the picture. The display is generic so this module does not depend on
/// the Wayland one; `main` plugs the window in.
pub struct Fanout<D: FrameHandler> {
    pub display: Option<D>,
    pub raw: Option<RawBgraWriter>,
    pub latest: Option<LatestFrame>,
}

impl<D: FrameHandler> Fanout<D> {
    pub fn new(display: Option<D>, raw: Option<RawBgraWriter>, keep_latest: bool) -> Self {
        Fanout {
            display,
            raw,
            latest: keep_latest.then(LatestFrame::default),
        }
    }
}

impl<D: FrameHandler> FrameHandler for Fanout<D> {
    fn on_frame(&mut self, frame: &PackedFrame) -> VmmResult<()> {
        // Paint before writing: a disk write must not sit between the
        // decoder and the screen on an interactive console.
        if let Some(display) = &mut self.display {
            display.on_frame(frame)?;
        }
        if let Some(raw) = &mut self.raw {
            raw.on_frame(frame)?;
        }
        if let Some(latest) = &mut self.latest {
            latest.on_frame(frame)?;
        }
        Ok(())
    }
}
