//! Brute-force lockout — §8.4.
//!
//! ```text
//! per-source-IP counter:
//!  auth_fail -> attempts += 1
//!  attempts >= max_auth_attempts(10) -> LOCKED for lockout_duration_secs(300)
//!  while LOCKED -> 429 Too Many Requests (WSS) / 401 (RTSPS), no credential check
//!  auth_success or lockout expiry -> attempts = 0
//! ```
//!
//! The policy applies to **both** the WSS and RTSPS auth paths, so one
//! instance of [`LockoutTable`] is shared between the two listeners.

use std::collections::HashMap;
use std::net::IpAddr;
use std::time::{Duration, Instant};

/// What the caller should do with a connection attempt.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Decision {
    /// Check the credentials.
    Proceed,
    /// Locked out: answer without checking credentials at all.
    Locked { remaining: Duration },
}

impl Decision {
    pub const fn is_locked(&self) -> bool {
        matches!(self, Decision::Locked { .. })
    }
}

#[derive(Debug, Clone, Copy)]
struct Entry {
    attempts: u32,
    locked_until: Option<Instant>,
}

/// Per-source-IP failure counters, shared by the WSS and RTSPS listeners.
pub struct LockoutTable {
    max_attempts: u32,
    lockout: Duration,
    entries: HashMap<IpAddr, Entry>,
}

impl LockoutTable {
    pub fn new(max_auth_attempts: u32, lockout_duration_secs: u64) -> Self {
        LockoutTable {
            max_attempts: max_auth_attempts,
            lockout: Duration::from_secs(lockout_duration_secs),
            entries: HashMap::new(),
        }
    }

    /// Build from `[control_wss]`, which is where §11 puts the knobs.
    pub fn from_config(cfg: &libvmm_config::ControlWss) -> Self {
        Self::new(cfg.max_auth_attempts, cfg.lockout_duration_secs)
    }

    /// Should this source be allowed to present credentials?
    ///
    /// Call this *before* any credential comparison: while locked, §8.4 says
    /// no credential check happens at all.
    pub fn check(&mut self, source: IpAddr) -> Decision {
        self.check_at(source, Instant::now())
    }

    /// Record a failed authentication.
    pub fn record_failure(&mut self, source: IpAddr) -> Decision {
        self.record_failure_at(source, Instant::now())
    }

    /// Record a success, which clears the counter.
    pub fn record_success(&mut self, source: IpAddr) {
        self.entries.remove(&source);
    }

    /// How many failures this source has accumulated.
    pub fn attempts(&self, source: IpAddr) -> u32 {
        self.entries.get(&source).map_or(0, |e| e.attempts)
    }

    // -- time-injected variants, so the policy is testable without sleeping --

    pub fn check_at(&mut self, source: IpAddr, now: Instant) -> Decision {
        let Some(entry) = self.entries.get_mut(&source) else {
            return Decision::Proceed;
        };
        match entry.locked_until {
            Some(until) if now < until => Decision::Locked {
                remaining: until - now,
            },
            Some(_) => {
                // Lockout expired: attempts reset to 0 (§8.4).
                entry.attempts = 0;
                entry.locked_until = None;
                Decision::Proceed
            }
            None => Decision::Proceed,
        }
    }

    pub fn record_failure_at(&mut self, source: IpAddr, now: Instant) -> Decision {
        let entry = self.entries.entry(source).or_insert(Entry {
            attempts: 0,
            locked_until: None,
        });
        entry.attempts += 1;
        if entry.attempts >= self.max_attempts {
            let until = now + self.lockout;
            entry.locked_until = Some(until);
            return Decision::Locked {
                remaining: self.lockout,
            };
        }
        Decision::Proceed
    }

    /// Drop entries whose lockout has expired, so the table cannot grow
    /// without bound from scanning traffic.
    pub fn evict_expired(&mut self, now: Instant) {
        self.entries.retain(|_, e| match e.locked_until {
            Some(until) => now < until,
            None => e.attempts > 0,
        });
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}
