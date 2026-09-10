//! `custom-vmm` — hypervisor entrypoint, KVM owner, control plane (§1.1).

#![deny(clippy::unwrap_used, clippy::expect_used)]

mod boot;
// The ActionHandler trait lives behind the TLS feature, because §8.2 permits
// no listener without it.
mod console_source;
mod control;
mod topology;

use clap::Parser;
use libvmm_config::MachineConfig;
use std::path::PathBuf;
use std::process::ExitCode;

/// Legacy-free native Rust KVM hypervisor.
#[derive(Parser, Debug)]
#[command(name = "custom-vmm", version, about, long_about = None)]
struct Args {
    /// Declarative machine configuration (§11).
    #[arg(short, long, value_name = "FILE")]
    config: PathBuf,

    /// Validate the configuration and print the machine plan without
    /// touching /dev/kvm.
    #[arg(long)]
    check: bool,

    /// Take a backup to PATH and exit (§10).
    #[arg(long, value_name = "PATH")]
    backup: Option<PathBuf>,

    /// Log level: error, warn, info, debug, trace.
    #[arg(long, default_value = "info")]
    log_level: String,

    /// Shut the machine down after this many seconds instead of waiting for a
    /// control client to ask.
    #[arg(long, value_name = "SECONDS")]
    run_for: Option<u64>,
}

fn main() -> ExitCode {
    let args = Args::parse();
    env_logger::Builder::new()
        .parse_filters(&args.log_level)
        .format_timestamp_micros()
        .init();

    match run(&args) {
        Ok(code) => code,
        Err(e) => {
            // §1.6: every error carries a stable numeric code.
            log::error!("[{} {}] {e}", e.domain(), e.code());
            ExitCode::from((e.code() / 1000).min(255) as u8)
        }
    }
}

fn run(args: &Args) -> libvmm_core::VmmResult<ExitCode> {
    // CONFIG_LOAD (§1.5). An unknown key or a violated invariant aborts here.
    let config = MachineConfig::load(&args.config)?;
    log::info!("loaded {} ({})", args.config.display(), config.vm.name);

    let plan = boot::plan(config)?;
    for w in &plan.warnings {
        log::warn!("{w}");
    }

    if args.check {
        print!("{}", boot::describe(&plan));
        println!("\nconfiguration is valid.");
        return Ok(ExitCode::SUCCESS);
    }

    if let Some(path) = &args.backup {
        return run_backup(&plan, path);
    }

    let (lifecycle, error) = boot::run(plan, args.run_for.map(std::time::Duration::from_secs));
    log::info!("lifecycle: {}", format_path(&lifecycle));

    match error {
        None => Ok(ExitCode::SUCCESS),
        Some(e) if boot::reached_expected_limit(lifecycle.state()) => {
            log::warn!("[{} {}] {e}", e.domain(), e.code());
            Ok(ExitCode::from(3))
        }
        Some(e) => Err(e),
    }
}

/// §10 — the CLI backup trigger. Runs the whole §10.1 sequence and writes
/// the `.vmbk`, reporting progress on the terminal instead of over WSS.
fn run_backup(plan: &boot::BootPlan, path: &std::path::Path) -> libvmm_core::VmmResult<ExitCode> {
    use libvmm_storage::backup;

    // §10.1 step 1: nice(19) + IOPRIO_CLASS_IDLE.
    backup::lower_priority(
        plan.config.backup.nice_priority,
        plan.config.backup.ioprio_idle,
    )?;

    let actions = control::VmmActions::new(std::sync::Arc::new(plan.config.clone()));
    let last_percent = std::cell::Cell::new(u64::MAX);

    // The handler emits the same frames the WSS path does; render them.
    let report = |frame: libvmm_control::proto::ServerFrame| match frame {
        libvmm_control::proto::ServerFrame::Progress {
            percent,
            bytes,
            total_bytes,
            ..
        } => {
            let step = (percent as u64) / 10;
            if last_percent.get() != step {
                last_percent.set(step);
                log::info!("  {percent}% ({bytes}/{total_bytes} bytes)");
            }
        }
        libvmm_control::proto::ServerFrame::Complete { path, sha256, .. } => {
            log::info!("complete: {path}");
            log::info!("sha256:   {sha256}");
        }
        other => log::debug!("{}", other.to_json()),
    };

    use libvmm_control::ActionHandler;
    match actions.on_backup(0, &path.display().to_string(), &report) {
        Ok(()) => Ok(ExitCode::SUCCESS),
        Err(e) => Err(e.into()),
    }
}

fn format_path(lc: &libvmm_core::Lifecycle) -> String {
    lc.history()
        .iter()
        .map(|s| s.to_string())
        .collect::<Vec<_>>()
        .join(" -> ")
}
