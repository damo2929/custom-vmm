//! virtio-gpu, 2D only (virtio 1.x §5.7).
//!
//! This is the guest's display. The driver creates a *host-private*
//! resource, attaches guest pages to back it, points a scanout at it, and
//! then per frame transfers the dirty rectangle into the resource and
//! flushes it. §5.7.6 is explicit that resources live on the host and that
//! unaccelerated 2D transfers only ever go guest → host, which is why the
//! VMM ends up owning a tightly-packed BGRX buffer that an encoder can read
//! directly.
//!
//! Only the eight commands a real driver sends are implemented. That is not
//! a guess: edk2's `OvmfPkg/VirtioGpuDxe` — a complete, shipping driver —
//! implements exactly these eight and nothing else.
//!
//! Deliberately absent:
//!
//! * **Every feature bit.** `VIRTIO_GPU_F_VIRGL`, `_EDID`, `_RESOURCE_UUID`,
//!   `_RESOURCE_BLOB` and `_CONTEXT_INIT` are all optional and all declined.
//!   With `num_capsets = 0` the driver never asks for a capset, and without
//!   `_EDID` it takes the mode from `GET_DISPLAY_INFO` instead.
//! * **3D.** Nothing here needs virglrenderer, and adopting it would couple
//!   a hypervisor to a render server.
//!
//! The cursor queue exists because §5.7.2 declares it unconditionally, but
//! its commands are answered `OK_NODATA` and discarded — the same thing
//! QEMU does, which pushes a zero-length response. It still has to be
//! *drained*, or a 16-entry ring fills and the guest blocks.

use std::collections::BTreeMap;

use libvmm_core::VmmResult;

use crate::queue::{DescriptorChain, GuestMemory};

/// Control queue.
pub const CONTROL_QUEUE: u16 = 0;
/// Cursor queue.
pub const CURSOR_QUEUE: u16 = 1;
pub const NUM_QUEUES: u16 = 2;

/// QEMU's 2D queue sizes, which every guest has been tested against.
pub const CONTROL_QUEUE_SIZE: u16 = 64;
pub const CURSOR_QUEUE_SIZE: u16 = 16;

/// `virtio_gpu_config` is four little-endian u32s.
pub const CONFIG_LEN: u32 = 16;

/// The fixed-size scanout array in a display-info response.
const MAX_SCANOUTS: usize = 16;

/// `virtio_gpu_ctrl_hdr`.
const HDR_LEN: usize = 24;

mod cmd {
    pub const GET_DISPLAY_INFO: u32 = 0x0100;
    pub const RESOURCE_CREATE_2D: u32 = 0x0101;
    pub const RESOURCE_UNREF: u32 = 0x0102;
    pub const SET_SCANOUT: u32 = 0x0103;
    pub const RESOURCE_FLUSH: u32 = 0x0104;
    pub const TRANSFER_TO_HOST_2D: u32 = 0x0105;
    pub const RESOURCE_ATTACH_BACKING: u32 = 0x0106;
    pub const RESOURCE_DETACH_BACKING: u32 = 0x0107;
    pub const UPDATE_CURSOR: u32 = 0x0300;
    pub const MOVE_CURSOR: u32 = 0x0301;
}

mod resp {
    pub const OK_NODATA: u32 = 0x1100;
    pub const OK_DISPLAY_INFO: u32 = 0x1101;
    pub const ERR_UNSPEC: u32 = 0x1200;
    pub const ERR_INVALID_RESOURCE_ID: u32 = 0x1202;
    pub const ERR_INVALID_PARAMETER: u32 = 0x1204;
}

/// The only scanout format Linux's primary plane asks for.
///
/// `DRM_FORMAT_HOST_XRGB8888` maps to `B8G8R8X8_UNORM`, whose bytes in
/// memory are B, G, R, then padding. `B8G8R8A8_UNORM` (1) is the *cursor*
/// format, not the scanout one.
pub const FORMAT_B8G8R8X8: u32 = 2;
pub const FORMAT_B8G8R8A8: u32 = 1;

/// A rectangle, as the protocol carries it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// One guest page range backing a resource.
#[derive(Debug, Clone, Copy)]
struct MemEntry {
    addr: u64,
    length: u32,
}

/// A host-private 2D resource.
struct Resource {
    width: u32,
    height: u32,
    /// Tightly packed, `width * 4` bytes per row.
    pixels: Vec<u8>,
    backing: Vec<MemEntry>,
}

impl Resource {
    fn stride(&self) -> usize {
        self.width as usize * 4
    }
}

/// A frame the guest has asked to be shown.
#[derive(Debug, Clone)]
pub struct Scanout {
    pub width: u32,
    pub height: u32,
    /// BGRX, `width * 4` bytes per row.
    pub pixels: Vec<u8>,
    /// What the guest said changed, merged since the last frame was taken.
    pub damage: Rect,
}

/// virtio-gpu, 2D.
pub struct VirtioGpu {
    /// The mode reported by `GET_DISPLAY_INFO`.
    pub width: u32,
    pub height: u32,
    resources: BTreeMap<u32, Resource>,
    /// Which resource scanout 0 is showing.
    scanout_resource: Option<u32>,
    /// Set by `RESOURCE_FLUSH`, taken by the capture thread.
    pending: Option<Scanout>,
    /// Damage accumulated since the last frame was taken.
    damage: Option<Rect>,
    pub frames: u64,
}

impl VirtioGpu {
    pub fn new(width: u32, height: u32) -> Self {
        VirtioGpu {
            width,
            height,
            resources: BTreeMap::new(),
            scanout_resource: None,
            pending: None,
            damage: None,
            frames: 0,
        }
    }

    /// `virtio_gpu_config`: no events, one scanout, no capsets.
    pub fn config(&self) -> [u8; CONFIG_LEN as usize] {
        let mut c = [0u8; CONFIG_LEN as usize];
        // events_read and events_clear stay zero: with a fixed mode we never
        // raise VIRTIO_GPU_EVENT_DISPLAY, so the whole event mechanism is
        // inert.
        c[8..12].copy_from_slice(&1u32.to_le_bytes()); // num_scanouts
        c[12..16].copy_from_slice(&0u32.to_le_bytes()); // num_capsets
        c
    }

    /// Take the frame the guest last flushed, if there is one.
    ///
    /// Returning `None` when nothing was flushed is the whole point: an idle
    /// guest sends no commands at all, because the driver's
    /// `drm_atomic_helper_damage_merged` returns early when nothing changed.
    /// The absence of a frame *is* the "nothing happened" signal, and it
    /// costs neither a dirty-log ioctl nor a scan of the framebuffer.
    pub fn take_frame(&mut self) -> Option<Scanout> {
        self.pending.take()
    }

    /// Service one chain from the control queue.
    ///
    /// Returns the number of bytes written into the guest's writable
    /// descriptors, which is what the used ring must report.
    pub fn handle_control<M: GuestMemory>(
        &mut self,
        mem: &M,
        chain: &DescriptorChain,
    ) -> VmmResult<u32> {
        let request = read_readable(mem, chain)?;
        if request.len() < HDR_LEN {
            return self.respond(mem, chain, resp::ERR_UNSPEC, &[]);
        }
        let command = u32_at(&request, 0);
        let body = &request[HDR_LEN..];

        match command {
            cmd::GET_DISPLAY_INFO => {
                let info = self.display_info();
                self.respond(mem, chain, resp::OK_DISPLAY_INFO, &info)
            }
            cmd::RESOURCE_CREATE_2D => {
                let code = self.resource_create_2d(body);
                self.respond(mem, chain, code, &[])
            }
            cmd::RESOURCE_UNREF => {
                let id = u32_at(body, 0);
                let code = if self.resources.remove(&id).is_some() {
                    if self.scanout_resource == Some(id) {
                        self.scanout_resource = None;
                    }
                    resp::OK_NODATA
                } else {
                    resp::ERR_INVALID_RESOURCE_ID
                };
                self.respond(mem, chain, code, &[])
            }
            cmd::SET_SCANOUT => {
                let code = self.set_scanout(body);
                self.respond(mem, chain, code, &[])
            }
            cmd::RESOURCE_FLUSH => {
                let code = self.resource_flush(body);
                self.respond(mem, chain, code, &[])
            }
            cmd::TRANSFER_TO_HOST_2D => {
                let code = self.transfer_to_host_2d(mem, body);
                self.respond(mem, chain, code, &[])
            }
            cmd::RESOURCE_ATTACH_BACKING => {
                let code = self.attach_backing(body);
                self.respond(mem, chain, code, &[])
            }
            cmd::RESOURCE_DETACH_BACKING => {
                let id = u32_at(body, 0);
                let code = match self.resources.get_mut(&id) {
                    Some(r) => {
                        r.backing.clear();
                        resp::OK_NODATA
                    }
                    None => resp::ERR_INVALID_RESOURCE_ID,
                };
                self.respond(mem, chain, code, &[])
            }
            other => {
                // Anything else is a feature we declined. Saying so is
                // better than silence: a driver that gets no response at all
                // waits five seconds and then reports a timeout that names
                // nothing.
                log::debug!("virtio-gpu: unsupported command {other:#06x}");
                self.respond(mem, chain, resp::ERR_UNSPEC, &[])
            }
        }
    }

    /// Service one chain from the cursor queue.
    ///
    /// The cursor plane is not implemented. The queue must still be drained
    /// and each chain completed, or the guest's 16-entry ring fills and it
    /// blocks waiting for a buffer it will never get back.
    pub fn handle_cursor<M: GuestMemory>(
        &mut self,
        mem: &M,
        chain: &DescriptorChain,
    ) -> VmmResult<u32> {
        let request = read_readable(mem, chain)?;
        if request.len() >= HDR_LEN {
            let command = u32_at(&request, 0);
            if command != cmd::UPDATE_CURSOR && command != cmd::MOVE_CURSOR {
                log::debug!("virtio-gpu: unexpected cursor command {command:#06x}");
            }
        }
        // QEMU pushes a zero-length response here; there is no payload for a
        // cursor command.
        Ok(0)
    }

    fn display_info(&self) -> Vec<u8> {
        // The response is always the full 16 entries, not `num_scanouts`
        // of them: the struct has a fixed-size array.
        let mut out = vec![0u8; MAX_SCANOUTS * 24];
        out[0..4].copy_from_slice(&0u32.to_le_bytes()); // x
        out[4..8].copy_from_slice(&0u32.to_le_bytes()); // y
        out[8..12].copy_from_slice(&self.width.to_le_bytes());
        out[12..16].copy_from_slice(&self.height.to_le_bytes());
        out[16..20].copy_from_slice(&1u32.to_le_bytes()); // enabled
        out[20..24].copy_from_slice(&0u32.to_le_bytes()); // flags
        out
    }

    fn resource_create_2d(&mut self, body: &[u8]) -> u32 {
        if body.len() < 16 {
            return resp::ERR_INVALID_PARAMETER;
        }
        let id = u32_at(body, 0);
        let format = u32_at(body, 4);
        let width = u32_at(body, 8);
        let height = u32_at(body, 12);

        // Resource 0 is reserved: the protocol uses it to mean "no
        // resource", which is how SET_SCANOUT disables a head.
        if id == 0 {
            return resp::ERR_INVALID_RESOURCE_ID;
        }
        if format != FORMAT_B8G8R8X8 && format != FORMAT_B8G8R8A8 {
            log::debug!("virtio-gpu: refusing format {format}");
            return resp::ERR_INVALID_PARAMETER;
        }
        // A guest-supplied size is hostile input. 8K is far beyond anything
        // a console needs, and the multiplication below must not overflow.
        if width == 0 || height == 0 || width > 8192 || height > 8192 {
            return resp::ERR_INVALID_PARAMETER;
        }
        let bytes = width as usize * height as usize * 4;
        self.resources.insert(
            id,
            Resource {
                width,
                height,
                pixels: vec![0u8; bytes],
                backing: Vec::new(),
            },
        );
        resp::OK_NODATA
    }

    fn attach_backing(&mut self, body: &[u8]) -> u32 {
        if body.len() < 8 {
            return resp::ERR_INVALID_PARAMETER;
        }
        let id = u32_at(body, 0);
        let count = u32_at(body, 4) as usize;
        // Each entry is 16 bytes and they follow in the same chain.
        if body.len() < 8 + count * 16 {
            return resp::ERR_INVALID_PARAMETER;
        }
        let Some(resource) = self.resources.get_mut(&id) else {
            return resp::ERR_INVALID_RESOURCE_ID;
        };
        resource.backing.clear();
        for i in 0..count {
            let at = 8 + i * 16;
            resource.backing.push(MemEntry {
                addr: u64_at(body, at),
                length: u32_at(body, at + 8),
            });
        }
        resp::OK_NODATA
    }

    fn set_scanout(&mut self, body: &[u8]) -> u32 {
        if body.len() < 24 {
            return resp::ERR_INVALID_PARAMETER;
        }
        let scanout_id = u32_at(body, 16);
        let resource_id = u32_at(body, 20);
        if scanout_id != 0 {
            return resp::ERR_INVALID_PARAMETER;
        }
        if resource_id == 0 {
            // Disabling the head is legal and is how the driver tears down.
            self.scanout_resource = None;
            return resp::OK_NODATA;
        }
        if !self.resources.contains_key(&resource_id) {
            return resp::ERR_INVALID_RESOURCE_ID;
        }
        self.scanout_resource = Some(resource_id);
        resp::OK_NODATA
    }

    fn transfer_to_host_2d<M: GuestMemory>(&mut self, mem: &M, body: &[u8]) -> u32 {
        if body.len() < 32 {
            return resp::ERR_INVALID_PARAMETER;
        }
        let rect = Rect {
            x: u32_at(body, 0),
            y: u32_at(body, 4),
            width: u32_at(body, 8),
            height: u32_at(body, 12),
        };
        let offset = u64_at(body, 16);
        let id = u32_at(body, 24);

        let Some(resource) = self.resources.get_mut(&id) else {
            return resp::ERR_INVALID_RESOURCE_ID;
        };
        // Clamp to the resource. The guest chose these numbers and they must
        // not be allowed to index outside the buffer we allocated.
        if rect.x > resource.width
            || rect.y > resource.height
            || rect.width > resource.width - rect.x
            || rect.height > resource.height - rect.y
        {
            return resp::ERR_INVALID_PARAMETER;
        }

        let stride = resource.stride();
        let row_bytes = rect.width as usize * 4;
        let mut row = vec![0u8; row_bytes];

        for line in 0..rect.height as usize {
            // Source and destination strides are both `width * 4`: the
            // guest's backing store for a 2D resource is tightly packed.
            let src = offset as usize + (line * stride) + rect.x as usize * 4;
            let dst = (rect.y as usize + line) * stride + rect.x as usize * 4;
            if dst + row_bytes > resource.pixels.len() {
                break;
            }
            if read_backing(mem, &resource.backing, src, &mut row).is_err() {
                return resp::ERR_UNSPEC;
            }
            resource.pixels[dst..dst + row_bytes].copy_from_slice(&row);
        }
        resp::OK_NODATA
    }

    fn resource_flush(&mut self, body: &[u8]) -> u32 {
        if body.len() < 20 {
            return resp::ERR_INVALID_PARAMETER;
        }
        let rect = Rect {
            x: u32_at(body, 0),
            y: u32_at(body, 4),
            width: u32_at(body, 8),
            height: u32_at(body, 12),
        };
        let id = u32_at(body, 16);

        if self.scanout_resource != Some(id) {
            // Flushing a resource that is not on screen is legal — it just
            // has nothing to show.
            return if self.resources.contains_key(&id) {
                resp::OK_NODATA
            } else {
                resp::ERR_INVALID_RESOURCE_ID
            };
        }
        let Some(resource) = self.resources.get(&id) else {
            return resp::ERR_INVALID_RESOURCE_ID;
        };

        self.damage = Some(match self.damage {
            None => rect,
            Some(d) => merge(d, rect),
        });
        let damage = self.damage.unwrap_or(rect);

        // Replace rather than queue. A capture thread that fell behind
        // wants the newest frame, not a backlog of stale ones.
        self.pending = Some(Scanout {
            width: resource.width,
            height: resource.height,
            pixels: resource.pixels.clone(),
            damage,
        });
        self.damage = None;
        self.frames += 1;
        resp::OK_NODATA
    }

    /// Write a response header plus `payload` into the chain's writable
    /// descriptors.
    fn respond<M: GuestMemory>(
        &self,
        mem: &M,
        chain: &DescriptorChain,
        code: u32,
        payload: &[u8],
    ) -> VmmResult<u32> {
        let mut out = Vec::with_capacity(HDR_LEN + payload.len());
        out.extend_from_slice(&code.to_le_bytes());
        out.extend_from_slice(&0u32.to_le_bytes()); // flags
        out.extend_from_slice(&0u64.to_le_bytes()); // fence_id
        out.extend_from_slice(&0u32.to_le_bytes()); // ctx_id
        out.extend_from_slice(&0u32.to_le_bytes()); // ring_idx + padding
        out.extend_from_slice(payload);

        let mut written = 0usize;
        for d in chain.writable() {
            if written >= out.len() {
                break;
            }
            let take = ((d.len as usize).min(out.len() - written)).min(out.len());
            mem.write(d.addr, &out[written..written + take])?;
            written += take;
        }
        Ok(written as u32)
    }
}

/// Merge two rectangles into one that covers both.
fn merge(a: Rect, b: Rect) -> Rect {
    let x = a.x.min(b.x);
    let y = a.y.min(b.y);
    let right = (a.x + a.width).max(b.x + b.width);
    let bottom = (a.y + a.height).max(b.y + b.height);
    Rect {
        x,
        y,
        width: right - x,
        height: bottom - y,
    }
}

/// Concatenate every readable descriptor into one buffer.
fn read_readable<M: GuestMemory>(mem: &M, chain: &DescriptorChain) -> VmmResult<Vec<u8>> {
    let mut out = Vec::new();
    for d in chain.readable() {
        let mut buf = vec![0u8; d.len as usize];
        mem.read(d.addr, &mut buf)?;
        out.extend_from_slice(&buf);
    }
    Ok(out)
}

/// Read `out.len()` bytes starting `offset` into a resource's backing list.
///
/// The backing is a scatter-gather list of guest pages, so a single row can
/// straddle any number of entries.
fn read_backing<M: GuestMemory>(
    mem: &M,
    backing: &[MemEntry],
    offset: usize,
    out: &mut [u8],
) -> VmmResult<()> {
    let mut remaining = out.len();
    let mut cursor = 0usize;
    let mut skip = offset;

    for entry in backing {
        let len = entry.length as usize;
        if skip >= len {
            skip -= len;
            continue;
        }
        let available = len - skip;
        let take = available.min(remaining);
        mem.read(entry.addr + skip as u64, &mut out[cursor..cursor + take])?;
        cursor += take;
        remaining -= take;
        skip = 0;
        if remaining == 0 {
            return Ok(());
        }
    }
    // A short backing list is the guest's error; zero the rest rather than
    // leaving whatever was in the row buffer from the previous line.
    out[cursor..].fill(0);
    Ok(())
}

fn u32_at(b: &[u8], at: usize) -> u32 {
    b.get(at..at + 4)
        .and_then(|s| s.try_into().ok())
        .map(u32::from_le_bytes)
        .unwrap_or(0)
}

fn u64_at(b: &[u8], at: usize) -> u64 {
    b.get(at..at + 8)
        .and_then(|s| s.try_into().ok())
        .map(u64::from_le_bytes)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::queue::{Descriptor, GuestMemory};
    use std::cell::RefCell;

    /// Guest memory as a flat buffer, so a command can be assembled at a
    /// known address and the response read back.
    struct FakeMem {
        bytes: RefCell<Vec<u8>>,
    }

    impl FakeMem {
        fn new() -> Self {
            FakeMem {
                bytes: RefCell::new(vec![0u8; 1 << 20]),
            }
        }
        fn put(&self, at: u64, data: &[u8]) {
            self.bytes.borrow_mut()[at as usize..at as usize + data.len()].copy_from_slice(data);
        }
        fn get(&self, at: u64, len: usize) -> Vec<u8> {
            self.bytes.borrow()[at as usize..at as usize + len].to_vec()
        }
    }

    impl GuestMemory for FakeMem {
        fn read(&self, gpa: u64, out: &mut [u8]) -> VmmResult<()> {
            out.copy_from_slice(&self.bytes.borrow()[gpa as usize..gpa as usize + out.len()]);
            Ok(())
        }
        fn write(&self, gpa: u64, data: &[u8]) -> VmmResult<()> {
            self.put(gpa, data);
            Ok(())
        }
        fn is_valid_range(&self, gpa: u64, len: u64) -> bool {
            (gpa + len) as usize <= self.bytes.borrow().len()
        }
    }

    /// A chain of one readable request and one writable response.
    fn chain(req: u64, req_len: u32, resp: u64, resp_len: u32) -> DescriptorChain {
        let mut c = DescriptorChain::new();
        c.push(
            0,
            Descriptor {
                addr: req,
                len: req_len,
                writable: false,
            },
        )
        .expect("push");
        c.push(
            0,
            Descriptor {
                addr: resp,
                len: resp_len,
                writable: true,
            },
        )
        .expect("push");
        c
    }

    fn header(command: u32) -> Vec<u8> {
        let mut h = vec![0u8; HDR_LEN];
        h[0..4].copy_from_slice(&command.to_le_bytes());
        h
    }

    #[test]
    fn the_config_advertises_one_scanout_and_no_capsets() {
        let gpu = VirtioGpu::new(1920, 1080);
        let c = gpu.config();
        assert_eq!(c.len(), 16);
        assert_eq!(
            u32::from_le_bytes([c[8], c[9], c[10], c[11]]),
            1,
            "scanouts"
        );
        // A non-zero capset count makes the driver ask for capsets we do
        // not implement.
        assert_eq!(u32::from_le_bytes([c[12], c[13], c[14], c[15]]), 0);
    }

    #[test]
    fn display_info_reports_the_mode_and_is_always_sixteen_entries() {
        let mut gpu = VirtioGpu::new(1920, 1080);
        let mem = FakeMem::new();
        mem.put(0x1000, &header(cmd::GET_DISPLAY_INFO));
        let c = chain(0x1000, HDR_LEN as u32, 0x2000, 512);
        let written = gpu.handle_control(&mem, &c).expect("display info");

        // 24-byte header plus a fixed 16-entry array, whatever num_scanouts
        // says. Sizing the response by num_scanouts would short the driver.
        assert_eq!(written as usize, HDR_LEN + MAX_SCANOUTS * 24);
        let r = mem.get(0x2000, written as usize);
        assert_eq!(
            u32::from_le_bytes([r[0], r[1], r[2], r[3]]),
            resp::OK_DISPLAY_INFO
        );
        let first = &r[HDR_LEN..];
        assert_eq!(
            u32::from_le_bytes([first[8], first[9], first[10], first[11]]),
            1920
        );
        assert_eq!(
            u32::from_le_bytes([first[12], first[13], first[14], first[15]]),
            1080
        );
        assert_eq!(
            u32::from_le_bytes([first[16], first[17], first[18], first[19]]),
            1,
            "enabled"
        );
    }

    fn create_2d(gpu: &mut VirtioGpu, mem: &FakeMem, id: u32, format: u32, w: u32, h: u32) -> u32 {
        let mut req = header(cmd::RESOURCE_CREATE_2D);
        req.extend_from_slice(&id.to_le_bytes());
        req.extend_from_slice(&format.to_le_bytes());
        req.extend_from_slice(&w.to_le_bytes());
        req.extend_from_slice(&h.to_le_bytes());
        mem.put(0x1000, &req);
        let c = chain(0x1000, req.len() as u32, 0x2000, 64);
        gpu.handle_control(mem, &c).expect("create");
        let r = mem.get(0x2000, 4);
        u32::from_le_bytes([r[0], r[1], r[2], r[3]])
    }

    #[test]
    fn a_resource_is_created_for_the_scanout_format() {
        let mut gpu = VirtioGpu::new(64, 32);
        let mem = FakeMem::new();
        assert_eq!(
            create_2d(&mut gpu, &mem, 1, FORMAT_B8G8R8X8, 64, 32),
            resp::OK_NODATA
        );
    }

    #[test]
    fn hostile_resource_parameters_are_refused() {
        let mut gpu = VirtioGpu::new(64, 32);
        let mem = FakeMem::new();
        // Resource 0 is the protocol's "no resource" sentinel.
        assert_eq!(
            create_2d(&mut gpu, &mem, 0, FORMAT_B8G8R8X8, 64, 32),
            resp::ERR_INVALID_RESOURCE_ID
        );
        // A size chosen to overflow `w * h * 4`.
        assert_eq!(
            create_2d(&mut gpu, &mem, 1, FORMAT_B8G8R8X8, u32::MAX, u32::MAX),
            resp::ERR_INVALID_PARAMETER
        );
        assert_eq!(
            create_2d(&mut gpu, &mem, 1, FORMAT_B8G8R8X8, 0, 32),
            resp::ERR_INVALID_PARAMETER
        );
        // A format we do not implement.
        assert_eq!(
            create_2d(&mut gpu, &mem, 1, 99, 64, 32),
            resp::ERR_INVALID_PARAMETER
        );
    }

    /// Create, back, scan out, transfer and flush — and check the pixels.
    #[test]
    fn a_flushed_frame_carries_the_pixels_the_guest_transferred() {
        const W: u32 = 8;
        const H: u32 = 4;
        let mut gpu = VirtioGpu::new(W, H);
        let mem = FakeMem::new();

        assert_eq!(
            create_2d(&mut gpu, &mem, 1, FORMAT_B8G8R8X8, W, H),
            resp::OK_NODATA
        );

        // Backing: one entry covering the whole resource, at 0x8000.
        let bytes = (W * H * 4) as usize;
        let mut req = header(cmd::RESOURCE_ATTACH_BACKING);
        req.extend_from_slice(&1u32.to_le_bytes()); // resource id
        req.extend_from_slice(&1u32.to_le_bytes()); // nr_entries
        req.extend_from_slice(&0x8000u64.to_le_bytes());
        req.extend_from_slice(&(bytes as u32).to_le_bytes());
        req.extend_from_slice(&0u32.to_le_bytes()); // padding
        mem.put(0x1000, &req);
        gpu.handle_control(&mem, &chain(0x1000, req.len() as u32, 0x2000, 64))
            .expect("attach");

        // The guest's pixels: a recognisable ramp.
        let pixels: Vec<u8> = (0..bytes).map(|i| (i % 251) as u8).collect();
        mem.put(0x8000, &pixels);

        // Point scanout 0 at it.
        let mut req = header(cmd::SET_SCANOUT);
        req.extend_from_slice(&[0u8; 16]); // rect
        req.extend_from_slice(&0u32.to_le_bytes()); // scanout id
        req.extend_from_slice(&1u32.to_le_bytes()); // resource id
        mem.put(0x1000, &req);
        gpu.handle_control(&mem, &chain(0x1000, req.len() as u32, 0x2000, 64))
            .expect("set scanout");

        // Transfer the whole thing, then flush it.
        let mut req = header(cmd::TRANSFER_TO_HOST_2D);
        req.extend_from_slice(&0u32.to_le_bytes()); // x
        req.extend_from_slice(&0u32.to_le_bytes()); // y
        req.extend_from_slice(&W.to_le_bytes());
        req.extend_from_slice(&H.to_le_bytes());
        req.extend_from_slice(&0u64.to_le_bytes()); // offset
        req.extend_from_slice(&1u32.to_le_bytes()); // resource id
        req.extend_from_slice(&0u32.to_le_bytes()); // padding
        mem.put(0x1000, &req);
        gpu.handle_control(&mem, &chain(0x1000, req.len() as u32, 0x2000, 64))
            .expect("transfer");

        assert!(
            gpu.take_frame().is_none(),
            "a transfer alone is not a frame"
        );

        let mut req = header(cmd::RESOURCE_FLUSH);
        req.extend_from_slice(&0u32.to_le_bytes());
        req.extend_from_slice(&0u32.to_le_bytes());
        req.extend_from_slice(&W.to_le_bytes());
        req.extend_from_slice(&H.to_le_bytes());
        req.extend_from_slice(&1u32.to_le_bytes()); // resource id
        mem.put(0x1000, &req);
        gpu.handle_control(&mem, &chain(0x1000, req.len() as u32, 0x2000, 64))
            .expect("flush");

        let frame = gpu.take_frame().expect("the flush must produce a frame");
        assert_eq!((frame.width, frame.height), (W, H));
        assert_eq!(frame.pixels, pixels, "the guest's pixels, unmodified");
        assert_eq!(
            frame.damage,
            Rect {
                x: 0,
                y: 0,
                width: W,
                height: H
            }
        );
        assert!(gpu.take_frame().is_none(), "a frame is taken once");
    }

    #[test]
    fn transferring_outside_the_resource_is_refused() {
        let mut gpu = VirtioGpu::new(8, 4);
        let mem = FakeMem::new();
        create_2d(&mut gpu, &mem, 1, FORMAT_B8G8R8X8, 8, 4);

        let mut req = header(cmd::TRANSFER_TO_HOST_2D);
        req.extend_from_slice(&4u32.to_le_bytes()); // x
        req.extend_from_slice(&0u32.to_le_bytes()); // y
        req.extend_from_slice(&8u32.to_le_bytes()); // width — runs past the edge
        req.extend_from_slice(&4u32.to_le_bytes());
        req.extend_from_slice(&0u64.to_le_bytes());
        req.extend_from_slice(&1u32.to_le_bytes());
        req.extend_from_slice(&0u32.to_le_bytes());
        mem.put(0x1000, &req);
        gpu.handle_control(&mem, &chain(0x1000, req.len() as u32, 0x2000, 64))
            .expect("transfer");
        let r = mem.get(0x2000, 4);
        assert_eq!(
            u32::from_le_bytes([r[0], r[1], r[2], r[3]]),
            resp::ERR_INVALID_PARAMETER
        );
    }

    #[test]
    fn a_command_for_a_resource_that_does_not_exist_says_so() {
        let mut gpu = VirtioGpu::new(8, 4);
        let mem = FakeMem::new();
        let mut req = header(cmd::RESOURCE_UNREF);
        req.extend_from_slice(&7u32.to_le_bytes());
        mem.put(0x1000, &req);
        gpu.handle_control(&mem, &chain(0x1000, req.len() as u32, 0x2000, 64))
            .expect("unref");
        let r = mem.get(0x2000, 4);
        assert_eq!(
            u32::from_le_bytes([r[0], r[1], r[2], r[3]]),
            resp::ERR_INVALID_RESOURCE_ID
        );
    }

    #[test]
    fn a_cursor_command_is_completed_with_no_payload() {
        let mut gpu = VirtioGpu::new(8, 4);
        let mem = FakeMem::new();
        mem.put(0x1000, &header(cmd::UPDATE_CURSOR));
        let c = chain(0x1000, HDR_LEN as u32, 0x2000, 64);
        // The queue must be drained or the guest's ring fills, but there is
        // nothing to say back.
        assert_eq!(gpu.handle_cursor(&mem, &c).expect("cursor"), 0);
    }
}
