//! Capture and encode pipeline — §7.1.
//!
//! ```text
//! virtio-gpu scanout (01:00.0) --> ARGB framebuffer (1920x1080) --+
//!                                        +--> H.264 encode
//!                         (VA-API or NVENC, CVBR) | target ~1800 kbps
//!                                        | HARD CAP 2000 kbps
//! virtio-snd PCM (01:00.1) 48kHz S16LE stereo --> Vorbis 128kbps -+
//! ```
//!
//! Rate control is constrained VBR: the encoder's max bitrate MUST be
//! programmed to `max_bitrate_kbps` (2000), instantaneous output MUST remain
//! below it, and `bitrate_kbps` (1800) is the target average. The HRD/VBV
//! buffer is sized to the cap.

use libvmm_config::{Display, HardwareAccelerator, RateControl};
use libvmm_core::{MediaError, VmmResult};
use vmm_codec_sys::{AudioCodec, VideoCodec};

/// The hard ceiling from change-log item 10. Not configurable upwards.
pub const HARD_CAP_KBPS: u32 = 2000;

/// The encoder parameters actually programmed into the hardware.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct VideoEncoderParams {
    /// Which codec to encode. Negotiated per session rather than configured
    /// — see [`crate::negotiate`] — so the same machine can serve H.264 to
    /// one client and AV1 to the next.
    pub codec: VideoCodec,
    pub width: u32,
    pub height: u32,
    pub framerate: u32,
    pub accelerator: HardwareAccelerator,
    pub rate_control: RateControl,
    /// Target average.
    pub target_kbps: u32,
    /// Programmed as the encoder's maximum. Output must stay below it.
    pub max_kbps: u32,
    /// HRD/VBV buffer, sized to the cap (§7.1).
    pub vbv_buffer_bits: u32,
    /// One keyframe per two seconds of video.
    pub gop_length: u32,
}

impl VideoEncoderParams {
    pub fn from_config(d: &Display) -> VmmResult<Self> {
        let e = &d.encoder;
        if e.max_kbps_exceeds_cap() {
            return Err(MediaError::EncoderInit {
                accelerator: accelerator_name(e.hardware_accelerator),
                detail: format!(
                    "max_bitrate_kbps {} exceeds the hard {HARD_CAP_KBPS} kbps ceiling (§7.1)",
                    e.max_bitrate_kbps
                ),
            }
            .into());
        }
        if e.bitrate_kbps >= e.max_bitrate_kbps {
            return Err(MediaError::EncoderInit {
                accelerator: accelerator_name(e.hardware_accelerator),
                detail: format!(
                    "target {} kbps must stay below the {} kbps ceiling",
                    e.bitrate_kbps, e.max_bitrate_kbps
                ),
            }
            .into());
        }

        Ok(VideoEncoderParams {
            // The configured default; `for_codec` overrides it once a
            // session has negotiated something else.
            codec: VideoCodec::default(),
            width: d.width,
            height: d.height,
            framerate: d.framerate_cap,
            accelerator: e.hardware_accelerator,
            rate_control: e.rate_control,
            target_kbps: e.bitrate_kbps,
            max_kbps: e.max_bitrate_kbps,
            // Sizing the VBV buffer to one second at the cap is what keeps
            // instantaneous VBR output below the ceiling.
            vbv_buffer_bits: e.max_bitrate_kbps.saturating_mul(1000),
            gop_length: d.framerate_cap.saturating_mul(2).max(1),
        })
    }

    /// Would this instantaneous rate breach the hard cap?
    pub const fn exceeds_cap(&self, instantaneous_kbps: u32) -> bool {
        instantaneous_kbps > self.max_kbps
    }

    /// The same parameters for a different codec, as negotiated.
    pub const fn for_codec(mut self, codec: VideoCodec) -> Self {
        self.codec = codec;
        self
    }
}

/// Extension trait so the ceiling check reads the same in both places.
trait CeilingCheck {
    fn max_kbps_exceeds_cap(&self) -> bool;
}

impl CeilingCheck for libvmm_config::VideoEncoder {
    fn max_kbps_exceeds_cap(&self) -> bool {
        self.max_bitrate_kbps > HARD_CAP_KBPS
    }
}

pub const fn accelerator_name(a: HardwareAccelerator) -> &'static str {
    match a {
        HardwareAccelerator::Vaapi => "vaapi",
        HardwareAccelerator::Nvenc => "nvenc",
    }
}

/// Audio capture format, fixed at 48 kHz S16LE stereo (§7.1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioEncoderParams {
    /// Which codec to encode. Negotiated per session; Opus unless the
    /// client can only take Vorbis.
    pub codec: AudioCodec,
    pub sample_rate: u32,
    pub channels: u8,
    pub bitrate_kbps: u32,
}

impl AudioEncoderParams {
    pub fn from_config(d: &Display) -> Self {
        AudioEncoderParams {
            codec: AudioCodec::default(),
            sample_rate: d.audio_encoder.sample_rate,
            channels: d.audio_encoder.channels,
            bitrate_kbps: d.audio_encoder.bitrate_kbps,
        }
    }

    /// The same parameters for a different codec, as negotiated.
    pub const fn for_codec(mut self, codec: AudioCodec) -> Self {
        self.codec = codec;
        self
    }
}

/// The scanout format the capture thread reads (§7.1).
#[derive(Debug, Clone, Copy)]
pub struct ScanoutFormat {
    pub width: u32,
    pub height: u32,
    pub bits_per_pixel: u32,
}

impl ScanoutFormat {
    pub fn from_config(d: &Display) -> Self {
        ScanoutFormat {
            width: d.width,
            height: d.height,
            bits_per_pixel: d.color_depth_bits,
        }
    }

    pub const fn frame_bytes(&self) -> u64 {
        self.width as u64 * self.height as u64 * (self.bits_per_pixel as u64 / 8)
    }
}

/// Build the SDP a DESCRIBE returns (§7.2).
///
/// Two media sections, matching the interleaved channel assignment in §7.3:
/// video on channels 0-1, audio on 2-3.
/// Build the SDP a DESCRIBE returns (§7.2.2).
///
/// The codecs are arguments, not constants: since Revision B the server
/// announces what it *negotiated* with this client, which is not knowable
/// until the DESCRIBE carrying their capabilities arrives.
pub fn sdp(
    vm_name: &str,
    stream_path: &str,
    video: &VideoEncoderParams,
    audio: &AudioEncoderParams,
    video_codec: VideoCodec,
    audio_codec: AudioCodec,
    vorbis_configuration: Option<&str>,
) -> String {
    let pt_video = match video_codec {
        VideoCodec::H264 => crate::rtp::PAYLOAD_TYPE_H264,
        VideoCodec::Vp9 => crate::rtp::PAYLOAD_TYPE_VP9,
        VideoCodec::Av1 => crate::rtp::PAYLOAD_TYPE_AV1,
    };
    let pt_audio = match audio_codec {
        AudioCodec::Opus => crate::rtp::PAYLOAD_TYPE_OPUS,
        AudioCodec::Vorbis => crate::rtp::PAYLOAD_TYPE_VORBIS,
    };

    // Only H.264 carries a packetization-mode; the VP9 and AV1 payload
    // formats have no equivalent fmtp line, and inventing one would make
    // the SDP wrong rather than merely sparse.
    let video_fmtp = match video_codec {
        VideoCodec::H264 => format!("a=fmtp:{pt_video} packetization-mode=1\r\n"),
        VideoCodec::Vp9 | VideoCodec::Av1 => String::new(),
    };

    // Vorbis cannot decode a single packet without its three headers, so
    // RFC 5215 carries them here. Opus needs nothing (§7.1.3).
    let audio_fmtp = match (audio_codec, vorbis_configuration) {
        (AudioCodec::Vorbis, Some(config)) => {
            format!("a=fmtp:{pt_audio} configuration={config}\r\n")
        }
        _ => String::new(),
    };

    let audio_rtpmap = match audio_codec {
        AudioCodec::Opus => format!("opus/{}/{}", audio.sample_rate, audio.channels),
        AudioCodec::Vorbis => format!("vorbis/{}/{}", audio.sample_rate, audio.channels),
    };

    format!(
        "v=0\r\n\
         o=- 0 0 IN IP6 ::\r\n\
         s={vm_name}\r\n\
         c=IN IP6 ::\r\n\
         t=0 0\r\n\
         a=control:{stream_path}\r\n\
         m=video 0 RTP/AVP {pt_video}\r\n\
         a=rtpmap:{pt_video} {video_name}/{clock}\r\n\
         {video_fmtp}\
         a=framesize:{pt_video} {w}-{h}\r\n\
         a=framerate:{fps}\r\n\
         a=control:{stream_path}/video\r\n\
         m=audio 0 RTP/AVP {pt_audio}\r\n\
         a=rtpmap:{pt_audio} {audio_rtpmap}\r\n\
         {audio_fmtp}\
         a=control:{stream_path}/audio\r\n",
        video_name = video_codec.rtp_encoding_name(),
        clock = crate::rtp::CLOCK_RATE_VIDEO,
        w = video.width,
        h = video.height,
        fps = video.framerate,
    )
}
