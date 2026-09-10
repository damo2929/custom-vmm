//! Vorbis audio encode and decode — §7.1's original codec, kept as the
//! fallback for clients that cannot do Opus.
//!
//! Encoding uses libvorbisenc directly. Decoding goes through libavcodec,
//! because the client needs no more than a decoder and libavcodec's already
//! links here for video.
//!
//! The awkward part of Vorbis, and the reason Opus is preferred, is the
//! configuration: a decoder cannot start until it has all three header
//! packets (identification, comment, setup). They travel out of band in the
//! SDP (RFC 5215 §3.2), so a client joining a session mid-stream depends on
//! having seen the DESCRIBE. [`VorbisHeaders::packed_configuration`] builds
//! that payload, and [`VorbisDecoder::open`] consumes the same three packets
//! from the other side.

use super::{set_channel_layout, AvAudio};
use crate::error::{av_error, averror_eagain, averror_eof, CodecError, Result};
use crate::raw::ffmpeg as ff;
use crate::raw::vorbis as vb;

/// §7.1 fixes capture at 48 kHz stereo.
pub const SAMPLE_RATE: u32 = 48_000;
pub const CHANNELS: u8 = 2;

/// The three Vorbis header packets, in the order RFC 5215 requires.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VorbisHeaders {
    pub identification: Vec<u8>,
    pub comment: Vec<u8>,
    pub setup: Vec<u8>,
}

impl VorbisHeaders {
    /// The RFC 5215 §3.2 configuration payload: the first two headers
    /// length-prefixed as Xiph lacing values, then the three bodies.
    ///
    /// Only two lengths are written; the third is implied by what remains,
    /// which is what the packed-configuration format specifies.
    pub fn packed_configuration(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(
            self.identification.len() + self.comment.len() + self.setup.len() + 8,
        );
        write_xiph_lacing(&mut out, self.identification.len());
        write_xiph_lacing(&mut out, self.comment.len());
        out.extend_from_slice(&self.identification);
        out.extend_from_slice(&self.comment);
        out.extend_from_slice(&self.setup);
        out
    }

    /// The same three headers in libavcodec's `extradata` form.
    ///
    /// Nearly, but not quite, the RFC 5215 payload: FFmpeg's Xiph splitter
    /// expects a leading byte holding the header count minus one, which the
    /// RTP format omits because the count is fixed at three. Feeding it
    /// [`packed_configuration`](Self::packed_configuration) directly makes
    /// `avcodec_open2` fail with EPERM and no useful diagnostic, so the two
    /// are built separately rather than assumed identical.
    pub fn to_extradata(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(
            self.identification.len() + self.comment.len() + self.setup.len() + 8,
        );
        out.push(2); // three headers, minus one
        write_xiph_lacing(&mut out, self.identification.len());
        write_xiph_lacing(&mut out, self.comment.len());
        out.extend_from_slice(&self.identification);
        out.extend_from_slice(&self.comment);
        out.extend_from_slice(&self.setup);
        out
    }

    /// Recover the three headers from a packed configuration.
    pub fn from_packed_configuration(packed: &[u8]) -> Result<Self> {
        let mut at = 0usize;
        let mut lengths = [0usize; 2];
        for length in &mut lengths {
            loop {
                let byte = *packed.get(at).ok_or_else(|| {
                    CodecError::invalid("vorbis", "the packed configuration ended inside a length")
                })?;
                at += 1;
                *length += byte as usize;
                if byte != 255 {
                    break;
                }
            }
        }
        let (first, second) = (lengths[0], lengths[1]);
        let total = first
            .checked_add(second)
            .ok_or_else(|| CodecError::invalid("vorbis", "the header lengths overflow"))?;
        if packed.len() < at + total {
            return Err(CodecError::invalid(
                "vorbis",
                format!(
                    "the packed configuration is {} bytes, {} required by its own lengths",
                    packed.len(),
                    at + total
                ),
            ));
        }
        Ok(VorbisHeaders {
            identification: packed[at..at + first].to_vec(),
            comment: packed[at + first..at + total].to_vec(),
            setup: packed[at + total..].to_vec(),
        })
    }
}

/// Xiph-style length: 255 for each full run, then the remainder.
fn write_xiph_lacing(out: &mut Vec<u8>, mut length: usize) {
    while length >= 255 {
        out.push(255);
        length -= 255;
    }
    out.push(length as u8);
}

/// The four libvorbis structs, kept behind one allocation.
///
/// They must never move once initialised: `vorbis_analysis_init` stores a
/// pointer to `info` inside `dsp`, and `vorbis_block_init` stores a pointer
/// to `dsp` inside `block`. Holding them by value in the encoder and
/// returning it from `open` would move all four and leave those back
/// pointers dangling — a use-after-free the moment the first block is
/// analysed, not a latent risk.
struct State {
    info: vb::vorbis_info,
    comment: vb::vorbis_comment,
    dsp: vb::vorbis_dsp_state,
    block: vb::vorbis_block,
}

/// Which of the four structs libvorbis actually initialised.
///
/// `open` can fail part-way through, and each teardown function must only
/// run against a struct that was set up. libvorbis does null-guard its clear
/// functions, so clearing a zeroed struct happens not to crash — but that is
/// an implementation detail, not a documented contract.
#[derive(Default, Clone, Copy)]
struct Live {
    info: bool,
    comment: bool,
    dsp: bool,
    block: bool,
}

pub struct VorbisEncoder {
    state: Box<State>,
    channels: u8,
    sample_rate: u32,
    bitrate_kbps: u32,
    live: Live,
}

// SAFETY: every libvorbis struct here is owned exclusively by this handle,
// and libvorbis permits one encoder per thread, which &mut self enforces.
unsafe impl Send for VorbisEncoder {}

impl std::fmt::Debug for VorbisEncoder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VorbisEncoder")
            .field("sample_rate", &self.sample_rate)
            .field("channels", &self.channels)
            .field("bitrate_kbps", &self.bitrate_kbps)
            .finish()
    }
}

impl VorbisEncoder {
    /// Open an encoder in managed-bitrate mode at `bitrate_kbps`.
    pub fn open(sample_rate: u32, channels: u8, bitrate_kbps: u32) -> Result<Self> {
        if channels == 0 || channels > 2 {
            return Err(CodecError::invalid(
                "libvorbis",
                format!("{channels} channels: §7.1 captures mono or stereo"),
            ));
        }
        if sample_rate == 0 {
            return Err(CodecError::invalid(
                "libvorbis",
                "sample rate must be non-zero",
            ));
        }
        if bitrate_kbps == 0 {
            return Err(CodecError::invalid("libvorbis", "bitrate must be non-zero"));
        }

        // Box first, initialise second: everything below takes the address
        // of a field, and those addresses have to outlive this function.
        let state = Box::new(State {
            // SAFETY: libvorbis initialises each struct below before use;
            // zeroed is the documented starting state for all four.
            info: unsafe { core::mem::zeroed() },
            comment: unsafe { core::mem::zeroed() },
            dsp: unsafe { core::mem::zeroed() },
            block: unsafe { core::mem::zeroed() },
        });
        let mut encoder = VorbisEncoder {
            state,
            channels,
            sample_rate,
            bitrate_kbps,
            live: Live::default(),
        };

        // SAFETY: info is a live vorbis_info that nothing else references.
        unsafe { vb::vorbis_info_init(&mut encoder.state.info) };
        encoder.live.info = true;

        // Managed bitrate with the nominal pinned to the configured rate and
        // both bounds unconstrained (-1). A hard minimum would force the
        // encoder to pad silence up to the bitrate, which on an idle console
        // is most of the session.
        let bitrate = i64::from(bitrate_kbps) * 1000;
        // SAFETY: info was just initialised.
        let rc = unsafe {
            vb::vorbis_encode_init(
                &mut encoder.state.info,
                channels as core::ffi::c_long,
                sample_rate as core::ffi::c_long,
                -1,
                bitrate as core::ffi::c_long,
                -1,
            )
        };
        if rc != 0 {
            // Drop clears `info`, which is initialised even though the
            // encode configuration was rejected.
            return Err(CodecError::init(
                "libvorbis",
                format!(
                    "vorbis_encode_init rejected {bitrate_kbps} kbps at \
                     {sample_rate} Hz / {channels}ch (code {rc})"
                ),
            ));
        }

        // Each step is recorded as it succeeds, so an early return tears
        // down exactly what exists and nothing more.
        // SAFETY: comment is a live vorbis_comment.
        unsafe { vb::vorbis_comment_init(&mut encoder.state.comment) };
        encoder.live.comment = true;

        // SAFETY: dsp is live and info was initialised and configured above.
        let rc =
            unsafe { vb::vorbis_analysis_init(&mut encoder.state.dsp, &mut encoder.state.info) };
        if rc != 0 {
            return Err(CodecError::init(
                "libvorbis",
                format!("vorbis_analysis_init failed (code {rc})"),
            ));
        }
        encoder.live.dsp = true;

        // SAFETY: block is live and dsp was initialised immediately above.
        let rc = unsafe { vb::vorbis_block_init(&mut encoder.state.dsp, &mut encoder.state.block) };
        if rc != 0 {
            return Err(CodecError::init(
                "libvorbis",
                format!("vorbis_block_init failed (code {rc})"),
            ));
        }
        encoder.live.block = true;

        Ok(encoder)
    }

    /// Open with the §7.1 capture format.
    pub fn open_default(bitrate_kbps: u32) -> Result<Self> {
        VorbisEncoder::open(SAMPLE_RATE, CHANNELS, bitrate_kbps)
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    pub fn channels(&self) -> u8 {
        self.channels
    }

    pub fn bitrate_kbps(&self) -> u32 {
        self.bitrate_kbps
    }

    /// The three header packets, for the SDP configuration (§7.2).
    pub fn headers(&mut self) -> Result<VorbisHeaders> {
        // SAFETY: zeroed is the documented starting state for ogg_packet;
        // headerout fills all three.
        let mut identification: vb::ogg_packet = unsafe { core::mem::zeroed() };
        let mut comment: vb::ogg_packet = unsafe { core::mem::zeroed() };
        let mut setup: vb::ogg_packet = unsafe { core::mem::zeroed() };

        // SAFETY: dsp and comment are live and initialised; the three packet
        // out-parameters are live locals. The buffers they come to point at
        // are owned by the dsp state and stay valid until it is cleared,
        // which outlives the copies made below.
        let rc = unsafe {
            vb::vorbis_analysis_headerout(
                &mut self.state.dsp,
                &mut self.state.comment,
                &mut identification,
                &mut comment,
                &mut setup,
            )
        };
        if rc != 0 {
            return Err(CodecError::process(
                "libvorbis",
                format!("vorbis_analysis_headerout failed (code {rc})"),
            ));
        }

        Ok(VorbisHeaders {
            identification: packet_bytes(&identification)?,
            comment: packet_bytes(&comment)?,
            setup: packet_bytes(&setup)?,
        })
    }

    /// Encode interleaved S16LE samples, returning whole Vorbis packets.
    pub fn encode(&mut self, interleaved: &[i16]) -> Result<Vec<Vec<u8>>> {
        let channels = self.channels as usize;
        if interleaved.len() % channels != 0 {
            return Err(CodecError::invalid(
                "libvorbis",
                format!(
                    "{} samples is not a whole number of {channels}-channel frames",
                    interleaved.len()
                ),
            ));
        }
        if interleaved.is_empty() {
            return Ok(Vec::new());
        }

        let frames = interleaved.len() / channels;

        // SAFETY: dsp is live; vorbis_analysis_buffer returns an array of
        // `channels` pointers, each to at least `frames` floats.
        let planes =
            unsafe { vb::vorbis_analysis_buffer(&mut self.state.dsp, frames as core::ffi::c_int) };
        if planes.is_null() {
            return Err(CodecError::process(
                "libvorbis",
                "vorbis_analysis_buffer returned no buffer",
            ));
        }

        // De-interleave into the normalised floats libvorbis wants. 32768
        // rather than 32767: it is the magnitude of i16::MIN, so the full
        // negative range maps inside [-1.0, 1.0].
        // SAFETY: planes[ch] is valid for `frames` writes, per the call
        // above. Both indices stay inside those bounds.
        unsafe {
            for channel in 0..channels {
                let plane = *planes.add(channel);
                for frame in 0..frames {
                    *plane.add(frame) =
                        f32::from(interleaved[frame * channels + channel]) / 32768.0;
                }
            }
        }

        // SAFETY: dsp is live and `frames` matches what was just written.
        let rc =
            unsafe { vb::vorbis_analysis_wrote(&mut self.state.dsp, frames as core::ffi::c_int) };
        if rc != 0 {
            return Err(CodecError::process(
                "libvorbis",
                format!("vorbis_analysis_wrote failed (code {rc})"),
            ));
        }

        self.collect()
    }

    /// Signal end of stream and drain the last packets.
    pub fn finish(&mut self) -> Result<Vec<Vec<u8>>> {
        // A zero-length write is libvorbis' end-of-stream marker.
        // SAFETY: dsp is live.
        let rc = unsafe { vb::vorbis_analysis_wrote(&mut self.state.dsp, 0) };
        if rc != 0 {
            return Err(CodecError::process(
                "libvorbis",
                format!("signalling end of stream failed (code {rc})"),
            ));
        }
        self.collect()
    }

    /// Drain every packet libvorbis has ready.
    fn collect(&mut self) -> Result<Vec<Vec<u8>>> {
        let mut packets = Vec::new();

        loop {
            // SAFETY: dsp and block are live and were initialised together.
            let available =
                unsafe { vb::vorbis_analysis_blockout(&mut self.state.dsp, &mut self.state.block) };
            if available == 0 {
                break;
            }
            if available < 0 {
                return Err(CodecError::process(
                    "libvorbis",
                    format!("vorbis_analysis_blockout failed (code {available})"),
                ));
            }

            // SAFETY: block holds a block blockout just handed us. A null
            // second argument means "no separate bitstream", which is the
            // managed-bitrate path.
            let rc = unsafe { vb::vorbis_analysis(&mut self.state.block, core::ptr::null_mut()) };
            if rc != 0 {
                return Err(CodecError::process(
                    "libvorbis",
                    format!("vorbis_analysis failed (code {rc})"),
                ));
            }
            // SAFETY: block has been analysed, which is addblock's contract.
            let rc = unsafe { vb::vorbis_bitrate_addblock(&mut self.state.block) };
            if rc != 0 {
                return Err(CodecError::process(
                    "libvorbis",
                    format!("vorbis_bitrate_addblock failed (code {rc})"),
                ));
            }

            loop {
                // SAFETY: zeroed is the documented starting state; dsp is
                // live and flushpacket fills the packet when it returns 1.
                let mut packet: vb::ogg_packet = unsafe { core::mem::zeroed() };
                let ready =
                    unsafe { vb::vorbis_bitrate_flushpacket(&mut self.state.dsp, &mut packet) };
                if ready == 0 {
                    break;
                }
                if ready < 0 {
                    return Err(CodecError::process(
                        "libvorbis",
                        format!("vorbis_bitrate_flushpacket failed (code {ready})"),
                    ));
                }
                packets.push(packet_bytes(&packet)?);
            }
        }

        Ok(packets)
    }
}

/// Copy an ogg_packet's body. The buffer belongs to libvorbis and is reused
/// on the next call, so nothing may hold a borrow of it.
fn packet_bytes(packet: &vb::ogg_packet) -> Result<Vec<u8>> {
    if packet.packet.is_null() || packet.bytes <= 0 {
        return Err(CodecError::process(
            "libvorbis",
            format!(
                "libvorbis produced an empty packet ({} bytes)",
                packet.bytes
            ),
        ));
    }
    // SAFETY: libvorbis guarantees `bytes` initialised bytes at `packet`
    // until the next call on the same dsp state.
    Ok(unsafe { core::slice::from_raw_parts(packet.packet, packet.bytes as usize) }.to_vec())
}

impl Drop for VorbisEncoder {
    fn drop(&mut self) {
        // Teardown order is libvorbis': block, then dsp, then comment, then
        // info. Clearing info first would pull the configuration out from
        // under the dsp state.
        //
        // SAFETY: each struct is cleared only if `open` recorded it as
        // initialised, and this is the only owner, so none has been cleared
        // already.
        unsafe {
            if self.live.block {
                vb::vorbis_block_clear(&mut self.state.block);
            }
            if self.live.dsp {
                vb::vorbis_dsp_clear(&mut self.state.dsp);
            }
            if self.live.comment {
                vb::vorbis_comment_clear(&mut self.state.comment);
            }
            if self.live.info {
                vb::vorbis_info_clear(&mut self.state.info);
            }
        }
        self.live = Live::default();
    }
}

/// Vorbis decode for the client, through libavcodec.
///
/// libavcodec wants the three header packets as `extradata` in Xiph's packed
/// form before it will decode anything, which is the same shape RFC 5215
/// carries in the SDP — so the client hands over exactly what it received.
pub struct VorbisDecoder {
    av: AvAudio,
    channels: u8,
    sample_rate: u32,
}

// SAFETY: every pointer is owned exclusively by this handle, and libavcodec
// permits one thread at a time per context.
unsafe impl Send for VorbisDecoder {}

impl VorbisDecoder {
    /// Open a decoder from the three header packets.
    pub fn open(headers: &VorbisHeaders, sample_rate: u32, channels: u8) -> Result<Self> {
        crate::logging::install();

        // SAFETY: the identifier is valid; the call looks up a table.
        let codec = unsafe { ff::avcodec_find_decoder(ff::AVCodecID_AV_CODEC_ID_VORBIS) };
        if codec.is_null() {
            return Err(CodecError::unavailable(
                "vorbis decode",
                "this libavcodec was built without a Vorbis decoder",
            ));
        }

        let mut decoder = VorbisDecoder {
            av: AvAudio::new(),
            channels,
            sample_rate,
        };

        // SAFETY: codec is non-null.
        decoder.av.ctx = unsafe { ff::avcodec_alloc_context3(codec) };
        if decoder.av.ctx.is_null() {
            return Err(CodecError::init(
                "vorbis decode",
                "avcodec_alloc_context3 failed",
            ));
        }

        let extradata = headers.to_extradata();
        // SAFETY: av_malloc returns a buffer libavcodec will free with the
        // context. The padding is what avcodec requires past the payload so
        // its bitreader can over-read safely.
        unsafe {
            let size = extradata.len();
            let padded = size + ff::AV_INPUT_BUFFER_PADDING_SIZE as usize;
            let buffer = ff::av_mallocz(padded) as *mut u8;
            if buffer.is_null() {
                return Err(CodecError::init(
                    "vorbis decode",
                    "allocating the decoder extradata failed",
                ));
            }
            core::ptr::copy_nonoverlapping(extradata.as_ptr(), buffer, size);
            (*decoder.av.ctx).extradata = buffer;
            (*decoder.av.ctx).extradata_size = size as i32;
            (*decoder.av.ctx).sample_rate = sample_rate as i32;
            set_channel_layout(&mut (*decoder.av.ctx).ch_layout, channels);
        }

        // SAFETY: ctx is configured and codec matches it.
        let rc = unsafe { ff::avcodec_open2(decoder.av.ctx, codec, core::ptr::null_mut()) };
        if rc < 0 {
            return Err(CodecError::init(
                "vorbis decode",
                format!(
                    "avcodec_open2: {}. The three header packets may not match the stream.",
                    av_error(rc)
                ),
            ));
        }
        decoder.av.alloc_objects("vorbis decode")?;
        Ok(decoder)
    }

    pub fn channels(&self) -> u8 {
        self.channels
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }

    /// Decode one Vorbis packet to interleaved S16LE samples.
    pub fn decode(&mut self, packet: &[u8]) -> Result<Vec<i16>> {
        if packet.is_empty() {
            return Ok(Vec::new());
        }
        // SAFETY: pointing the packet at the caller's buffer is sound
        // because send_packet consumes it during the call.
        unsafe {
            (*self.av.packet).data = packet.as_ptr() as *mut u8;
            (*self.av.packet).size = packet.len() as i32;
        }
        // SAFETY: the context is open and the packet is populated.
        let rc = unsafe { ff::avcodec_send_packet(self.av.ctx, self.av.packet) };
        // SAFETY: clear the borrowed pointer before it outlives the slice.
        unsafe {
            (*self.av.packet).data = core::ptr::null_mut();
            (*self.av.packet).size = 0;
        }
        if rc < 0 && rc != averror_eagain() {
            return Err(CodecError::process(
                "vorbis decode",
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
                    "vorbis decode",
                    format!("avcodec_receive_frame: {}", av_error(rc)),
                ));
            }
            // SAFETY: a successful receive leaves the frame populated.
            unsafe { super::append_samples(self.av.frame, &mut out, "vorbis decode")? };
            // SAFETY: frame is live; unref readies it for the next receive.
            unsafe { ff::av_frame_unref(self.av.frame) };
        }
    }
}
