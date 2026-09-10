//! MSI-X vectors and GSI routing — §2.2.
//!
//! Each queue is assigned an MSI-X vector; the vector's message is programmed
//! as a GSI route and fired via irqfd. No shared ISR line is used, and no
//! device may request a legacy INTx line (§1.4).

/// One MSI-X table entry, as the guest programmes it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct MsixEntry {
    pub addr_lo: u32,
    pub addr_hi: u32,
    pub data: u32,
    pub vector_control: u32,
}

impl MsixEntry {
    pub const MASKED_BIT: u32 = 1;

    pub const fn is_masked(&self) -> bool {
        self.vector_control & Self::MASKED_BIT != 0
    }

    pub const fn address(&self) -> u64 {
        ((self.addr_hi as u64) << 32) | self.addr_lo as u64
    }
}

/// A GSI route: which MSI message an irqfd write should deliver.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MsiRoute {
    pub gsi: u32,
    pub address: u64,
    pub data: u32,
}

/// Allocates GSIs and tracks the MSI routes the VM will install with
/// `KVM_SET_GSI_ROUTING`.
#[derive(Debug, Default)]
pub struct MsiRoutingTable {
    /// GSIs 0..24 belong to the userspace I/O APIC under split irqchip, so
    /// MSI routes start above them.
    next_gsi: u32,
    routes: Vec<MsiRoute>,
}

impl MsiRoutingTable {
    /// Start allocating above the split-irqchip GSI window (§1.4).
    pub fn new() -> Self {
        MsiRoutingTable {
            next_gsi: libvmm_core::kvm::SPLIT_IRQCHIP_GSI_COUNT,
            routes: Vec::new(),
        }
    }

    /// Reserve a GSI for a queue's MSI-X vector.
    ///
    /// The route is registered immediately with a null message; the guest
    /// fills in the address and data when it programmes the MSI-X table, at
    /// which point [`set_route`](Self::set_route) updates it in place. A
    /// reserved-but-unprogrammed GSI is inert: irqfd never fires on it.
    pub fn allocate(&mut self) -> u32 {
        let gsi = self.next_gsi;
        self.next_gsi += 1;
        self.routes.push(MsiRoute {
            gsi,
            address: 0,
            data: 0,
        });
        gsi
    }

    /// GSIs whose message the guest has actually programmed.
    pub fn programmed(&self) -> usize {
        self.routes.iter().filter(|r| r.address != 0).count()
    }

    /// Record or update the message a GSI delivers, from the guest's MSI-X
    /// table entry.
    pub fn set_route(&mut self, gsi: u32, entry: &MsixEntry) {
        let route = MsiRoute {
            gsi,
            address: entry.address(),
            data: entry.data,
        };
        match self.routes.iter_mut().find(|r| r.gsi == gsi) {
            Some(existing) => *existing = route,
            None => self.routes.push(route),
        }
    }

    pub fn routes(&self) -> &[MsiRoute] {
        &self.routes
    }

    pub fn len(&self) -> usize {
        self.routes.len()
    }

    pub fn is_empty(&self) -> bool {
        self.routes.is_empty()
    }
}
