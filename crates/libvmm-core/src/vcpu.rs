//! The vCPU run loop — §1.4's `VCPUS_RUN`, and §1.2's `vcpu-N` threads.
//!
//! One thread per vCPU, each in `KVM_RUN` until the guest leaves the kernel
//! for something userspace has to answer. What answers is
//! [`DeviceModel`](crate::devices::DeviceModel); this module is the loop
//! around it and the machinery for stopping it.
//!
//! # Stopping a vCPU that is inside `KVM_RUN`
//!
//! A flag is not enough. `KVM_RUN` blocks in the kernel — a halted guest
//! with no pending interrupt can sit there indefinitely — and nothing
//! userspace sets will be noticed until it returns on its own. The
//! established answer, and the one Firecracker and crosvm both use, is a
//! signal: a real-time signal with **no** `SA_RESTART` makes the ioctl
//! return `EINTR`, at which point the loop is back in our hands and can read
//! the flag.
//!
//! A signal alone still leaves one window. Between the loop reading the
//! stop flag and the kernel reaching its `signal_pending()` check, a
//! signal that arrives is *consumed* by the handler and is no longer
//! pending, so the `KVM_RUN` that follows blocks anyway. That is a lost
//! wakeup, and on a halted guest it never resolves.
//!
//! The kernel's answer is `immediate_exit`, which it checks on the way
//! *into* `KVM_RUN`. It is not reachable soundly from here: `VcpuFd::run`
//! holds `&mut kvm_run` for the duration, so writing the flag from the
//! stopping thread aliases that reference (rust-vmm/kvm#373, still open at
//! 0.25). `KVM_SET_SIGNAL_MASK`, which is how QEMU closes the window
//! atomically, is not exposed by kvm-ioctls at all.
//!
//! So the kick repeats instead. `shutdown` keeps signalling until the
//! thread is observed finished, which is the same guarantee by a different
//! route: a signal consumed in the window is simply followed by another,
//! and the one that lands while the vCPU is blocked returns `EINTR`.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use crate::devices::DeviceModel;
use crate::error::{KvmError, VmmResult};

/// Why a vCPU stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// The VMM asked it to.
    Stopped,
    /// The guest asked to power off (ACPI S5, or `KVM_SYSTEM_EVENT_SHUTDOWN`).
    PowerOff,
    /// The guest asked to reboot.
    Reset,
    /// A triple fault. Almost always the guest's own doing, but on a
    /// hypervisor this young it is at least as likely to be ours.
    TripleFault,
    /// KVM could not run the guest, or an exit could not be serviced.
    Fault(String),
}

impl std::fmt::Display for Outcome {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Outcome::Stopped => write!(f, "stopped by the VMM"),
            Outcome::PowerOff => write!(f, "the guest powered off"),
            Outcome::Reset => write!(f, "the guest requested a reset"),
            Outcome::TripleFault => write!(f, "triple fault"),
            Outcome::Fault(detail) => write!(f, "fault: {detail}"),
        }
    }
}

/// Counters across all vCPUs, for the shutdown report.
#[derive(Default)]
pub struct ExitCounters {
    pub io_in: AtomicU64,
    pub io_out: AtomicU64,
    pub mmio_read: AtomicU64,
    pub mmio_write: AtomicU64,
    pub halts: AtomicU64,
    pub interrupted: AtomicU64,
    pub other: AtomicU64,
}

impl ExitCounters {
    pub fn total(&self) -> u64 {
        self.io_in.load(Ordering::Relaxed)
            + self.io_out.load(Ordering::Relaxed)
            + self.mmio_read.load(Ordering::Relaxed)
            + self.mmio_write.load(Ordering::Relaxed)
            + self.halts.load(Ordering::Relaxed)
            + self.other.load(Ordering::Relaxed)
    }

    pub fn summary(&self) -> String {
        format!(
            "{} exit(s): {} io-in, {} io-out, {} mmio-read, {} mmio-write, {} halt, {} other \
             ({} interrupted)",
            self.total(),
            self.io_in.load(Ordering::Relaxed),
            self.io_out.load(Ordering::Relaxed),
            self.mmio_read.load(Ordering::Relaxed),
            self.mmio_write.load(Ordering::Relaxed),
            self.halts.load(Ordering::Relaxed),
            self.other.load(Ordering::Relaxed),
            self.interrupted.load(Ordering::Relaxed),
        )
    }
}

// ---------------------------------------------------------------------------
// The stop signal
// ---------------------------------------------------------------------------

#[cfg(target_os = "linux")]
mod signal {
    use std::sync::Once;

    /// The signal that interrupts `KVM_RUN`.
    ///
    /// A real-time signal rather than `SIGUSR1`: those are the process's to
    /// use, and a hypervisor that steals one breaks any embedder that wanted
    /// it. `SIGRTMIN` as glibc reports it already excludes the numbers NPTL
    /// reserves for itself.
    pub fn vcpu_signal() -> libc::c_int {
        libc::SIGRTMIN()
    }

    extern "C" fn handler(_signum: libc::c_int) {
        // Deliberately empty. The delivery is the message: it makes the
        // blocked `KVM_RUN` return EINTR, and the loop takes it from there.
    }

    static INSTALL: Once = Once::new();

    /// Install the handler once per process.
    pub fn install() {
        INSTALL.call_once(|| {
            // SAFETY: `action` is fully initialised below before use, and
            // `sigaction` is called with a valid signal number and a
            // function pointer with C ABI.
            unsafe {
                let mut action: libc::sigaction = std::mem::zeroed();
                action.sa_sigaction = handler as *const () as usize;
                // No SA_RESTART: restarting is exactly what must not happen,
                // because a restarted ioctl never returns to the loop.
                action.sa_flags = 0;
                libc::sigemptyset(&mut action.sa_mask);
                libc::sigaction(vcpu_signal(), &action, std::ptr::null_mut());
            }
        });
    }

    /// Poke a thread so its `KVM_RUN` returns.
    pub fn poke(thread: libc::pthread_t) {
        if thread != 0 {
            // SAFETY: `thread` was published by that thread itself from
            // `pthread_self` and the thread is joined before this struct is
            // dropped, so the handle cannot be stale.
            unsafe {
                libc::pthread_kill(thread, vcpu_signal());
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The running machine
// ---------------------------------------------------------------------------

/// The state every vCPU thread shares.
pub struct RunState {
    pub devices: Mutex<DeviceModel>,
    pub exits: ExitCounters,
    stop: AtomicBool,
    outcome: Mutex<Option<Outcome>>,
    /// `pthread_t` per vCPU, published by each thread as it starts.
    threads: Mutex<Vec<u64>>,
}

impl RunState {
    pub fn new(devices: DeviceModel) -> Arc<Self> {
        Arc::new(RunState {
            devices: Mutex::new(devices),
            exits: ExitCounters::default(),
            stop: AtomicBool::new(false),
            outcome: Mutex::new(None),
            threads: Mutex::new(Vec::new()),
        })
    }

    pub fn stopping(&self) -> bool {
        self.stop.load(Ordering::Acquire)
    }

    /// Record why the machine stopped. The first reason wins: a triple fault
    /// followed by the VMM stopping the other vCPUs must report the fault,
    /// not the tidy-up.
    pub fn finish(&self, outcome: Outcome) {
        if let Ok(mut slot) = self.outcome.lock() {
            if slot.is_none() {
                *slot = Some(outcome);
            }
        }
        self.stop.store(true, Ordering::Release);
    }

    pub fn outcome(&self) -> Option<Outcome> {
        self.outcome.lock().ok().and_then(|o| o.clone())
    }

    /// Ask every vCPU to stop, and make sure they notice.
    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::Release);
        #[cfg(target_os = "linux")]
        if let Ok(threads) = self.threads.lock() {
            for thread in threads.iter() {
                signal::poke(*thread as libc::pthread_t);
            }
        }
    }
}

/// How often `shutdown` re-sends the stop signal to a vCPU that has not
/// yet left `KVM_RUN`. Short enough that a stop feels immediate, long
/// enough not to spin a core against a thread that is already unwinding.
const KICK_INTERVAL: Duration = Duration::from_millis(2);

/// When to start saying out loud that a vCPU is not stopping.
const KICK_WARN_AFTER: Duration = Duration::from_secs(1);

/// The `vcpu-N` threads of §1.2, running.
pub struct RunningVcpus {
    state: Arc<RunState>,
    handles: Vec<JoinHandle<()>>,
}

impl RunningVcpus {
    pub fn state(&self) -> &Arc<RunState> {
        &self.state
    }

    /// Has the machine stopped on its own — powered off, reset or faulted?
    pub fn finished(&self) -> Option<Outcome> {
        self.state.outcome()
    }

    /// Stop every vCPU and wait for its thread.
    ///
    /// The kick is repeated rather than sent once. See this module's header:
    /// a signal delivered in the window between the loop's flag check and
    /// the kernel's is consumed without effect, and the vCPU then blocks in
    /// `KVM_RUN` with nothing left to wake it.
    pub fn shutdown(mut self) -> Outcome {
        self.state.request_stop();
        for handle in self.handles.drain(..) {
            let mut waited = Duration::ZERO;
            while !handle.is_finished() {
                std::thread::sleep(KICK_INTERVAL);
                waited += KICK_INTERVAL;
                if waited == KICK_WARN_AFTER {
                    log::warn!("a vCPU has not left KVM_RUN after {waited:?}; still signalling");
                }
                // Re-kick. Harmless if the thread is already on its way out.
                self.state.request_stop();
            }
            let _ = handle.join();
        }
        self.state.outcome().unwrap_or(Outcome::Stopped)
    }
}

#[cfg(target_os = "linux")]
pub use live::spawn;

#[cfg(target_os = "linux")]
mod live {
    use super::*;
    use kvm_ioctls::{VcpuExit, VcpuFd};

    /// KVM system-event types. Not re-exported by `kvm-bindings` in a form
    /// worth importing for two constants.
    const SYSTEM_EVENT_SHUTDOWN: u32 = 1;
    const SYSTEM_EVENT_RESET: u32 = 2;
    const SYSTEM_EVENT_CRASH: u32 = 3;

    /// Start one thread per vCPU (§1.2).
    ///
    /// The vCPUs are consumed: after this the run loop owns them, which is
    /// what makes "who may call `KVM_RUN`" answerable by the type system
    /// rather than by convention.
    pub fn spawn(vcpus: Vec<VcpuFd>, state: Arc<RunState>) -> VmmResult<RunningVcpus> {
        signal::install();

        let mut handles = Vec::with_capacity(vcpus.len());
        for (index, vcpu) in vcpus.into_iter().enumerate() {
            let index = index as u32;
            let state = Arc::clone(&state);
            let handle = std::thread::Builder::new()
                .name(format!("vcpu-{index}"))
                .spawn(move || {
                    // Publish this thread's identity so `request_stop` can
                    // interrupt a blocked KVM_RUN.
                    #[cfg(target_os = "linux")]
                    if let Ok(mut threads) = state.threads.lock() {
                        // SAFETY: `pthread_self` is always safe and returns
                        // this thread's own handle.
                        threads.push(unsafe { libc::pthread_self() } as u64);
                    }
                    run_loop(index, vcpu, &state);
                })
                .map_err(|e| KvmError::VcpuRun {
                    index,
                    detail: format!("spawning the vcpu-{index} thread: {e}"),
                })?;
            handles.push(handle);
        }

        Ok(RunningVcpus { state, handles })
    }

    fn run_loop(index: u32, mut vcpu: VcpuFd, state: &Arc<RunState>) {
        log::debug!("vcpu-{index}: entering KVM_RUN");
        let outcome = loop {
            if state.stopping() {
                break Outcome::Stopped;
            }
            // KVM does not clear this itself, and a stale 1 from any
            // source would turn every `KVM_RUN` into an instant return.
            // Clearing it is hygiene, not synchronisation — the stop path
            // is the repeated signal from `shutdown`.
            vcpu.set_kvm_immediate_exit(0);

            match vcpu.run() {
                Ok(exit) => match service(index, exit, state) {
                    Some(outcome) => break outcome,
                    None => continue,
                },
                Err(e) if e.errno() == libc::EINTR || e.errno() == libc::EAGAIN => {
                    // The stop signal, or a spurious wakeup. Either way the
                    // loop head decides what happens next.
                    state.exits.interrupted.fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                Err(e) => {
                    break Outcome::Fault(format!("vcpu-{index}: KVM_RUN: {e}"));
                }
            }
        };

        match &outcome {
            Outcome::Stopped => log::debug!("vcpu-{index}: stopped"),
            other => log::info!("vcpu-{index}: {other}"),
        }
        // A vCPU that stopped because it was told to must not overwrite the
        // reason the machine is stopping.
        if !matches!(outcome, Outcome::Stopped) {
            state.finish(outcome);
            state.request_stop();
        }
    }

    /// Service one exit. `Some` ends the run.
    fn service(index: u32, exit: VcpuExit<'_>, state: &Arc<RunState>) -> Option<Outcome> {
        let counters = &state.exits;
        match exit {
            VcpuExit::IoIn(port, data) => {
                counters.io_in.fetch_add(1, Ordering::Relaxed);
                with_devices(state, |d| d.io_read(port, data));
                None
            }
            VcpuExit::IoOut(port, data) => {
                counters.io_out.fetch_add(1, Ordering::Relaxed);
                with_devices(state, |d| d.io_write(port, data));
                None
            }
            VcpuExit::MmioRead(addr, data) => {
                counters.mmio_read.fetch_add(1, Ordering::Relaxed);
                with_devices(state, |d| d.mmio_read(addr, data));
                None
            }
            VcpuExit::MmioWrite(addr, data) => {
                counters.mmio_write.fetch_add(1, Ordering::Relaxed);
                with_devices(state, |d| d.mmio_write(addr, data));
                None
            }
            // With the LAPIC in the kernel this should not arrive at all —
            // KVM handles the halt itself. If it does, the guest is idle and
            // waiting, which is not a reason to stop running it.
            VcpuExit::Hlt => {
                counters.halts.fetch_add(1, Ordering::Relaxed);
                None
            }
            VcpuExit::Shutdown => {
                // On x86 this is a triple fault, not an orderly shutdown,
                // whatever the name suggests.
                Some(Outcome::TripleFault)
            }
            VcpuExit::SystemEvent(kind, _data) => match kind {
                SYSTEM_EVENT_SHUTDOWN => Some(Outcome::PowerOff),
                SYSTEM_EVENT_RESET => Some(Outcome::Reset),
                SYSTEM_EVENT_CRASH => Some(Outcome::Fault(format!(
                    "vcpu-{index}: the guest reported a crash"
                ))),
                other => Some(Outcome::Fault(format!(
                    "vcpu-{index}: unhandled system event {other}"
                ))),
            },
            VcpuExit::FailEntry(reason, cpu) => Some(Outcome::Fault(format!(
                "vcpu-{index}: KVM could not enter the guest on cpu {cpu}, hardware reason \
                 {reason:#x}"
            ))),
            VcpuExit::InternalError => Some(Outcome::Fault(format!(
                "vcpu-{index}: KVM internal error — the guest state is not one KVM can run"
            ))),
            VcpuExit::Intr => {
                counters.interrupted.fetch_add(1, Ordering::Relaxed);
                None
            }
            // Everything else is either benign or not reachable on this
            // machine's configuration. Counting rather than ignoring is what
            // makes an unexpected one visible.
            other => {
                counters.other.fetch_add(1, Ordering::Relaxed);
                log::debug!("vcpu-{index}: unhandled exit {other:?}");
                None
            }
        }
    }

    /// Take the device lock, tolerating poison.
    ///
    /// §0.1 forbids a panic on the datapath, and a vCPU thread is the
    /// datapath. A poisoned device model is a device model some other thread
    /// panicked while touching — worth continuing with, because the
    /// alternative is a hypervisor that stops the guest because one register
    /// read went wrong.
    /// Run `f` against the device model, surviving a panic inside it.
    ///
    /// A device model that panics used to take the vCPU thread with it, and
    /// the guest then hung forever on an MMIO access nobody would ever
    /// answer — a silent, undebuggable stop with the real error printed on
    /// a thread nobody was watching. That is much worse than a crash: it
    /// looks like the guest's fault.
    ///
    /// So the panic is caught, the machine is stopped with the reason
    /// attached, and the guest dies loudly instead. The lock is
    /// deliberately taken *outside* the guarded section so that a poisoned
    /// mutex is still recovered on the next access.
    fn with_devices(state: &Arc<RunState>, f: impl FnOnce(&mut DeviceModel)) {
        let mut devices = state
            .devices
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&mut devices)));
        if let Err(payload) = outcome {
            let what = panic_message(&payload);
            log::error!("a device model panicked: {what}");
            state.finish(Outcome::Fault(format!("device model panicked: {what}")));
        }
    }
}

/// Best effort at the text of a panic payload.
fn panic_message(payload: &Box<dyn std::any::Any + Send>) -> String {
    if let Some(s) = payload.downcast_ref::<&str>() {
        (*s).to_string()
    } else if let Some(s) = payload.downcast_ref::<String>() {
        s.clone()
    } else {
        "a panic with no message".to_string()
    }
}

/// Not Linux: there is no KVM, so there are no vCPU threads.
#[cfg(not(target_os = "linux"))]
pub fn spawn<T>(_vcpus: Vec<T>, _state: Arc<RunState>) -> VmmResult<RunningVcpus> {
    Err(KvmError::OpenDevice("KVM is Linux-only".to_string()).into())
}
