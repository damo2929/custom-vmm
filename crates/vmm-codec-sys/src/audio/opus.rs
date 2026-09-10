//! Opus audio encode and decode — the preferred half of §7.1's audio leg.
//!
//! Opus is chosen ahead of the specification's Vorbis wherever the client
//! supports it, for the reasons §7.1 cares about: it is designed for
//! interactive use, so a 20 ms frame carries ~20 ms of algorithmic delay
//! against Vorbis' ~46 ms at the same rate; it costs less CPU at both ends;
//! and it has a real RTP payload format (RFC 7587) in which the payload *is*
//! the Opus packet, with no descriptor and no out-of-band codebooks.
//!
//! That last point removes a whole class of failure. Vorbis needs its three
//! header packets delivered before a single frame can be decoded, so a
//! client joining mid-session must wait for, or re-request, the
//! configuration. An Opus stream is decodable from any packet.
//!
//! [`super::vorbis`] remains for clients that cannot do Opus; the choice is
//! made by the negotiation in `libvmm-media`, not here.
//!
//! Both ends go through libavcodec's `libopus` wrapper rather than linking
//! libopus directly: the wrapper exposes every control this needs
//! (`application`, `frame_duration`, `fec`) and keeps the audio path free of
//! another direct C dependency.

use super::{set_channel_layout, AvAudio};
use crate::error::{av_error, averror_eagain, averror_eof, CodecError, Result};
use crate::raw::ffmpeg as ff;
use std::ffi::CString;

/// §7.1 captures at 48 kHz, which is also Opus' native rate — no resampling.
pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: u8 = 2;

/// Frame length in milliseconds.
///
/// 20 ms is the interoperable default and what RFC 7587 assumes when the SDP
/// says nothing. 10 ms halves the algorithmic delay for roughly 15% more
/// bitrate, which is the right trade on a console if latency is the priority.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FrameDuration {
    Ms10,
    Ms20,
}

impl FrameDuration {
    pub const fn millis(self) -> u32 {
        match self {
            FrameDuration::Ms10 => 10,
            FrameDuration::Ms20 => 20,
        }
    }

    /// Sample frames per Opus frame at 48 kHz.
    pub const fn samples(self) -> u32 {
        SAMPLE_RATE / 1000 * self.millis()
    }

    /// The `frame_duration` option value libavcodec expects.
    const fn as_option(self) -> &'static str {
        match self {
            FrameDuration::Ms10 => "10",
            FrameDuration::Ms20 => "20",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AudioConfig {
    pub sample_rate: u32,
    pub channels: u8,
    pub bitrate_kbps: u32,
    pub frame_duration: FrameDuration,
}

impl Default for AudioConfig {
    fn default() -> Self {
        AudioConfig {
            sample_rate: SAMPLE_RATE,
            channels: CHANNELS,
            bitrate_kbps: 128,
            frame_duration: FrameDuration::Ms20,
        }
    }
}

impl AudioConfig {
    fn validate(&self) -> Result<()> {
        if self.channels == 0 || self.channels > 2 {
            return Err(CodecError::invalid(
                "opus",
                format!("{} channels: §7.1 captures mono or stereo", self.channels),
            ));
        }
        // Opus internally works at 48 kHz and RFC 7587 fixes the RTP clock
        // there regardless of the input rate, so anything else would need a
        // resampler this path deliberately does not have.
        if self.sample_rate != SAMPLE_RATE {
            return Err(CodecError::invalid(
                "opus",
                format!(
                    "{} Hz: Opus is a 48 kHz codec and §7.1 captures at 48 kHz",
                    self.sample_rate
                ),
            ));
        }
        if self.bitrate_kbps == 0 {
            return Err(CodecError::invalid("opus", "bitrate must be non-zero"));
        }
        Ok(())
    }
}

pub struct OpusEncoder {
    av: AvAudio,
    config: AudioConfig,
    /// Sample frames the encoder consumes per call, from libavcodec.
    frame_size: u32,
    /// Samples accepted but not yet handed to libavcodec.
    pending: Vec<i16>,
    /// Presentation timestamp of the next frame, in 48 kHz samples.
    next_pts: i64,
}

// SAFETY: every pointer is owned exclusively by this handle, and libavcodec
// permits one thread at a time per context, which &mut self enforces.
unsafe impl Send for OpusEncoder {}

impl std::fmt::Debug for OpusEncoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpusEncoder")
            .field("sample_rate", &self.config.sample_rate)
            .field("channels", &self.config.channels)
            .field("bitrate_kbps", &self.config.bitrate_kbps)
            .field("frame_ms", &self.config.frame_duration.millis())
            .finish()
    }
}

impl OpusEncoder {
    pub fn open(config: AudioConfig) -> Result<Self> {
        config.validate()?;
        crate::logging::install();

        let name = CString::new("libopus").map_err(|e| CodecError::init("opus", e.to_string()))?;
        // SAFETY: name is a valid C string; the call only looks up a table.
        let codec = unsafe { ff::avcodec_find_encoder_by_name(name.as_ptr()) };
        if codec.is_null() {
            return Err(CodecError::unavailable(
                "opus",
                "this libavcodec was built without the libopus encoder",
            ));
        }

        let mut encoder = OpusEncoder {
            av: AvAudio::new(),
            config,
            frame_size: 0,
            pending: Vec::new(),
            next_pts: 0,
        };

        // SAFETY: codec is non-null.
        encoder.av.ctx = unsafe { ff::avcodec_alloc_context3(codec) };
        if encoder.av.ctx.is_null() {
            return Err(CodecError::init("opus", "avcodec_alloc_context3 failed"));
        }

        // SAFETY: ctx is freshly allocated; these are public fields, and
        // set_channel_layout is given the context's own layout.
        unsafe {
            let c = encoder.av.ctx;
            (*c).sample_rate = config.sample_rate as i32;
            (*c).sample_fmt = ff::AVSampleFormat_AV_SAMPLE_FMT_S16;
            (*c).bit_rate = i64::from(config.bitrate_kbps) * 1000;
            (*c).time_base = ff::AVRational {
                num: 1,
                den: config.sample_rate as i32,
            };
            set_channel_layout(&mut (*c).ch_layout, config.channels);
        }

        // "lowdelay" is Opus' RESTRICTED_LOWDELAY: it disables the SILK
        // layer and the prediction that costs a frame of lookahead, leaving
        // only CELT. That trades a little coding efficiency for the lowest
        // algorithmic delay the codec offers, which is the right side of the
        // trade for an interactive console.
        encoder.set_option("application", "lowdelay")?;
        encoder.set_option("frame_duration", config.frame_duration.as_option())?;
        // In-band FEC costs bitrate and only helps for lossy links; §7.3
        // interleaves media over the TCP control connection, where a lost
        // packet is retransmitted rather than dropped.
        encoder.set_option("fec", "0")?;
        // Discontinuous transmission is deliberately *not* enabled. It
        // would stop sending during silence, which breaks the RTP timestamp
        // continuity the client relies on to keep audio aligned with video.
        // FFmpeg's libopus wrapper exposes no `dtx` option and leaves it off,
        // so there is nothing to set here — only something not to turn on.

        // SAFETY: ctx is configured and codec matches it.
        let rc = unsafe { ff::avcodec_open2(encoder.av.ctx, codec, core::ptr::null_mut()) };
        if rc < 0 {
            return Err(CodecError::init(
                "opus",
                format!("avcodec_open2 on libopus: {}", av_error(rc)),
            ));
        }

        // SAFETY: the context is open, so frame_size is populated.
        let frame_size = unsafe { (*encoder.av.ctx).frame_size };
        if frame_size <= 0 {
            return Err(CodecError::init(
                "opus",
                format!("libopus reported a frame size of {frame_size}"),
            ));
        }
        encoder.frame_size = frame_size as u32;

        encoder.av.alloc_objects("opus")?;

        // SAFETY: frame is freshly allocated; these are public fields.
        unsafe {
            let f = encoder.av.frame;
            (*f).format = ff::AVSampleFormat_AV_SAMPLE_FMT_S16;
            (*f).nb_samples = frame_size;
            (*f).sample_rate = config.sample_rate as i32;
            set_channel_layout(&mut (*f).ch_layout, config.channels);
            let rc = ff::av_frame_get_buffer(f, 0);
            if rc < 0 {
                return Err(CodecError::init(
                    "opus",
                    format!("av_frame_get_buffer: {}", av_error(rc)),
                ));
            }
        }

        Ok(encoder)
    }

    pub fn open_default(bitrate_kbps: u32) -> Result<Self> {
        OpusEncoder::open(AudioConfig {
            bitrate_kbps,
            ..AudioConfig::default()
        })
    }

    fn set_option(&self, name: &str, value: &str) -> Result<()> {
        let key = CString::new(name).map_err(|e| CodecError::init("opus", e.to_string()))?;
        let val = CString::new(value).map_err(|e| CodecError::init("opus", e.to_string()))?;
        // SAFETY: ctx is live and not yet opened; priv_data belongs to the
        // encoder, and av_opt_set errors on an unknown option.
        let rc = unsafe { ff::av_opt_set((*self.av.ctx).priv_data, key.as_ptr(), val.as_ptr(), 0) };
        if rc < 0 {
            return Err(CodecError::init(
                "opus",
                format!("libopus rejected {name}={value}: {}", av_error(rc)),
            ));
        }
        Ok(())
    }

    pub fn sample_rate(&self) -> u32 {
        self.config.sample_rate
    }

    pub fn channels(&self) -> u8 {
        self.config.channels
    }

    pub fn bitrate_kbps(&self) -> u32 {
        self.config.bitrate_kbps
    }

    /// Sample frames per Opus packet.
    pub fn frame_size(&self) -> u32 {
        self.frame_size
    }

    pub fn frame_duration(&self) -> FrameDuration {
        self.config.frame_duration
    }

    /// Encode interleaved S16LE samples.
    ///
    /// Opus codes fixed-length frames, so a submission that is not a whole
    /// number of them leaves a remainder buffered here until the next call.
    /// Each returned packet is one complete Opus packet, ready to be an RTP
    /// payload exactly as it stands (RFC 7587 §4.2).
    pub fn encode(&mut self, interleaved: &[i16]) -> Result<Vec<OpusPacket>> {
        let channels = self.config.channels as usize;
        if channels == 0 || interleaved.len() % channels != 0 {
            return Err(CodecError::invalid(
                "opus",
                format!(
                    "{} samples is not a whole number of {channels}-channel frames",
                    interleaved.len()
                ),
            ));
        }

        self.pending.extend_from_slice(interleaved);

        let per_frame = self.frame_size as usize * channels;
        let mut packets = Vec::new();
        let mut consumed = 0usize;

        while self.pending.len() - consumed >= per_frame {
            let block_start = consumed;
            consumed += per_frame;
            let pts = self.next_pts;
            self.next_pts += i64::from(self.frame_size);
            if let Some(packet) = self.submit(block_start, per_frame, pts)? {
                packets.push(packet);
            }
        }

        self.pending.drain(..consumed);
        Ok(packets)
    }

    /// Copy one block into the reusable frame and encode it.
    fn submit(&mut self, offset: usize, len: usize, pts: i64) -> Result<Option<OpusPacket>> {
        // SAFETY: the frame was allocated with av_frame_get_buffer, so it is
        // writable; make_writable drops any shared reference first.
        let rc = unsafe { ff::av_frame_make_writable(self.av.frame) };
        if rc < 0 {
            return Err(CodecError::process(
                "opus",
                format!("av_frame_make_writable: {}", av_error(rc)),
            ));
        }

        // SAFETY: the frame holds nb_samples * channels interleaved i16s in
        // data[0], which is exactly `len` values, and the source slice has
        // at least that many from `offset`.
        unsafe {
            let dst = (*self.av.frame).data[0] as *mut i16;
            core::ptr::copy_nonoverlapping(self.pending.as_ptr().add(offset), dst, len);
            (*self.av.frame).pts = pts;
        }

        // SAFETY: the context is open and the frame is populated.
        let rc = unsafe { ff::avcodec_send_frame(self.av.ctx, self.av.frame) };
        if rc < 0 && rc != averror_eof() {
            return Err(CodecError::process(
                "opus",
                format!("avcodec_send_frame: {}", av_error(rc)),
            ));
        }
        self.receive()
    }

    fn receive(&mut self) -> Result<Option<OpusPacket>> {
        // SAFETY: the context is open and packet is live.
        let rc = unsafe { ff::avcodec_receive_packet(self.av.ctx, self.av.packet) };
        if rc == averror_eagain() || rc == averror_eof() {
            return Ok(None);
        }
        if rc < 0 {
            return Err(CodecError::process(
                "opus",
                format!("avcodec_receive_packet: {}", av_error(rc)),
            ));
        }
        // SAFETY: a successful receive leaves `size` bytes at `data`.
        let packet = unsafe {
            let p = &*self.av.packet;
            OpusPacket {
                data: core::slice::from_raw_parts(p.data, p.size.max(0) as usize).to_vec(),
                pts: p.pts,
                samples: self.frame_size,
            }
        };
        // SAFETY: packet is live; unref readies it for the next receive.
        unsafe { ff::av_packet_unref(self.av.packet) };
        Ok(Some(packet))
    }

    /// Flush the encoder at end of stream.
    ///
    /// Any partial frame still buffered is padded with silence rather than
    /// dropped, so the stream ends on a frame boundary and the last few
    /// milliseconds of audio are not lost.
    pub fn finish(&mut self) -> Result<Vec<OpusPacket>> {
        let channels = self.config.channels as usize;
        let per_frame = self.frame_size as usize * channels;
        let mut packets = Vec::new();

        if !self.pending.is_empty() {
            self.pending.resize(per_frame, 0);
            let pts = self.next_pts;
            self.next_pts += i64::from(self.frame_size);
            if let Some(packet) = self.submit(0, per_frame, pts)? {
                packets.push(packet);
            }
            self.pending.clear();
        }

        // SAFETY: a null frame is libavcodec's documented drain signal.
        let rc = unsafe { ff::avcodec_send_frame(self.av.ctx, core::ptr::null()) };
        if rc < 0 && rc != averror_eof() {
            return Err(CodecError::process(
                "opus",
                format!("flushing the encoder: {}", av_error(rc)),
            ));
        }
        // Bounded: the encoder holds at most a handful of frames.
        for _ in 0..64 {
            match self.receive()? {
                Some(packet) => packets.push(packet),
                None => break,
            }
        }
        Ok(packets)
    }
}

/// One encoded Opus packet.
///
/// RFC 7587 puts exactly this, unwrapped, in the RTP payload.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpusPacket {
    pub data: Vec<u8>,
    /// Presentation timestamp in 48 kHz samples, which is also the RTP
    /// timestamp RFC 7587 requires.
    pub pts: i64,
    /// Sample frames this packet represents.
    pub samples: u32,
}

pub struct OpusDecoder {
    av: AvAudio,
    channels: u8,
    sample_rate: u32,
}

// SAFETY: as with the encoder.
unsafe impl Send for OpusDecoder {}

impl OpusDecoder {
    pub fn open(sample_rate: u32, channels: u8) -> Result<Self> {
        crate::logging::install();

        // SAFETY: the identifier is valid; the call looks up a table.
        let codec = unsafe { ff::avcodec_find_decoder(ff::AVCodecID_AV_CODEC_ID_OPUS) };
        if codec.is_null() {
            return Err(CodecError::unavailable(
                "opus decode",
                "this libavcodec was built without an Opus decoder",
            ));
        }

        let mut decoder = OpusDecoder {
            av: AvAudio::new(),
            channels,
            sample_rate,
        };

        // SAFETY: codec is non-null.
        decoder.av.ctx = unsafe { ff::avcodec_alloc_context3(codec) };
        if decoder.av.ctx.is_null() {
            return Err(CodecError::init(
                "opus decode",
                "avcodec_alloc_context3 failed",
            ));
        }
        // SAFETY: ctx is freshly allocated; these are public fields.
        unsafe {
            let c = decoder.av.ctx;
            (*c).sample_rate = sample_rate as i32;
            (*c).request_sample_fmt = ff::AVSampleFormat_AV_SAMPLE_FMT_S16;
            set_channel_layout(&mut (*c).ch_layout, channels);
        }

        // SAFETY: ctx is configured and codec matches it.
        let rc = unsafe { ff::avcodec_open2(decoder.av.ctx, codec, core::ptr::null_mut()) };
        if rc < 0 {
            return Err(CodecError::init(
                "opus decode",
                format!("avcodec_open2: {}", av_error(rc)),
            ));
        }
        decoder.av.alloc_objects("opus decode")?;
        Ok(decoder)
    }

    pub fn channels(&self) -> u8 {
        self.channels
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Decode one Opus packet to interleaved S16LE samples.
    pub fn decode(&mut self, packet: &[u8]) -> Result<Vec<i16>> {
        if packet.is_empty() {
            return Ok(Vec::new());
        }

        // SAFETY: packet is live. Pointing it at the caller's buffer without
        // copying is sound because avcodec_send_packet consumes the data
        // during the call, and the slice outlives it.
        unsafe {
            (*self.av.packet).data = packet.as_ptr() as *mut u8;
            (*self.av.packet).size = packet.len() as i32;
        }
        // SAFETY: the context is open and the packet is populated.
        let rc = unsafe { ff::avcodec_send_packet(self.av.ctx, self.av.packet) };
        // SAFETY: clear the borrowed pointer before it can outlive the slice.
        unsafe {
            (*self.av.packet).data = core::ptr::null_mut();
            (*self.av.packet).size = 0;
        }
        if rc < 0 && rc != averror_eagain() {
            return Err(CodecError::process(
                "opus decode",
                format!("avcodec_send_packet: {}", av_error(rc)),
            ));
        }

        let mut out = Vec::new();
        loop {
            // SAFETY: the context is open and frame is live.
            let rc = unsafe { ff::avcodec_receive_frame(self.av.ctx, self.av.frame) };
            if rc == averror_eagain() || rc == averror_eof() {
                return Ok(out);
            }
            if rc < 0 {
                return Err(CodecError::process(
                    "opus decode",
                    format!("avcodec_receive_frame: {}", av_error(rc)),
                ));
            }
            // SAFETY: a successful receive leaves the frame populated.
            unsafe { super::append_samples(self.av.frame, &mut out, "opus decode")? };
            // SAFETY: frame is live; unref readies it for the next receive.
            unsafe { ff::av_frame_unref(self.av.frame) };
        }
    }
}
