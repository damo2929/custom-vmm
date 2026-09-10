//! §5.1–5.3 — the virtio-scsi device, driven the way a guest drives it.
//!
//! These build a real split virtqueue in real host memory, program the
//! device through its MMIO registers exactly as a driver would, ring the
//! doorbell, and read back what the worker wrote. Nothing is called
//! directly: if the worker threads do not run, or the transport is
//! misconfigured, or the response lands at the wrong offset, these fail.
//!
//! That matters more here than usual. The device answers on a *different
//! thread* from the one that rings the bell, so a unit test that called the
//! dispatch function would pass with the entire threading design broken.

use std::sync::Arc;
use std::time::{Duration, Instant};

use libvmm_config::DriveMedium;
use libvmm_core::devices::{MmioDevice, MsiSender};
use libvmm_storage::engines::file::FileEngine;
use libvmm_storage::scsi::REQ_HEADER_LEN;
use libvmm_storage::scsi_pci::{ScsiTarget, VirtioScsiPci, FIRST_REQUEST_QUEUE};
use libvmm_virtio::mem::{GuestRam, Region};
use libvmm_virtio::pci_cap::BarLayout;
use libvmm_virtio::transport::VirtioTransport;

// --- a guest, as far as the device is concerned ----------------------------

const RAM_SIZE: usize = 1 << 20;
const BAR_BASE: u64 = 0xC000_0000;

/// Guest-physical layout of the test ring. Addresses are arbitrary but must
/// be aligned as the virtio specification requires.
const DESC_TABLE: u64 = 0x1000;
const AVAIL_RING: u64 = 0x2000;
const USED_RING: u64 = 0x3000;
const REQUEST_BUF: u64 = 0x4000;
const RESPONSE_BUF: u64 = 0x5000;
const QUEUE_SIZE: u16 = 64;

/// Host memory standing in for guest RAM. The device takes raw pointers into
/// it, so it must outlive the device — hence the `Box` and the explicit
/// ordering in each test.
struct Ram {
    bytes: Box<[u8]>,
}

impl Ram {
    fn new() -> Self {
        Self::new_sized(RAM_SIZE)
    }

    fn new_sized(len: usize) -> Self {
        Ram {
            bytes: vec![0u8; len].into_boxed_slice(),
        }
    }

    fn guest(&mut self) -> GuestRam {
        let len = self.bytes.len();
        GuestRam::new(vec![Region {
            gpa: 0,
            host: self.bytes.as_mut_ptr(),
            len,
        }])
    }

    fn put(&mut self, at: u64, data: &[u8]) {
        let at = at as usize;
        self.bytes[at..at + data.len()].copy_from_slice(data);
    }

    fn get(&self, at: u64, len: usize) -> &[u8] {
        let at = at as usize;
        &self.bytes[at..at + len]
    }
}

/// MSI-X messages the device sent, so a test can assert an interrupt was
/// actually raised rather than assume it.
#[derive(Default)]
struct RecordingInterrupts {
    signals: std::sync::Mutex<Vec<(u64, u32)>>,
}

impl MsiSender for RecordingInterrupts {
    fn signal(&self, address: u64, data: u32) {
        self.signals.lock().unwrap().push((address, data));
    }
}

// --- driving the device ----------------------------------------------------

mod common {
    pub const DRIVER_FEATURE_SELECT: u64 = 0x08;
    pub const DRIVER_FEATURE: u64 = 0x0C;
    pub const DEVICE_STATUS: u64 = 0x14;
    pub const QUEUE_SELECT: u64 = 0x16;
    pub const QUEUE_SIZE: u64 = 0x18;
    pub const QUEUE_MSIX_VECTOR: u64 = 0x1A;
    pub const QUEUE_ENABLE: u64 = 0x1C;
    pub const QUEUE_DESC: u64 = 0x20;
    pub const QUEUE_DRIVER: u64 = 0x28;
    pub const QUEUE_DEVICE: u64 = 0x30;
}

/// `VIRTIO_F_VERSION_1`, bit 32 — the only feature these tests need.
const VIRTIO_F_VERSION_1: u32 = 1;

fn w(dev: &mut VirtioScsiPci, layout: &BarLayout, off: u64, value: u64, len: usize) {
    let bytes = value.to_le_bytes();
    dev.write(
        BAR_BASE + u64::from(layout.common_offset) + off,
        &bytes[..len],
    );
}

/// Bring the device up the way a driver does: ack features, configure the
/// one request queue, set DRIVER_OK.
fn bring_up(dev: &mut VirtioScsiPci, layout: &BarLayout) {
    // ACKNOWLEDGE | DRIVER
    w(dev, layout, common::DEVICE_STATUS, 1, 1);
    w(dev, layout, common::DEVICE_STATUS, 1 | 2, 1);
    // Ack VIRTIO_F_VERSION_1 in feature word 1.
    w(dev, layout, common::DRIVER_FEATURE_SELECT, 1, 4);
    w(
        dev,
        layout,
        common::DRIVER_FEATURE,
        u64::from(VIRTIO_F_VERSION_1),
        4,
    );
    // FEATURES_OK
    w(dev, layout, common::DEVICE_STATUS, 1 | 2 | 8, 1);

    w(
        dev,
        layout,
        common::QUEUE_SELECT,
        u64::from(FIRST_REQUEST_QUEUE),
        2,
    );
    w(dev, layout, common::QUEUE_SIZE, u64::from(QUEUE_SIZE), 2);
    w(dev, layout, common::QUEUE_DESC, DESC_TABLE, 8);
    w(dev, layout, common::QUEUE_DRIVER, AVAIL_RING, 8);
    w(dev, layout, common::QUEUE_DEVICE, USED_RING, 8);
    w(dev, layout, common::QUEUE_MSIX_VECTOR, 0, 2);
    w(dev, layout, common::QUEUE_ENABLE, 1, 2);
    // DRIVER_OK
    w(dev, layout, common::DEVICE_STATUS, 1 | 2 | 8 | 4, 1);
}

/// Unmask MSI-X vector 0 and give it an address, so completions are
/// delivered rather than left pending.
fn arm_msix(dev: &mut VirtioScsiPci, layout: &BarLayout) {
    let table = BAR_BASE + u64::from(layout.msix_table_offset);
    dev.write(table, &0xFEE0_0000u32.to_le_bytes()); // address low
    dev.write(table + 4, &0u32.to_le_bytes()); // address high
    dev.write(table + 8, &0x4021u32.to_le_bytes()); // data
    dev.write(table + 12, &0u32.to_le_bytes()); // control: unmasked
}

/// A split descriptor.
fn desc(addr: u64, len: u32, flags: u16, next: u16) -> Vec<u8> {
    let mut d = Vec::with_capacity(16);
    d.extend_from_slice(&addr.to_le_bytes());
    d.extend_from_slice(&len.to_le_bytes());
    d.extend_from_slice(&flags.to_le_bytes());
    d.extend_from_slice(&next.to_le_bytes());
    d
}

/// Build a `virtio_scsi_req_cmd` for `target`, LUN 0.
///
/// The LUN is written the way a real initiator writes it — SAM single-level
/// addressing, so byte 2 carries the `0x40` address-method prefix. edk2 does
/// this and so does Linux. Encoding a bare zero here would have let a device
/// that mis-parses the field pass its own tests.
fn request(target: u8, cdb: &[u8]) -> Vec<u8> {
    let lun: u16 = 0;
    let mut r = vec![0u8; REQ_HEADER_LEN];
    r[0] = 1;
    r[1] = target;
    r[2] = (((lun >> 8) & 0x3F) as u8) | 0x40;
    r[3] = (lun & 0xFF) as u8;
    r[19..19 + cdb.len()].copy_from_slice(cdb);
    r
}

/// Place a two-descriptor chain — request out, response in — and ring the
/// doorbell for the request queue.
///
/// `nth` is how many requests have already been submitted. The available
/// ring index is monotonic and the device remembers where it got to, so a
/// second request that reuses the previous index is correctly ignored — a
/// mistake this test made once and which looked exactly like the device
/// failing to answer.
fn submit(
    ram: &mut Ram,
    dev: &mut VirtioScsiPci,
    layout: &BarLayout,
    req: &[u8],
    resp_len: u32,
    nth: u16,
) {
    const VIRTQ_DESC_F_NEXT: u16 = 1;
    const VIRTQ_DESC_F_WRITE: u16 = 2;

    ram.put(REQUEST_BUF, req);
    ram.put(RESPONSE_BUF, &vec![0u8; resp_len as usize]);

    ram.put(
        DESC_TABLE,
        &desc(REQUEST_BUF, req.len() as u32, VIRTQ_DESC_F_NEXT, 1),
    );
    ram.put(
        DESC_TABLE + 16,
        &desc(RESPONSE_BUF, resp_len, VIRTQ_DESC_F_WRITE, 0),
    );

    // avail: flags, idx, then the ring. Every entry points at descriptor 0,
    // the head of the one chain this test reuses.
    ram.put(AVAIL_RING, &0u16.to_le_bytes());
    ram.put(
        AVAIL_RING + 4 + u64::from(nth % QUEUE_SIZE) * 2,
        &0u16.to_le_bytes(),
    );
    ram.put(AVAIL_RING + 2, &(nth + 1).to_le_bytes());

    let doorbell = BAR_BASE
        + u64::from(layout.notify_offset)
        + u64::from(FIRST_REQUEST_QUEUE) * u64::from(layout.notify_off_multiplier);
    dev.write(doorbell, &0u16.to_le_bytes());
}

/// Wait for the worker to publish `count` used entries. The whole point of
/// the design is that this happens on another thread, so the test has to
/// wait for it rather than assume it already happened.
fn await_used(ram: &Ram, count: u16, deadline: Duration) -> bool {
    let until = Instant::now() + deadline;
    while Instant::now() < until {
        // used.idx is at offset 2 of the used ring.
        let idx = u16::from_le_bytes(ram.get(USED_RING + 2, 2).try_into().unwrap());
        if idx >= count {
            return true;
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    false
}

fn engine(name: &str, block_size: u32, blocks: u64) -> Box<FileEngine> {
    let mut path = std::env::temp_dir();
    path.push(format!("vmm-scsi-pci-{}-{name}", std::process::id()));
    let _ = std::fs::remove_file(&path);
    Box::new(
        FileEngine::open(&path, blocks * u64::from(block_size), block_size, name)
            .expect("open the backing file"),
    )
}

/// Build a device with one target and one request queue.
fn device(
    name: &str,
    ram: &mut Ram,
    medium: DriveMedium,
    block_size: u32,
    interrupts: Arc<RecordingInterrupts>,
) -> (VirtioScsiPci, BarLayout) {
    // Queue 0 control, queue 1 event, queue 2 the single request queue.
    let queues = FIRST_REQUEST_QUEUE + 1;
    let layout = BarLayout::new(queues, 36);
    let mut transport = VirtioTransport::new(
        "virtio-scsi",
        queues,
        QUEUE_SIZE,
        36,
        1u64 << 32, // VIRTIO_F_VERSION_1
    );
    transport.bar_base = Some(BAR_BASE);

    let target = ScsiTarget::new(0, engine(medium.as_str(), block_size, 1024), medium);
    let mut dev = VirtioScsiPci::new(
        name,
        transport,
        ram.guest(),
        vec![target],
        interrupts,
        1, // one request queue, so one worker
        4,
        u64::from(layout.msix_table_offset),
        u64::from(layout.msix_pba_offset),
    )
    .expect("build the controller");
    dev.set_bar_base(BAR_BASE);
    (dev, layout)
}

// --- the tests -------------------------------------------------------------

/// The response comes back on a worker thread. Nothing in this test touches
/// the SCSI layer directly.
#[test]
fn an_ssd_reports_itself_as_a_non_rotating_direct_access_device() {
    let mut ram = Ram::new();
    let interrupts = Arc::new(RecordingInterrupts::default());
    let (mut dev, layout) = device(
        "ssd",
        &mut ram,
        DriveMedium::Ssd,
        512,
        Arc::clone(&interrupts),
    );
    bring_up(&mut dev, &layout);
    arm_msix(&mut dev, &layout);

    // INQUIRY, standard page.
    submit(
        &mut ram,
        &mut dev,
        &layout,
        &request(0, &[0x12, 0x00, 0x00, 0x00, 36]),
        108 + 36,
        0,
    );
    assert!(
        await_used(&ram, 1, Duration::from_secs(5)),
        "the queue worker must complete the request on its own thread"
    );

    // The response header is 108 bytes; the INQUIRY data follows it.
    let data = ram.get(RESPONSE_BUF + 108, 36);
    assert_eq!(
        data[0] & 0x1F,
        0x00,
        "peripheral device type 0 — a direct-access device, which is what \
         binds Linux's `sd` driver"
    );
    assert_eq!(data[1] & 0x80, 0, "a fixed disk is not removable");

    // VPD page 0xB1 carries the one field that makes this an SSD.
    submit(
        &mut ram,
        &mut dev,
        &layout,
        &request(0, &[0x12, 0x01, 0xB1, 0x00, 64]),
        108 + 64,
        1,
    );
    assert!(
        await_used(&ram, 2, Duration::from_secs(5)),
        "second request"
    );
    let page = ram.get(RESPONSE_BUF + 108, 64);
    assert_eq!(page[1], 0xB1, "VPD page 0xB1");
    assert_eq!(
        u16::from_be_bytes([page[4], page[5]]),
        libvmm_config::NON_ROTATING,
        "MEDIUM ROTATION RATE 1 is the only way a guest learns a disk is \
         solid-state; Linux publishes it as queue/rotational = 0"
    );

    assert!(
        !interrupts.signals.lock().unwrap().is_empty(),
        "a completion must raise the MSI-X vector the driver bound to the queue"
    );
    dev.shutdown();
}

/// The same device, a different target: an optical drive answers a different
/// command set and says so in the first byte a guest reads.
#[test]
fn a_dvd_rom_reports_itself_as_removable_mmc_media() {
    let mut ram = Ram::new();
    let interrupts = Arc::new(RecordingInterrupts::default());
    let (mut dev, layout) = device("dvd", &mut ram, DriveMedium::DvdRom, 2048, interrupts);
    bring_up(&mut dev, &layout);
    arm_msix(&mut dev, &layout);

    submit(
        &mut ram,
        &mut dev,
        &layout,
        &request(0, &[0x12, 0x00, 0x00, 0x00, 36]),
        108 + 36,
        0,
    );
    assert!(await_used(&ram, 1, Duration::from_secs(5)));

    let data = ram.get(RESPONSE_BUF + 108, 36);
    assert_eq!(
        data[0] & 0x1F,
        0x05,
        "peripheral device type 5 — CD/DVD, which binds Linux's `sr` driver \
         and the El Torito path in UEFI's PartitionDxe"
    );
    assert_eq!(data[1] & 0x80, 0x80, "optical media are removable");
    dev.shutdown();
}

/// A write to an ISO must be refused by the drive, with a sense key the
/// guest understands, and must never reach the engine.
#[test]
fn a_write_to_an_optical_drive_comes_back_write_protected() {
    let mut ram = Ram::new();
    let interrupts = Arc::new(RecordingInterrupts::default());
    let (mut dev, layout) = device("bd", &mut ram, DriveMedium::BdRom, 2048, interrupts);
    bring_up(&mut dev, &layout);
    arm_msix(&mut dev, &layout);

    // WRITE (10), one block at LBA 0.
    submit(
        &mut ram,
        &mut dev,
        &layout,
        &request(0, &[0x2A, 0, 0, 0, 0, 0, 0, 0, 1]),
        108,
        0,
    );
    assert!(await_used(&ram, 1, Duration::from_secs(5)));

    let resp = ram.get(RESPONSE_BUF, 108);
    // status is at offset 10; sense begins at 12.
    assert_eq!(resp[10], 0x02, "CHECK CONDITION");
    assert_eq!(resp[12 + 2] & 0x0F, 0x07, "sense key DATA PROTECT");
    assert_eq!(
        (resp[12 + 12], resp[12 + 13]),
        (0x27, 0x00),
        "ASC/ASCQ WRITE PROTECTED — a generic I/O error here makes a guest \
         retry forever instead of mounting read-only"
    );
    dev.shutdown();
}

/// §5.1's 1:1 invariant: one request queue per vCPU, and one worker per
/// request queue. The device reports the count to the driver in its config
/// space, and that is what Linux's blk-mq maps hardware queues from.
#[test]
fn the_device_offers_one_request_queue_per_vcpu_each_with_its_own_worker() {
    const VCPUS: u16 = 4;
    let mut ram = Ram::new();
    let interrupts = Arc::new(RecordingInterrupts::default());

    let queues = FIRST_REQUEST_QUEUE + VCPUS;
    let layout = BarLayout::new(queues, 36);
    let mut transport = VirtioTransport::new("virtio-scsi", queues, QUEUE_SIZE, 36, 1u64 << 32);
    transport.bar_base = Some(BAR_BASE);
    let mut dev = VirtioScsiPci::new(
        // A name no other test uses, because the thread list below is
        // process-wide and the tests run in parallel.
        "mqtest",
        transport,
        ram.guest(),
        vec![ScsiTarget::new(
            0,
            engine("mq", 512, 1024),
            DriveMedium::Ssd,
        )],
        interrupts,
        VCPUS,
        VCPUS as usize + 2,
        u64::from(layout.msix_table_offset),
        u64::from(layout.msix_pba_offset),
    )
    .expect("build the controller");
    dev.set_bar_base(BAR_BASE);

    // `virtio_scsi_config.num_queues` is the first field, and it counts the
    // request queues only — not the control and event queues.
    let mut num_queues = [0u8; 4];
    dev.read(BAR_BASE + u64::from(layout.device_offset), &mut num_queues);
    assert_eq!(
        u32::from_le_bytes(num_queues),
        u32::from(VCPUS),
        "one request queue per vCPU"
    );

    // One worker thread per request queue, named for the queue it serves.
    let names = std::fs::read_dir("/proc/self/task")
        .expect("read the thread list")
        .filter_map(|e| {
            let p = e.ok()?.path().join("comm");
            Some(std::fs::read_to_string(p).ok()?.trim().to_string())
        })
        .filter(|n| n.starts_with("mqtest-q"))
        .count();
    assert_eq!(
        names, VCPUS as usize,
        "each request queue must have its own worker thread; found {names}"
    );

    dev.shutdown();
}

/// The point of a disk. A WRITE (10) followed by a READ (10) must return
/// what was written, through guest memory, on the worker thread.
///
/// The read path is the one worth pinning: it hands the engine a pointer
/// *into guest RAM* and lets `pread` fill it, with no bounce buffer. If that
/// pointer arithmetic is wrong the data lands somewhere else and the guest
/// reads zeroes — which looks like an empty disk, not like a bug.
#[test]
fn a_block_written_to_an_ssd_reads_back_byte_for_byte() {
    const PATTERN: u8 = 0xA7;
    let mut ram = Ram::new();
    let interrupts = Arc::new(RecordingInterrupts::default());
    let (mut dev, layout) = device("rw", &mut ram, DriveMedium::Ssd, 512, interrupts);
    bring_up(&mut dev, &layout);
    arm_msix(&mut dev, &layout);

    // WRITE (10): one 512-byte block at LBA 3. The data follows the request
    // header in the same readable descriptor, which is how a driver sends
    // it.
    let mut write = request(0, &[0x2A, 0, 0, 0, 0, 3, 0, 0, 1]);
    write.extend_from_slice(&[PATTERN; 512]);
    submit(&mut ram, &mut dev, &layout, &write, 108, 0);
    assert!(await_used(&ram, 1, Duration::from_secs(5)));
    assert_eq!(
        ram.get(RESPONSE_BUF, 108)[10],
        0x00,
        "the write must succeed with GOOD status"
    );

    // READ (10) the same block back.
    submit(
        &mut ram,
        &mut dev,
        &layout,
        &request(0, &[0x28, 0, 0, 0, 0, 3, 0, 0, 1]),
        108 + 512,
        1,
    );
    assert!(await_used(&ram, 2, Duration::from_secs(5)));

    let status = ram.get(RESPONSE_BUF, 108)[10];
    assert_eq!(status, 0x00, "the read must succeed with GOOD status");
    let data = ram.get(RESPONSE_BUF + 108, 512);
    assert!(
        data.iter().all(|b| *b == PATTERN),
        "the block read back must be the block written; got {:#04x}..{:#04x}",
        data[0],
        data[511]
    );
    dev.shutdown();
}

/// An ISO is read the same way, in 2048-byte sectors, and the drive reports
/// that block size. A guest that believes an optical drive uses 512-byte
/// sectors reads every file at a quarter of its real offset.
#[test]
fn an_optical_drive_reports_2048_byte_sectors_and_reads_them() {
    let mut ram = Ram::new();
    let interrupts = Arc::new(RecordingInterrupts::default());
    let (mut dev, layout) = device("iso", &mut ram, DriveMedium::CdRom, 2048, interrupts);
    bring_up(&mut dev, &layout);
    arm_msix(&mut dev, &layout);

    // READ CAPACITY (10): last LBA then block size, both big-endian.
    submit(
        &mut ram,
        &mut dev,
        &layout,
        &request(0, &[0x25, 0, 0, 0, 0, 0, 0, 0, 0, 0]),
        108 + 8,
        0,
    );
    assert!(await_used(&ram, 1, Duration::from_secs(5)));
    let cap = ram.get(RESPONSE_BUF + 108, 8);
    assert_eq!(
        u32::from_be_bytes([cap[4], cap[5], cap[6], cap[7]]),
        2048,
        "an optical drive has 2048-byte sectors; that is the sector size of \
         the media, not a choice"
    );
    assert_eq!(
        u32::from_be_bytes([cap[0], cap[1], cap[2], cap[3]]),
        1023,
        "1024 sectors were created, so the last addressable one is 1023"
    );

    // READ (10) of one sector must return 2048 bytes, not 512.
    submit(
        &mut ram,
        &mut dev,
        &layout,
        &request(0, &[0x28, 0, 0, 0, 0, 0, 0, 0, 1]),
        108 + 2048,
        1,
    );
    assert!(await_used(&ram, 2, Duration::from_secs(5)));
    assert_eq!(
        ram.get(RESPONSE_BUF, 108)[10],
        0x00,
        "reading a sector from the ISO must succeed"
    );
    dev.shutdown();
}

/// A large multi-descriptor read must return the file's bytes, in order.
///
/// Set `VMM_TEST_ISO` to a real ISO to run it. It reproduces the exact read
/// the Windows Boot Manager issues while loading `bootmgfw.efi` — 503
/// sectors, just over a megabyte — because that is where a boot from an
/// installer image actually stops if the scatter-gather walk is wrong. A
/// single-sector read exercises none of it: one descriptor, one `pread`, no
/// arithmetic to get wrong.
#[test]
fn a_megabyte_read_spanning_many_descriptors_matches_the_file() {
    let Ok(iso_path) = std::env::var("VMM_TEST_ISO") else {
        eprintln!("skipping: set VMM_TEST_ISO to a real ISO image");
        return;
    };
    const LBA: u64 = 1607;
    const BLOCKS: u32 = 503;
    const SECTOR: usize = 2048;
    let length = BLOCKS as usize * SECTOR;

    // Guest RAM big enough for the ring plus the payload.
    let mut ram = Ram::new_sized(RAM_SIZE.max(length + (RESPONSE_BUF as usize) + 0x1000));
    let interrupts = Arc::new(RecordingInterrupts::default());

    let queues = FIRST_REQUEST_QUEUE + 1;
    let layout = BarLayout::new(queues, 36);
    let mut transport = VirtioTransport::new("virtio-scsi", queues, QUEUE_SIZE, 36, 1u64 << 32);
    transport.bar_base = Some(BAR_BASE);
    let engine = libvmm_storage::engines::file::FileEngine::open_read_only(
        std::path::Path::new(&iso_path),
        2048,
        "iso",
    )
    .expect("open the ISO");
    let mut dev = VirtioScsiPci::new(
        "bigread",
        transport,
        ram.guest(),
        vec![ScsiTarget::new(0, Box::new(engine), DriveMedium::DvdRom)],
        interrupts,
        1,
        4,
        u64::from(layout.msix_table_offset),
        u64::from(layout.msix_pba_offset),
    )
    .expect("build the controller");
    dev.set_bar_base(BAR_BASE);
    bring_up(&mut dev, &layout);
    arm_msix(&mut dev, &layout);

    // A chain of 4 KiB data-in descriptors after the response header, which
    // is how a real driver scatters a transfer this size.
    const VIRTQ_DESC_F_NEXT: u16 = 1;
    const VIRTQ_DESC_F_WRITE: u16 = 2;
    // 64 KiB pieces: enough descriptors to exercise the scatter-gather walk
    // while fitting the ring. edk2 itself uses at most four descriptors per
    // request, so a real transfer this size arrives as one big buffer; the
    // split case is the harder one and covers both.
    const CHUNK: usize = 64 * 1024;
    let cdb = [
        0x28,
        0,
        (LBA >> 24) as u8,
        (LBA >> 16) as u8,
        (LBA >> 8) as u8,
        LBA as u8,
        0,
        (BLOCKS >> 8) as u8,
        BLOCKS as u8,
    ];
    let req = request(0, &cdb);
    ram.put(REQUEST_BUF, &req);

    let mut index = 0u16;
    ram.put(
        DESC_TABLE,
        &desc(REQUEST_BUF, req.len() as u32, VIRTQ_DESC_F_NEXT, 1),
    );
    index += 1;
    ram.put(
        DESC_TABLE + 16,
        &desc(RESPONSE_BUF, 108, VIRTQ_DESC_F_WRITE | VIRTQ_DESC_F_NEXT, 2),
    );
    index += 1;
    let data_base = RESPONSE_BUF + 0x1000;
    let chunks = length.div_ceil(CHUNK);
    for c in 0..chunks {
        let len = CHUNK.min(length - c * CHUNK);
        let last = c + 1 == chunks;
        let flags = if last {
            VIRTQ_DESC_F_WRITE
        } else {
            VIRTQ_DESC_F_WRITE | VIRTQ_DESC_F_NEXT
        };
        ram.put(
            DESC_TABLE + u64::from(index) * 16,
            &desc(data_base + (c * CHUNK) as u64, len as u32, flags, index + 1),
        );
        index += 1;
    }
    assert!(
        usize::from(index) <= QUEUE_SIZE as usize,
        "the chain must fit the ring"
    );

    ram.put(AVAIL_RING, &0u16.to_le_bytes());
    ram.put(AVAIL_RING + 4, &0u16.to_le_bytes());
    ram.put(AVAIL_RING + 2, &1u16.to_le_bytes());
    let doorbell = BAR_BASE
        + u64::from(layout.notify_offset)
        + u64::from(FIRST_REQUEST_QUEUE) * u64::from(layout.notify_off_multiplier);
    dev.write(doorbell, &0u16.to_le_bytes());

    assert!(
        await_used(&ram, 1, Duration::from_secs(10)),
        "no completion"
    );
    assert_eq!(ram.get(RESPONSE_BUF, 108)[10], 0x00, "GOOD status");

    let expected = {
        use std::io::{Read, Seek, SeekFrom};
        let mut f = std::fs::File::open(&iso_path).expect("open the ISO");
        f.seek(SeekFrom::Start(LBA * SECTOR as u64)).expect("seek");
        let mut buf = vec![0u8; length];
        f.read_exact(&mut buf).expect("read");
        buf
    };
    let got = ram.get(data_base, length);
    assert_eq!(
        got.len(),
        expected.len(),
        "the whole transfer must be delivered"
    );
    if got != expected {
        let at = got
            .iter()
            .zip(&expected)
            .position(|(a, b)| a != b)
            .unwrap_or(0);
        panic!(
            "byte {at} of the transfer differs: got {:#04x}, expected {:#04x} \
             (sector {}, offset {} within it)",
            got[at],
            expected[at],
            at / SECTOR,
            at % SECTOR
        );
    }
    dev.shutdown();
}
