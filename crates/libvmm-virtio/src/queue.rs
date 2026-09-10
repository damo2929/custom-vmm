//! Virtqueue engine — split and packed rings.
//!
//! §2.3 requires `VIRTIO_F_RING_PACKED` to be offered, with split rings
//! accepted as a fallback, so both layouts are implemented here behind one
//! [`Virtqueue`] interface.
//!
//! Datapath rule (§0.1): nothing in the hot path allocates or panics.
//! Descriptor walking writes into a caller-provided [`DescriptorChain`]
//! buffer, and every fault is returned as `Virtio(BadDescriptor)`.

use crate::features::RingLayout;
use libvmm_core::{VirtioError, VmmResult};

/// Maximum descriptors we will follow in one chain. A malicious or buggy
/// driver can build a descriptor loop; this bounds the walk so a queue worker
/// cannot be spun forever.
pub const MAX_CHAIN_LEN: usize = 1024;

/// Largest ring the device will accept.
pub const MAX_QUEUE_SIZE: u16 = 1024;

// Split-ring descriptor flags.
pub const VIRTQ_DESC_F_NEXT: u16 = 1;
pub const VIRTQ_DESC_F_WRITE: u16 = 2;
pub const VIRTQ_DESC_F_INDIRECT: u16 = 4;

// Packed-ring descriptor flags.
pub const VIRTQ_DESC_F_AVAIL: u16 = 1 << 7;
pub const VIRTQ_DESC_F_USED: u16 = 1 << 15;

/// Read/write access to guest RAM, so the queue engine can be exercised
/// against a plain byte vector in tests and against the KVM mappings in
/// production.
pub trait GuestMemory {
    /// Copy `out.len()` bytes from guest physical address `gpa`.
    fn read(&self, gpa: u64, out: &mut [u8]) -> VmmResult<()>;
    /// Copy `data` into guest physical address `gpa`.
    fn write(&self, gpa: u64, data: &[u8]) -> VmmResult<()>;
    /// Is `[gpa, gpa+len)` entirely backed by guest RAM?
    fn is_valid_range(&self, gpa: u64, len: u64) -> bool;

    fn read_u16(&self, gpa: u64) -> VmmResult<u16> {
        let mut b = [0u8; 2];
        self.read(gpa, &mut b)?;
        Ok(u16::from_le_bytes(b))
    }
    fn read_u32(&self, gpa: u64) -> VmmResult<u32> {
        let mut b = [0u8; 4];
        self.read(gpa, &mut b)?;
        Ok(u32::from_le_bytes(b))
    }
    fn read_u64(&self, gpa: u64) -> VmmResult<u64> {
        let mut b = [0u8; 8];
        self.read(gpa, &mut b)?;
        Ok(u64::from_le_bytes(b))
    }
    fn write_u16(&self, gpa: u64, v: u16) -> VmmResult<()> {
        self.write(gpa, &v.to_le_bytes())
    }
    fn write_u32(&self, gpa: u64, v: u32) -> VmmResult<()> {
        self.write(gpa, &v.to_le_bytes())
    }
}

/// One buffer within a descriptor chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Descriptor {
    pub addr: u64,
    pub len: u32,
    /// True when the device writes into this buffer (data-in / response).
    pub writable: bool,
}

/// A walked descriptor chain. Fixed capacity so the datapath never allocates.
pub struct DescriptorChain {
    /// Split ring: the head index, returned in the used ring.
    /// Packed ring: the buffer id from the last descriptor.
    pub head: u16,
    descriptors: [Descriptor; MAX_CHAIN_LEN],
    len: usize,
}

impl Default for DescriptorChain {
    fn default() -> Self {
        Self::new()
    }
}

impl DescriptorChain {
    pub fn new() -> Self {
        DescriptorChain {
            head: 0,
            descriptors: [Descriptor {
                addr: 0,
                len: 0,
                writable: false,
            }; MAX_CHAIN_LEN],
            len: 0,
        }
    }

    pub fn clear(&mut self) {
        self.len = 0;
        self.head = 0;
    }

    pub fn as_slice(&self) -> &[Descriptor] {
        &self.descriptors[..self.len]
    }

    pub fn len(&self) -> usize {
        self.len
    }

    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Device-readable buffers (driver -> device): the request and data-out.
    pub fn readable(&self) -> impl Iterator<Item = &Descriptor> {
        self.as_slice().iter().filter(|d| !d.writable)
    }

    /// Device-writable buffers (device -> driver): the response and data-in.
    pub fn writable(&self) -> impl Iterator<Item = &Descriptor> {
        self.as_slice().iter().filter(|d| d.writable)
    }

    pub fn readable_bytes(&self) -> u64 {
        self.readable().map(|d| d.len as u64).sum()
    }

    pub fn writable_bytes(&self) -> u64 {
        self.writable().map(|d| d.len as u64).sum()
    }

    /// Append a descriptor, refusing a chain longer than the ring allows.
    ///
    /// `pub(crate)` rather than private so device models can build a chain
    /// in their own tests without a live guest.
    pub(crate) fn push(&mut self, queue: u16, d: Descriptor) -> VmmResult<()> {
        if self.len >= MAX_CHAIN_LEN {
            return Err(VirtioError::BadDescriptor {
                queue,
                detail: format!("chain exceeds {MAX_CHAIN_LEN} descriptors (loop?)"),
            }
            .into());
        }
        self.descriptors[self.len] = d;
        self.len += 1;
        Ok(())
    }
}

/// Ring addresses programmed through `COMMON_CFG`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RingAddresses {
    pub desc: u64,
    pub driver: u64,
    pub device: u64,
}

impl RingAddresses {
    pub const fn is_programmed(&self) -> bool {
        self.desc != 0 && self.driver != 0 && self.device != 0
    }
}

/// One virtqueue.
pub struct Virtqueue {
    pub index: u16,
    pub size: u16,
    pub layout: RingLayout,
    pub addresses: RingAddresses,
    pub enabled: bool,
    pub msix_vector: u16,
    /// Split ring: next `avail` index we have not consumed.
    next_avail: u16,
    /// Split ring: next slot to write in the used ring.
    next_used: u16,
    /// Packed ring: current position and wrap counters.
    packed_avail_wrap: bool,
    packed_used_wrap: bool,
    /// Next descriptor-ring slot to inspect for an available chain.
    packed_next: u16,
    /// Next descriptor-ring slot to write a completion into.
    packed_used_next: u16,
}

impl Virtqueue {
    pub fn new(index: u16, size: u16, layout: RingLayout) -> Self {
        Virtqueue {
            index,
            size,
            layout,
            addresses: RingAddresses::default(),
            enabled: false,
            msix_vector: 0xFFFF,
            next_avail: 0,
            next_used: 0,
            packed_avail_wrap: true,
            packed_used_wrap: true,
            packed_next: 0,
            packed_used_next: 0,
        }
    }

    /// Validate the driver-programmed configuration before the queue is
    /// allowed to run.
    pub fn enable(&mut self) -> VmmResult<()> {
        if self.size == 0 || !self.size.is_power_of_two() || self.size > MAX_QUEUE_SIZE {
            return Err(VirtioError::BadQueueSize {
                size: self.size,
                max: MAX_QUEUE_SIZE,
            }
            .into());
        }
        if !self.addresses.is_programmed() {
            return Err(VirtioError::QueueNotConfigured { queue: self.index }.into());
        }
        self.enabled = true;
        Ok(())
    }

    pub fn reset(&mut self) {
        let (index, size, layout) = (self.index, self.size, self.layout);
        *self = Virtqueue::new(index, size, layout);
    }

    /// Pop the next available chain, or `Ok(false)` if the ring is empty.
    ///
    /// The chain is written into `out`, which the caller owns for the life of
    /// the worker thread — the datapath allocates nothing.
    pub fn pop<M: GuestMemory>(&mut self, mem: &M, out: &mut DescriptorChain) -> VmmResult<bool> {
        if !self.enabled {
            return Ok(false);
        }
        out.clear();
        match self.layout {
            RingLayout::Split => self.pop_split(mem, out),
            RingLayout::Packed => self.pop_packed(mem, out),
        }
    }

    /// Return a completed chain to the driver, reporting `written` bytes.
    pub fn push<M: GuestMemory>(
        &mut self,
        mem: &M,
        chain: &DescriptorChain,
        written: u32,
    ) -> VmmResult<()> {
        match self.layout {
            RingLayout::Split => self.push_split(mem, chain.head, written),
            RingLayout::Packed => self.push_packed(mem, chain.head, written),
        }
    }

    // -- split ring ---------------------------------------------------------
    //
    // desc:   16 bytes per entry {addr u64, len u32, flags u16, next u16}
    // avail:  {flags u16, idx u16, ring[size] u16, used_event u16}
    // used:   {flags u16, idx u16, ring[size]{id u32, len u32}, avail_event u16}

    fn pop_split<M: GuestMemory>(&mut self, mem: &M, out: &mut DescriptorChain) -> VmmResult<bool> {
        let avail_idx = mem.read_u16(self.addresses.driver + 2)?;
        if avail_idx == self.next_avail {
            return Ok(false);
        }
        let slot = self.next_avail % self.size;
        let head = mem.read_u16(self.addresses.driver + 4 + slot as u64 * 2)?;
        if head >= self.size {
            return Err(VirtioError::BadDescriptor {
                queue: self.index,
                detail: format!("head index {head} is beyond the {}-entry ring", self.size),
            }
            .into());
        }

        out.head = head;
        let mut current = head;
        loop {
            let base = self.addresses.desc + current as u64 * 16;
            let addr = mem.read_u64(base)?;
            let len = mem.read_u32(base + 8)?;
            let flags = mem.read_u16(base + 12)?;
            let next = mem.read_u16(base + 14)?;

            if flags & VIRTQ_DESC_F_INDIRECT != 0 {
                self.walk_indirect(mem, addr, len, out)?;
            } else {
                self.check_buffer(mem, addr, len)?;
                out.push(
                    self.index,
                    Descriptor {
                        addr,
                        len,
                        writable: flags & VIRTQ_DESC_F_WRITE != 0,
                    },
                )?;
            }

            if flags & VIRTQ_DESC_F_NEXT == 0 {
                break;
            }
            if next >= self.size {
                return Err(VirtioError::BadDescriptor {
                    queue: self.index,
                    detail: format!("next index {next} is beyond the {}-entry ring", self.size),
                }
                .into());
            }
            current = next;
        }

        self.next_avail = self.next_avail.wrapping_add(1);
        Ok(true)
    }

    /// Follow a `VIRTQ_DESC_F_INDIRECT` table. Indirect tables may not nest.
    fn walk_indirect<M: GuestMemory>(
        &self,
        mem: &M,
        table: u64,
        table_len: u32,
        out: &mut DescriptorChain,
    ) -> VmmResult<()> {
        if table_len as usize % 16 != 0 {
            return Err(VirtioError::BadDescriptor {
                queue: self.index,
                detail: format!("indirect table length {table_len} is not a multiple of 16"),
            }
            .into());
        }
        let count = (table_len / 16) as u16;
        if count == 0 {
            return Ok(());
        }
        let mut current = 0u16;
        for _ in 0..count {
            let base = table + current as u64 * 16;
            let addr = mem.read_u64(base)?;
            let len = mem.read_u32(base + 8)?;
            let flags = mem.read_u16(base + 12)?;
            let next = mem.read_u16(base + 14)?;
            if flags & VIRTQ_DESC_F_INDIRECT != 0 {
                return Err(VirtioError::BadDescriptor {
                    queue: self.index,
                    detail: "indirect descriptor tables may not nest".to_string(),
                }
                .into());
            }
            self.check_buffer(mem, addr, len)?;
            out.push(
                self.index,
                Descriptor {
                    addr,
                    len,
                    writable: flags & VIRTQ_DESC_F_WRITE != 0,
                },
            )?;
            if flags & VIRTQ_DESC_F_NEXT == 0 {
                break;
            }
            if next >= count {
                return Err(VirtioError::BadDescriptor {
                    queue: self.index,
                    detail: format!("indirect next {next} is beyond the {count}-entry table"),
                }
                .into());
            }
            current = next;
        }
        Ok(())
    }

    fn push_split<M: GuestMemory>(&mut self, mem: &M, head: u16, written: u32) -> VmmResult<()> {
        let slot = self.next_used % self.size;
        let entry = self.addresses.device + 4 + slot as u64 * 8;
        mem.write_u32(entry, head as u32)?;
        mem.write_u32(entry + 4, written)?;
        self.next_used = self.next_used.wrapping_add(1);
        // The index is published last: the driver must never see a used entry
        // before its contents are visible.
        mem.write_u16(self.addresses.device + 2, self.next_used)?;
        Ok(())
    }

    // -- packed ring --------------------------------------------------------
    //
    // desc: 16 bytes per entry {addr u64, len u32, id u16, flags u16}
    // A descriptor is available when AVAIL == wrap counter and USED != it.

    fn pop_packed<M: GuestMemory>(
        &mut self,
        mem: &M,
        out: &mut DescriptorChain,
    ) -> VmmResult<bool> {
        let base = self.addresses.desc + self.packed_next as u64 * 16;
        let flags = mem.read_u16(base + 14)?;
        let avail = flags & VIRTQ_DESC_F_AVAIL != 0;
        let used = flags & VIRTQ_DESC_F_USED != 0;
        if avail != self.packed_avail_wrap || used == self.packed_avail_wrap {
            return Ok(false);
        }

        let mut position = self.packed_next;
        let mut count = 0u16;
        loop {
            let d = self.addresses.desc + position as u64 * 16;
            let addr = mem.read_u64(d)?;
            let len = mem.read_u32(d + 8)?;
            let id = mem.read_u16(d + 12)?;
            let flags = mem.read_u16(d + 14)?;

            if flags & VIRTQ_DESC_F_INDIRECT != 0 {
                // `addr` is a table of descriptors, not data. Handing the
                // table to the device as though it were the request is how
                // a driver's first command arrives as garbage.
                self.walk_indirect_packed(mem, addr, len, out)?;
            } else {
                self.check_buffer(mem, addr, len)?;
                out.push(
                    self.index,
                    Descriptor {
                        addr,
                        len,
                        writable: flags & VIRTQ_DESC_F_WRITE != 0,
                    },
                )?;
            }
            // The buffer id of the *last* descriptor identifies the chain.
            out.head = id;

            count += 1;
            if flags & VIRTQ_DESC_F_NEXT == 0 {
                break;
            }
            if count >= self.size {
                return Err(VirtioError::BadDescriptor {
                    queue: self.index,
                    detail: "packed chain is longer than the ring".to_string(),
                }
                .into());
            }
            position = self.advance(position, 1).0;
        }

        let (next, wrapped) = self.advance(self.packed_next, count);
        self.packed_next = next;
        if wrapped {
            self.packed_avail_wrap = !self.packed_avail_wrap;
        }
        Ok(true)
    }

    /// Walk a packed-ring indirect descriptor table.
    ///
    /// Not the same shape as the split one, which is why it cannot share
    /// [`Self::walk_indirect`]. A packed descriptor is
    /// `{ addr: u64, len: u32, id: u16, flags: u16 }` — `id` sits where a
    /// split descriptor keeps `flags`, and there is no `next` field at all.
    /// §2.8.7 has the entries consumed **in order**, so nothing here
    /// follows a chain: the table is simply an array.
    fn walk_indirect_packed<M: GuestMemory>(
        &self,
        mem: &M,
        table: u64,
        table_len: u32,
        out: &mut DescriptorChain,
    ) -> VmmResult<()> {
        if table_len as usize % 16 != 0 {
            return Err(VirtioError::BadDescriptor {
                queue: self.index,
                detail: format!("packed indirect table length {table_len} is not a multiple of 16"),
            }
            .into());
        }
        for i in 0..(table_len / 16) {
            let base = table + u64::from(i) * 16;
            let addr = mem.read_u64(base)?;
            let len = mem.read_u32(base + 8)?;
            let flags = mem.read_u16(base + 14)?;
            if flags & VIRTQ_DESC_F_INDIRECT != 0 {
                return Err(VirtioError::BadDescriptor {
                    queue: self.index,
                    detail: "indirect descriptor tables may not nest".to_string(),
                }
                .into());
            }
            self.check_buffer(mem, addr, len)?;
            out.push(
                self.index,
                Descriptor {
                    addr,
                    len,
                    writable: flags & VIRTQ_DESC_F_WRITE != 0,
                },
            )?;
        }
        Ok(())
    }

    fn push_packed<M: GuestMemory>(&mut self, mem: &M, id: u16, written: u32) -> VmmResult<()> {
        // A packed ring has one descriptor ring shared by both sides: the
        // device writes the completion in place at the used position, setting
        // AVAIL and USED to the used wrap counter.
        let base = self.addresses.desc + self.packed_used_next as u64 * 16;
        mem.write_u32(base + 8, written)?;
        mem.write_u16(base + 12, id)?;
        let flags = if self.packed_used_wrap {
            VIRTQ_DESC_F_AVAIL | VIRTQ_DESC_F_USED
        } else {
            0
        };
        // Flags are written last so the driver never observes a half-written
        // completion.
        mem.write_u16(base + 14, flags)?;

        let (next, wrapped) = self.advance(self.packed_used_next, 1);
        self.packed_used_next = next;
        if wrapped {
            self.packed_used_wrap = !self.packed_used_wrap;
        }
        Ok(())
    }

    /// Advance a ring position by `n`, reporting whether it wrapped.
    fn advance(&self, position: u16, n: u16) -> (u16, bool) {
        let raw = position as u32 + n as u32;
        ((raw % self.size as u32) as u16, raw >= self.size as u32)
    }

    fn check_buffer<M: GuestMemory>(&self, mem: &M, addr: u64, len: u32) -> VmmResult<()> {
        if len == 0 {
            return Ok(());
        }
        if !mem.is_valid_range(addr, len as u64) {
            return Err(VirtioError::BadDescriptor {
                queue: self.index,
                detail: format!("buffer {addr:#x}+{len} is not backed by guest RAM"),
            }
            .into());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;

    struct FakeMem {
        bytes: RefCell<Vec<u8>>,
    }

    impl FakeMem {
        fn new() -> Self {
            FakeMem {
                bytes: RefCell::new(vec![0u8; 1 << 16]),
            }
        }
        fn put(&self, at: u64, data: &[u8]) {
            self.bytes.borrow_mut()[at as usize..at as usize + data.len()].copy_from_slice(data);
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

    /// A packed descriptor: addr, len, id, flags.
    fn packed_desc(addr: u64, len: u32, id: u16, flags: u16) -> Vec<u8> {
        let mut d = Vec::with_capacity(16);
        d.extend_from_slice(&addr.to_le_bytes());
        d.extend_from_slice(&len.to_le_bytes());
        d.extend_from_slice(&id.to_le_bytes());
        d.extend_from_slice(&flags.to_le_bytes());
        d
    }

    /// A packed indirect descriptor points at a *table*, not at data.
    ///
    /// This was a real defect: `pop_packed` ignored `VIRTQ_DESC_F_INDIRECT`
    /// and handed the descriptor table to the device as though it were the
    /// request. The guest's first virtio-gpu command therefore arrived as
    /// the nonsense command `0x4684498` — which is what a descriptor's
    /// address looks like when read as a command type.
    ///
    /// Note the layout differs from the split ring's: `id` sits where a
    /// split descriptor keeps `flags`, and entries are consumed in order
    /// with no `next` chaining (§2.8.7).
    #[test]
    fn a_packed_indirect_descriptor_is_followed_into_its_table() {
        const DESC: u64 = 0x1000;
        const TABLE: u64 = 0x2000;
        const REQUEST: u64 = 0x3000;
        const RESPONSE: u64 = 0x4000;

        let mem = FakeMem::new();
        // The table: one readable request, one writable response.
        mem.put(TABLE, &packed_desc(REQUEST, 24, 0, 0));
        mem.put(
            TABLE + 16,
            &packed_desc(RESPONSE, 64, 0, VIRTQ_DESC_F_WRITE),
        );
        // The ring entry pointing at it, marked available.
        mem.put(
            DESC,
            &packed_desc(TABLE, 32, 7, VIRTQ_DESC_F_INDIRECT | VIRTQ_DESC_F_AVAIL),
        );

        let mut q = Virtqueue::new(0, 64, RingLayout::Packed);
        q.addresses = RingAddresses {
            desc: DESC,
            driver: 0x5000,
            device: 0x6000,
        };
        q.enable().expect("enable");

        let mut chain = DescriptorChain::new();
        assert!(
            q.pop(&mem, &mut chain).expect("pop"),
            "a chain is available"
        );

        // Two descriptors from inside the table, not the table itself.
        assert_eq!(chain.len(), 2, "the indirect table must be walked");
        let ds: Vec<_> = chain.as_slice().to_vec();
        assert_eq!(ds[0].addr, REQUEST, "the request buffer, not the table");
        assert_eq!(ds[0].len, 24);
        assert!(!ds[0].writable);
        assert_eq!(ds[1].addr, RESPONSE);
        assert!(ds[1].writable);
        // The buffer id comes from the referring descriptor.
        assert_eq!(chain.head, 7);
    }

    #[test]
    fn a_packed_indirect_table_may_not_nest() {
        const DESC: u64 = 0x1000;
        const TABLE: u64 = 0x2000;
        let mem = FakeMem::new();
        mem.put(TABLE, &packed_desc(0x3000, 16, 0, VIRTQ_DESC_F_INDIRECT));
        mem.put(
            DESC,
            &packed_desc(TABLE, 16, 1, VIRTQ_DESC_F_INDIRECT | VIRTQ_DESC_F_AVAIL),
        );

        let mut q = Virtqueue::new(0, 64, RingLayout::Packed);
        q.addresses = RingAddresses {
            desc: DESC,
            driver: 0x5000,
            device: 0x6000,
        };
        q.enable().expect("enable");
        let mut chain = DescriptorChain::new();
        assert!(q.pop(&mem, &mut chain).is_err(), "nesting must be refused");
    }

    #[test]
    fn a_packed_indirect_table_of_a_bad_length_is_refused() {
        const DESC: u64 = 0x1000;
        let mem = FakeMem::new();
        // 20 is not a multiple of the 16-byte descriptor size.
        mem.put(
            DESC,
            &packed_desc(0x2000, 20, 1, VIRTQ_DESC_F_INDIRECT | VIRTQ_DESC_F_AVAIL),
        );
        let mut q = Virtqueue::new(0, 64, RingLayout::Packed);
        q.addresses = RingAddresses {
            desc: DESC,
            driver: 0x5000,
            device: 0x6000,
        };
        q.enable().expect("enable");
        let mut chain = DescriptorChain::new();
        assert!(q.pop(&mem, &mut chain).is_err());
    }
}
