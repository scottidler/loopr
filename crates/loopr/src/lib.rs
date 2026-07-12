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
        Command::Plan { .. } | Command::Work { .. } | Command::Director { .. } => {
            // Client commands that need a live daemon: ensure one exists
            // before the parent installs its own telemetry subscriber.
            // `work override` (Phase 18) aborts/retries an in-flight Work,
            // which only the running daemon can act on, so it ensures a
            // daemon the same way `plan override` does.
            //
            // `daemon status` is deliberately NOT in this arm: a read-only
            // status query must not auto-fork a daemon (a read becoming a
            // mutate, making "no daemon running" nearly unreachable). It
            // falls through to dispatch -> daemon_status, which handles the
            // no-daemon case by printing "no daemon running".
            //
            // Phase 16 of `docs/design/2026-07-11-verified-swarm.md`
            // extended that same reasoning to every other read verb:
            // `Plans`/`Works`/`Bundles`/`Ticks`/`Show` used to auto-fork
            // here too, which meant "no daemon running" was nearly
            // unreachable for a `plans`/`show` call. They now fall
            // through to `dispatch`, which checks for a live daemon
            // itself and reports "no daemon running" instead of forking
            // one (mirroring the `daemon_status` pattern above).
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
    // `Command::Sessions` verbs manage the active-session pointer EXPLICITLY
    // (new/resume/end). Routing them through the implicit allocate-and-claim
    // resolver makes `sessions new` create two sessions (orphaning one) and
    // `sessions end` allocate-then-end on a pointer-less target. They take a
    // read-only resolution that never claims the pointer.
    let session_id = if matches!(command, Command::Sessions { .. }) {
        session::resolve_session_id_readonly(&effective, cli.session.as_deref())?
    } else {
        session::resolve_session_id(&effective, cli.session.as_deref())?
    };
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
            cli::PlanCmd::Create { goal } => plan_create_command(target, goal, output_format),
            cli::PlanCmd::Override { plan_id, to } => plan_override_command(target, plan_id, to, output_format),
        },
        Command::Work { cmd } => match cmd {
            cli::WorkCmd::Override { work_id, status } => work_override_command(target, work_id, status, output_format),
        },
        Command::Plans => commands::list::run(target, output_format, ipc::RecordKind::Plan),
        Command::Works => commands::list::run(target, output_format, ipc::RecordKind::Work),
        Command::Bundles => commands::list::run(target, output_format, ipc::RecordKind::Bundle),
        Command::Ticks => commands::list::run(target, output_format, ipc::RecordKind::Tick),
        // `commands::show::run` checks for a live daemon itself, AFTER its
        // local id-prefix validation (a malformed id must fail on that
        // check alone, never touching the daemon-running probe or IPC).
        Command::Show { id } => commands::show::run(target, output_format, id),
        Command::Daemon { cmd } => match cmd {
            // `Start` is handled above in `run` (pre-telemetry); it never
            // reaches `dispatch`.
            DaemonCmd::Start { .. } => {
                unreachable!("DaemonCmd::Start is fork-hoisted in run() before dispatch")
            }
            DaemonCmd::Stop => daemon_stop(target),
            DaemonCmd::Status => daemon_status(target, output_format),
        },
        Command::Watch { plan } => commands::watch::run(target, plan),
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
        Command::Director { cmd } => commands::director::run(target, cmd, output_format),
        Command::Budget { cmd } => match cmd {
            cli::BudgetCmd::Reset => budget_reset_command(target, output_format),
        },
        Command::Tui => Err(LooprError::TuiNotInstalled),
    }
}

/// `loopr daemon stop` body. Idempotent: if no daemon is running, prints
/// "no daemon running" and exits 0. Otherwise SIGTERMs the daemon, polls
/// for exit with escalation to SIGKILL, and cleans up residual sentinels.
#[tracing::instrument(
    name = "client.daemon_stop",
    level = "info",
    skip_all,
    fields(target = %target.display(), subcommand = "daemon-stop"),
    err,
)]
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

/// Read-verb guard (Phase 16 of `docs/design/2026-07-11-verified-swarm.md`):
/// wraps `daemon::is_running` with the "no daemon running" print so every
/// caller that doesn't need to interleave its own local checks (unlike
/// `commands::show::run`, which checks its id-prefix first) can early
/// return on `false`. This is the exact check `daemon_status` used before
/// this phase; `budget_reset_command` shares it too.
fn daemon_is_running(target: &Path) -> Result<bool, LooprError> {
    if daemon::is_running(target)? {
        Ok(true)
    } else {
        println!("no daemon running");
        Ok(false)
    }
}

/// `loopr daemon status` body. Connects to the daemon, handshakes, asks
/// for `system.status`, and renders the `StatusResult` through
/// `output::render` (YAML for a TTY, JSON for a pipe; `-o` overrides). The
/// no-daemon path is plain text — it is not a structured data result.
#[tracing::instrument(
    name = "client.daemon_status",
    level = "info",
    skip_all,
    fields(target = %target.display(), subcommand = "daemon-status"),
    err,
)]
fn daemon_status(target: &Path, output_format: Option<output::Format>) -> Result<(), LooprError> {
    if !daemon_is_running(target)? {
        return Ok(());
    }

    let status: ipc::StatusResult =
        transport::ipc_call(target, ipc::MethodName::SystemStatus, &serde_json::Value::Null)?;
    let fmt = output::Format::resolve(output_format);
    let rendered = output::render(&status, fmt).map_err(|e| LooprError::ClientIo(format!("render status: {e}")))?;
    println!("{rendered}");
    Ok(())
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
fn plan_create_command(target: &Path, goal: String, output_format: Option<output::Format>) -> Result<(), LooprError> {
    let params = ipc::PlanCreateParams { goal };
    let result: ipc::PlanCreateResult = transport::ipc_call(target, ipc::MethodName::PlanCreate, &params)?;
    let fmt = output::Format::resolve(output_format);
    let rendered =
        output::render(&result, fmt).map_err(|e| LooprError::ClientIo(format!("render plan.create: {e}")))?;
    println!("{rendered}");
    Ok(())
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
    fields(target = %target.display(), plan_id = %plan_id, to = to.as_str(), subcommand = "plan-override"),
    err,
)]
fn plan_override_command(
    target: &Path,
    plan_id: String,
    to: cli::PlanOverrideTo,
    output_format: Option<output::Format>,
) -> Result<(), LooprError> {
    let params = ipc::PlanOverrideParams {
        plan_id,
        target_status: to.as_str().to_string(),
    };
    let result: ipc::PlanOverrideResult = transport::ipc_call(target, ipc::MethodName::PlanOverride, &params)?;
    let fmt = output::Format::resolve(output_format);
    let rendered =
        output::render(&result, fmt).map_err(|e| LooprError::ClientIo(format!("render plan.override: {e}")))?;
    println!("{rendered}");
    Ok(())
}

/// `loopr work override <work-id> --status <target>` body. Phase 18 of
/// `docs/design/2026-07-11-verified-swarm.md`: operator FSM override on a
/// single Work. `--status ready` retries a Blocked Work; `--status
/// blocked` aborts an InProgress Work (the daemon cancels its Implementer
/// task and reaps the subprocess tree). The daemon performs the FSM check
/// + persist + any re-dispatch/abort side effect; this is a thin IPC
/// client.
#[tracing::instrument(
    name = "client.work_override_command",
    level = "info",
    skip_all,
    fields(target = %target.display(), work_id = %work_id, status = status.as_str(), subcommand = "work-override"),
    err,
)]
fn work_override_command(
    target: &Path,
    work_id: String,
    status: cli::WorkOverrideTo,
    output_format: Option<output::Format>,
) -> Result<(), LooprError> {
    let params = ipc::WorkOverrideParams {
        work_id,
        target_status: status.as_str().to_string(),
    };
    let result: ipc::WorkOverrideResult = transport::ipc_call(target, ipc::MethodName::WorkOverride, &params)?;
    let fmt = output::Format::resolve(output_format);
    let rendered =
        output::render(&result, fmt).map_err(|e| LooprError::ClientIo(format!("render work.override: {e}")))?;
    println!("{rendered}");
    Ok(())
}

/// `loopr budget reset` body. Phase 15 of
/// `docs/design/2026-07-11-verified-swarm.md`: clears the daemon's
/// one-shot per-run budget soft-pause guard via `MethodName::BudgetReset`.
/// Deliberately NOT in `run()`'s auto-fork arm (mirrors `daemon status`):
/// resetting a budget-brake state on a daemon that does not exist is
/// meaningless, so this checks for a live daemon itself rather than
/// forking one just to reset a guard that starts clear on every boot.
#[tracing::instrument(
    name = "client.budget_reset_command",
    level = "info",
    skip_all,
    fields(target = %target.display(), subcommand = "budget-reset"),
    err,
)]
fn budget_reset_command(target: &Path, output_format: Option<output::Format>) -> Result<(), LooprError> {
    if !daemon_is_running(target)? {
        return Ok(());
    }

    let result: ipc::BudgetResetResult =
        transport::ipc_call(target, ipc::MethodName::BudgetReset, &serde_json::Value::Null)?;
    let fmt = output::Format::resolve(output_format);
    let rendered =
        output::render(&result, fmt).map_err(|e| LooprError::ClientIo(format!("render budget.reset: {e}")))?;
    println!("{rendered}");
    Ok(())
}

#[cfg(test)]
mod tests;
