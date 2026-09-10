//! The VMM's control-action handler — what §8.5's actions actually do.
//!
//! The listener validates and dispatches; this decides. Every action returns
//! a `ControlError` on failure so the client gets an error frame carrying the
//! Appendix A code rather than a dropped connection (§8.5).

use libvmm_config::MachineConfig;
use libvmm_control::proto::{InputFrame, ServerFrame};
use libvmm_control::ActionHandler;
use libvmm_core::ControlError;
use libvmm_storage::backup;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;

/// A lifecycle request raised by a control client, for the main thread to act
/// on (§1.5: `RUNNING --(WSS powerdown)--> GUEST_SHUTDOWN`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Request {
    Powerdown,
    Reboot,
}

pub struct VmmActions {
    config: Arc<MachineConfig>,
    /// Set when a client asks for powerdown or reboot; the main thread polls.
    pending: std::sync::Mutex<Option<Request>>,
    /// §8.5 error 6423: only one backup at a time.
    backup_running: AtomicBool,
    pub inputs_received: AtomicU64,
    /// Shared with the console scanout so the picture can show what arrived.
    /// With no virtio-input datapath this is the only place an input event
    /// has an observable effect, which is what makes the loop testable.
    pub input_state: Arc<crate::console_source::SharedInput>,
}

impl VmmActions {
    pub fn new(config: Arc<MachineConfig>) -> Self {
        VmmActions {
            config,
            pending: std::sync::Mutex::new(None),
            backup_running: AtomicBool::new(false),
            inputs_received: AtomicU64::new(0),
            input_state: Arc::new(crate::console_source::SharedInput::default()),
        }
    }

    /// Take any pending lifecycle request.
    pub fn take_request(&self) -> Option<Request> {
        self.pending.lock().ok().and_then(|mut p| p.take())
    }

    fn raise(&self, request: Request) -> Result<(), ControlError> {
        self.pending
            .lock()
            .map(|mut p| *p = Some(request))
            .map_err(|_| ControlError::Internal {
                action: format!("{request:?}"),
                detail: "the request slot is poisoned".to_string(),
            })
    }
}

impl ActionHandler for VmmActions {
    fn on_input(&self, client: u64, frame: &InputFrame) -> Result<(), ControlError> {
        // The frame is already range-checked by the parser (§8.5), so what is
        // left is routing it to the right virtio-input function.
        let (bus, device, function) = frame.target_bdf();
        self.inputs_received.fetch_add(1, Ordering::Relaxed);
        log::trace!(
            "client {client}: input seq {} -> {bus:02x}:{device:02x}.{function}",
            frame.seq()
        );
        // The virtio-input datapath is not implemented in this build, so the
        // event is counted and routed but not injected. Reporting an error
        // here would make every keystroke an error frame; the boot log says
        // plainly that the datapath is absent.
        //
        // It is mirrored into the shared input state, which the console
        // scanout draws. That is not a substitute for injection — the guest
        // still sees nothing — but it makes the client-to-VMM half of the
        // path observable instead of silent.
        match frame {
            InputFrame::Keyboard { code, value, .. } => self.input_state.on_key(*code, *value),
            InputFrame::Tablet { x, y, buttons, .. } => {
                self.input_state
                    .on_pointer(*x, *y, buttons.left, buttons.right, buttons.middle)
            }
        }
        Ok(())
    }

    fn on_powerdown(&self, client: u64) -> Result<(), ControlError> {
        log::info!("client {client}: powerdown -> ACPI Power Button SCI");
        self.raise(Request::Powerdown)
    }

    fn on_reboot(&self, client: u64) -> Result<(), ControlError> {
        log::info!("client {client}: reboot -> pulse FADT.RESET_REG");
        self.raise(Request::Reboot)
    }

    fn on_backup(
        &self,
        client: u64,
        path: &str,
        progress: &dyn Fn(ServerFrame),
    ) -> Result<(), ControlError> {
        // §8.5 error 6423.
        if self
            .backup_running
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return Err(ControlError::BackupBusy);
        }
        let _guard = BackupGuard(&self.backup_running);

        log::info!("client {client}: backup -> {path}");

        // §10.1 step 2: PREFLIGHT. A non-snapshot engine aborts here rather
        // than producing an inconsistent copy.
        let drives = backup::preflight(&self.config).map_err(|e| ControlError::Internal {
            action: "backup".to_string(),
            detail: format!("[{}] {e}", e.code()),
        })?;

        log::info!(
            "  preflight passed: {} snapshot-capable drive(s)",
            drives.len()
        );

        // §10.1 step 3: QUIESCE, best-effort. There is no guest agent channel
        // yet, so this always times out into a crash-consistent backup, which
        // §10.1 explicitly permits with warning 8005.
        let quiesce = backup::QuiesceOutcome::CrashConsistent;
        if !quiesce.quiesced() {
            let warning = libvmm_core::BackupError::QuiesceTimeout {
                timeout_secs: self.config.backup.quiesce_timeout_secs,
            };
            log::warn!("  [{}] {warning}", warning.code());
        }

        // §10.1 step 4: SNAPSHOT each drive, plus firmware and TPM.
        let staging = std::path::Path::new(path)
            .parent()
            .unwrap_or_else(|| std::path::Path::new("."))
            .join(format!(".vmbk-staging-{}", std::process::id()));
        std::fs::create_dir_all(&staging).map_err(|e| ControlError::Internal {
            action: "backup".to_string(),
            detail: format!("[8010] creating {}: {e}", staging.display()),
        })?;
        let _cleanup = StagingGuard(staging.clone());

        let mut items = Vec::new();
        for drive in &drives {
            let engine = libvmm_storage::engines::open(
                &drive.binding(),
                &format!("drive {}", drive.drive_id),
                0,
                512,
            )
            .map_err(|e| ControlError::Internal {
                action: "backup".to_string(),
                detail: format!("[{}] {e}", e.code()),
            })?;

            let archive_path = backup::member_path_for_drive(drive);
            let destination = staging.join(format!("drive_{}.raw", drive.drive_id));
            let handle = backup::snapshot_or_8001(engine.as_ref(), &destination, drive.drive_id)
                .map_err(|e| ControlError::Internal {
                    action: "backup".to_string(),
                    detail: format!("[{}] {e}", e.code()),
                })?;
            log::info!("  snapshot {archive_path} via {:?}", handle.method);
            items.push(backup::Item {
                archive_path,
                source: handle.path,
            });
        }

        // Firmware and TPM images travel with the drives (§10.3).
        for (binding, archive_path) in [
            (&self.config.firmware.storage, backup::MEMBER_FIRMWARE),
            (&self.config.tpm.storage, backup::MEMBER_TPM),
        ] {
            if archive_path == backup::MEMBER_TPM && !self.config.tpm.enabled {
                continue;
            }
            let Some(source) = binding.file_path.as_ref() else {
                continue;
            };
            if !source.exists() {
                log::warn!("  {} is absent, omitting {archive_path}", source.display());
                continue;
            }
            let destination = staging.join(archive_path.replace('/', "_"));
            match std::fs::copy(source, &destination) {
                Ok(_) => items.push(backup::Item {
                    archive_path: archive_path.to_string(),
                    source: destination,
                }),
                Err(e) => {
                    return Err(ControlError::Internal {
                        action: "backup".to_string(),
                        detail: format!("[8002] copying {archive_path}: {e}"),
                    })
                }
            }
        }

        // The ACPI tables the machine was built with (§10.3). These are the
        // injected blobs, not the generated set — a restore regenerates the
        // latter but cannot reconstruct an OEM licence table.
        for (path, archive_path) in [
            (self.config.acpi.msdm_path.as_ref(), backup::MEMBER_MSDM),
            (self.config.acpi.slic_path.as_ref(), backup::MEMBER_SLIC),
        ] {
            let Some(source) = path.filter(|p| p.exists()) else {
                continue;
            };
            let destination = staging.join(archive_path.replace('/', "_"));
            match std::fs::copy(source, &destination) {
                Ok(_) => items.push(backup::Item {
                    archive_path: archive_path.to_string(),
                    source: destination,
                }),
                Err(e) => {
                    return Err(ControlError::Internal {
                        action: "backup".to_string(),
                        detail: format!("[8002] copying {archive_path}: {e}"),
                    })
                }
            }
        }

        // The machine definition itself (§10.3). This is the *effective*
        // configuration — every §11 default resolved — so a restore does not
        // depend on the defaults of whatever build reads it back.
        let config_toml =
            toml::to_string_pretty(self.config.as_ref()).map_err(|e| ControlError::Internal {
                action: "backup".to_string(),
                detail: format!("[8010] serialising the configuration: {e}"),
            })?;
        let config_path = staging.join("vm_config.toml");
        std::fs::write(&config_path, config_toml).map_err(|e| ControlError::Internal {
            action: "backup".to_string(),
            detail: format!("[8010] writing vm_config.toml: {e}"),
        })?;
        items.push(backup::Item {
            archive_path: backup::MEMBER_CONFIG.to_string(),
            source: config_path,
        });

        // §10.1 step 5: THAW runs only if step 3 froze, which it did not.
        debug_assert!(!quiesce.needs_thaw());

        // §10.1 step 6: STREAM.
        let manifest = backup::Manifest {
            created_utc: utc_now(),
            vm_name: self.config.vm.name.clone(),
            vm_id: self.config.vm.id.clone(),
            quiesced: quiesce.quiesced(),
            zstd_level: self.config.backup.zstd_level,
            members: Vec::new(),
        };

        let result = backup::write_vmbk(
            std::path::Path::new(path),
            &items,
            manifest,
            self.config.backup.zstd_level,
            // §10.1 step 7: progress frames over WSS.
            &|done, total| progress(ServerFrame::backup_progress(done, total)),
        )
        .map_err(|e| ControlError::Internal {
            action: "backup".to_string(),
            detail: format!("[{}] {e}", e.code()),
        })?;

        log::info!(
            "  wrote {} ({} bytes from {}, {:.1}x, sha256 {})",
            result.path.display(),
            result.bytes_written,
            result.bytes_read,
            result.ratio(),
            result.sha256
        );
        progress(ServerFrame::backup_complete(path, &result.sha256));
        Ok(())
    }
}

/// An RFC 3339 timestamp for the manifest, without pulling in a date crate.
fn utc_now() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    // Days since the epoch, converted with the civil-from-days algorithm.
    let days = (secs / 86_400) as i64;
    let time = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        time / 3600,
        (time % 3600) / 60,
        time % 60
    )
}

/// Removes the snapshot staging directory however the backup ends (§10.1
/// "cleanup: release snapshots").
struct StagingGuard(std::path::PathBuf);

impl Drop for StagingGuard {
    fn drop(&mut self) {
        if let Err(e) = std::fs::remove_dir_all(&self.0) {
            log::warn!("backup: could not clean up {}: {e}", self.0.display());
        }
    }
}

/// Clears the busy flag however the backup ends.
struct BackupGuard<'a>(&'a AtomicBool);

impl Drop for BackupGuard<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::Release);
    }
}
