//! The §7.1 capture/encode pipeline, end to end.
//!
//! ```text
//! virtio-gpu scanout (BGRA) --libswscale--> I420 --H.264--> Annex-B --RFC 6184--> RTP
//! virtio-snd PCM (S16LE)    ------------------------Vorbis--> packets --RFC 5215--> RTP
//! ```
//!
//! [`VideoPipeline`] and [`AudioPipeline`] own one encoder and one
//! packetiser each, and are driven a frame or a PCM block at a time by the
//! capture threads. Everything they need is allocated once at open, so the
//! per-frame path only touches buffers that already exist (§0.1).
//!
//! Both report the backend they ended up on, because §7.1's accelerator
//! setting is a request rather than a guarantee: a host with no H.264
//! encode entrypoint falls back to software, and the operator needs to see
//! that in the boot log rather than infer it from a frame rate.

use crate::encoder::{AudioEncoderParams, VideoEncoderParams};
use crate::packetize::{
    av1 as av1_rtp, h264 as h264_rtp, opus as opus_rtp, vorbis as vorbis_rtp, vp9 as vp9_rtp,
    Packet,
};
use libvmm_config::HardwareAccelerator;
use libvmm_core::{MediaError, VmmError, VmmResult};
use std::path::Path;
use std::time::{Duration, Instant};
use vmm_codec_sys::{
    Accelerator, AudioCodec, Backend, CodecError, EncoderConfig, OpusEncoder, PackedFormat,
    PackedFrame, Scaler, VideoCodec, VideoEncoder, VorbisEncoder, VorbisHeaders, Yuv420Frame,
};

fn encode_error(stream: &'static str, e: CodecError) -> VmmError {
    MediaError::Encode {
        stream,
        detail: e.to_string(),
    }
    .into()
}

/// Map §11's `hardware_accelerator` onto the codec crate's own enum.
fn accelerator(a: HardwareAccelerator) -> Accelerator {
    match a {
        HardwareAccelerator::Vaapi => Accelerator::Vaapi,
        HardwareAccelerator::Nvenc => Accelerator::Nvenc,
    }
}

/// §7.1: a keyframe at least every two seconds.
///
/// Both encoders are configured with a GOP of `framerate * 2` frames, which
/// satisfies this only while the guest renders at the configured rate. It
/// often will not: a desktop sitting idle produces a handful of frames a
/// second, and on a 30 fps machine a 60-frame GOP would then stretch to ten
/// or twenty seconds between keyframes. A client joining in that gap sees
/// nothing until the next one.
///
/// So the interval is held on the clock as well as the frame counter: if two
/// seconds have passed since the last keyframe, the next frame is coded as
/// an IDR whatever the GOP says.
pub const MAX_KEYFRAME_INTERVAL: Duration = Duration::from_secs(2);

/// Tracks the encoder's output against the §7.1 ceiling.
///
/// Checking one frame in isolation is the wrong test, and getting it wrong
/// is easy: extrapolating a single frame to a whole second flags every IDR,
/// because a keyframe is legitimately several times the size of the inter
/// frames around it. That size difference is exactly what the HRD buffer
/// exists to absorb.
///
/// What §7.1 actually constrains is the rate over the VBV window: the bits
/// emitted in any span the buffer covers must fit in the buffer. Since the
/// buffer is sized to the ceiling, the budget for that window *is*
/// `vbv_buffer_bits`, which makes the test a plain sliding sum.
struct VbvMonitor {
    /// Bits emitted per frame over the window, oldest first.
    window: std::collections::VecDeque<u64>,
    /// Frames the VBV buffer spans.
    capacity: usize,
    /// Bits the window may hold, which is the buffer size.
    budget: u64,
    bits: u64,
}

impl VbvMonitor {
    fn new(params: &VideoEncoderParams) -> Self {
        let ceiling_bits_per_second = u64::from(params.max_kbps) * 1000;
        // How many frames the buffer covers, at least one.
        let capacity = if ceiling_bits_per_second == 0 {
            1
        } else {
            let seconds = f64::from(params.vbv_buffer_bits) / ceiling_bits_per_second as f64;
            ((seconds * f64::from(params.framerate)).ceil() as usize).max(1)
        };
        VbvMonitor {
            window: std::collections::VecDeque::with_capacity(capacity),
            capacity,
            budget: u64::from(params.vbv_buffer_bits),
            bits: 0,
        }
    }

    /// Record a frame, returning the window rate in kbps when it breaches.
    fn record(&mut self, frame_bytes: usize, framerate: u32) -> Option<u32> {
        let bits = (frame_bytes as u64).saturating_mul(8);
        self.window.push_back(bits);
        self.bits = self.bits.saturating_add(bits);
        while self.window.len() > self.capacity {
            if let Some(old) = self.window.pop_front() {
                self.bits = self.bits.saturating_sub(old);
            }
        }

        if self.bits <= self.budget {
            return None;
        }

        // Report the breach as a rate, which is what an operator reads.
        let seconds = self.window.len() as f64 / f64::from(framerate.max(1));
        let kbps = if seconds > 0.0 {
            (self.bits as f64 / seconds / 1000.0) as u32
        } else {
            u32::MAX
        };
        Some(kbps)
    }
}

/// Capture, encode and packetise the video stream.
/// The RTP packetiser for the negotiated codec.
///
/// Each codec has its own payload format and they share nothing: H.264 has
/// STAP-A and FU-A over NAL units (RFC 6184), VP9 a flag-and-picture-ID
/// descriptor, AV1 a one-byte aggregation header over OBUs. Dispatching here
/// rather than behind a trait keeps each packetiser's own signature — H.264
/// needs no keyframe flag because it is visible in the NAL types, while VP9
/// and AV1 both do.
enum VideoPacketizer {
    H264(h264_rtp::Packetizer),
    Vp9(vp9_rtp::Packetizer),
    Av1(av1_rtp::Packetizer),
}

impl VideoPacketizer {
    fn new(codec: VideoCodec, ssrc: u32) -> Self {
        match codec {
            VideoCodec::H264 => VideoPacketizer::H264(h264_rtp::Packetizer::new(ssrc)),
            VideoCodec::Vp9 => VideoPacketizer::Vp9(vp9_rtp::Packetizer::new(ssrc)),
            VideoCodec::Av1 => VideoPacketizer::Av1(av1_rtp::Packetizer::new(ssrc)),
        }
    }

    fn timestamp(&self, pts: i64, framerate: u32) -> u32 {
        // All three use the 90 kHz video clock, so the conversion is the
        // same; each packetiser owns its own copy so neither depends on the
        // others existing.
        match self {
            VideoPacketizer::H264(_) => h264_rtp::Packetizer::timestamp(pts, framerate),
            VideoPacketizer::Vp9(_) => vp9_rtp::Packetizer::timestamp(pts, framerate),
            VideoPacketizer::Av1(_) => av1_rtp::Packetizer::timestamp(pts, framerate),
        }
    }

    fn packetize(
        &mut self,
        frame: &[u8],
        timestamp: u32,
        keyframe: bool,
    ) -> VmmResult<Vec<Packet>> {
        match self {
            VideoPacketizer::H264(p) => p.packetize(frame, timestamp),
            VideoPacketizer::Vp9(p) => p.packetize(frame, timestamp, keyframe),
            VideoPacketizer::Av1(p) => p.packetize(frame, timestamp, keyframe),
        }
    }
}

/// What one encoded scanout produced.
///
/// `keyframe` describes the coded frame the packets carry, not any single
/// packet: a keyframe is fragmented across many RTP packets and a session
/// may only begin at the first of them.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VideoOutput {
    pub packets: Vec<Packet>,
    pub keyframe: bool,
}

pub struct VideoPipeline {
    scaler: Scaler,
    encoder: VideoEncoder,
    packetizer: VideoPacketizer,
    /// Reused every frame so the datapath does not allocate (§0.1).
    staging: Yuv420Frame,
    params: VideoEncoderParams,
    vbv: VbvMonitor,
    /// When the last keyframe was emitted, for [`MAX_KEYFRAME_INTERVAL`].
    /// `None` until the first frame, which is always an IDR anyway.
    last_keyframe: Option<Instant>,
    /// Frames submitted since the last keyframe, which tracks the encoder's
    /// own GOP counter one for one.
    frames_since_keyframe: u32,
    /// Keyframes the clock forced that the GOP would not have produced.
    forced_keyframes: u64,
    frames_in: u64,
    frames_out: u64,
    bytes_out: u64,
    /// Windows in which output breached the §7.1 ceiling.
    ceiling_breaches: u64,
}

impl std::fmt::Debug for VideoPipeline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VideoPipeline")
            .field("geometry", &(self.params.width, self.params.height))
            .field("backend", &self.encoder.backend().as_str())
            .field("frames_out", &self.frames_out)
            .finish()
    }
}

impl std::fmt::Debug for AudioPipeline {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AudioPipeline")
            .field("sample_rate", &self.params.sample_rate)
            .field("channels", &self.params.channels)
            .field("codec", &self.params.codec.as_str())
            .finish()
    }
}

impl VideoPipeline {
    /// Open the pipeline. `render_node` overrides the DRM node the VA-API
    /// probe uses; `None` tries the usual ones.
    pub fn open(
        params: VideoEncoderParams,
        ssrc: u32,
        render_node: Option<&Path>,
    ) -> VmmResult<Self> {
        let config = EncoderConfig {
            codec: params.codec,
            width: params.width,
            height: params.height,
            framerate: params.framerate,
            target_kbps: params.target_kbps,
            max_kbps: params.max_kbps,
            vbv_buffer_bits: params.vbv_buffer_bits,
            gop_length: params.gop_length,
            accelerator: accelerator(params.accelerator),
        };

        let encoder = VideoEncoder::open(config, render_node).map_err(|e| {
            // An encoder that cannot open at all is an init failure, which
            // §7.1 codes as 5001 against the accelerator that was asked for.
            MediaError::EncoderInit {
                accelerator: crate::encoder::accelerator_name(params.accelerator),
                detail: e.to_string(),
            }
        })?;

        if let Some(reason) = encoder.fallback_reason() {
            log::warn!(
                "{} encode fell back to {}: {reason}",
                params.codec.as_str(),
                encoder.acceleration()
            );
        } else {
            log::info!(
                "{} encode on {}",
                params.codec.as_str(),
                encoder.acceleration()
            );
        }

        // The virtio-gpu scanout of §7.1 is B8G8R8A8_UNORM.
        let scaler = Scaler::to_i420(params.width, params.height, PackedFormat::Bgra)
            .map_err(|e| encode_error("video", e))?;
        let staging = Yuv420Frame::black(params.width, params.height)
            .map_err(|e| encode_error("video", e))?;

        Ok(VideoPipeline {
            scaler,
            encoder,
            packetizer: VideoPacketizer::new(params.codec, ssrc),
            staging,
            vbv: VbvMonitor::new(&params),
            last_keyframe: None,
            frames_since_keyframe: 0,
            forced_keyframes: 0,
            params,
            frames_in: 0,
            frames_out: 0,
            bytes_out: 0,
            ceiling_breaches: 0,
        })
    }

    /// Which backend the encoder settled on.
    pub fn backend(&self) -> Backend {
        self.encoder.backend()
    }

    /// The codec this pipeline encodes.
    pub fn codec(&self) -> VideoCodec {
        self.encoder.codec()
    }

    /// A description of the encoder, for the boot log and `devices` output.
    pub fn acceleration(&self) -> &str {
        self.encoder.acceleration()
    }

    /// Why a hardware request fell back to software, if it did.
    pub fn fallback_reason(&self) -> Option<&str> {
        self.encoder.fallback_reason()
    }

    pub fn params(&self) -> &VideoEncoderParams {
        &self.params
    }

    /// `(frames in, frames out, bytes out, VBV ceiling breaches)`.
    pub fn stats(&self) -> (u64, u64, u64, u64) {
        (
            self.frames_in,
            self.frames_out,
            self.bytes_out,
            self.ceiling_breaches,
        )
    }

    /// Keyframes the two-second clock forced because the GOP had not yet
    /// come round. A steady count means the guest is rendering more slowly
    /// than `framerate_cap`, which is normal for an idle desktop.
    pub fn forced_keyframes(&self) -> u64 {
        self.forced_keyframes
    }

    /// Push one captured scanout. Returns the RTP packets it produced.
    ///
    /// An empty result is normal, not an error: a hardware encoder has a
    /// frame of pipeline delay, so the first push produces nothing and each
    /// later one returns the previous frame. The software path returns every
    /// frame immediately. [`drain`](VideoPipeline::drain) collects whatever
    /// is still held at the end of a session.
    pub fn push_scanout(&mut self, scanout: &PackedFrame, pts: i64) -> VmmResult<Vec<Packet>> {
        self.push_scanout_at(scanout, pts, Instant::now())
    }

    /// [`push_scanout`](VideoPipeline::push_scanout) with the clock supplied,
    /// so the keyframe interval can be tested without sleeping.
    pub fn push_scanout_at(
        &mut self,
        scanout: &PackedFrame,
        pts: i64,
        now: Instant,
    ) -> VmmResult<Vec<Packet>> {
        Ok(self.encode_scanout(scanout, pts, now)?.packets)
    }

    /// Push one captured scanout, reporting whether what came out is a
    /// keyframe.
    ///
    /// The flag is not cosmetic. §7 fans one encoded stream out to `0..n`
    /// sessions, and a session that joins mid-GOP must discard packets
    /// until a keyframe arrives: handing its decoder inter frames that
    /// reference pictures it never received produces either garbage or
    /// silence, depending on how forgiving the decoder is. Only the encoder
    /// knows which frames are safe to start on, so only it can say.
    pub fn encode_scanout(
        &mut self,
        scanout: &PackedFrame,
        pts: i64,
        now: Instant,
    ) -> VmmResult<VideoOutput> {
        if scanout.width != self.params.width || scanout.height != self.params.height {
            return Err(MediaError::Capture {
                detail: format!(
                    "scanout is {}x{}, the encoder was opened for {}x{}",
                    scanout.width, scanout.height, self.params.width, self.params.height
                ),
            }
            .into());
        }

        self.scaler
            .convert_to_i420(scanout, &mut self.staging)
            .map_err(|e| encode_error("video", e))?;
        self.staging.pts = pts;
        self.frames_in += 1;

        // Force an IDR when the clock says one is overdue *and* the GOP is
        // not about to deliver one anyway. Without the second condition a
        // guest rendering at exactly the configured rate would have every
        // GOP boundary forced a frame early, which costs a redundant IDR
        // every two seconds for no benefit. The first frame needs no
        // forcing: the encoder opens with a keyframe.
        let overdue = self
            .last_keyframe
            .is_some_and(|last| now.duration_since(last) >= MAX_KEYFRAME_INTERVAL);
        let gop_will_deliver = self.frames_since_keyframe + 1 >= self.params.gop_length;
        let force = overdue && !gop_will_deliver;

        self.frames_since_keyframe = self.frames_since_keyframe.saturating_add(1);

        let encoded = if force {
            self.forced_keyframes += 1;
            self.encoder.encode_keyframe(&self.staging)
        } else {
            self.encoder.encode(&self.staging)
        }
        .map_err(|e| encode_error("video", e))?;

        match encoded {
            Some(frame) => {
                let keyframe = frame.keyframe;
                if keyframe {
                    self.last_keyframe = Some(now);
                    self.frames_since_keyframe = 0;
                }
                Ok(VideoOutput {
                    packets: self.emit(frame)?,
                    keyframe,
                })
            }
            // Nothing came out, so nothing can be started on: a hardware
            // encoder holding its first frame is not a keyframe boundary.
            None => Ok(VideoOutput {
                packets: Vec::new(),
                keyframe: false,
            }),
        }
    }

    /// Drain whatever the encoder is still holding, at end of session.
    pub fn drain(&mut self) -> VmmResult<Vec<Packet>> {
        let frames = self.encoder.drain().map_err(|e| encode_error("video", e))?;
        let mut packets = Vec::new();
        for frame in frames {
            packets.extend(self.emit(frame)?);
        }
        Ok(packets)
    }

    fn emit(&mut self, frame: vmm_codec_sys::EncodedFrame) -> VmmResult<Vec<Packet>> {
        if let Some(kbps) = self.vbv.record(frame.data.len(), self.params.framerate) {
            // The encoder's own VBV should prevent this. Count it rather
            // than dropping the frame: discarding one would corrupt the GOP,
            // and a rising count is the signal that the encoder is not
            // honouring the ceiling it was programmed with.
            self.ceiling_breaches += 1;
            log::warn!(
                "H.264 output reached {kbps} kbps over the VBV window, above the \
                 {} kbps ceiling (§7.1)",
                self.params.max_kbps
            );
        }

        self.frames_out += 1;
        self.bytes_out += frame.data.len() as u64;

        let timestamp = self.packetizer.timestamp(frame.pts, self.params.framerate);
        self.packetizer
            .packetize(&frame.data, timestamp, frame.keyframe)
    }
}

/// The encoder behind an [`AudioPipeline`], one variant per codec.
enum AudioBackend {
    /// RFC 7587: the payload is the Opus packet, with no descriptor and no
    /// configuration to deliver.
    Opus {
        encoder: Box<OpusEncoder>,
        packetizer: opus_rtp::Packetizer,
    },
    /// RFC 5215: a payload header per packet, and three header packets that
    /// must reach the client out of band before it can decode anything.
    Vorbis {
        encoder: Box<VorbisEncoder>,
        packetizer: vorbis_rtp::Packetizer,
        headers: Box<VorbisHeaders>,
    },
}

/// Capture, encode and packetise the audio stream.
pub struct AudioPipeline {
    backend: AudioBackend,
    params: AudioEncoderParams,
    frames_in: u64,
    packets_out: u64,
    bytes_out: u64,
}

impl AudioPipeline {
    /// Open the pipeline for the negotiated codec.
    pub fn open(params: AudioEncoderParams, ssrc: u32) -> VmmResult<Self> {
        let backend = match params.codec {
            AudioCodec::Opus => {
                let encoder = OpusEncoder::open(vmm_codec_sys::AudioConfig {
                    sample_rate: params.sample_rate,
                    channels: params.channels,
                    bitrate_kbps: params.bitrate_kbps,
                    frame_duration: vmm_codec_sys::FrameDuration::Ms20,
                })
                .map_err(|e| MediaError::AudioEncoderInit(e.to_string()))?;

                log::info!(
                    "Opus encode at {} kbps, {} Hz / {}ch — {} ms frames, low-delay mode",
                    params.bitrate_kbps,
                    params.sample_rate,
                    params.channels,
                    encoder.frame_duration().millis()
                );

                AudioBackend::Opus {
                    packetizer: opus_rtp::Packetizer::new(ssrc, params.sample_rate),
                    encoder: Box::new(encoder),
                }
            }
            AudioCodec::Vorbis => {
                let mut encoder =
                    VorbisEncoder::open(params.sample_rate, params.channels, params.bitrate_kbps)
                        .map_err(|e| MediaError::AudioEncoderInit(e.to_string()))?;
                let headers = encoder
                    .headers()
                    .map_err(|e| MediaError::AudioEncoderInit(e.to_string()))?;

                // RFC 5215 needs a 24-bit identifier for the codebook
                // configuration. Deriving it from the configuration itself
                // means a receiver reconnecting to a re-opened encoder sees
                // the same value when — and only when — the codebooks match.
                let ident = configuration_ident(&headers);

                log::info!(
                    "Vorbis encode at {} kbps, {} Hz / {}ch — configuration ident {ident:#08x}",
                    params.bitrate_kbps,
                    params.sample_rate,
                    params.channels
                );

                AudioBackend::Vorbis {
                    encoder: Box::new(encoder),
                    packetizer: vorbis_rtp::Packetizer::new(ssrc, ident, params.sample_rate),
                    headers: Box::new(headers),
                }
            }
        };

        Ok(AudioPipeline {
            backend,
            params,
            frames_in: 0,
            packets_out: 0,
            bytes_out: 0,
        })
    }

    /// The codec this pipeline encodes.
    pub fn codec(&self) -> AudioCodec {
        self.params.codec
    }

    /// The three Vorbis header packets, for the SDP `configuration`
    /// parameter. `None` for Opus, which needs no such thing — that absence
    /// is the main reason Opus is preferred.
    pub fn vorbis_headers(&self) -> Option<&VorbisHeaders> {
        match &self.backend {
            AudioBackend::Vorbis { headers, .. } => Some(headers),
            AudioBackend::Opus { .. } => None,
        }
    }

    /// The 24-bit Vorbis configuration identifier, when Vorbis is in use.
    pub fn ident(&self) -> Option<u32> {
        match &self.backend {
            AudioBackend::Vorbis { packetizer, .. } => Some(packetizer.ident()),
            AudioBackend::Opus { .. } => None,
        }
    }

    pub fn params(&self) -> &AudioEncoderParams {
        &self.params
    }

    /// `(sample frames in, packets out, bytes out)`.
    pub fn stats(&self) -> (u64, u64, u64) {
        (self.frames_in, self.packets_out, self.bytes_out)
    }

    /// Push interleaved S16LE PCM. Returns the RTP packets it produced.
    pub fn push_pcm(&mut self, interleaved: &[i16]) -> VmmResult<Vec<Packet>> {
        let channels = self.params.channels as usize;
        if channels == 0 || interleaved.len() % channels != 0 {
            return Err(MediaError::Capture {
                detail: format!(
                    "{} samples is not a whole number of {channels}-channel frames",
                    interleaved.len()
                ),
            }
            .into());
        }

        let frames = (interleaved.len() / channels) as u64;
        self.frames_in += frames;

        let mut out = Vec::new();
        match &mut self.backend {
            AudioBackend::Opus {
                encoder,
                packetizer,
            } => {
                let packets = encoder
                    .encode(interleaved)
                    .map_err(|e| encode_error("audio", e))?;
                for packet in packets {
                    self.packets_out += 1;
                    self.bytes_out += packet.data.len() as u64;
                    // RFC 7587 timestamps each packet by the position of its
                    // first sample, which the encoder already tracks.
                    out.extend(packetizer.packetize(&packet)?);
                }
            }
            AudioBackend::Vorbis {
                encoder,
                packetizer,
                ..
            } => {
                let packets = encoder
                    .encode(interleaved)
                    .map_err(|e| encode_error("audio", e))?;
                for packet in packets {
                    self.packets_out += 1;
                    self.bytes_out += packet.len() as u64;
                    out.extend(packetizer.packetize(&packet)?);
                }
                // RFC 5215's timestamp is the position of the packet's first
                // sample, so the clock advances after the packets it
                // produced have been stamped.
                packetizer.advance(frames);
            }
        }
        Ok(out)
    }

    /// Signal end of stream and drain the last packets.
    pub fn drain(&mut self) -> VmmResult<Vec<Packet>> {
        let mut out = Vec::new();
        match &mut self.backend {
            AudioBackend::Opus {
                encoder,
                packetizer,
            } => {
                for packet in encoder.finish().map_err(|e| encode_error("audio", e))? {
                    self.packets_out += 1;
                    self.bytes_out += packet.data.len() as u64;
                    out.extend(packetizer.packetize(&packet)?);
                }
            }
            AudioBackend::Vorbis {
                encoder,
                packetizer,
                ..
            } => {
                for packet in encoder.finish().map_err(|e| encode_error("audio", e))? {
                    self.packets_out += 1;
                    self.bytes_out += packet.len() as u64;
                    out.extend(packetizer.packetize(&packet)?);
                }
            }
        }
        Ok(out)
    }
}

/// A stable 24-bit identifier for one codebook configuration.
///
/// RFC 5215 does not say how to choose it, only that it must identify the
/// configuration. Hashing the configuration itself gives a value that is
/// equal exactly when the codebooks are.
fn configuration_ident(headers: &VorbisHeaders) -> u32 {
    // FNV-1a, folded to 24 bits. A cryptographic hash would be pointless
    // here: this is an equality tag, not a security boundary.
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in headers
        .identification
        .iter()
        .chain(&headers.comment)
        .chain(&headers.setup)
    {
        hash ^= *byte as u64;
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    ((hash ^ (hash >> 32)) & 0x00ff_ffff) as u32
}

#[cfg(test)]
mod tests {
    use super::*;
    use libvmm_config::{HardwareAccelerator, RateControl};

    /// 30 fps, 1800 kbps target, 2000 kbps ceiling, a one-second VBV — the
    /// §7.1 reference numbers.
    fn params() -> VideoEncoderParams {
        VideoEncoderParams {
            codec: vmm_codec_sys::VideoCodec::H264,
            width: 1920,
            height: 1080,
            framerate: 30,
            accelerator: HardwareAccelerator::Vaapi,
            rate_control: RateControl::Vbr,
            target_kbps: 1800,
            max_kbps: 2000,
            vbv_buffer_bits: 2_000_000,
            gop_length: 60,
        }
    }

    #[test]
    fn the_window_spans_the_vbv_buffer() {
        // A one-second buffer at 30 fps covers 30 frames.
        assert_eq!(VbvMonitor::new(&params()).capacity, 30);

        let mut half = params();
        half.vbv_buffer_bits = 1_000_000; // half a second
        assert_eq!(VbvMonitor::new(&half).capacity, 15);
    }

    #[test]
    fn a_lone_keyframe_does_not_breach() {
        let params = params();
        let mut monitor = VbvMonitor::new(&params);
        // A 60 kB IDR is ~14 Mbps extrapolated to a second, which is what
        // the old per-frame test wrongly flagged. Inside a one-second buffer
        // it is only 480 kbit and fits easily.
        assert_eq!(monitor.record(60_000, params.framerate), None);
    }

    #[test]
    fn a_keyframe_followed_by_normal_frames_does_not_breach() {
        let params = params();
        let mut monitor = VbvMonitor::new(&params);
        assert_eq!(monitor.record(60_000, params.framerate), None);
        // 29 inter frames at ~7 kB: 60000 + 29*7000 = 263 kB = 2.10 Mbit.
        // Slightly over the 2.0 Mbit budget, so this must be caught.
        let mut breached = false;
        for _ in 0..29 {
            breached |= monitor.record(7_000, params.framerate).is_some();
        }
        assert!(
            breached,
            "a full second totalling 2.1 Mbit must breach a 2.0 Mbit budget"
        );
    }

    #[test]
    fn a_stream_inside_the_budget_never_breaches() {
        let params = params();
        let mut monitor = VbvMonitor::new(&params);
        // 1800 kbps at 30 fps is 7500 bytes per frame. Run several seconds.
        for _ in 0..300 {
            assert_eq!(monitor.record(7_500, params.framerate), None);
        }
    }

    #[test]
    fn the_window_forgets_frames_that_have_left_it() {
        let params = params();
        let mut monitor = VbvMonitor::new(&params);
        // One very large frame, then a full window of silence: once the big
        // frame has aged out, the monitor must be clean again.
        monitor.record(240_000, params.framerate);
        for _ in 0..30 {
            monitor.record(100, params.framerate);
        }
        assert_eq!(
            monitor.record(100, params.framerate),
            None,
            "the oversized frame should have left the window"
        );
    }

    #[test]
    fn a_breach_is_reported_as_a_rate() {
        let params = params();
        let mut monitor = VbvMonitor::new(&params);
        // Half a window at double the ceiling.
        let mut reported = None;
        for _ in 0..15 {
            if let Some(kbps) = monitor.record(16_667, params.framerate) {
                reported = Some(kbps);
            }
        }
        let kbps = reported.expect("sustained double-rate output must breach");
        assert!(
            (3_500..4_500).contains(&kbps),
            "expected roughly 4000 kbps, got {kbps}"
        );
    }
}
