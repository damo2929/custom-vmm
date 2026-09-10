//! Guest memory as the device models see it.
//!
//! Virtqueues live in guest RAM: the descriptor table, the available and
//! used rings, and every buffer they point at. A device therefore needs to
//! read and write arbitrary guest-physical addresses, and needs to do so
//! from a device thread rather than from the vCPU that happens to be
//! running.
//!
//! Every address here comes from the guest. None of it is trusted: an
//! access that is not wholly inside one mapped region is refused rather
//! than clamped, because a descriptor that straddles the end of RAM is a
//! driver bug or an attack, and silently truncating it would turn either
//! into corruption.

use std::sync::Arc;

use libvmm_core::error::{KvmError, VmmResult};

/// One region of guest RAM, as a host pointer.
///
/// This deliberately does not borrow the `HostMapping` it came from: device
/// threads outlive any single borrow, and the mapping is owned by the
/// `Machine` for the whole life of the VM.
#[derive(Debug, Clone, Copy)]
pub struct Region {
    pub gpa: u64,
    pub host: *mut u8,
    pub len: usize,
}

// SAFETY: the pointer is an mmap of guest RAM that lives as long as the VM,
// and is shared with the guest anyway. Synchronisation is the guest's
// problem and ours, not the type system's — see `read_at`.
unsafe impl Send for Region {}
unsafe impl Sync for Region {}

/// Guest RAM, addressable by guest-physical address.
#[derive(Debug, Clone, Default)]
pub struct GuestRam {
    regions: Arc<Vec<Region>>,
}

impl GuestRam {
    pub fn new(regions: Vec<Region>) -> Self {
        GuestRam {
            regions: Arc::new(regions),
        }
    }

    /// The region wholly containing `gpa..gpa + len`, if there is one.
    fn region_for(&self, gpa: u64, len: usize) -> Option<(&Region, usize)> {
        let end = gpa.checked_add(len as u64)?;
        self.regions.iter().find_map(|r| {
            let r_end = r.gpa.checked_add(r.len as u64)?;
            (gpa >= r.gpa && end <= r_end).then(|| (r, (gpa - r.gpa) as usize))
        })
    }

    fn fault(gpa: u64, len: usize, what: &str) -> libvmm_core::error::VmmError {
        KvmError::MemoryMap {
            size_mb: 0,
            detail: format!(
                "guest {what} of {len} bytes at {gpa:#x} is not inside any mapped region"
            ),
        }
        .into()
    }
}

impl crate::queue::GuestMemory for GuestRam {
    fn read(&self, gpa: u64, data: &mut [u8]) -> VmmResult<()> {
        let (region, offset) = self
            .region_for(gpa, data.len())
            .ok_or_else(|| Self::fault(gpa, data.len(), "read"))?;
        // SAFETY: `region_for` proved the whole range is inside this
        // mapping. The guest may write concurrently; see the module header.
        unsafe {
            std::ptr::copy_nonoverlapping(region.host.add(offset), data.as_mut_ptr(), data.len());
        }
        Ok(())
    }

    /// Is `gpa..gpa + len` wholly inside one region?
    ///
    /// The queue asks before trusting a descriptor, so a chain pointing
    /// outside RAM is rejected as a bad descriptor rather than faulting
    /// halfway through a copy.
    fn is_valid_range(&self, gpa: u64, len: u64) -> bool {
        usize::try_from(len)
            .ok()
            .and_then(|len| self.region_for(gpa, len))
            .is_some()
    }

    fn write(&self, gpa: u64, data: &[u8]) -> VmmResult<()> {
        let (region, offset) = self
            .region_for(gpa, data.len())
            .ok_or_else(|| Self::fault(gpa, data.len(), "write"))?;
        // SAFETY: as above.
        unsafe {
            std::ptr::copy_nonoverlapping(data.as_ptr(), region.host.add(offset), data.len());
        }
        Ok(())
    }
}
