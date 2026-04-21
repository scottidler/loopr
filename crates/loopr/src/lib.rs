use std::path::Path;

pub mod cli;
pub mod config;
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
            // Idempotency (AC 11): if a live, version-matching daemon is
            // already running, this is a no-op. Otherwise clean stale
            // sentinel state and fork fresh.
            let pid_file = daemon::sentinel::pid_path(&effective);
            if let Some(pid) = daemon::sentinel::read_pid(&pid_file)?
                && daemon::sentinel::is_daemon_alive(pid)
                && daemon::sentinel::version_matches(
                    &daemon::sentinel::version_path(&effective),
                    daemon::DAEMON_VERSION,
                )?
            {
                println!("daemon already running at pid {pid}");
                return Ok(());
            }
            daemon::ensure_daemon(&effective)?;
            println!("daemon started");
            return Ok(());
        }
        Command::Daemon {
            cmd: DaemonCmd::Start { foreground: true },
        } => {
            // AC 14: a background daemon already running must block a
            // foreground start with a clear error; we don't want the
            // LockLost path's internal message.
            let pid_file = daemon::sentinel::pid_path(&effective);
            if let Some(pid) = daemon::sentinel::read_pid(&pid_file)?
                && daemon::sentinel::is_daemon_alive(pid)
            {
                return Err(LooprError::DaemonStartup(format!(
                    "daemon already running at pid {pid}; use `loopr daemon stop` first"
                )));
            }
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
        | Command::List { .. }
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
        Command::Plan { goal } => plan_command(target, goal),
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
            // reaches `dispatch`.
            DaemonCmd::Start { .. } => Err(LooprError::StageUnimplemented {
                stage: 4,
                subcommand: "daemon-start",
            }),
            DaemonCmd::Stop => daemon_stop(target),
            DaemonCmd::Status => daemon_status(target),
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
        Command::List { kind } => list_command(target, kind),
    }
}

/// `loopr daemon stop` body. Idempotent: if no daemon is running, prints
/// "no daemon running" and exits 0. Otherwise SIGTERMs the daemon, polls
/// for exit with escalation to SIGKILL, and cleans up residual sentinels.
fn daemon_stop(target: &Path) -> Result<(), LooprError> {
    let pid_file = daemon::sentinel::pid_path(target);
    match daemon::sentinel::read_pid(&pid_file)? {
        None => {
            println!("no daemon running");
            Ok(())
        }
        Some(pid) if !daemon::sentinel::is_daemon_alive(pid) => {
            daemon::sentinel::clean(target);
            println!("no daemon running");
            Ok(())
        }
        Some(_) => {
            daemon::sentinel::kill_stale(target)?;
            Ok(())
        }
    }
}

/// `loopr daemon status` body. Connects to the daemon, handshakes, asks
/// for `system.status`, prints the response in a human-readable form.
/// Idempotent on the no-daemon path.
fn daemon_status(target: &Path) -> Result<(), LooprError> {
    let pid_file = daemon::sentinel::pid_path(target);
    match daemon::sentinel::read_pid(&pid_file)? {
        Some(pid) if daemon::sentinel::is_daemon_alive(pid) => {}
        _ => {
            println!("no daemon running");
            return Ok(());
        }
    }

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| LooprError::ClientIo(format!("runtime build: {e}")))?;
    rt.block_on(async {
        let mut client = transport::connect_or_wait(target).await?;
        client.handshake().await?;
        let (resp, _events) = client
            .request(ipc::MethodName::SystemStatus, serde_json::Value::Null)
            .await?;
        if let Some(err) = resp.error {
            return Err(LooprError::Rpc(err));
        }
        let result_value = resp
            .result
            .ok_or_else(|| LooprError::ClientIo("status response missing result".into()))?;
        let status: ipc::StatusResult =
            serde_json::from_value(result_value).map_err(|e| LooprError::ClientIo(format!("decode status: {e}")))?;
        println!("pid:           {}", status.pid);
        println!("started-at:    {}", status.started_at);
        println!("active-plans:  {}", status.active_plans);
        println!("active-works:  {}", status.active_works);
        Ok(())
    })
}

/// `loopr plan "x"` body: connect to the daemon, handshake, issue a
/// typed `MethodName::PlanCreate` request. Stage 5 replaces Stage 4's
/// `request_raw` escape hatch with this typed path; the daemon persists
/// the plan through its `Store` and returns the created record.
fn plan_command(target: &Path, goal: String) -> Result<(), LooprError> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| LooprError::ClientIo(format!("runtime build: {e}")))?;
    rt.block_on(async {
        let mut client = transport::connect_or_wait(target).await?;
        client.handshake().await?;
        let params = ipc::PlanCreateParams { goal };
        let params_value = serde_json::to_value(&params)
            .map_err(|e| LooprError::ClientIo(format!("serialize plan.create params: {e}")))?;
        let (resp, _events) = client.request(ipc::MethodName::PlanCreate, params_value).await?;
        if let Some(err) = resp.error {
            return Err(LooprError::Rpc(err));
        }
        let result_value = resp
            .result
            .ok_or_else(|| LooprError::ClientIo("plan.create response missing result".into()))?;
        let result: ipc::PlanCreateResult = serde_json::from_value(result_value)
            .map_err(|e| LooprError::ClientIo(format!("decode plan.create: {e}")))?;
        println!("plan:   {}", result.plan.id);
        println!("goal:   {}", result.plan.goal);
        println!("status: {}", result.plan.status);
        Ok(())
    })
}

/// `loopr list <kind>` dispatcher. Stage 5 wires only `plans`; the other
/// kinds return `StageUnimplemented` until their stage lands, and an
/// unrecognized kind is reported as a client-side error rather than
/// silently swallowed.
fn list_command(target: &Path, kind: String) -> Result<(), LooprError> {
    match kind.as_str() {
        "plans" => list_plans(target),
        "specs" | "phases" | "works" => Err(LooprError::StageUnimplemented {
            stage: 6,
            subcommand: "list",
        }),
        "bundles" | "ticks" => Err(LooprError::StageUnimplemented {
            stage: 7,
            subcommand: "list",
        }),
        other => Err(LooprError::ClientIo(format!(
            "unknown list kind: {other} (expected one of: plans, specs, phases, works, bundles, ticks)"
        ))),
    }
}

fn list_plans(target: &Path) -> Result<(), LooprError> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| LooprError::ClientIo(format!("runtime build: {e}")))?;
    rt.block_on(async {
        let mut client = transport::connect_or_wait(target).await?;
        client.handshake().await?;
        let (resp, _events) = client
            .request(ipc::MethodName::PlanList, serde_json::Value::Null)
            .await?;
        if let Some(err) = resp.error {
            return Err(LooprError::Rpc(err));
        }
        let result_value = resp
            .result
            .ok_or_else(|| LooprError::ClientIo("plan.list response missing result".into()))?;
        let result: ipc::PlanListResult =
            serde_json::from_value(result_value).map_err(|e| LooprError::ClientIo(format!("decode plan.list: {e}")))?;
        if result.plans.is_empty() {
            println!("no plans");
        } else {
            for plan in &result.plans {
                println!("{}  {:<10}  {}", plan.id, plan.status, plan.goal);
            }
        }
        Ok(())
    })
}

#[cfg(test)]
mod tests;
