//! §1.4 `VCPUS_RUN`: the guest actually executes.
//!
//! Everything else in this tree can be tested against a plan or a buffer.
//! This cannot: the only evidence that `KVM_RUN` works is a guest
//! instruction having an effect the VMM can see. So these boot a handful of
//! real x86 bytes on a real vCPU and check what came out of the other side.
//!
//! The payload is 16-bit real mode, because that is the mode an x86 CPU
//! comes out of reset in and the mode §3.1's reset vector lands in. It is
//! assembled by hand — the bytes are annotated below — rather than pulled in
//! with an assembler crate, which would be a build dependency for nine
//! instructions.
//!
//! Where there is no `/dev/kvm` these skip, because the absence of a
//! hypervisor says nothing about the code (AGENTS.md).

#![cfg(target_os = "linux")]

use std::time::{Duration, Instant};

use libvmm_core::devices::{DeviceModel, SerialLog};
use libvmm_core::memory::{self, GuestMemoryMap};
use libvmm_core::pci::{Bdf, PciBus, PciFunction};
use libvmm_core::vcpu::{self, Outcome, RunState};

/// A machine small enough to build quickly and without hugepages.
fn config(vcpus: u32) -> libvmm_config::MachineConfig {
    let mut cfg = libvmm_config::MachineConfig::from_toml_str(include_str!(
        "../../../config/reference-vm.toml"
    ))
    .expect("the reference config must load");
    cfg.compute.vcpus = vcpus;
    cfg.memory.size_mb = 1024;
    cfg.memory.low_ram_mb = 1024;
    cfg.memory.high_ram_mb = 0;
    // 1 GiB hugepages are a host property, not something a test may assume.
    cfg.memory.hugepages_1gb = false;
    cfg
}

fn kvm_available() -> bool {
    std::path::Path::new("/dev/kvm").exists()
        && std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .open("/dev/kvm")
            .is_ok()
}

/// `out dx, al` for each byte of `text`, then a halt and a spin.
///
/// ```text
///   BA F8 03      mov dx, 0x3F8      ; COM1 transmit register
///   B0 xx         mov al, <byte>     ; repeated per character
///   EE            out dx, al
///   ...
///   F4            hlt                ; idle rather than burn the host core
///   EB FD         jmp -3             ; back to the hlt, forever
/// ```
fn print_program(text: &[u8]) -> Vec<u8> {
    let mut code = vec![0xBA, 0xF8, 0x03];
    for byte in text {
        code.push(0xB0);
        code.push(*byte);
        code.push(0xEE);
    }
    // Halt, then jump back to the halt. A guest that runs off the end of
    // its program executes whatever follows, which on a fresh mapping is
    // zeros — `add [bx+si], al` — and eventually triple-faults. Spinning
    // makes the end of the program an explicit state rather than an
    // accident.
    code.push(0xF4);
    code.push(0xEB);
    code.push(0xFD);
    code
}

/// Wrap `payload` in an image whose last 16 bytes are the reset vector.
///
/// `load_firmware` places an image so its *last* byte is at 0xFFFF_FFFF, but
/// execution begins at 0xFFFF_FFF0 — sixteen bytes earlier. Only a
/// 16-byte image therefore starts where the CPU starts, and every payload
/// here is longer than that. So the top sixteen bytes are a stub that jumps
/// backwards to the payload, which sits immediately below it:
///
/// ```text
///   0xFFFF_FFF0  E9 xx xx    jmp <payload>     ; the reset vector
///   0xFFFF_FFF3  F4 F4 ...   hlt padding       ; never reached
///   0xFFFF_FFFF                                ; top of memory
/// ```
///
/// The jump is `rel16` because the CPU is in real mode with CS.base at
/// 0xFFFF_0000, so IP is `address - 0xFFFF_0000` and the whole image has to
/// live in that top 64 KiB.
fn at_reset_vector(payload: &[u8]) -> Vec<u8> {
    const STUB: usize = 16;
    assert!(
        payload.len() < 0x8000,
        "the payload has to stay within reach of a 16-bit relative jump"
    );

    let mut image = payload.to_vec();

    // After the jump executes, IP is the address of the next instruction:
    // the stub's start plus its three bytes. The payload begins `len` bytes
    // below the stub, so the displacement is -(len + 3).
    let displacement = -((payload.len() as i32) + 3) as i16;
    image.push(0xE9);
    image.extend_from_slice(&displacement.to_le_bytes());
    // Halt padding out to the full sixteen bytes, so that a stub which
    // somehow fell through would stop rather than run into whatever the
    // loader left above it.
    image.resize(payload.len() + STUB, 0xF4);
    image
}

/// Bring up a machine with `program` at the reset vector and run it.
struct Booted {
    running: vcpu::RunningVcpus,
    log: std::sync::Arc<SerialLog>,
    // Keeps the VM, its memory mappings and the guest RAM alive for as long
    // as the vCPUs are running. Dropping it would unmap the memory out from
    // under them.
    _machine: libvmm_core::kvm::Machine,
}

fn boot(program: &[u8], vcpus: u32) -> Booted {
    let cfg = config(vcpus);
    let map = GuestMemoryMap::new(&cfg.memory).expect("memory map");
    let mut machine = libvmm_core::kvm::Machine::bringup(&cfg, map).expect("KVM bring-up");

    // `load_firmware` places the image at the *end* of the ROM region,
    // which is exactly where the reset vector needs it.
    machine
        .load_firmware(&at_reset_vector(program))
        .expect("load the program");

    let mut bus = PciBus::new();
    bus.insert(PciFunction::new(
        Bdf::new(0, 0, 0),
        0x8086,
        0x29C0,
        0x00_06_00_00,
        0,
    ));
    let log = SerialLog::new();
    let devices = DeviceModel::new(
        bus,
        cfg.memory.low_ram_mb * memory::MIB,
        cfg.memory.high_ram_mb * memory::MIB,
        std::sync::Arc::clone(&log),
    );

    let state = RunState::new(devices);
    let running = vcpu::spawn(machine.take_vcpus(), state).expect("vcpu threads");
    Booted {
        running,
        log,
        _machine: machine,
    }
}

/// Wait for the serial log to contain `needle`.
fn wait_for(log: &SerialLog, needle: &str, timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if log.text().contains(needle) {
            return true;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    false
}

#[test]
fn a_guest_instruction_reaches_the_serial_port() {
    if !kvm_available() {
        eprintln!("skipping: no usable /dev/kvm");
        return;
    }

    let booted = boot(&print_program(b"BOOT OK\n"), 1);
    let found = wait_for(&booted.log, "BOOT OK", Duration::from_secs(10));
    let text = booted.log.text();
    let outcome = booted.running.shutdown();

    assert!(
        found,
        "the guest's own `out dx, al` must reach the UART. Serial log was {text:?}, the run \
         ended {outcome}"
    );
}

#[test]
fn the_reset_vector_is_where_execution_begins() {
    if !kvm_available() {
        eprintln!("skipping: no usable /dev/kvm");
        return;
    }

    // The image's last sixteen bytes are a jump, placed so that they land
    // exactly on 0xFFFF_FFF0 — the address a physical x86 fetches from
    // after reset. Nothing else in the image is reachable except through
    // that jump: if the vCPU began anywhere else it would execute the
    // payload from the wrong offset, or the zeros below it, and nothing
    // would ever be printed.
    let booted = boot(&print_program(b"R\n"), 1);
    let found = wait_for(&booted.log, "R", Duration::from_secs(10));
    booted.running.shutdown();
    assert!(found, "execution must begin at {:#x}", memory::RESET_VECTOR);
}

#[test]
fn every_vcpu_runs_and_all_of_them_stop_when_asked() {
    if !kvm_available() {
        eprintln!("skipping: no usable /dev/kvm");
        return;
    }

    // §1.2 is one thread per vCPU. The application processors sit in
    // KVM_RUN waiting for the INIT-SIPI the boot processor never sends, so
    // the point of this test is the *stop*: a vCPU blocked in the kernel has
    // to be interruptible, or shutdown hangs forever.
    let booted = boot(&print_program(b"SMP\n"), 4);
    assert!(wait_for(&booted.log, "SMP", Duration::from_secs(10)));

    let started = Instant::now();
    let outcome = booted.running.shutdown();
    let took = started.elapsed();

    assert_eq!(outcome, Outcome::Stopped);
    assert!(
        took < Duration::from_secs(5),
        "stopping four vCPUs took {took:?}; a vCPU blocked in KVM_RUN is not being \
         interrupted"
    );
}

#[test]
fn the_exit_counters_record_what_the_guest_did() {
    if !kvm_available() {
        eprintln!("skipping: no usable /dev/kvm");
        return;
    }

    let booted = boot(&print_program(b"COUNT\n"), 1);
    assert!(wait_for(&booted.log, "COUNT", Duration::from_secs(10)));
    let exits = booted
        .running
        .state()
        .exits
        .io_out
        .load(std::sync::atomic::Ordering::Relaxed);
    booted.running.shutdown();

    // "COUNT\n" is six characters, each one `out dx, al`.
    assert!(
        exits >= 6,
        "expected at least one io-out exit per character, got {exits}"
    );
}

#[test]
fn a_guest_that_triple_faults_is_reported_as_such() {
    if !kvm_available() {
        eprintln!("skipping: no usable /dev/kvm");
        return;
    }

    // `ud2` on its own is not enough. Low RAM is mapped and zeroed, so the
    // real-mode interrupt vector for #UD is a perfectly valid 0000:0000,
    // and the CPU dispatches there and executes zeros — `add [bx+si], al`
    // — without ever faulting. To get a triple fault the exception has to
    // be *undeliverable*, so this loads an IDT with a limit of zero first:
    // vector 6 is then out of range, which raises #GP, which is equally
    // undeliverable, which is a double and then a triple fault.
    //
    // ```text
    //   31 C0                 xor ax, ax
    //   8E D8                 mov ds, ax          ; address low memory
    //   C7 06 00 10 00 00     mov word [0x1000], 0  ; IDT limit = 0
    //   C7 06 02 10 00 00     mov word [0x1002], 0  ; IDT base  = 0
    //   C7 06 04 10 00 00     mov word [0x1004], 0
    //   0F 01 1E 00 10        lidt [0x1000]
    //   0F 0B                 ud2
    // ```
    let program = [
        0x31, 0xC0, //
        0x8E, 0xD8, //
        0xC7, 0x06, 0x00, 0x10, 0x00, 0x00, //
        0xC7, 0x06, 0x02, 0x10, 0x00, 0x00, //
        0xC7, 0x06, 0x04, 0x10, 0x00, 0x00, //
        0x0F, 0x01, 0x1E, 0x00, 0x10, //
        0x0F, 0x0B, //
    ];
    let booted = boot(&program, 1);

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut outcome = None;
    while Instant::now() < deadline {
        if let Some(o) = booted.running.finished() {
            outcome = Some(o);
            break;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    booted.running.shutdown();

    assert_eq!(
        outcome,
        Some(Outcome::TripleFault),
        "a triple fault must be reported, not swallowed"
    );
}
