//! Boot lifecycle state machine — §1.5.
//!
//! ```text
//! CONFIG_LOAD -> MEM_ALLOC -> KVM_SETUP -> DEVICE_INIT -> FIRMWARE_MAP
//!                                                            |
//!                                                       LISTENERS_UP
//!                                                            |
//!                                                        VCPUS_RUN -> RUNNING
//! ```
//!
//! The ordering MUST from §1.5 is enforced by the transition table, not by
//! convention: listeners bind only after `DEVICE_INIT` succeeds, and vCPUs
//! start only after listeners are up, so a client can never connect to a
//! half-initialised machine.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum State {
    ConfigLoad,
    MemAlloc,
    KvmSetup,
    DeviceInit,
    FirmwareMap,
    ListenersUp,
    VcpusRun,
    Running,
    /// `RUNNING --(WSS reboot)--> RESET --> VCPUS_RUN`
    Reset,
    /// `RUNNING --(WSS powerdown / ACPI)--> GUEST_SHUTDOWN`
    GuestShutdown,
    Teardown,
    Exit,
    /// Terminal failure; carries the phase that failed.
    Abort(Phase),
}

/// The phase an `ABORT` came from, matching the §1.5 diagram's labels.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Phase {
    Config,
    Mem,
    Kvm,
    Device,
    Firmware,
    Listeners,
}

/// Events that drive the machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Event {
    /// The current phase completed successfully.
    Completed,
    /// The current phase failed; the machine aborts.
    Failed,
    /// `{"action":"powerdown"}` or a guest-initiated ACPI S5.
    Powerdown,
    /// `{"action":"reboot"}` — pulses FADT.RESET_REG.
    Reboot,
    /// Teardown finished.
    Exited,
}

impl fmt::Display for State {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            State::ConfigLoad => "CONFIG_LOAD",
            State::MemAlloc => "MEM_ALLOC",
            State::KvmSetup => "KVM_SETUP",
            State::DeviceInit => "DEVICE_INIT",
            State::FirmwareMap => "FIRMWARE_MAP",
            State::ListenersUp => "LISTENERS_UP",
            State::VcpusRun => "VCPUS_RUN",
            State::Running => "RUNNING",
            State::Reset => "RESET",
            State::GuestShutdown => "GUEST_SHUTDOWN",
            State::Teardown => "TEARDOWN",
            State::Exit => "EXIT",
            State::Abort(p) => return write!(f, "ABORT({p:?})"),
        };
        f.write_str(s)
    }
}

impl State {
    /// The phase whose failure aborts from this state.
    const fn phase(self) -> Option<Phase> {
        match self {
            State::ConfigLoad => Some(Phase::Config),
            State::MemAlloc => Some(Phase::Mem),
            State::KvmSetup => Some(Phase::Kvm),
            State::DeviceInit => Some(Phase::Device),
            State::FirmwareMap => Some(Phase::Firmware),
            State::ListenersUp => Some(Phase::Listeners),
            _ => None,
        }
    }

    pub const fn is_terminal(self) -> bool {
        matches!(self, State::Exit | State::Abort(_))
    }

    /// True once the guest is executing.
    pub const fn is_live(self) -> bool {
        matches!(self, State::Running)
    }

    /// §1.5 ordering MUST: listeners bind in the `LISTENERS_UP` phase, which
    /// is reachable only once `DEVICE_INIT` and `FIRMWARE_MAP` have succeeded.
    /// Every earlier state answers false, so a client cannot connect to a
    /// half-initialised machine.
    pub const fn listeners_may_bind(self) -> bool {
        matches!(self, State::ListenersUp)
    }

    /// §1.5 ordering MUST: vCPUs start in the `VCPUS_RUN` phase, which
    /// follows `LISTENERS_UP` (or a `RESET`).
    pub const fn vcpus_may_start(self) -> bool {
        matches!(self, State::VcpusRun)
    }
}

/// Drives the §1.5 machine and records the path taken, for the boot log.
#[derive(Debug)]
pub struct Lifecycle {
    state: State,
    history: Vec<State>,
}

impl Default for Lifecycle {
    fn default() -> Self {
        Self::new()
    }
}

impl Lifecycle {
    pub fn new() -> Self {
        Lifecycle {
            state: State::ConfigLoad,
            history: vec![State::ConfigLoad],
        }
    }

    pub fn state(&self) -> State {
        self.state
    }

    pub fn history(&self) -> &[State] {
        &self.history
    }

    /// Apply an event. Returns the new state, or `None` if the event is not
    /// legal here — an illegal transition is a bug, and is reported rather
    /// than silently ignored.
    pub fn on(&mut self, event: Event) -> Option<State> {
        let next = Self::transition(self.state, event)?;
        self.state = next;
        self.history.push(next);
        Some(next)
    }

    fn transition(state: State, event: Event) -> Option<State> {
        use Event::*;
        use State::*;

        match (state, event) {
            // Failure in any boot phase aborts, tagged with that phase.
            (s, Failed) => s.phase().map(Abort),

            // The linear boot path.
            (ConfigLoad, Completed) => Some(MemAlloc),
            (MemAlloc, Completed) => Some(KvmSetup),
            (KvmSetup, Completed) => Some(DeviceInit),
            (DeviceInit, Completed) => Some(FirmwareMap),
            (FirmwareMap, Completed) => Some(ListenersUp),
            (ListenersUp, Completed) => Some(VcpusRun),
            (VcpusRun, Completed) => Some(Running),

            // Runtime transitions.
            (Running, Powerdown) => Some(GuestShutdown),
            (Running, Reboot) => Some(Reset),
            (Reset, Completed) => Some(VcpusRun),
            (GuestShutdown, Completed) => Some(Teardown),
            (Teardown, Exited) | (Teardown, Completed) => Some(Exit),

            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn happy_path_reaches_running() {
        let mut l = Lifecycle::new();
        for _ in 0..7 {
            l.on(Event::Completed).expect("boot step must be legal");
        }
        assert_eq!(l.state(), State::Running);
        assert!(l.state().is_live());
    }

    #[test]
    fn listeners_bind_only_after_device_init_and_firmware_map() {
        let mut l = Lifecycle::new();
        // CONFIG_LOAD, MEM_ALLOC, KVM_SETUP, DEVICE_INIT, FIRMWARE_MAP all refuse.
        for _ in 0..5 {
            assert!(
                !l.state().listeners_may_bind(),
                "{} must not bind listeners",
                l.state()
            );
            l.on(Event::Completed).unwrap();
        }
        assert_eq!(l.state(), State::ListenersUp);
        assert!(l.state().listeners_may_bind());
    }

    #[test]
    fn vcpus_start_only_after_listeners_are_up() {
        let mut l = Lifecycle::new();
        // Every phase through LISTENERS_UP refuses to start vCPUs.
        for _ in 0..6 {
            assert!(
                !l.state().vcpus_may_start(),
                "{} must not start vCPUs",
                l.state()
            );
            l.on(Event::Completed).unwrap();
        }
        assert_eq!(l.state(), State::VcpusRun);
        assert!(l.state().vcpus_may_start());
    }

    #[test]
    fn each_phase_aborts_with_its_own_tag() {
        let expected = [
            (0, Phase::Config),
            (1, Phase::Mem),
            (2, Phase::Kvm),
            (3, Phase::Device),
            (4, Phase::Firmware),
            (5, Phase::Listeners),
        ];
        for (steps, phase) in expected {
            let mut l = Lifecycle::new();
            for _ in 0..steps {
                l.on(Event::Completed).unwrap();
            }
            assert_eq!(l.on(Event::Failed), Some(State::Abort(phase)));
            assert!(l.state().is_terminal());
        }
    }

    #[test]
    fn reboot_returns_through_reset_to_vcpus_run() {
        let mut l = Lifecycle::new();
        for _ in 0..7 {
            l.on(Event::Completed).unwrap();
        }
        assert_eq!(l.on(Event::Reboot), Some(State::Reset));
        assert_eq!(l.on(Event::Completed), Some(State::VcpusRun));
        assert!(l.state().vcpus_may_start());
        assert_eq!(l.on(Event::Completed), Some(State::Running));
    }

    #[test]
    fn powerdown_tears_down_and_exits() {
        let mut l = Lifecycle::new();
        for _ in 0..7 {
            l.on(Event::Completed).unwrap();
        }
        assert_eq!(l.on(Event::Powerdown), Some(State::GuestShutdown));
        assert_eq!(l.on(Event::Completed), Some(State::Teardown));
        assert_eq!(l.on(Event::Exited), Some(State::Exit));
        assert!(l.state().is_terminal());
    }

    #[test]
    fn illegal_transitions_are_refused_not_ignored() {
        let mut l = Lifecycle::new();
        // You cannot reboot a machine that has not booted.
        assert_eq!(l.on(Event::Reboot), None);
        assert_eq!(l.state(), State::ConfigLoad);
    }
}
