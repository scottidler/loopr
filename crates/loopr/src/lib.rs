use std::path::Path;

pub mod cli;
pub mod daemon;
pub mod error;
pub mod guard;
pub mod logs;
pub mod target;
pub mod transport;

pub use cli::{Cli, Command, DaemonCmd, LogsCmd};
pub use error::LooprError;

pub fn run(cli: Cli) -> Result<(), LooprError> {
    let cwd = std::env::current_dir().map_err(|_| LooprError::TargetInvalid {
        path: std::path::PathBuf::from("."),
    })?;
    let env_target = std::env::var("LOOPR_TARGET").ok();
    let effective = target::resolve(cli.chdir.as_deref(), env_target.as_deref(), &cwd)?;
    guard::check(&effective)?;

    // Fork hoist: handle or ensure the daemon BEFORE any telemetry init.
    // `tracing::subscriber::set_global_default` sets a per-process "already
    // installed" flag; `fork()` COW-inherits that flag, so a second
    // telemetry::init in the grandchild would crash on AlreadyInitialized.
    // By routing every fork-triggering command through this block (which
    // runs PRE-telemetry), the parent never touches a subscriber before
    // the fork, and the grandchild inherits a clean subscriber state.
    //
    // This is also where `daemon start --foreground` is recognized so the
    // foreground branch (which IS the daemon) doesn't init a second
    // subscriber on top of the one `daemon_main` will install.
    match &cli.command {
        Command::Daemon {
            cmd: DaemonCmd::Start { foreground: false },
        } => {
            // Background daemon start: parent forks and exits. Never inits
            // client-side telemetry.
            daemon::ensure_daemon(&effective)?;
            println!("daemon started");
            return Ok(());
        }
        Command::Daemon {
            cmd: DaemonCmd::Start { foreground: true },
        } => {
            // Foreground daemon: this process IS the daemon. No fork. Run
            // daemon_main directly so its own telemetry init is the only
            // subscriber installation on this process.
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .map_err(|e| LooprError::DaemonStartup(format!("runtime build: {e}")))?;
            return rt.block_on(daemon::daemon_main(effective));
        }
        Command::Plan { .. }
        | Command::Decompose { .. }
        | Command::Execute { .. }
        | Command::Integrate
        | Command::Daemon { cmd: DaemonCmd::Status } => {
            // Client commands that need a live daemon: ensure one exists
            // before the parent installs its own telemetry subscriber.
            daemon::ensure_daemon_if_needed(&effective)?;
        }
        _ => {}
    }

    let label = cli.command.label();

    // Telemetry init. Declaration order is load-bearing: Rust drops locals in
    // reverse order at scope exit. We want:
    //   1. `enter`       dropped first  -> exits the span
    //   2. `invocation`  dropped next   -> emits the span's close event
    //   3. `guard`       dropped last   -> flushes the line writers
    // Do NOT add explicit drops; RAII handles ordering correctly.
    //
    // Variables are named (not `_guard` / `_enter`) because
    // `rules/rust.md` forbids the leading-underscore crutch for used locals;
    // these locals ARE used - their Drop timing is the whole point.
    let directive = resolve_log_directive(cli.log_level.as_deref());
    let runs_dir = effective.join(".loopr").join("runs");
    std::fs::create_dir_all(&runs_dir)
        .map_err(|e| LooprError::TelemetryInit(format!("create {}: {e}", runs_dir.display())))?;
    let run_id =
        telemetry::RunId::allocate(&runs_dir).map_err(|e| LooprError::TelemetryInit(format!("run id alloc: {e}")))?;
    let guard =
        telemetry::init(&effective, &run_id, &directive).map_err(|e| LooprError::TelemetryInit(e.to_string()))?;
    let invocation = tracing::info_span!(
        "loopr.invocation",
        run_id = %run_id,
        subcommand = label,
    );
    let enter = invocation.enter();
    tracing::info!("loopr::run dispatching subcommand={label}");
    tracing::debug!("loopr::run dispatching subcommand={label} at debug level");

    // RAII drop order at scope exit: `enter` (last-declared) drops first and
    // exits the span; `invocation` drops next and emits the span's close
    // event through the still-installed subscriber; `guard` drops last and
    // flushes the line writers. Explicit `drop(...)` calls would invert this
    // and truncate the close event - see the Open Questions row on
    // "Explicit `drop()` calls" in the Stage 2 design doc. Shared references
    // here keep clippy from flagging the liveness-only locals.
    let _ = &guard;
    let _ = &enter;

    dispatch(&effective, &run_id, cli.command)
}

/// Resolve the log-filter directive string from CLI flag > env var > default
/// (`info`). The string is NOT parsed here; `telemetry::init` validates once
/// at the top (before any I/O) and then lets each layer do its own fresh
/// `EnvFilter::try_new`. This sidesteps the old `filter_clone` hack that
/// round-tripped an already-parsed `EnvFilter` back through its `Display`
/// form, since `EnvFilter` does not impl `Clone`.
fn resolve_log_directive(flag: Option<&str>) -> String {
    flag.map(str::to_owned)
        .or_else(|| std::env::var(telemetry::LOG_ENV_VAR).ok())
        .unwrap_or_else(|| "info".to_string())
}

fn dispatch(target: &Path, run_id: &telemetry::RunId, command: Command) -> Result<(), LooprError> {
    match command {
        Command::Init => Err(LooprError::StageUnimplemented {
            stage: 5,
            subcommand: "init",
        }),
        Command::Plan { .. } => Err(LooprError::StageUnimplemented {
            stage: 5,
            subcommand: "plan",
        }),
        Command::Decompose { .. } => Err(LooprError::StageUnimplemented {
            stage: 6,
            subcommand: "decompose",
        }),
        Command::Execute { .. } => Err(LooprError::StageUnimplemented {
            stage: 7,
            subcommand: "execute",
        }),
        Command::Integrate => Err(LooprError::StageUnimplemented {
            stage: 8,
            subcommand: "integrate",
        }),
        Command::Daemon { cmd } => match cmd {
            // `Start` is handled above in `run` (pre-telemetry); it never
            // reaches `dispatch`. `Stop` and `Status` are Phase 5 bodies.
            DaemonCmd::Start { .. } => Err(LooprError::StageUnimplemented {
                stage: 4,
                subcommand: "daemon-start",
            }),
            DaemonCmd::Stop => Err(LooprError::StageUnimplemented {
                stage: 4,
                subcommand: "daemon-stop",
            }),
            DaemonCmd::Status => Err(LooprError::StageUnimplemented {
                stage: 4,
                subcommand: "daemon-status",
            }),
        },
        Command::Score { .. } => Err(LooprError::StageUnimplemented {
            stage: 9,
            subcommand: "score",
        }),
        Command::Logs { cmd } => match cmd {
            // `logs` subcommands pass the current run_id as the `exclude`
            // parameter so the query doesn't return its own in-flight run
            // dir (which would otherwise be newest and shadow the real
            // target of the query).
            LogsCmd::Tail { lines } => logs::handle_tail(target, lines, Some(run_id)),
            LogsCmd::Runs => logs::handle_runs(target, Some(run_id)),
        },
        Command::List { .. } => Err(LooprError::StageUnimplemented {
            stage: 5,
            subcommand: "list",
        }),
    }
}

#[cfg(test)]
mod tests;
