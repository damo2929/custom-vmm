//! Boot orchestration — drives the §1.5 lifecycle.
//!
//! The ordering MUSTs are enforced by [`libvmm_core::lifecycle`]; this module
//! is what performs each phase's work and reports it.

use crate::topology;
use libvmm_config::MachineConfig;
use libvmm_core::acpi::AcpiTableSet;
use libvmm_core::lifecycle::{Event, Lifecycle, State};
use libvmm_core::memory::GuestMemoryMap;
use libvmm_core::smbios::SmbiosType1;
use libvmm_core::{kvm, VmmError, VmmResult};
use libvmm_storage::backup;

/// Everything the boot phases produce, whether or not KVM was touched.
pub struct BootPlan {
    pub config: MachineConfig,
    pub memory: GuestMemoryMap,
    pub acpi: AcpiTableSet,
    pub smbios: Vec<u8>,
    pub fabric: topology::Fabric,
    pub threads: Vec<String>,
    pub bringup: Vec<kvm::BringupStep>,
    pub warnings: Vec<String>,
}

/// Run every phase up to (but not including) `KVM_SETUP`.
///
/// This is the part that needs no `/dev/kvm`, so `--check` can validate a
/// machine on any host — and the same code path runs on a real boot.
pub fn plan(config: MachineConfig) -> VmmResult<BootPlan> {
    let mut warnings = config.warnings();

    // MEM_ALLOC (layout).
    let memory = GuestMemoryMap::new(&config.memory)?;

    // FIRMWARE_MAP inputs: ACPI + SMBIOS.
    let acpi = libvmm_core::acpi::builder::build(&config, &memory, &|p| std::fs::read(p))?;
    warnings.extend(acpi.warnings.iter().cloned());
    let smbios = SmbiosType1::from_config(&config.vm).to_bytes(0x0100);

    // DEVICE_INIT: PCIe fabric and MSI routing.
    let fabric = topology::build(&config);
    let threads = topology::thread_plan(&config);
    let bringup = kvm::bringup_plan(&config, &memory, fabric.routing.len() as u32);

    // A backup would abort on this machine — worth knowing at boot, not when
    // an operator first tries to take one (§10.2).
    if let Err(e) = backup::preflight(&config) {
        warnings.push(format!("backup: {e} (error {})", e.code()));
    }

    Ok(BootPlan {
        config,
        memory,
        acpi,
        smbios,
        fabric,
        threads,
        bringup,
        warnings,
    })
}

/// Report what a plan describes, in the order §1.5 executes it.
pub fn describe(plan: &BootPlan) -> String {
    let cfg = &plan.config;
    let mut s = String::new();

    s.push_str(&format!(
        "machine {} ({})\n  {} vCPU, {} MiB RAM, {} storage queues (1:1 with vCPUs)\n\n",
        cfg.vm.name,
        cfg.vm.id,
        cfg.compute.vcpus,
        cfg.memory.size_mb,
        libvmm_storage::queue_count(cfg.compute.vcpus),
    ));

    s.push_str(&plan.memory.describe());

    s.push_str("\nPCIe fabric:\n");
    for d in &plan.fabric.devices {
        s.push_str(&format!(
            "  {}  {:<24} {} queue(s)\n",
            d.bdf, d.name, d.queues
        ));
    }

    s.push_str(&format!(
        "\nMSI-X: {} GSI routes reserved ({} programmed by the guest so far), no INTx lines (§1.4)\n",
        plan.fabric.routing.len(),
        plan.fabric.routing.programmed()
    ));
    s.push_str("\nqueue doorbells (bound to eventfds with KVM_IOEVENTFD, §2.2):\n");
    for (bdf, addresses) in &plan.fabric.doorbells {
        match addresses.as_slice() {
            [] => {}
            [only] => s.push_str(&format!("  {bdf}  {only:#012x}\n")),
            [first, .., last] => s.push_str(&format!(
                "  {bdf}  {first:#012x} .. {last:#012x}  ({} queues)\n",
                addresses.len()
            )),
        }
    }
    s.push_str(&format!(
        "\nECAM: {} function(s) enumerable at {:#x}\n",
        plan.fabric.bus.bdfs().count(),
        libvmm_core::memory::ECAM_BASE
    ));

    s.push_str("\nACPI tables:\n");
    for t in &plan.acpi.tables {
        s.push_str(&format!(
            "  {:#010x}  {:<5} {:>6} bytes{}\n",
            t.gpa,
            t.signature,
            t.bytes.len(),
            if t.injected {
                "  (injected verbatim)"
            } else {
                ""
            }
        ));
    }
    s.push_str(&format!(
        "  {:#010x}  RSDP  {:>6} bytes\n",
        plan.acpi.rsdp_gpa,
        plan.acpi.rsdp.len()
    ));

    s.push_str(&format!(
        "\nSMBIOS Type 1: {} bytes, serial = {:?} (verbatim, §3.2)\n",
        plan.smbios.len(),
        SmbiosType1::serial_from_bytes(&plan.smbios).unwrap_or_default()
    ));

    s.push_str(&format!(
        "\nthreads ({}):\n  {}\n",
        plan.threads.len(),
        plan.threads.join(", ")
    ));

    s.push_str(&format!(
        "\nKVM bring-up: {} steps (§1.4)\n",
        plan.bringup.len()
    ));

    if !plan.warnings.is_empty() {
        s.push_str("\nwarnings:\n");
        for w in &plan.warnings {
            s.push_str(&format!("  ! {w}\n"));
        }
    }
    s
}

/// Boot for real, as far as this build supports.
///
/// Each phase's work happens while the machine is *in* that state, so a
/// failure aborts with the right §1.5 tag. Phases this first cut does not
/// implement stop the machine explicitly rather than appearing to succeed.
pub fn run(plan: BootPlan, run_for: Option<std::time::Duration>) -> (Lifecycle, Option<VmmError>) {
    let mut lc = Lifecycle::new();

    // CONFIG_LOAD already succeeded — the plan could not have been built
    // otherwise.
    advance(&mut lc);

    // MEM_ALLOC + KVM_SETUP. `Machine::bringup` performs both, so a failure
    // is attributed by domain: a mapping failure aborts MEM_ALLOC, anything
    // else aborts KVM_SETUP.
    debug_assert_eq!(lc.state(), State::MemAlloc);
    let ram_mib = plan.memory.ram_bytes() / (1024 * 1024);

    #[cfg(target_os = "linux")]
    let machine = match kvm::Machine::bringup(&plan.config, plan.memory) {
        Ok(m) => m,
        Err(e) => {
            if !matches!(e, VmmError::Kvm(libvmm_core::KvmError::MemoryMap { .. })) {
                // Get into KVM_SETUP so the abort is tagged Phase::Kvm.
                advance(&mut lc);
            }
            lc.on(Event::Failed);
            return (lc, Some(e));
        }
    };
    log::info!(
        "MEM_ALLOC complete: {ram_mib} MiB in {} slots",
        machine.mappings.len()
    );
    advance(&mut lc);

    debug_assert_eq!(lc.state(), State::KvmSetup);
    log::info!(
        "KVM_SETUP complete: split irqchip ({} GSIs), {} vCPU at the OVMF reset vector",
        kvm::SPLIT_IRQCHIP_GSI_COUNT,
        machine.vcpus.len()
    );
    advance(&mut lc);

    // DEVICE_INIT: open the storage engines. An engine that cannot open must
    // fail the phase, not be skipped.
    debug_assert_eq!(lc.state(), State::DeviceInit);
    for drive in &plan.config.storage.drives {
        let target = format!("drive {}", drive.drive_id);
        match libvmm_storage::engines::open(&drive.binding(), &target, 0, 512) {
            Ok(engine) => log::info!(
                "  {target} on {}: {} bytes, {} blocks, persistent={}, snapshot={}",
                engine.kind().as_str(),
                engine.capacity(),
                engine.capacity_blocks(),
                engine.is_persistent(),
                drive.engine.is_snapshot_capable(),
            ),
            Err(e) => {
                lc.on(Event::Failed);
                return (lc, Some(e));
            }
        }
    }
    // §7.1's media plane is part of DEVICE_INIT: the display is a PCIe
    // function, so a host that cannot encode at all must fail the phase
    // here rather than at the first DESCRIBE, when a client is waiting.
    //
    // What it cannot do any more is prove that the *negotiated* encoder
    // opens. Under Amendment B.1 no codec is chosen until the first
    // DESCRIBE, so a codec-specific open failure necessarily surfaces
    // there; DEVICE_INIT proves the machine can encode something, which is
    // the part that is a device fault rather than a client mismatch.
    let media = match open_media_plane(&plan.config) {
        Ok(media) => media,
        Err(e) => {
            lc.on(Event::Failed);
            return (lc, Some(e));
        }
    };

    log::info!(
        "DEVICE_INIT complete: {} PCIe function(s), {} MSI-X GSI(s)",
        plan.fabric.devices.len(),
        plan.fabric.routing.len()
    );
    advance(&mut lc);

    // FIRMWARE_MAP: OVMF into the read-only slot, ACPI into low memory.
    debug_assert_eq!(lc.state(), State::FirmwareMap);
    let firmware = match std::fs::read(&plan.config.firmware.code_path) {
        Ok(f) => f,
        Err(e) => {
            lc.on(Event::Failed);
            return (
                lc,
                Some(
                    libvmm_core::KvmError::FirmwareLoad {
                        path: plan.config.firmware.code_path.display().to_string(),
                        detail: e.to_string(),
                    }
                    .into(),
                ),
            );
        }
    };
    if let Err(e) = machine
        .load_firmware(&firmware)
        .and_then(|_| machine.load_acpi(&plan.acpi))
    {
        lc.on(Event::Failed);
        return (lc, Some(e));
    }
    log::info!(
        "FIRMWARE_MAP complete: {} bytes of OVMF read-only at {:#x}, {} ACPI tables staged",
        firmware.len(),
        libvmm_core::memory::OVMF_CODE_BASE,
        plan.acpi.tables.len()
    );
    advance(&mut lc);

    // LISTENERS_UP — §1.5 requires this before any vCPU runs, so a client
    // cannot connect to a half-initialised machine.
    debug_assert!(lc.state().listeners_may_bind());
    let control = match start_control_listener(&plan.config) {
        Ok(Some(started)) => {
            log::info!(
                "LISTENERS_UP: control channel on wss://{}/console (§8.1)",
                started.address
            );
            Some(started)
        }
        Ok(None) => {
            log::info!("LISTENERS_UP: the control listener is disabled by configuration");
            None
        }
        Err(e) => {
            lc.on(Event::Failed);
            return (lc, Some(e));
        }
    };
    let console_listener = match start_media_plane(&plan.config, media.as_ref(), control.as_ref()) {
        Ok(started) => started,
        Err(e) => {
            lc.on(Event::Failed);
            return (lc, Some(e));
        }
    };
    advance(&mut lc);

    // VCPUS_RUN. The vCPU loop, MMIO exit servicing and the device datapaths
    // are not implemented in this first cut, so no guest code executes.
    //
    // The control plane *is* live, though, so rather than exiting we hold
    // here and service it: a client's `powerdown` and `reboot` drive the real
    // §1.5 runtime transitions (RUNNING -> GUEST_SHUTDOWN -> TEARDOWN -> EXIT,
    // and RUNNING -> RESET -> VCPUS_RUN).
    debug_assert!(lc.state().vcpus_may_start());
    log::warn!(
        "VCPUS_RUN: the vCPU run loop is not implemented in this build, so no guest code \
         executes. The machine is fully constructed and the control plane is live."
    );

    match control {
        Some(started) => {
            advance(&mut lc); // -> RUNNING
            serve_control_plane(&mut lc, &started, run_for);
            // Stop accepting console clients before the plane goes away,
            // so no session can join a machine that is shutting down.
            if let Some(console) = &console_listener {
                console
                    .stop
                    .store(true, std::sync::atomic::Ordering::Relaxed);
            }
            if let Some(plane) = &media {
                plane.shutdown();
                report_media(plane);
            }
            (lc, None)
        }
        None => (
            lc,
            Some(
                libvmm_core::KvmError::VcpuRun {
                    index: 0,
                    detail: "the vCPU run loop is not implemented in this build, and no control \
                             listener is enabled to hold the machine open"
                        .to_string(),
                }
                .into(),
            ),
        ),
    }
}

/// Summarise what the §7 media plane did over the session.
///
/// With no vCPU loop the scanout is a stand-in rather than a guest, but the
/// counts are real: they say whether capture ran, whether the encoder
/// produced anything, and whether any session had to be resynchronised.
fn report_media(plane: &libvmm_media::MediaPlane) {
    let s = plane.stats();
    log::info!(
        "media plane: {} session(s) served; {} frame(s) captured ({} superseded before \
         encode), {} encoded, {} bytes, {} over the ceiling, {} forced keyframe(s)",
        s.sessions_total,
        s.frames_captured,
        s.frames_dropped,
        s.frames_encoded,
        s.video_bytes,
        s.ceiling_breaches,
        s.forced_keyframes,
    );
    log::info!(
        "  audio: {} sample(s) captured, {} packet(s), {} bytes",
        s.pcm_samples,
        s.audio_packets,
        s.audio_bytes
    );
}

/// Hold at RUNNING, servicing §8.5 lifecycle actions until the guest is asked
/// to shut down or `run_for` elapses.
fn serve_control_plane(
    lc: &mut Lifecycle,
    started: &Started,
    run_for: Option<std::time::Duration>,
) {
    use crate::control::Request;

    log::info!("RUNNING: accepting control clients (powerdown or reboot to stop)");
    let deadline = run_for.map(|d| std::time::Instant::now() + d);

    loop {
        if deadline.is_some_and(|d| std::time::Instant::now() >= d) {
            log::info!("run duration elapsed; shutting down");
            lc.on(Event::Powerdown);
            break;
        }

        match started.actions.take_request() {
            Some(Request::Powerdown) => {
                // RUNNING --(WSS powerdown / ACPI)--> GUEST_SHUTDOWN
                lc.on(Event::Powerdown);
                log::info!("{} (ACPI Power Button SCI raised)", lc.state());
                break;
            }
            Some(Request::Reboot) => {
                // RUNNING --(WSS reboot)--> RESET --> VCPUS_RUN --> RUNNING
                lc.on(Event::Reboot);
                log::info!("{} (pulsing FADT.RESET_REG)", lc.state());
                advance(lc); // RESET -> VCPUS_RUN
                advance(lc); // VCPUS_RUN -> RUNNING
                log::info!("reset complete, back to {}", lc.state());
            }
            None => std::thread::sleep(std::time::Duration::from_millis(50)),
        }
    }

    // GUEST_SHUTDOWN --> TEARDOWN --> EXIT
    advance(lc);
    log::info!(
        "{}: {} input frame(s) were routed during this session",
        lc.state(),
        started
            .actions
            .inputs_received
            .load(std::sync::atomic::Ordering::Relaxed)
    );
    lc.on(Event::Exited);
}

/// Construct the §7 media plane (§1.2's `media-capture` and `media-encode`).
///
/// Nothing is spawned and no encoder is opened here: the plane probes the
/// host, reports what it can encode, and fails DEVICE_INIT only if the
/// answer is "nothing". Which codec gets opened is Amendment B.1's first
/// DESCRIBE, and the threads start at LISTENERS_UP.
fn open_media_plane(
    config: &MachineConfig,
) -> VmmResult<Option<std::sync::Arc<libvmm_media::MediaPlane>>> {
    if !config.display.enabled {
        log::info!("  display disabled by configuration; no media plane");
        return Ok(None);
    }
    let plane = libvmm_media::MediaPlane::new(config.clone(), None)?;
    let d = &config.display;
    log::info!(
        "  display {}x{}@{} — up to {} kbps (ceiling {}), codec chosen at the first DESCRIBE \
         (Amendment B.1)",
        d.width,
        d.height,
        d.framerate_cap,
        d.encoder.bitrate_kbps,
        d.encoder.max_bitrate_kbps,
    );
    Ok(Some(plane))
}

/// Start the media plane's threads and bind the RTSPS console (§7.2).
///
/// Ordering matters and is the §1.5 reason this happens at LISTENERS_UP:
/// `media-capture` and `media-encode` must be up before the listener
/// accepts, so a client cannot DESCRIBE a plane that has no threads.
///
/// The scanout the plane captures is a stand-in while no guest executes —
/// see `console_source`. It is wired to the control listener's input state,
/// so what a client types in its window comes back in the picture.
fn start_media_plane(
    config: &libvmm_config::MachineConfig,
    plane: Option<&std::sync::Arc<libvmm_media::MediaPlane>>,
    control: Option<&Started>,
) -> VmmResult<Option<RtspStarted>> {
    use libvmm_media::server::{Credentials, RtspServer};

    let Some(plane) = plane else {
        return Ok(None);
    };

    let input = control
        .map(|c| std::sync::Arc::clone(&c.actions.input_state))
        .unwrap_or_default();
    plane.start(Box::new(crate::console_source::DemoScanout::new(
        config.display.width,
        config.display.height,
        config.display.framerate_cap,
        input,
    )))?;
    log::info!("LISTENERS_UP: media-capture and media-encode running (§1.2)");

    if !config.display.rtsps.enabled {
        log::info!("LISTENERS_UP: the RTSPS console is disabled by configuration");
        return Ok(None);
    }

    let identity = libvmm_control::tls::SelfSignedIdentity::generate(&config.vm.name)?;
    let tls = libvmm_control::tls::server_config(&identity)?;
    let addr = format!("[::]:{}", config.display.rtsps.port);
    let server = std::sync::Arc::new(RtspServer::bind(
        &addr,
        tls,
        Credentials {
            username: config.display.rtsps.username.clone(),
            password: config.display.rtsps.password.clone(),
        },
        std::sync::Arc::clone(plane),
    )?);
    let bound = server
        .local_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| addr.clone());
    let stop = server.stop_handle();

    let serving = std::sync::Arc::clone(&server);
    std::thread::Builder::new()
        .name("rtsp-listener".to_string())
        .spawn(move || serving.accept_loop())
        .map_err(|e| {
            libvmm_core::MediaError::BadRequest(format!("spawning the rtsp-listener thread: {e}"))
        })?;

    log::info!(
        "LISTENERS_UP: RTSPS console on rtsps://{bound}{} (§7.2), 0..n sessions",
        config.display.rtsps.stream_path
    );
    Ok(Some(RtspStarted { stop }))
}

struct RtspStarted {
    stop: std::sync::Arc<std::sync::atomic::AtomicBool>,
}

/// Bind and serve the §8 control listener on the `wss-listener` thread.
///
/// Returns the bound address, or `None` when `[control_wss] enabled = false`.
fn start_control_listener(config: &libvmm_config::MachineConfig) -> VmmResult<Option<Started>> {
    use libvmm_control::ControlListener;

    if !config.control_wss.enabled {
        return Ok(None);
    }
    let actions = std::sync::Arc::new(crate::control::VmmActions::new(std::sync::Arc::new(
        config.clone(),
    )));
    let listener = std::sync::Arc::new(ControlListener::new(
        config.control_wss.clone(),
        &config.vm.name,
        std::sync::Arc::clone(&actions) as std::sync::Arc<dyn libvmm_control::ActionHandler>,
    )?);
    let socket = listener.bind()?;
    let addr = socket
        .local_addr()
        .map(|a| a.to_string())
        .unwrap_or_else(|_| listener.bind_address());

    // §1.2: one `wss-listener` thread, on housekeeping cores.
    std::thread::Builder::new()
        .name("wss-listener".to_string())
        .spawn(move || listener.serve(socket))
        .map_err(|e| libvmm_core::ControlError::Bind {
            addr: addr.clone(),
            detail: format!("spawning the wss-listener thread: {e}"),
        })?;

    Ok(Some(Started {
        address: addr,
        actions,
    }))
}

/// A running control listener and the handler it dispatches to.
pub struct Started {
    pub address: String,
    pub actions: std::sync::Arc<crate::control::VmmActions>,
}

/// Advance one phase, logging the transition.
fn advance(lc: &mut Lifecycle) {
    let from = lc.state();
    match lc.on(Event::Completed) {
        Some(to) => log::debug!("{from} -> {to}"),
        // The transition table is exhaustive over the boot path, so this
        // cannot happen; log rather than panic on the boot thread.
        None => log::error!("illegal lifecycle transition out of {from}"),
    }
}

/// Whether a state means the machine got as far as this build can go.
pub fn reached_expected_limit(state: State) -> bool {
    matches!(state, State::VcpusRun)
}
