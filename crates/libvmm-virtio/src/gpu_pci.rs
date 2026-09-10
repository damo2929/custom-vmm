//! virtio-gpu behind virtio-pci: the piece the vCPU actually talks to.
//!
//! This owns the MSI-X table, routes MMIO in the device's BAR to either the
//! transport register file or that table, and runs the queues when the
//! guest rings a doorbell.
//!
//! Queues are serviced **inline, on the vCPU thread that took the exit**.
//! That is the simplest thing that works and it is honest about what it
//! costs: every doorbell is a VM exit, and the guest is stopped while its
//! own frame is copied. `KVM_IOEVENTFD` on the doorbell plus a worker
//! thread is the fix, and `BarLayout::doorbell_address` already exists for
//! it. For a console at thirty frames a second this is not the bottleneck.
//!
//! Interrupts go out with `KVM_SIGNAL_MSI` rather than an irqfd. The
//! message is carried in the MSI-X table entry the guest wrote, so no GSI
//! and no routing table are involved — which matters because GSI routing is
//! not implemented in this VMM yet, and `signal_msi` does not need it.

use std::sync::{Arc, Mutex};

use libvmm_core::devices::{MmioDevice, MsiSender};

use crate::gpu::{Scanout, VirtioGpu, CONTROL_QUEUE, CURSOR_QUEUE};
use crate::mem::GuestRam;
use crate::queue::DescriptorChain;
use crate::transport::{Action, VirtioTransport, VIRTQ_MSI_NO_VECTOR};

/// One MSI-X table entry: the guest writes it, we send it back verbatim.
#[derive(Debug, Clone, Copy, Default)]
struct MsixEntry {
    address_lo: u32,
    address_hi: u32,
    data: u32,
    control: u32,
}

impl MsixEntry {
    /// Bit 0 of vector control is the per-vector mask.
    fn masked(&self) -> bool {
        self.control & 1 != 0
    }

    fn address(&self) -> u64 {
        u64::from(self.address_lo) | (u64::from(self.address_hi) << 32)
    }
}

/// The scanout, shared with whatever is capturing it.
#[derive(Default)]
pub struct SharedScanout {
    pub frame: Mutex<Option<Scanout>>,
    /// Frames the guest has flushed, whether or not anyone collected them.
    ///
    /// Counted here rather than at the capture end because the capture
    /// thread only polls once a codec is bound — that is, once a client is
    /// watching — and "the guest is drawing" is a different question from
    /// "someone is looking".
    pub flushed: std::sync::atomic::AtomicU64,
}

/// virtio-gpu as a PCI function.
pub struct VirtioGpuPci {
    transport: VirtioTransport,
    gpu: VirtioGpu,
    mem: GuestRam,
    msix: Vec<MsixEntry>,
    /// Set when a vector fires while masked; the guest reads it to find out
    /// what it missed on unmask.
    pba: Vec<bool>,
    interrupts: Arc<dyn MsiSender>,
    scanout: Arc<SharedScanout>,
    chain: DescriptorChain,
    started: bool,
}

impl VirtioGpuPci {
    pub fn new(
        width: u32,
        height: u32,
        mem: GuestRam,
        interrupts: Arc<dyn MsiSender>,
        scanout: Arc<SharedScanout>,
    ) -> Self {
        let transport = VirtioTransport::new(
            "virtio-gpu",
            crate::gpu::NUM_QUEUES,
            crate::gpu::CONTROL_QUEUE_SIZE,
            crate::gpu::CONFIG_LEN,
            // Only the transport features. Every virtio-gpu feature bit is
            // optional and every one is declined: no virgl, no EDID, no
            // blob resources. edk2's driver does exactly the same, masking
            // the offer down to VERSION_1 with the comment "We only want
            // the most basic 2D features."
            crate::features::COMMON_OFFER,
        );
        // One vector per queue plus one for configuration changes.
        let vectors = crate::gpu::NUM_QUEUES as usize + 1;
        VirtioGpuPci {
            transport,
            gpu: VirtioGpu::new(width, height),
            mem,
            msix: vec![MsixEntry::default(); vectors],
            pba: vec![false; vectors],
            interrupts,
            scanout,
            chain: DescriptorChain::new(),
            started: false,
        }
    }

    pub fn bar_size(&self) -> u64 {
        self.transport.layout.bar_size
    }

    pub fn layout(&self) -> &crate::pci_cap::BarLayout {
        &self.transport.layout
    }

    /// Tell the device where the guest mapped its BAR.
    pub fn set_bar_base(&mut self, base: u64) {
        self.transport.bar_base = Some(base);
    }

    pub fn frames(&self) -> u64 {
        self.gpu.frames
    }

    /// Send the MSI-X message for `vector`, honouring its mask.
    fn interrupt(&mut self, vector: u16) {
        let Some(entry) = self.msix.get(vector as usize).copied() else {
            return;
        };
        if entry.masked() {
            // Record it in the pending-bit array. The guest reads the PBA on
            // unmask to find out what it missed; dropping the interrupt
            // instead is how a device ends up wedged after a mask cycle.
            if let Some(bit) = self.pba.get_mut(vector as usize) {
                *bit = true;
            }
            return;
        }
        if entry.address() == 0 {
            return;
        }
        self.interrupts.signal(entry.address(), entry.data);
    }

    /// Drain every queue the guest has kicked.
    fn run_queues(&mut self) {
        for queue in self.transport.take_kicks() {
            let mut serviced = false;
            // A bounded pass: a guest that refills as fast as we drain must
            // not be able to hold this vCPU forever.
            for _ in 0..1024 {
                let mut chain = std::mem::take(&mut self.chain);
                chain.clear();
                let popped = match self.transport.pop(queue, &self.mem, &mut chain) {
                    Ok(p) => p,
                    Err(e) => {
                        log::warn!("virtio-gpu: queue {queue}: {e}");
                        self.chain = chain;
                        break;
                    }
                };
                if !popped {
                    self.chain = chain;
                    break;
                }

                let written = match queue {
                    CONTROL_QUEUE => self.gpu.handle_control(&self.mem, &chain),
                    CURSOR_QUEUE => self.gpu.handle_cursor(&self.mem, &chain),
                    other => {
                        log::warn!("virtio-gpu: kick on unknown queue {other}");
                        Ok(0)
                    }
                };
                let written = match written {
                    Ok(w) => w,
                    Err(e) => {
                        log::warn!("virtio-gpu: command failed: {e}");
                        0
                    }
                };
                if let Err(e) = self.transport.push(queue, &self.mem, &chain, written) {
                    log::warn!("virtio-gpu: completing queue {queue}: {e}");
                }
                self.chain = chain;
                serviced = true;
            }

            // Publish a finished frame before telling the guest we are done
            // with its buffers, so a capture that wakes on the interrupt
            // cannot see the older frame.
            if let Some(frame) = self.gpu.take_frame() {
                self.scanout
                    .flushed
                    .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                if let Ok(mut slot) = self.scanout.frame.lock() {
                    *slot = Some(frame);
                }
            }

            if serviced {
                if let Some(vector) = self.transport.queue_vector(queue) {
                    self.interrupt(vector);
                }
            }
        }
    }

    /// Where the MSI-X table and PBA sit inside the BAR.
    fn msix_region(&self, offset: u64) -> Option<(bool, usize, usize)> {
        let l = &self.transport.layout;
        let table = u64::from(l.msix_table_offset);
        let table_len = (self.msix.len() * 16) as u64;
        if (table..table + table_len).contains(&offset) {
            let byte = (offset - table) as usize;
            return Some((true, byte / 16, byte % 16));
        }
        let pba = u64::from(l.msix_pba_offset);
        if (pba..pba + 8).contains(&offset) {
            return Some((false, 0, (offset - pba) as usize));
        }
        None
    }
}

impl MmioDevice for VirtioGpuPci {
    fn name(&self) -> &'static str {
        "virtio-gpu"
    }

    fn claims(&self, addr: u64) -> bool {
        self.transport.contains(addr)
    }

    fn read(&mut self, addr: u64, data: &mut [u8]) {
        let Some(base) = self.transport.bar_base else {
            data.fill(0);
            return;
        };
        let offset = addr - base;

        if let Some((is_table, entry, byte)) = self.msix_region(offset) {
            if is_table {
                let e = self.msix.get(entry).copied().unwrap_or_default();
                let words = [e.address_lo, e.address_hi, e.data, e.control];
                let value = words.get(byte / 4).copied().unwrap_or(0);
                let shifted = u64::from(value) >> ((byte % 4) * 8);
                for (i, b) in data.iter_mut().enumerate() {
                    *b = (shifted >> (i * 8)) as u8;
                }
            } else {
                // The pending-bit array, one bit per vector.
                let mut bits = 0u64;
                for (i, pending) in self.pba.iter().enumerate() {
                    if *pending {
                        bits |= 1 << i;
                    }
                }
                let shifted = bits >> (byte * 8);
                for (i, b) in data.iter_mut().enumerate() {
                    *b = (shifted >> (i * 8)) as u8;
                }
            }
            return;
        }

        let isr_start = u64::from(self.transport.layout.isr_offset);
        if (isr_start..isr_start + u64::from(self.transport.layout.isr_length)).contains(&offset) {
            let isr = self.transport.read_isr_and_clear();
            data.fill(0);
            if let Some(first) = data.first_mut() {
                *first = isr;
            }
            return;
        }

        let config = self.gpu.config();
        self.transport.read(addr, data, &config);
    }

    fn write(&mut self, addr: u64, data: &[u8]) {
        let Some(base) = self.transport.bar_base else {
            return;
        };
        let offset = addr - base;

        if let Some((is_table, entry, byte)) = self.msix_region(offset) {
            if is_table && byte % 4 == 0 && data.len() == 4 {
                let value = u32::from_le_bytes(data.try_into().unwrap_or([0; 4]));
                if let Some(e) = self.msix.get_mut(entry) {
                    match byte / 4 {
                        0 => e.address_lo = value,
                        1 => e.address_hi = value,
                        2 => e.data = value,
                        _ => {
                            let was_masked = e.masked();
                            e.control = value;
                            // Unmasking a vector with a pending bit must
                            // deliver the interrupt the guest missed.
                            if was_masked
                                && !e.masked()
                                && self.pba.get(entry).copied().unwrap_or(false)
                            {
                                if let Some(bit) = self.pba.get_mut(entry) {
                                    *bit = false;
                                }
                                let vector = entry as u16;
                                self.interrupt(vector);
                            }
                        }
                    }
                }
            }
            return;
        }

        match self.transport.write(addr, data) {
            Action::Notify(_) => self.run_queues(),
            Action::DriverOk => {
                if !self.started {
                    self.started = true;
                    let acked = self.transport.acked_features();
                    log::info!(
                        "virtio-gpu: driver ready, {}x{}, {:?} ring, features {:#x} [{}]",
                        self.gpu.width,
                        self.gpu.height,
                        crate::features::ring_layout(acked),
                        acked,
                        crate::features::describe(acked).join(" ")
                    );
                }
                // The driver may have queued work before setting DRIVER_OK.
                self.run_queues();
            }
            Action::Reset => {
                self.started = false;
                for e in &mut self.msix {
                    *e = MsixEntry::default();
                }
                self.pba.fill(false);
                log::info!("virtio-gpu: reset by the driver");
            }
            Action::Failed => log::warn!("virtio-gpu: the driver gave up on this device"),
            Action::None => {}
        }
    }
}

/// Vector assignments the driver is expected to program.
pub mod vector {
    pub const CONFIG: u16 = 0;
    pub const CONTROL: u16 = 1;
    pub const CURSOR: u16 = 2;
    pub const NONE: u16 = super::VIRTQ_MSI_NO_VECTOR;
}
