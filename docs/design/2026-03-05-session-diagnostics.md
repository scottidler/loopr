# Design Document: Session Diagnostics

**Author:** Scott Idler / Claude
**Date:** 2026-03-05
**Status:** Implemented
**Review Passes Completed:** 7/7

## Summary

Loopr's diagnostic data is fragmented across three layers (in-memory event buffers, unstructured per-agent log files, and TaskStore domain records), with the most valuable debugging context — chat transcripts, tool call details, LLM responses, and state transition timelines — lost on TUI/daemon restart. This design adds session-scoped log files, debug-level tap points at seven key locations (1a, 1b, 2–6), a `loopr diagnose` CLI subcommand for querying session history, and an auto-generated session summary for fast context injection into follow-up debugging sessions.

## Problem Statement

### Background

Loopr is a TUI-based orchestrator. The TUI and daemon are **separate OS processes from the same binary** — when you run `loopr` (TUI mode), it calls `ensure_daemon()` which checks for a running daemon process and spawns one via `std::process::Command::new(exe).arg("daemon").spawn()` if none exists. The TUI then connects to the daemon over a Unix domain socket (IPC). The daemon is the long-lived process that manages agents (Coordinator, Implementer, Reviewer, Researcher, Integrator), makes LLM calls, executes tools, and transitions domain records through state machines. Debugging requires understanding what happened across all these layers during a single daemon session.

The current logging infrastructure includes:
- **Main log:** `~/.local/share/loopr/logs/loopr.log` — single append-only file, no session boundaries
- **Per-agent logs:** `~/.local/share/loopr/logs/agents/agent-{type}-{agent_session_id}.log` — dual output (file + main log), hardcoded directory
- **In-memory ring buffers:** 1000 events per agent session, not persisted, lost on restart
- **Chat history:** `Vec<ChatMessage>` in TUI memory only, lost on exit
- **TaskStore:** Persists domain records (plans, specs, works, etc.) and AgentSession metadata, but NOT execution traces

### Problem

When debugging TUI failures, the developer must:
1. Exit the Claude Code session where they're getting help
2. Run loopr (TUI) and observe failures manually
3. Return to a new Claude Code session and **manually describe** what happened

There is no artifact the developer can hand to the next Claude session that contains the full diagnostic context of what loopr did. The main log is a single unsegmented file mixing all sessions. The in-memory event buffers are gone. The chat history is gone. Tool call arguments and results are gone.

### Goals
- Session-scoped log files: each daemon start writes to a new timestamped log file
- Debug-level logging at seven critical tap points that currently only emit in-memory events
- A `loopr diagnose` CLI subcommand that queries logs, TaskStore, and agent sessions
- Auto-generated session summary on daemon shutdown for fast context injection
- All new logging uses `log::debug!()` — no new recording infrastructure

### Non-Goals
- Structured/JSON logging format (text logs are grep-able and sufficient)
- Log rotation or garbage collection (out of scope, can be added later)
- TUI-side session replay or history browser
- Persisting the full in-memory event ring buffer to disk
- Changing per-agent logger format or behavior — only its output directory changes (from hardcoded to session-scoped)

## Proposed Solution

### Overview

Five changes, all building on the existing `log` crate infrastructure:

1. **Daemon session ID** — a correlation ID generated at daemon startup, propagated to all subsystems
2. **Session directory** — each daemon start creates a session folder; all logs (main + per-agent) and summary live inside it
3. **Seven debug tap points** — add `log::debug!()` calls where diagnostic data currently only exists in memory
4. **`loopr diagnose` subcommand** — CLI tool to dump all session context for LLM debugging
5. **Session summary** — auto-generate a `summary.md` file on daemon shutdown

### Architecture

```
~/.local/share/loopr/sessions/
├── 20260305T143200/                        # one folder per daemon session
│   ├── loopr.log                           # session main log
│   ├── summary.md                          # auto-generated on shutdown
│   └── agents/
│       ├── coordinator-ag01ABC.log         # per-agent logs
│       └── implementer-ag02DEF.log
├── 20260305T161045/                        # another session
│   ├── loopr.log
│   ├── summary.md
│   └── agents/
│       └── ...
└── latest -> 20260305T161045/              # symlink to most recent session
```

All diagnostic artifacts from a single daemon run are **physically colocated** in one directory. No join key needed to correlate files — they're in the same folder. `rm -rf` a session folder cleans up everything.

**TaskStore lives outside this tree** (in `.taskstore/`) because domain records (plans, specs, works) span multiple sessions. The `AgentSession.daemon_session_id` field provides the cross-reference from TaskStore records into the session folder.

### Component 0: Daemon Session ID

Every daemon start captures a **session ID** which is simply the startup timestamp in compact form: `20260305T143200` (UTC, no separators). This ID is:

1. **Generated once** in `daemon_main()` at startup: `chrono::Utc::now().format("%Y%m%dT%H%M%S").to_string()`
2. **Stored** in `DaemonContext` as `session_id: String` and `session_dir: PathBuf`
3. **Propagated** to every subsystem:
   - Used as the session directory name: `sessions/{session_id}/`
   - Passed to `AgentSession` records as `daemon_session_id: String` (new field)
   - Passed to `AgentLogger::new()` as the parent directory for per-agent logs
   - Available via `system.status` IPC response
4. **Used by `diagnose`** to locate: session dir contains all logs; `AgentSession.daemon_session_id` cross-references from TaskStore

This is the same pattern as a **request/trace/correlation ID** in web systems. Instead of HTTP headers, the ID flows through `DaemonContext` → agent spawn → `AgentSession.daemon_session_id`. The session ID is the directory name and appears in `AgentSession` records, providing exact correlation.

**Why the timestamp works:** Daemon starts are at least a second apart. The timestamp is human-readable (`20260305T143200` = March 5, 2026 at 14:32), sortable, and unique without any random component.

**Field addition to `AgentSession`:**
```rust
pub struct AgentSession {
    // ... existing fields ...
    pub daemon_session_id: String,  // NEW: e.g. "20260305T143200"
}
```

### Component 1: Session Directory + Scoped Logging

**File:** `src/lib.rs` — `setup_logging()`

Each daemon start creates a session directory and writes `loopr.log` inside it:

```rust
/// Set up logging. When `session_id` is provided (daemon), creates a session
/// directory at `~/.local/share/loopr/sessions/{session_id}/` with `loopr.log`
/// inside it and updates the `latest` symlink. When None (TUI, one-shot CLI),
/// writes to `~/.local/share/loopr/logs/loopr.log` as before.
pub fn setup_logging(
    config: &Config,
    cli_log_level: Option<&str>,
    session_id: Option<&str>,
) -> eyre::Result<PathBuf> {
    let base_dir = dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("loopr");

    let log_file = if let Some(sid) = session_id {
        // Session-scoped: create session directory
        let session_dir = base_dir.join("sessions").join(sid);
        fs::create_dir_all(session_dir.join("agents"))?;

        // Update `latest` symlink (relative, points to session dir name)
        let sessions_dir = base_dir.join("sessions");
        let latest = sessions_dir.join("latest");
        let _ = fs::remove_file(&latest);
        #[cfg(unix)]
        std::os::unix::fs::symlink(sid, &latest)?;

        session_dir.join("loopr.log")
    } else {
        // TUI / one-shot CLI: append to shared log
        let log_dir = base_dir.join("logs");
        fs::create_dir_all(&log_dir)?;
        log_dir.join("loopr.log")
    };

    let target = Box::new(
        fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_file)?,
    );

    let level = resolve_log_level(config, cli_log_level);
    env_logger::Builder::new()
        .filter_level(level)
        .target(env_logger::Target::Pipe(target))
        .format(/* unchanged */)
        .init();

    info!("Logging initialized (level: {}), writing to: {}", level, log_file.display());
    Ok(log_file)
}
```

**Callers in `main.rs`:**
```rust
match cli_args.command {
    Some(Command::Daemon) => {
        let session_id = chrono::Utc::now().format("%Y%m%dT%H%M%S").to_string();
        let log_path = setup_logging(&config, cli_log_level, Some(&session_id))?;
        // session_id and log_path passed to DaemonContext
        // DaemonContext also stores session_dir = log_path.parent()
    }
    Some(Command::Tui) | None => {
        // TUI is a separate process — it doesn't own the session, the daemon does.
        // TUI uses non-session logging; chat messages are logged daemon-side (Tap 1a/1b).
        setup_logging(&config, cli_log_level, None)?;
    }
    Some(Command::Diagnose { .. }) => {
        // diagnose runs locally, no logging setup needed beyond default
        // (handled before this match arm — see Phase 3 implementation notes)
    }
    _ => {
        setup_logging(&config, cli_log_level, None)?;
    }
}
```

**Important:** Only the **daemon** creates a session directory because it's the long-lived process that agents, tools, and state transitions run through. The TUI is a separate OS process (spawned by `ensure_daemon()` if needed, connected via Unix socket IPC) — its chat messages are logged when the daemon processes them (Tap 1a/1b log to the daemon's session log).

**Agent logger change:** `AgentLogger::new()` currently hardcodes the log directory to `~/.local/share/loopr/logs/agents/`. Change it to accept a `session_dir: &Path` parameter and write to `{session_dir}/agents/{type}-{agent_session_id}.log`. The `DaemonContext` propagates `session_dir` to agent spawning, which passes it to `AgentLogger::new()`.

**On daemon shutdown**, print the session ID to stderr so the user can grab it:
```
loopr session 20260305T143200 ended. Run `loopr diagnose dump` for diagnostics.
```
This also prints on TUI exit (since the TUI triggers daemon shutdown). The user can copy-paste the session ID or just use `dump` which defaults to latest.

### Component 2: Seven Debug Tap Points

All tap points use `log::debug!()` with a consistent prefix format for grep-ability.

#### Tap 1a: User chat message received by daemon
**File:** The daemon handler that receives user chat messages via IPC (the agentic loop entry point on the daemon side). When the daemon receives a chat message from the TUI:
```rust
log::debug!("[chat] user: {}", user_message_text);
```

#### Tap 1b: Assistant response sent to TUI
**File:** The daemon handler that streams LLM responses back to the TUI. When a complete response is finalized:
```rust
log::debug!("[chat] assistant: {} chars", response_text.len());
```

**Why daemon-side, not TUI-side:** The TUI is a separate process with its own (non-session-scoped) log. Logging in the daemon ensures chat messages land in the session log alongside all other diagnostic events. The user message text arrives via IPC and the response is generated by the daemon's agentic loop — both are already in the daemon's address space.

#### Tap 2: LLM response finalized
**File:** `src/tools/agentic_loop.rs:75` (after LLM complete returns)
```rust
log::debug!(
    "[agent:{}] llm_response: stop_reason={:?} blocks={} text_len={}",
    ctx.exec_id, stop_reason, content_blocks.len(), extract_text(&content_blocks).len()
);
```

#### Tap 3: Tool call dispatched
**File:** `src/tools/agentic_loop.rs:110` (enhance existing debug line)
```rust
log::debug!(
    "[agent:{}] tool_call: tool={} id={} args={}",
    ctx.exec_id, call.name, call.id, call.input
);
```

#### Tap 4: Tool result received
**File:** `src/tools/agentic_loop.rs:125` (after exit_code is computed, before the event send)
```rust
log::debug!(
    "[agent:{}] tool_result: tool={} is_error={} exit={} duration={}ms content_len={}",
    ctx.exec_id, call.name, result.is_error, exit_code, duration_ms, result.content.len()
);
```
Note: This tap goes after line 125 where `exit_code` is derived from `result.is_error`, not immediately after `execute()`.

#### Tap 5: State transitions
**File:** `src/daemon/handlers.rs` — before each `transition_completed` event send (6 locations: plan, spec, phase, work, bundle, tick)
```rust
log::debug!(
    "[transition] {}.{}: {} -> {} by {}",
    "work", id, from, target_status, role
);
```

#### Tap 6: Agent status changes
**File:** `src/daemon/handlers.rs` — before each `agent_status_changed` event send

For spawn (line 4365), there is no prior status:
```rust
log::debug!("[agent_status] {}: -> Starting (type={:?})", id, agent_type);
```

For cancel/pause/resume (lines 4439, 4498, 4550), old status is available from the session before mutation:
```rust
log::debug!("[agent_status] {}: {:?} -> {:?}", session_id, old_status, new_status);
```

Also in `src/agents/executor.rs` (3 locations) and `src/daemon/supervisor.rs` (5 locations) where `agent_status_changed` is emitted — same pattern, logging the new status being set.

### Component 3: `loopr diagnose` Subcommand

**File:** `src/cli/mod.rs` — add to Command enum

```rust
/// Session diagnostics (logs, state, agent history)
Diagnose {
    #[command(subcommand)]
    cmd: DiagnoseCmd,
},
```

```rust
#[derive(Debug, Clone, clap::Subcommand)]
pub enum DiagnoseCmd {
    /// Dump ALL diagnostic data from the last session — the primary command.
    /// Outputs everything an agentic LLM needs to understand what happened:
    /// session log (filtered to debug+ events), TaskStore state snapshot,
    /// agent sessions with errors, and per-agent log excerpts.
    Dump {
        /// Session ID (e.g. 20260305T143200). Defaults to latest.
        #[arg(short, long)]
        session: Option<String>,
        /// Only include log lines matching this pattern (applied to session log)
        #[arg(short, long)]
        filter: Option<String>,
    },
    /// Show the session log only
    Log {
        /// Session ID. Defaults to latest.
        #[arg(short, long)]
        session: Option<String>,
        /// Only show lines matching this pattern
        #[arg(short, long)]
        filter: Option<String>,
        /// Number of lines from end (like tail -n)
        #[arg(short, long)]
        tail: Option<usize>,
    },
    /// List available session logs
    Sessions {
        /// Number of sessions to show (default: 10)
        #[arg(short, long, default_value = "10")]
        count: usize,
    },
    /// Show the session summary only
    Summary {
        /// Session ID. Defaults to latest.
        #[arg(short, long)]
        session: Option<String>,
    },
    /// Show TaskStore state snapshot (all collections with counts and recent changes)
    State,
    /// Show agent session history with status, iterations, and errors
    Agents {
        /// Only show failed agents
        #[arg(long)]
        failed: bool,
    },
}
```

**The `dump` subcommand is the primary interface.** It assembles a single output containing all diagnostic context from a session, designed to be piped or pasted into an agentic LLM session. The other subcommands are drill-down tools for when you need a specific slice.

**`dump` output structure:**
```
=== LOOPR SESSION DIAGNOSTIC DUMP ===
Session: 20260305T143200
Session dir: ~/.local/share/loopr/sessions/20260305T143200/

=== SESSION SUMMARY ===
[contents of summary.md if available, or generated on the fly]

=== TASKSTORE STATE ===
Plans: 1 (Active: 1)
Specs: 2 (Active: 2)
Phases: 3 (Active: 1, Done: 2)
Works: 5 (InProgress: 1, Review: 1, Done: 2, Failed: 1)
Bundles: 3 (Merged: 2, Rejected: 1)
[... counts for all collections]

=== AGENT SESSIONS ===
| ID | Type | Status | Iterations | Work/Bundle | Error |
|----|------|--------|------------|-------------|-------|
[... all agent sessions from this daemon run]

=== FAILED WORK ITEMS ===
[For each Work in Failed status: ID, spec summary, last error, and transition history.
 This is the most actionable artifact for the next debugging session.]

=== AGENT LOG: agent-implementer-ag02DEF (FAILED) ===
[last 100 lines of per-agent log for failed agents, full log for others if short]

=== SESSION LOG (debug+ events) ===
[full session log, or filtered subset]
```

**Implementation:** `src/cli/diagnose.rs` — a new module that reads files directly. **No daemon required for any subcommand.** `state` and `agents` read TaskStore JSONL files directly rather than requiring IPC. This is critical because the developer often needs diagnostics after a crash or daemon shutdown.

Key functions:
- `find_session_dir(session: Option<&str>)` — resolve session directory (follow `latest` symlink or find by timestamp)
- `read_taskstore_state()` — open `.taskstore/` JSONL files, count records by status
- `read_failed_works()` — read Work records in Failed status with their spec summaries and error context
- `read_agent_sessions()` — parse `agent_sessions.jsonl` from TaskStore
- `list_agent_logs(session_dir: &Path)` — glob `{session_dir}/agents/*.log` — all agent logs for this session are colocated, no cross-referencing needed
- `generate_summary(session_dir: &Path)` — parse `loopr.log` for key events (can be called on-the-fly if no `summary.md` exists)

### Component 4: Session Summary

**File:** `src/session_summary.rs` (new module, added to `lib.rs`)

On daemon shutdown, generate `~/.local/share/loopr/sessions/{session_id}/summary.md`:

```markdown
# Loopr Session 20260305T143200 (started 2026-03-05T14:32:00)

## Duration: 5m 42s

## State Changes
- work.wi_001: Draft -> InProgress -> Review -> Done
- work.wi_002: Draft -> InProgress -> Failed
- bundle.b_001: Draft -> Proposed -> Merged

## Agent Sessions
| ID | Type | Status | Iterations | Error |
|----|------|--------|------------|-------|
| ag01 | Coordinator | Done | 4 | — |
| ag02 | Implementer | Failed | 5 | compilation error: expected Response |

## Errors
1. [ag02 iter 3] tool_result: tool=shell exit=1 duration=2300ms
2. [ag02 iter 5] max iterations reached

## Tool Calls: 23 total (19 success, 4 failed)
```

**Generation:** Parse the session's log file for `[transition]`, `[agent_status]`, `[agent:*] tool_result`, and error-level lines. This avoids needing any new data structures — the summary is derived entirely from the log file.

### Data Model

No new data models. All diagnostic data flows through the existing `log` crate to session-scoped files.

The only structural changes are: `setup_logging()` returning `PathBuf` (the session log file path, from which the session directory is derived), and `AgentLogger::new()` accepting a `session_dir` parameter instead of hardcoding the log directory.

### API Design

**New CLI commands:**

| Command | Daemon Required? | Description |
|---------|-----------------|-------------|
| `loopr diagnose dump` | No | **Primary.** Full diagnostic dump for LLM context injection |
| `loopr diagnose log` | No | Cat/tail/filter session log |
| `loopr diagnose sessions` | No | List session log files with timestamps and sizes |
| `loopr diagnose summary` | No | Show session summary markdown |
| `loopr diagnose state` | No | Read TaskStore JSONL files for state snapshot |
| `loopr diagnose agents` | No | Read agent_sessions from TaskStore JSONL |

**No subcommand requires a running daemon.** All read from files on disk (session logs, per-agent logs, TaskStore JSONL). This is a hard requirement — diagnostics must work after crashes.

### Implementation Plan

**Phase 1: Daemon session ID + session directory**
- Generate timestamp session ID (`%Y%m%dT%H%M%S` → e.g. `20260305T143200`) in daemon startup path
- Add `session_id: String` and `session_dir: PathBuf` fields to `DaemonContext`
- Add `daemon_session_id: String` field to `AgentSession` (set on spawn)
- Modify `setup_logging()` in `src/lib.rs` to accept optional session ID, create `sessions/{session_id}/loopr.log` + `sessions/latest` symlink
- Modify `AgentLogger::new()` to accept `session_dir: &Path`, write to `{session_dir}/agents/{type}-{id}.log`
- Return `PathBuf` from `setup_logging()`
- Update `main.rs` to propagate session ID and session dir
- Update tests

**Phase 2: Debug tap points**
- Add `log::debug!()` at 7 tap points (1a, 1b, 2, 3, 4, 5, 6) across 4+ files (~20 call sites total: Tap 5 has 6 transition locations, Tap 6 has 8+ status change locations)
- All use consistent `[prefix]` format for grep-ability
- No behavior changes — debug level only

**Phase 3: `loopr diagnose` subcommand**
- Add `DiagnoseCmd` enum to `src/cli/mod.rs`
- Add `Diagnose` variant to `Command` enum
- Create `src/cli/diagnose.rs` with implementations for all subcommands
- Handle `Diagnose` in `main.rs` **before** daemon connection — `diagnose` commands run locally, not via IPC. This is different from all other CLI commands that go through `dispatch.rs`.
- Add CLI parser tests

**Phase 4: Session summary**
- Add summary generation function (parse log file, extract key events)
- Call on daemon shutdown
- Wire `diagnose summary` to read the file

## Alternatives Considered

### Alternative 1: Structured JSON Session Log (SessionRecorder)
- **Description:** A parallel `SessionRecorder` struct that writes structured JSONL events to a separate session file
- **Pros:** Machine-parseable, rich typed events, could support replay
- **Cons:** Duplicates the `log` infrastructure, two recording paths to maintain, two files to correlate
- **Why not chosen:** The existing `log` crate already reaches every tap point. Adding debug lines is simpler than a parallel recording system. Grep works well enough on text logs.

### Alternative 2: Persist In-Memory Ring Buffers to Disk
- **Description:** Write the `VecDeque<AgentEvent>` to JSONL files on daemon shutdown
- **Pros:** Preserves the structured event data that's currently lost
- **Cons:** Ring buffer only holds 1000 events (may have already dropped important ones), adds serialization code, still not session-scoped
- **Why not chosen:** The debug tap points capture the same information to the log file in real time, without the 1000-event limit.

### Alternative 3: Add Diagnostics to TaskStore
- **Description:** Create a new `DiagnosticEvent` record type in TaskStore and persist all events there
- **Pros:** Full persistence, queryable via existing TaskStore API, survives daemon restart
- **Cons:** High write volume (every tool call, every LLM response), bloats TaskStore, mixes diagnostic telemetry with domain records
- **Why not chosen:** TaskStore is for domain state, not telemetry. Log files are the right medium for diagnostic data.

## Technical Considerations

### Dependencies
- No new crates required. Uses existing `log`, `env_logger`, `chrono`, `dirs`, `clap`.
- `std::os::unix::fs::symlink` for the `latest` symlink (unix-only, which is fine — loopr targets Linux).

### Performance
- `log::debug!()` calls are no-ops when the log level is Info or higher (the default). Zero overhead in production.
- Session summary generation parses the log file once on shutdown — bounded by session duration (typically small files).
- `diagnose` reads files directly from the session directory without daemon IPC — fast even after crashes.

### Security
- No new attack surface. Log files contain LLM prompts/responses and tool outputs, which may include sensitive code. Same exposure as the existing per-agent logs.

### Testing Strategy
- **`setup_logging` changes:** Unit tests verifying session directory creation, `loopr.log` placement, and symlink creation (in `src/lib.rs` tests)
- **`AgentLogger` changes:** Update existing tests in `src/agents/agent_logger.rs` to verify logs write to `{session_dir}/agents/` instead of the hardcoded path
- **Tap points:** No direct tests needed — they're `debug!()` calls. Covered by existing agentic_loop and handler tests.
- **CLI parsing:** Unit tests for `DiagnoseCmd` variants in `src/cli/mod.rs` tests (following existing pattern)
- **`diagnose log/sessions/summary`:** Integration tests that create sample log files and verify output
- **Session summary:** Unit test with a known log file content → verify generated summary

### Rollout Plan
- Phase 1 (logging) can land independently — immediate value
- Phase 2 (tap points) can land independently — requires `--log-level debug` to see output
- Phase 3 (CLI) depends on Phase 1 for session-scoped files
- Phase 4 (summary) depends on Phase 2 for tap point content in logs

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Session directories accumulate without cleanup | Medium | Low | Out of scope; user can `rm -rf` old session dirs. Future: add `diagnose gc` subcommand |
| Summary generation fails on malformed logs | Low | Low | Summary is best-effort; errors logged but don't block shutdown |
| Debug tap points add noise at debug level | Low | Low | Only visible with `--log-level debug`; consistent prefixes enable filtering |
| `setup_logging` signature change breaks callers | Low | Medium | Only `main.rs` calls it; small change |
| Daemon crash before summary generation | Medium | Low | `diagnose dump` generates summary on-the-fly from log file if `summary.md` is missing |
| `dump` output too large for LLM context | Medium | Medium | Failed agent logs capped at last 100 lines; session log can be filtered via `--filter`; summary section provides condensed overview even if log is huge |
| TaskStore lock file present after crash | Low | Low | `diagnose` reads JSONL files directly (read-only), ignores SQLite lock; doesn't need exclusive access |
| TUI chat messages not in daemon log | Medium | High | Tap 1a/1b log in daemon's IPC handler when it receives/responds to chat messages. See Option A below. |

**Important edge case: TUI is a separate process from the daemon.** The TUI connects to the daemon via IPC. To get chat messages into the daemon's session log, we have two options:

1. **Option A (preferred):** The TUI's chat submit handler already sends messages to the daemon via IPC for the agentic loop. The daemon can log received chat messages at its end. This keeps the session log unified in one process.
2. **Option B:** Have the TUI also create a session-scoped log and include it in `diagnose dump`.

We choose **Option A**: add a debug log in the daemon's chat/agentic IPC handler when it receives a user message, and when the LLM response is sent back to the TUI. This keeps all session diagnostics in the daemon's log file.

## Open Questions
- [x] Should `diagnose state` work without a running daemon by reading TaskStore JSONL files directly? **Yes — all diagnose subcommands work without daemon.**
- [ ] Should old session logs be automatically cleaned up after N days?
- [ ] Should the session summary include the last N chat messages from the TUI? (Requires the TUI chat tap point in Tap 1 to log message content at debug level — which we do. Summary generator can extract `[chat]` prefixed lines.)

## References
- Existing logging: `src/lib.rs:87-134`
- Per-agent logger: `src/agents/agent_logger.rs`
- Agent events ring buffer: `src/daemon/context.rs:169-181`
- Agentic tool loop: `src/tools/agentic_loop.rs`
- State transition handlers: `src/daemon/handlers.rs` (lines 604, 901, 1199, 1558, 1950, 2251)
- Agent status changes: `src/daemon/handlers.rs` (lines 4365, 4439, 4498, 4550), `src/agents/executor.rs`, `src/daemon/supervisor.rs`
- CLI structure: `src/cli/mod.rs`
- Dispatch: `src/cli/dispatch.rs`
