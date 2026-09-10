//! §5.1–5.3 — virtio-scsi behind virtio-pci, multiqueue, with worker
//! threads.
//!
//! One HBA carries every drive. That is the reason §5 chose SCSI over
//! virtio-blk: SCSI has always addressed mixed device types on one bus, so
//! an SSD and a DVD-ROM sit behind the same controller as two targets and
//! need no second device between them.
//!
//! ## Queues are not run on the vCPU thread
//!
//! This is the one structural difference from [`libvmm_virtio::gpu_pci`], and it is
//! not a refinement — it is a correctness requirement. The GPU services its
//! queues inline on the vCPU thread that took the doorbell exit, which costs
//! a frame copy while the guest is stopped. Doing that for a disk would stop
//! the guest for the duration of a `pread`, so a guest waiting on I/O could
//! not run *any* other task on that vCPU, and a slow backing store would
//! present as a frozen machine rather than a slow disk.
//!
//! So every request queue has its own worker thread. The vCPU thread's job
//! on a doorbell is to set a flag and wake a condvar, which it does while
//! holding no lock a worker needs for long. Worker `k` then pops from queue
//! `k`, does the transfer, pushes the used descriptor, and raises the MSI-X
//! vector the guest bound to that queue.
//!
//! ## Multiqueue, and what it buys
//!
//! The number of request queues equals the number of vCPUs — §5.1's 1:1
//! invariant, which is why `num_queues` is absent from the configuration.
//! Linux's blk-mq then maps one hardware queue per CPU and a request
//! submitted on CPU *k* is answered by worker *k*, with no shared submission
//! path to contend on.
//!
//! Locking follows from that. The transport is locked only to pop and push
//! descriptors — microseconds, never across an I/O — and each target holds
//! its own lock, so two workers driving two different drives never contend
//! at all. Two workers driving the *same* drive serialise on that drive,
//! which is the truthful behaviour: they are one device.
//!
//! ## What is not here
//!
//! The doorbell is still an MMIO exit rather than a `KVM_IOEVENTFD`. That
//! costs one exit per batch of requests, not one per request, because the
//! worker drains the queue until it is empty before sleeping again.
//! `BarLayout::doorbell_address` exists for the ioeventfd when the exit rate
//! starts to matter.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex};

use crate::engine::{IoOp, IoSlice, StorageEngine};
use crate::scsi::{
    self, CommandOutcome, DriveIdentity, RequestHeader, ResponseHeader, REQ_HEADER_LEN,
    RESP_HEADER_LEN,
};
use libvmm_config::DriveMedium;
use libvmm_core::devices::{MmioDevice, MsiSender};

use libvmm_virtio::mem::GuestRam;
use libvmm_virtio::queue::{DescriptorChain, GuestMemory};
use libvmm_virtio::transport::{Action, VirtioTransport, VIRTQ_MSI_NO_VECTOR};

/// virtio device ID 8 (virtio 1.x §5.6).
pub const VIRTIO_ID_SCSI: u16 = 8;

/// Queue 0 is the control queue, queue 1 the event queue, and the request
/// queues follow. Fixed by the specification, not by us.
pub const CONTROL_QUEUE: u16 = 0;
pub const EVENT_QUEUE: u16 = 1;
pub const FIRST_REQUEST_QUEUE: u16 = 2;

/// `virtio_scsi_config` is 36 bytes (virtio 1.x §5.6.4).
const CONFIG_LEN: usize = 36;
/// The CDB and sense buffer sizes this device fixes. They are configurable
/// in the specification and negotiated through the config space; the SCSI
/// layer here is built for these two.
const CDB_SIZE: u32 = 32;
const SENSE_SIZE: u32 = 96;

/// One MSI-X table entry: the guest writes it, we send it back verbatim.
#[derive(Debug, Clone, Copy, Default)]
struct MsixEntry {
    address_lo: u32,
    address_hi: u32,
    data: u32,
    control: u32,
}

impl MsixEntry {
    fn masked(&self) -> bool {
        self.control & 1 != 0
    }
    fn address(&self) -> u64 {
        u64::from(self.address_lo) | (u64::from(self.address_hi) << 32)
    }
}

/// One drive on the bus.
///
/// The engine and the identity travel together because a SCSI answer needs
/// both: the capacity comes from the engine, and what kind of device is
/// reporting it comes from the identity.
pub struct ScsiTarget {
    pub drive_id: u32,
    pub engine: Box<dyn StorageEngine>,
    pub identity: DriveIdentity,
}

impl ScsiTarget {
    pub fn new(drive_id: u32, engine: Box<dyn StorageEngine>, medium: DriveMedium) -> Self {
        let kind = engine.kind();
        ScsiTarget {
            drive_id,
            engine,
            // A medium is present whenever there is an engine behind it. An
            // optical drive with no ISO is not constructed at all — it would
            // be a target with no backing store, and the configuration
            // refuses that (error 1044).
            identity: DriveIdentity::with_medium(drive_id, kind, medium, true),
        }
    }
}

/// Everything the vCPU thread and the workers both touch.
struct Shared {
    transport: Mutex<VirtioTransport>,
    mem: GuestRam,
    /// One lock per drive. Two workers on two drives never contend; two on
    /// one drive serialise, which is what one drive means.
    targets: Vec<Mutex<ScsiTarget>>,
    msix: Mutex<Vec<MsixEntry>>,
    pba: Mutex<Vec<bool>>,
    interrupts: Arc<dyn MsiSender>,
    /// Per-request-queue wakeup. `bool` is "there is work"; the condvar is
    /// how the vCPU thread hands it over without blocking on the worker.
    wake: Vec<(Mutex<bool>, Condvar)>,
    running: AtomicBool,
    /// Requests completed, for tests and for the log line at shutdown.
    pub completed: AtomicU64,
}

/// The device the vCPU talks to.
pub struct VirtioScsiPci {
    /// Names this HBA in logs and in its worker thread names. §5's topology
    /// gives every drive its own controller, so this is the drive id.
    name: String,
    /// Which configuration-space function this device sits behind, so the
    /// platform can tell it where firmware moved its BAR.
    bdf: Option<libvmm_core::pci::Bdf>,
    shared: Arc<Shared>,
    workers: Vec<std::thread::JoinHandle<()>>,
    config: Vec<u8>,
    bar_base: u64,
    msix_table_offset: u64,
    msix_pba_offset: u64,
    msix_vectors: usize,
    started: bool,
    layout: libvmm_virtio::pci_cap::BarLayout,
}

impl VirtioScsiPci {
    /// `request_queues` is the vCPU count (§5.1's 1:1 invariant).
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        name: impl Into<String>,
        transport: VirtioTransport,
        mem: GuestRam,
        targets: Vec<ScsiTarget>,
        interrupts: Arc<dyn MsiSender>,
        request_queues: u16,
        msix_vectors: usize,
        msix_table_offset: u64,
        msix_pba_offset: u64,
    ) -> libvmm_core::VmmResult<Self> {
        let name = name.into();
        let layout = transport.layout;
        let max_target = targets.iter().map(|t| t.drive_id).max().unwrap_or(0);
        let config = scsi_config(request_queues, max_target);

        let shared = Arc::new(Shared {
            transport: Mutex::new(transport),
            mem,
            targets: targets.into_iter().map(Mutex::new).collect(),
            msix: Mutex::new(vec![MsixEntry::default(); msix_vectors]),
            pba: Mutex::new(vec![false; msix_vectors]),
            interrupts,
            wake: (0..request_queues)
                .map(|_| (Mutex::new(false), Condvar::new()))
                .collect(),
            running: AtomicBool::new(true),
            completed: AtomicU64::new(0),
        });

        // One worker per request queue, named so `top -H` and a stack dump
        // say which queue is busy.
        //
        // The name carries the controller, not just the queue index,
        // because there is one controller per drive: `scsi0-q3` is queue 3
        // of drive 0. Linux caps a thread name at 15 characters and
        // silently truncates beyond that, so it is kept short deliberately.
        let workers = (0..request_queues)
            .map(|k| {
                let shared = Arc::clone(&shared);
                std::thread::Builder::new()
                    .name(format!("{name}-q{k}"))
                    .spawn(move || worker(shared, k))
                    .map_err(|e| {
                        libvmm_core::StorageError::QueueWorkerSpawn {
                            controller: name.to_string(),
                            queue: k,
                            detail: e.to_string(),
                        }
                        .into()
                    })
            })
            .collect::<libvmm_core::VmmResult<Vec<_>>>()?;

        Ok(VirtioScsiPci {
            name,
            bdf: None,
            shared,
            workers,
            config,
            bar_base: 0,
            msix_table_offset,
            msix_pba_offset,
            msix_vectors,
            started: false,
            layout,
        })
    }

    /// The size of the BAR this device needs.
    pub fn bar_size(&self) -> u64 {
        self.shared
            .transport
            .lock()
            .map(|t| t.layout.bar_size)
            .unwrap_or(0)
    }

    /// The BAR layout, for building the PCI capability chain.
    ///
    /// Kept beside the transport rather than read back out of it: the
    /// layout is fixed when the device is built and never changes, and a
    /// getter that can fail on a lock is a getter every caller has to have
    /// an opinion about.
    pub const fn layout(&self) -> libvmm_virtio::pci_cap::BarLayout {
        self.layout
    }

    /// Requests completed since bring-up.
    pub fn completed(&self) -> u64 {
        self.shared.completed.load(Ordering::Relaxed)
    }

    /// Stop the workers and join them.
    ///
    /// Dropping without this leaves threads blocked on a condvar that
    /// nothing will signal, which is a hang at process exit rather than a
    /// leak.
    pub fn shutdown(&mut self) {
        self.shared.running.store(false, Ordering::Release);
        for (lock, cv) in &self.shared.wake {
            if let Ok(mut flag) = lock.lock() {
                *flag = true;
            }
            cv.notify_all();
        }
        for h in self.workers.drain(..) {
            let _ = h.join();
        }
    }

    /// Set the base address the BAR was programmed to.
    pub fn set_bar_base(&mut self, base: u64) {
        self.bar_base = base;
        if let Ok(mut t) = self.shared.transport.lock() {
            t.bar_base = Some(base);
        }
    }

    /// Bind this device to the configuration-space function it lives behind,
    /// so the platform can follow firmware's BAR assignment.
    pub fn set_bdf(&mut self, bdf: libvmm_core::pci::Bdf) {
        self.bdf = Some(bdf);
    }

    fn wake_queue(&self, queue: u16) {
        if queue < FIRST_REQUEST_QUEUE {
            // The control and event queues carry task-management requests
            // and asynchronous events. Neither is used here: there is
            // nothing to abort and no hot-plug, so a doorbell on them is
            // noted and the queue is left alone rather than answered
            // wrongly.
            log::debug!("virtio-scsi: doorbell on queue {queue}, which carries no traffic here");
            return;
        }
        let k = usize::from(queue - FIRST_REQUEST_QUEUE);
        let Some((lock, cv)) = self.shared.wake.get(k) else {
            log::warn!("virtio-scsi: doorbell on queue {queue}, which does not exist");
            return;
        };
        if let Ok(mut flag) = lock.lock() {
            *flag = true;
        }
        cv.notify_one();
    }

    /// Wake every request queue — used at DRIVER_OK, because the driver may
    /// have queued work before it set the bit.
    fn wake_all(&self) {
        for q in 0..self.shared.wake.len() {
            self.wake_queue(FIRST_REQUEST_QUEUE + q as u16);
        }
    }
}

impl Drop for VirtioScsiPci {
    fn drop(&mut self) {
        self.shutdown();
    }
}

/// `virtio_scsi_config` (virtio 1.x §5.6.4).
fn scsi_config(request_queues: u16, max_target: u32) -> Vec<u8> {
    let mut c = vec![0u8; CONFIG_LEN];
    c[0..4].copy_from_slice(&u32::from(request_queues).to_le_bytes());
    // seg_max: descriptors usable for one request, leaving room for the
    // request and response headers in the same chain.
    c[4..8].copy_from_slice(&(libvmm_virtio::queue::MAX_CHAIN_LEN as u32 - 2).to_le_bytes());
    // max_sectors, in 512-byte units regardless of the drive's block size.
    c[8..12].copy_from_slice(&0x0000_FFFFu32.to_le_bytes());
    c[12..16].copy_from_slice(&128u32.to_le_bytes()); // cmd_per_lun
    c[16..20].copy_from_slice(&16u32.to_le_bytes()); // event_info_size
    c[20..24].copy_from_slice(&SENSE_SIZE.to_le_bytes());
    c[24..28].copy_from_slice(&CDB_SIZE.to_le_bytes());
    c[28..30].copy_from_slice(&0u16.to_le_bytes()); // max_channel
                                                    // Targets are addressed by drive_id, so the highest one bounds the scan.
    c[30..32].copy_from_slice(&((max_target + 1).min(u32::from(u16::MAX)) as u16).to_le_bytes());
    c[32..36].copy_from_slice(&0u32.to_le_bytes()); // max_lun: LUN 0 only
    c
}

impl MmioDevice for VirtioScsiPci {
    fn name(&self) -> &'static str {
        "virtio-scsi"
    }

    fn claims(&self, addr: u64) -> bool {
        self.shared
            .transport
            .lock()
            .map(|t| t.contains(addr))
            .unwrap_or(false)
    }

    fn read(&mut self, addr: u64, data: &mut [u8]) {
        let offset = addr.wrapping_sub(self.bar_base);
        if let Some(v) = self.msix_read(offset, data.len()) {
            fill(data, v);
            return;
        }
        if let Ok(t) = self.shared.transport.lock() {
            t.read(addr, data, &self.config);
        }
    }

    fn bdf(&self) -> Option<libvmm_core::pci::Bdf> {
        self.bdf
    }

    fn set_bar_base(&mut self, bar: usize, base: u64) {
        // One BAR, and it is BAR 0: a virtio device puts its whole
        // register file behind a single window.
        if bar == 0 {
            VirtioScsiPci::set_bar_base(self, base);
        }
    }

    fn write(&mut self, addr: u64, data: &[u8]) {
        let offset = addr.wrapping_sub(self.bar_base);
        if self.msix_write(offset, data) {
            return;
        }
        let action = match self.shared.transport.lock() {
            Ok(mut t) => t.write(addr, data),
            Err(_) => Action::None,
        };
        match action {
            Action::Notify(q) => self.wake_queue(q),
            Action::DriverOk => {
                if !self.started {
                    self.started = true;
                    let acked = self
                        .shared
                        .transport
                        .lock()
                        .map(|t| t.acked_features())
                        .unwrap_or(0);
                    log::info!(
                        "{}: driver ready, {} target(s), {} request queue(s) with a \
                         worker each, {:?} ring, features {:#x} [{}]",
                        self.name,
                        self.shared.targets.len(),
                        self.shared.wake.len(),
                        libvmm_virtio::features::ring_layout(acked),
                        acked,
                        libvmm_virtio::features::describe(acked).join(" ")
                    );
                }
                self.wake_all();
            }
            Action::Reset => {
                self.started = false;
                if let Ok(mut m) = self.shared.msix.lock() {
                    for e in m.iter_mut() {
                        *e = MsixEntry::default();
                    }
                }
                if let Ok(mut p) = self.shared.pba.lock() {
                    p.fill(false);
                }
                log::info!("{}: reset by the driver", self.name);
            }
            Action::Failed => log::warn!("{}: the driver gave up on this device", self.name),
            Action::None => {}
        }
    }
}

impl VirtioScsiPci {
    fn msix_read(&self, offset: u64, len: usize) -> Option<u64> {
        let table = self.msix_table_offset;
        let table_len = (self.msix_vectors * 16) as u64;
        if (table..table + table_len).contains(&offset) {
            let entry = ((offset - table) / 16) as usize;
            let field = (offset - table) % 16;
            let m = self.shared.msix.lock().ok()?;
            let e = m.get(entry)?;
            return Some(u64::from(match field {
                0 => e.address_lo,
                4 => e.address_hi,
                8 => e.data,
                _ => e.control,
            }));
        }
        let pba = self.msix_pba_offset;
        let pba_len = self.msix_vectors.div_ceil(64) as u64 * 8;
        if (pba..pba + pba_len).contains(&offset) {
            let p = self.shared.pba.lock().ok()?;
            let base = ((offset - pba) * 8) as usize;
            let mut bits = 0u64;
            for i in 0..len.min(8) * 8 {
                if p.get(base + i).copied().unwrap_or(false) {
                    bits |= 1 << i;
                }
            }
            return Some(bits);
        }
        None
    }

    /// Returns true when the write landed in the MSI-X table.
    fn msix_write(&mut self, offset: u64, data: &[u8]) -> bool {
        let table = self.msix_table_offset;
        let table_len = (self.msix_vectors * 16) as u64;
        if !(table..table + table_len).contains(&offset) {
            // The PBA is read-only; a write to it is silently discarded, as
            // the specification requires.
            let pba = self.msix_pba_offset;
            let pba_len = self.msix_vectors.div_ceil(64) as u64 * 8;
            return (pba..pba + pba_len).contains(&offset);
        }
        let entry = ((offset - table) / 16) as usize;
        let field = (offset - table) % 16;
        let value = value_of(data);

        let unmasked_with_pending = {
            let Ok(mut m) = self.shared.msix.lock() else {
                return true;
            };
            let Some(e) = m.get_mut(entry) else {
                return true;
            };
            let was_masked = e.masked();
            match field {
                0 => e.address_lo = value,
                4 => e.address_hi = value,
                8 => e.data = value,
                _ => e.control = value,
            }
            // Unmasking a vector with a pending interrupt must deliver it;
            // otherwise the guest waits forever for a completion that was
            // already produced.
            let now_unmasked = was_masked && !e.masked();
            now_unmasked.then(|| (e.address(), e.data))
        };

        if let Some((address, data)) = unmasked_with_pending {
            let pending = self
                .shared
                .pba
                .lock()
                .map(|mut p| {
                    let was = p.get(entry).copied().unwrap_or(false);
                    if was {
                        p[entry] = false;
                    }
                    was
                })
                .unwrap_or(false);
            if pending {
                self.shared.interrupts.signal(address, data);
            }
        }
        true
    }
}

// ---------------------------------------------------------------------------
// The worker
// ---------------------------------------------------------------------------

/// Service request queue `k` until the device is torn down.
fn worker(shared: Arc<Shared>, k: u16) {
    let queue = FIRST_REQUEST_QUEUE + k;
    let mut chain = DescriptorChain::new();

    while shared.running.load(Ordering::Acquire) {
        // Wait for a doorbell. The flag is cleared *before* draining, not
        // after: a doorbell that arrives while we are draining must leave
        // the flag set, or the request it announced is left in the queue
        // until the next unrelated doorbell — a stall that only shows up
        // under load.
        {
            let (lock, cv) = &shared.wake[usize::from(k)];
            let Ok(mut flag) = lock.lock() else { return };
            while !*flag {
                if !shared.running.load(Ordering::Acquire) {
                    return;
                }
                let Ok(next) = cv.wait(flag) else { return };
                flag = next;
            }
            *flag = false;
        }

        if !shared.running.load(Ordering::Acquire) {
            return;
        }

        loop {
            // The transport lock is held only across the pop, never across
            // the I/O below it.
            let popped = match shared.transport.lock() {
                Ok(mut t) => t.pop(queue, &shared.mem, &mut chain),
                Err(_) => return,
            };
            match popped {
                Ok(true) => {}
                Ok(false) => break,
                Err(e) => {
                    log::warn!("virtio-scsi: queue {queue}: {e}");
                    break;
                }
            }

            let written = serve(&shared, &chain);

            match shared.transport.lock() {
                Ok(mut t) => {
                    if let Err(e) = t.push(queue, &shared.mem, &chain, written) {
                        log::warn!("virtio-scsi: queue {queue}: completing: {e}");
                    }
                }
                Err(_) => return,
            }
            shared.completed.fetch_add(1, Ordering::Relaxed);
            raise(&shared, queue);
        }
    }
}

/// Deliver the MSI-X vector bound to `queue`.
fn raise(shared: &Shared, queue: u16) {
    let vector = match shared.transport.lock() {
        Ok(t) => t.queue_vector(queue),
        Err(_) => return,
    };
    let Some(vector) = vector else { return };
    if vector == VIRTQ_MSI_NO_VECTOR {
        return;
    }
    let entry = usize::from(vector);

    let Ok(m) = shared.msix.lock() else { return };
    let Some(e) = m.get(entry).copied() else {
        return;
    };
    drop(m);

    if e.masked() {
        // Record it as pending; unmasking will deliver it.
        if let Ok(mut p) = shared.pba.lock() {
            if let Some(bit) = p.get_mut(entry) {
                *bit = true;
            }
        }
        return;
    }
    shared.interrupts.signal(e.address(), e.data);
}

/// Answer one request. Returns the number of bytes written into the guest's
/// device-writable buffers.
fn serve(shared: &Shared, chain: &DescriptorChain) -> u32 {
    // The request header is the first readable descriptor; anything after it
    // is data-out. The response header is the first writable one; anything
    // after it is data-in.
    let mut request = vec![0u8; REQ_HEADER_LEN];
    let mut consumed = 0usize;
    let mut data_out: Vec<u8> = Vec::new();
    for d in chain.readable() {
        let mut buf = vec![0u8; d.len as usize];
        if shared.mem.read(d.addr, &mut buf).is_err() {
            return 0;
        }
        if consumed < REQ_HEADER_LEN {
            let take = buf.len().min(REQ_HEADER_LEN - consumed);
            request[consumed..consumed + take].copy_from_slice(&buf[..take]);
            consumed += take;
            data_out.extend_from_slice(&buf[take..]);
        } else {
            data_out.extend_from_slice(&buf);
        }
    }

    let writable: Vec<_> = chain.writable().copied().collect();
    if writable.is_empty() {
        return 0;
    }

    let req = match RequestHeader::parse(&request) {
        Ok(r) => r,
        Err(e) => {
            log::debug!("virtio-scsi: malformed request: {e}");
            return write_response(shared, &writable, &ResponseHeader::bad_target(), &[]);
        }
    };

    // §5.2's LUN encoding is `1, target, 0, lun`. Targets here are drive
    // ids, and LUN is always 0: a target with more than one logical unit
    // would be a drive that is two drives.
    let target_id = u32::from(req.target());
    let index = shared
        .targets
        .iter()
        .position(|t| t.lock().map(|t| t.drive_id == target_id).unwrap_or(false));
    let (Some(index), 0) = (index, req.logical_unit()) else {
        return write_response(shared, &writable, &ResponseHeader::bad_target(), &[]);
    };

    let Ok(mut target) = shared.targets[index].lock() else {
        return write_response(shared, &writable, &ResponseHeader::backend_lost(), &[]);
    };

    // Discard is intrinsic to a block drive here: every one of them is
    // presented as an SSD, and an SSD supports TRIM. The medium decides,
    // not a per-request flag.
    let discard = target.identity.medium.supports_discard();
    let outcome = {
        let ScsiTarget {
            engine, identity, ..
        } = &mut *target;
        scsi::dispatch(&req, engine.as_ref(), identity, discard, &data_out)
    };

    log::trace!(
        "scsi: target {} lun {} CDB {:#04x} data_out {}",
        req.target(),
        req.logical_unit(),
        req.cdb[0],
        data_out.len()
    );

    // Any command answered CHECK CONDITION is worth a line: a guest that
    // gets one usually gives up quietly, and the only other evidence is a
    // vendor error code from a boot loader. `0xc0000185` from the Windows
    // Boot Manager, for instance, says "I/O device error" and nothing about
    // which command produced it.
    if let CommandOutcome::Immediate { response, .. } = &outcome {
        if response.status != crate::scsi::SCSI_STATUS_GOOD
            || response.response != crate::scsi::VIRTIO_SCSI_S_OK
        {
            log::debug!(
                "scsi: CDB {:#04x} -> status {:#04x}, sense key {:#x} asc/ascq {:#04x}/{:#04x}",
                req.cdb[0],
                response.status,
                response.sense[2] & 0x0F,
                response.sense[12],
                response.sense[13],
            );
        }
    }

    match outcome {
        CommandOutcome::Immediate { response, data } => {
            write_response(shared, &writable, &response, &data)
        }
        CommandOutcome::Flush => {
            let response = match target.engine.flush() {
                Ok(()) => ResponseHeader::default(),
                Err(e) => scsi::response_for_error(&e),
            };
            write_response(shared, &writable, &response, &[])
        }
        CommandOutcome::Unmap { ranges } => {
            let mut response = ResponseHeader::default();
            for (lba, blocks) in ranges {
                if let Err(e) = target.engine.discard(lba, u64::from(blocks)) {
                    response = scsi::response_for_error(&e);
                    break;
                }
            }
            write_response(shared, &writable, &response, &[])
        }
        CommandOutcome::Transfer { op, lba, blocks } => {
            log::trace!("scsi: {} lba {lba} blocks {blocks}", op.as_str());
            transfer(shared, &mut target, &writable, &data_out, op, lba, blocks)
        }
    }
}

/// Perform a READ or WRITE against the engine, straight into or out of guest
/// memory.
fn transfer(
    shared: &Shared,
    target: &mut ScsiTarget,
    writable: &[libvmm_virtio::queue::Descriptor],
    data_out: &[u8],
    op: IoOp,
    lba: u64,
    blocks: u32,
) -> u32 {
    let block_size = target.engine.block_size() as usize;
    let length = blocks as usize * block_size;

    match op {
        IoOp::Read => {
            // The data-in buffers are the descriptors after the response
            // header. Reading straight into guest memory is the whole point
            // of `IoSlice`: no bounce buffer, no copy.
            let mut slices = Vec::new();
            let mut remaining = length;
            let mut skip = RESP_HEADER_LEN;
            for d in writable {
                let mut len = d.len as usize;
                let mut addr = d.addr;
                if skip > 0 {
                    let take = skip.min(len);
                    skip -= take;
                    len -= take;
                    addr += take as u64;
                    if len == 0 {
                        continue;
                    }
                }
                let len = len.min(remaining);
                if len == 0 {
                    break;
                }
                let Some(host) = shared.mem.host_ptr(addr, len) else {
                    return write_response(shared, writable, &ResponseHeader::backend_lost(), &[]);
                };
                // SAFETY: `host_ptr` returned a pointer to `len` bytes that
                // lie wholly inside one guest RAM region, and guest RAM
                // outlives every worker.
                slices.push(unsafe { IoSlice::new(host, len) });
                remaining -= len;
                if remaining == 0 {
                    break;
                }
            }
            let response = match target.engine.submit(op, lba, &slices, 0) {
                Ok(()) => ResponseHeader::default(),
                Err(e) => scsi::response_for_error(&e),
            };
            let transferred = (length - remaining) as u32;
            let header = write_response(shared, writable, &response, &[]);
            header + transferred
        }
        IoOp::Write => {
            // The data to write is already in `data_out`, copied out of the
            // guest's readable descriptors.
            let take = data_out.len().min(length);
            let mut buf = data_out[..take].to_vec();
            let slices = if buf.is_empty() {
                Vec::new()
            } else {
                // SAFETY: `buf` is a live local allocation of exactly this
                // length, and `submit` does not retain the pointer.
                vec![unsafe { IoSlice::new(buf.as_mut_ptr(), buf.len()) }]
            };
            let response = match target.engine.submit(op, lba, &slices, 0) {
                Ok(()) => ResponseHeader::default(),
                Err(e) => scsi::response_for_error(&e),
            };
            write_response(shared, writable, &response, &[])
        }
    }
}

/// Write the response header, then `data`, across the writable descriptors.
fn write_response(
    shared: &Shared,
    writable: &[libvmm_virtio::queue::Descriptor],
    response: &ResponseHeader,
    data: &[u8],
) -> u32 {
    let mut header = vec![0u8; RESP_HEADER_LEN];
    response.write_into(&mut header);

    let mut payload = header;
    payload.extend_from_slice(data);

    let mut written = 0usize;
    for d in writable {
        if written >= payload.len() {
            break;
        }
        let len = (d.len as usize).min(payload.len() - written);
        if shared
            .mem
            .write(d.addr, &payload[written..written + len])
            .is_err()
        {
            break;
        }
        written += len;
    }
    written as u32
}

fn value_of(data: &[u8]) -> u32 {
    let mut v = 0u32;
    for (i, b) in data.iter().enumerate().take(4) {
        v |= u32::from(*b) << (i * 8);
    }
    v
}

fn fill(data: &mut [u8], value: u64) {
    for (i, byte) in data.iter_mut().enumerate() {
        *byte = ((value >> (i * 8)) & 0xFF) as u8;
    }
}
