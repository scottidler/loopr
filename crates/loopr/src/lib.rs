use std::path::Path;

pub mod cli;
pub mod commands;
pub mod config;
pub mod daemon;
pub mod error;
pub mod guard;
pub mod logs;
pub mod output;
pub mod session;
pub mod summary;
pub mod target;
pub mod transport;

pub use cli::{Cli, Command, DaemonCmd, LogsCmd, SessionsCmd};
pub use error::LooprError;
pub use output::Format;

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
    // Normalize bare invocation (`loopr` with no subcommand) to `Command::Tui`.
    // Every downstream code path (auto-fork, dispatch) matches on a concrete
    // Command, not an Option.
    let command = cli.command.unwrap_or(Command::Tui);

    // `init` must not silently re-root. `target::resolve` walks `-C`/CWD up
    // to the enclosing git toplevel (correct for READ verbs operating on an
    // existing target), but `loopr -C ~/repo/subdir init` walking up to
    // `~/repo` would write `.loopr/`, hooks, and excludes into the WRONG
    // place. Refuse when the named path differs from the walked root; read
    // verbs keep the convenient walk.
    if matches!(command, Command::Init { .. }) {
        let named = target::canonical_start(cli.chdir.as_deref(), env_target.as_deref(), &cwd)?;
        if named != effective {
            return Err(LooprError::InitTargetMismatch {
                named,
                resolved: effective,
            });
        }
    }

    match &command {
        Command::Daemon {
            cmd: DaemonCmd::Start {
                foreground: false,
                accept_corruption,
            },
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
            daemon::ensure_daemon(&effective, *accept_corruption)?;
            println!("daemon started");
            return Ok(());
        }
        Command::Daemon {
            cmd: DaemonCmd::Start {
                foreground: true,
                accept_corruption,
            },
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
            // Clean stale sentinel state before claiming the pid file.
            // The background fork path does this in `ensure_daemon`, but the
            // foreground branch bypasses `ensure_daemon` and would otherwise
            // hit an opaque `LockLost` from a stale pid file left by a
            // SIGKILLed predecessor. The alive-check above already rejected
            // a LIVE daemon, so anything left here is stale.
            daemon::preflight_clean(&effective);
            // Foreground daemon: this process IS the daemon. No fork. Run
            // daemon_main directly so its own telemetry init is the only
            // subscriber installation on this process.
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .map_err(|e| LooprError::DaemonStartup(format!("runtime build: {e}")))?;
            return rt.block_on(daemon::daemon_main(effective, *accept_corruption));
        }
        Command::Plan { .. }
        | Command::Plans
        | Command::Works
        | Command::Bundles
        | Command::Ticks
        | Command::Show { .. }
        | Command::Director { .. } => {
            // Client commands that need a live daemon: ensure one exists
            // before the parent installs its own telemetry subscriber.
            //
            // `daemon status` is deliberately NOT in this arm: a read-only
            // status query must not auto-fork a daemon (a read becoming a
            // mutate, making "no daemon running" nearly unreachable). It
            // falls through to dispatch -> daemon_status, which handles the
            // no-daemon case by printing "no daemon running".
            daemon::ensure_daemon_if_needed(&effective)?;
        }
        _ => {}
    }

    let label = command.label();

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
    let session_id = session::resolve_session_id(&effective, cli.session.as_deref())?;
    let target_slug =
        telemetry::target_slug(&effective).map_err(|e| LooprError::TelemetryInit(format!("target_slug: {e}")))?;
    let process_runs_dir = telemetry::session_target_dir(&session_id, &target_slug)
        .map_err(|e| LooprError::TelemetryInit(format!("session_target_dir: {e}")))?
        .join("runs");
    std::fs::create_dir_all(&process_runs_dir)
        .map_err(|e| LooprError::TelemetryInit(format!("mkdir {}: {e}", process_runs_dir.display())))?;
    let process_id = telemetry::ProcessId::allocate(&process_runs_dir)
        .map_err(|e| LooprError::TelemetryInit(format!("process id alloc: {e}")))?;
    let guard = telemetry::init(&effective, &session_id, &target_slug, &process_id, &directive)
        .map_err(|e| LooprError::TelemetryInit(e.to_string()))?;
    let invocation = tracing::info_span!(
        "loopr.invocation",
        session_id = %session_id,
        process_id = %process_id,
        target_slug = %target_slug,
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

    dispatch(&effective, &session_id, &process_id, cli.output, command)
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

fn dispatch(
    target: &Path,
    session_id: &telemetry::SessionId,
    process_id: &telemetry::ProcessId,
    output_format: Option<output::Format>,
    command: Command,
) -> Result<(), LooprError> {
    match command {
        Command::Init { force } => commands::init::run(target, force),
        Command::Plan { cmd } => match cmd {
            cli::PlanCmd::Create { goal } => plan_create_command(target, goal),
            cli::PlanCmd::Override { plan_id, to } => plan_override_command(target, plan_id, to),
        },
        Command::Plans => commands::list::run(target, output_format, ipc::RecordKind::Plan),
        Command::Works => commands::list::run(target, output_format, ipc::RecordKind::Work),
        Command::Bundles => commands::list::run(target, output_format, ipc::RecordKind::Bundle),
        Command::Ticks => commands::list::run(target, output_format, ipc::RecordKind::Tick),
        Command::Show { id } => commands::show::run(target, output_format, id),
        Command::Daemon { cmd } => match cmd {
            // `Start` is handled above in `run` (pre-telemetry); it never
            // reaches `dispatch`.
            DaemonCmd::Start { .. } => {
                unreachable!("DaemonCmd::Start is fork-hoisted in run() before dispatch")
            }
            DaemonCmd::Stop => daemon_stop(target),
            DaemonCmd::Status => daemon_status(target),
        },
        Command::Logs { cmd } => match cmd {
            // `logs tail` excludes the caller's own process dir (whose
            // `loopr.log` is currently receiving this invocation's own
            // events and would otherwise shadow the interesting log).
            // `logs runs` excludes the current session so past sessions
            // are listed; current session is implicit.
            LogsCmd::Tail { lines } => logs::handle_tail(target, lines, Some(process_id)),
            LogsCmd::Runs => logs::handle_runs(target, Some(session_id)),
        },
        Command::Sessions { cmd } => commands::sessions::run(target, cmd),
        Command::Director { cmd } => commands::director::run(target, cmd),
        Command::Tui => Err(LooprError::TuiNotInstalled),
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
        client.handshake(None).await?;
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
#[tracing::instrument(
    name = "client.plan_command",
    level = "info",
    skip_all,
    fields(target = %target.display(), goal_len = goal.len(), subcommand = "plan"),
    err,
)]
fn plan_create_command(target: &Path, goal: String) -> Result<(), LooprError> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| LooprError::ClientIo(format!("runtime build: {e}")))?;
    rt.block_on(async {
        let mut client = transport::connect_or_wait(target).await?;
        client.handshake(None).await?;
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

/// `loopr plan override <plan-id> --to <status>` body. Phase 10 of
/// `docs/design/2026-05-09-director-phase-2.md`: revive a Stalled
/// Plan via the `Stalled -> Active` override, or transition the Plan
/// to any other operator-permitted status. The daemon performs the
/// FSM check + persist; this function is a thin IPC client.
#[tracing::instrument(
    name = "client.plan_override_command",
    level = "info",
    skip_all,
    fields(target = %target.display(), plan_id = %plan_id, to = %to, subcommand = "plan-override"),
    err,
)]
fn plan_override_command(target: &Path, plan_id: String, to: String) -> Result<(), LooprError> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|e| LooprError::ClientIo(format!("runtime build: {e}")))?;
    rt.block_on(async {
        let mut client = transport::connect_or_wait(target).await?;
        client.handshake(None).await?;
        let params = ipc::PlanOverrideParams {
            plan_id,
            target_status: to,
        };
        let params_value = serde_json::to_value(&params)
            .map_err(|e| LooprError::ClientIo(format!("serialize plan.override params: {e}")))?;
        let (resp, _events) = client.request(ipc::MethodName::PlanOverride, params_value).await?;
        if let Some(err) = resp.error {
            return Err(LooprError::Rpc(err));
        }
        let result_value = resp
            .result
            .ok_or_else(|| LooprError::ClientIo("plan.override response missing result".into()))?;
        let result: ipc::PlanOverrideResult = serde_json::from_value(result_value)
            .map_err(|e| LooprError::ClientIo(format!("decode plan.override: {e}")))?;
        println!("plan:   {}", result.plan.id);
        println!("status: {}", result.plan.status);
        Ok(())
    })
}

#[cfg(test)]
mod tests;
