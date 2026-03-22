# Design Document: Live-Run Fixes — Validator Bypass, Tool Detection, Coordinator Supervisor, Lifeguard Tuning

**Author:** Scott Idler
**Date:** 2026-03-01
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

The first live end-to-end run of loopr v3 exposed four interacting bugs that prevented the system from completing a goal. This document designs fixes for all four: (1) coordinator burns iterations on validation when the validator is disabled, (2) implementers use hardcoded Rust tools regardless of project type, (3) coordinator death from lifeguard is permanent — nobody restarts it, (4) lifeguard triggers too aggressively on tool errors caused by misconfiguration rather than agent loops.

## Problem Statement

### Background

MVP3 delivered the Implementer, Reviewer, and Coordinator agents with tool execution, streaming, and the lifeguard loop detector. The first live run attempted to have the coordinator plan, spec, and implement a JavaScript todo application. The system stalled after ~15 minutes with the coordinator dead and implementers burning iterations on wrong tools.

### Problem

Four bugs compound into a terminal session:

1. **Validator-disabled loop** — `build_generation_footer()` (`coordinator.rs:345`) unconditionally tells the coordinator to `ValidateDocument` on Draft records. When `validator.enabled: false`, the handler fails (`stores.validator` is `None`), the coordinator retries, and lifeguard kills it after 3 identical errors in the 10-error window. The coordinator burned ~10 iterations before death.

2. **Hardcoded Rust tools** — `AgentConfig::default()` (`config.rs:221-252`) defines only `cargo test`, `cargo clippy`, `cargo fmt`, `cargo build`. The `ContextBuilder` lists all configured tools verbatim (`context.rs:558-569`). The `ToolRunner` executes whatever the LLM picks (`executor.rs:418-423`). For a JS goal, the LLM sees only Rust tools, uses them, they fail, iterations wasted.

3. **No coordinator restart** — The coordinator's `run()` method (`coordinator.rs:1295-1297`) detects lifeguard's `NeedHelp` and exits immediately, bypassing the `max_restarts=3` retry loop. The daemon (`daemon/mod.rs:152-169`) auto-starts the coordinator once at boot but has no monitoring. The design doc (MVP4) promises "daemon auto-restarts coordinator after `idle_interval_secs * 2`" — this is not implemented. Session goes `Failed`, nobody restarts it, system is terminal.

4. **Lifeguard false positives** — When tools are misconfigured (bug #2), every `RunTool` produces the same error. Lifeguard's error layer (`lifeguard.rs:74-93`) triggers after 3 identical errors in 10 — which is correct for agent bugs but unfair when the errors are configuration problems the agent cannot fix. This is largely a downstream effect of bug #2 but the lifeguard should distinguish error classes.

### Goals

- Coordinator skips validation when `validator.enabled: false` and auto-activates Draft records
- Tools are detected from the project (e.g. `package.json` → npm, `Cargo.toml` → cargo) or configurable per-goal
- Daemon-level supervisor monitors coordinator sessions and restarts on failure with backoff
- Lifeguard distinguishes configuration errors from agent loops

### Non-Goals

- Full project-type inference beyond marker-file detection (language servers, AST analysis)
- Coordinator self-healing (diagnosing and fixing its own errors without restart)
- Multi-coordinator support (only one coordinator per daemon)
- Lifeguard A-B-A-B oscillation detection (deferred to MVP5)
- Accumulated multi-iteration context for implementers (deferred to MVP5)

## Proposed Solution

### Fix 1: Validator-Disabled Bypass

**Overview:** Guard the validation footer and auto-activate Drafts when validator is disabled.

**Changes in `src/agents/coordinator.rs`:**

In `build_generation_footer()` at line 342, replace the unconditional validation check with a single `if/else` branch:

```rust
// Fix #8: Check if a Draft document exists that needs validation or activation.
if let Some(draft_info) = find_pending_draft_for_validation(stores) {
    if stores.validator.is_some() {
        // Validator enabled — ask coordinator to validate
        agent_log.info(&format!("Draft {} '{}' needs validation", draft_info.0, draft_info.1));
        return Some(format!(
            "A {} is in Draft status and needs validation before proceeding.\n\
             Use ValidateDocument to validate it.\n\
             Draft ID: {}\nTitle: {}",
            draft_info.0, draft_info.1, draft_info.2
        ));
    } else {
        // Validator disabled — tell coordinator to activate directly
        agent_log.info(&format!(
            "Draft {} '{}' — validator disabled, activate directly",
            draft_info.0, draft_info.1
        ));
        return Some(format!(
            "A {} is in Draft status. Validation is disabled — activate it directly.\n\
             Use TransitionStatus to move it from Draft to Active.\n\
             ID: {}\nTitle: {}",
            draft_info.0, draft_info.1, draft_info.2
        ));
    }
}
```

This is a single `find_pending_draft_for_validation` call that branches on validator state, avoiding redundant store reads.

**Why `stores.validator.is_some()` instead of `stores.config.validator.enabled`:** The `validator` field on `Stores` is the runtime truth — it's `None` exactly when disabled. Checking the `Option` is idiomatic and avoids coupling to config struct layout.

**Also guard the regeneration path** (lines 306-340): The regeneration logic that builds a "re-validate failed Draft" prompt should also be guarded. When the validator is disabled, a Draft with `validation_failures` should never exist, but defensively wrap the `determine_generation_level()` path with the same `stores.validator.is_some()` check so it returns `None` (no regeneration needed) when the validator is off.

### Fix 2: Project-Aware Tool Detection

**Overview:** Detect project type from marker files in the worktree and load appropriate tool sets. Fall back to config defaults if no markers found.

**Detection in `src/tools/mod.rs`:**

Add `ToolRunner::detect_or_default(worktree_path, configured_tools)`:

```rust
/// Built-in tool presets. Returns Vec<ToolEntry> because ToolEntry contains Strings
/// (can't be static). Constructed on demand per agent session.
fn js_preset() -> Vec<ToolEntry> {
    vec![
        ToolEntry { name: "test".into(), command: "npm test".into(), timeout_secs: 300, worktree: true },
        ToolEntry { name: "lint".into(), command: "npm run lint".into(), timeout_secs: 120, worktree: true },
        ToolEntry { name: "build".into(), command: "npm run build".into(), timeout_secs: 300, worktree: true },
    ]
}

fn python_preset() -> Vec<ToolEntry> {
    vec![
        ToolEntry { name: "test".into(), command: "pytest".into(), timeout_secs: 300, worktree: true },
        ToolEntry { name: "lint".into(), command: "ruff check .".into(), timeout_secs: 120, worktree: true },
        ToolEntry { name: "fmt-check".into(), command: "ruff format --check .".into(), timeout_secs: 30, worktree: true },
    ]
}

/// Marker files checked in priority order. First match wins.
const MARKER_ORDER: &[&str] = &["package.json", "pyproject.toml", "Cargo.toml"];

impl ToolRunner {
    /// Detect project type from marker files and return appropriate tools.
    /// Falls back to `configured` if no markers found.
    pub fn detect_or_default(worktree: &Path, configured: &[ToolEntry]) -> Self {
        for marker in MARKER_ORDER {
            if worktree.join(marker).exists() {
                let tools = match *marker {
                    "package.json" => js_preset(),
                    "pyproject.toml" => python_preset(),
                    "Cargo.toml" => return Self::new(configured), // Cargo.toml → use config defaults (already Rust)
                    _ => continue,
                };
                debug!("Detected project marker '{}', using {} tools", marker, tools.len());
                return Self::new(&tools);
            }
        }
        Self::new(configured)
    }
}
```

**Integration point:** Call `detect_or_default` in `run_agent_task()` (`executor.rs`) when setting up the agent context, using the worktree path that's already been created at that point. The returned `ToolRunner` is wrapped in `Arc<ToolRunner>` and stored in the `AgentContext` for that session. The global `stores.tool_runner` on `Stores` remains the config default (used by the coordinator and other non-worktree contexts).

**Config override:** Detection is unconditional — it always runs. But `Cargo.toml` detection falls through to the config defaults, which are already Rust tools. If the user has set custom tools in `[agents.tools]` (e.g., `make test`), and no marker file matches, the config tools are used as-is. Users who want full control set tools explicitly in config and detection is a no-op for their marker.

**Known limitation — greenfield projects:** Detection runs once at agent start against the worktree contents at that moment. For a "build a JS app" goal against a Rust repo, the worktree starts as a clone of the Rust repo. Detection finds `Cargo.toml` and gives Rust tools — even though the implementer will create `package.json` later. This is acceptable for now: the production use case is running loopr against a repo that already has its marker files. Greenfield-in-foreign-repo is a testing scenario. A future enhancement could re-run detection when marker files appear, or let the coordinator specify tools in the goal.

**Per-session tool runner:** Currently `AgentContext` holds `stores: Arc<Stores>` which contains `tool_runner: Arc<ToolRunner>`. Add a separate `tool_runner: Arc<ToolRunner>` field directly to `AgentContext` that shadows `stores.tool_runner`. In `run_agent_task()`:

```rust
let session_tool_runner = Arc::new(
    ToolRunner::detect_or_default(&worktree_path, &stores.config.agents.tools)
);
// Pass session_tool_runner into AgentContext instead of stores.tool_runner
```

### Fix 3: Daemon-Level Coordinator Supervisor

**Overview:** Add a supervisor task in the daemon that watches `agent.status_changed` events and restarts the coordinator on failure with exponential backoff.

**New module: `src/daemon/supervisor.rs`**

```rust
use std::sync::Arc;
use std::time::Duration;
use log::{info, warn};
use tokio::sync::broadcast;
use serde_json::json;

use crate::agents::{AgentStatus, AgentType, AgentEvent};
use crate::daemon::context::Stores;
use crate::ipc::protocol::DaemonEvent;
use crate::worktree::manager::WorktreeManager;

/// Configuration for the coordinator supervisor.
pub struct SupervisorConfig {
    pub enabled: bool,
    pub base_delay_secs: u64,
    pub max_delay_secs: u64,
    pub max_restarts: u32,
}

impl Default for SupervisorConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            base_delay_secs: 10,
            max_delay_secs: 300,
            max_restarts: 5,
        }
    }
}

/// Watches for coordinator session failures and restarts with backoff.
pub async fn run_supervisor(
    stores: Arc<Stores>,
    event_tx: broadcast::Sender<DaemonEvent>,
    worktree_mgr: WorktreeManager,
    integrator_config: crate::config::IntegratorConfig,
    config: SupervisorConfig,
) {
    let mut event_rx = event_tx.subscribe();
    let mut restart_count = 0u32;

    loop {
        let event = match event_rx.recv().await {
            Ok(ev) => ev,
            Err(broadcast::error::RecvError::Lagged(n)) => {
                warn!("supervisor lagged {} events", n);
                continue;
            }
            Err(broadcast::error::RecvError::Closed) => break,
        };

        // Only care about agent status changes
        if event.event != "agent.status_changed" {
            continue;
        }

        // Parse the event to check if it's a coordinator failure
        let agent_event: AgentEvent = match serde_json::from_value(event.data.clone()) {
            Ok(e) => e,
            Err(_) => continue,
        };

        let (session_id, status) = match agent_event {
            AgentEvent::StatusChange { session_id, status } => (session_id, status),
            _ => continue,
        };

        // Check if this session was a coordinator
        let is_coordinator = {
            let sessions = stores.agent_sessions.read().unwrap();
            sessions.get(&session_id)
                .map(|s| s.agent_type == AgentType::Coordinator)
                .unwrap_or(false)
        };

        if !is_coordinator {
            continue;
        }

        // Reset restart counter when a coordinator reaches Running
        if status == AgentStatus::Running && restart_count > 0 {
            info!("Coordinator reached Running, resetting supervisor restart counter");
            restart_count = 0;
            continue;
        }

        if status != AgentStatus::Failed {
            continue;
        }

        // Check if another coordinator is already running
        let has_active_coordinator = {
            let sessions = stores.agent_sessions.read().unwrap();
            sessions.values().any(|s| {
                s.agent_type == AgentType::Coordinator
                    && !s.status.is_terminal()
            })
        };

        if has_active_coordinator {
            continue;
        }

        if restart_count >= config.max_restarts {
            warn!(
                "Coordinator has failed {} times, supervisor giving up",
                restart_count
            );
            break;
        }

        restart_count += 1;
        let delay = Duration::from_secs(
            (config.base_delay_secs * 2u64.pow(restart_count - 1))
                .min(config.max_delay_secs)
        );

        info!(
            "Coordinator failed (attempt {}/{}), restarting in {:?}",
            restart_count, config.max_restarts, delay
        );
        // Sleep the backoff delay. If the daemon shuts down during sleep,
        // the next recv() will return Closed and we exit. No need to select
        // on events here — checking for shutdown during sleep adds complexity
        // (unrelated events can defeat the backoff) for minimal benefit.
        tokio::time::sleep(delay).await;

        // Restart via the same dispatch path as auto-start
        let start_req = crate::ipc::protocol::DaemonRequest::new(
            0,
            "agent.start",
            json!({ "agent_type": "coordinator" }),
        );
        let response = crate::daemon::handlers::dispatch(
            &stores,
            &event_tx,
            &worktree_mgr,
            &integrator_config,
            start_req,
        );

        if response.error.is_some() {
            warn!("Supervisor failed to restart coordinator: {:?}", response.error);
        } else {
            info!("Supervisor restarted coordinator (attempt {})", restart_count);
        }
    }
}
```

**Spawn in `daemon_main()`:** After `accept_loop` setup, spawn the supervisor as a background task:

```rust
// In daemon_main(), after auto-starting coordinator
let supervisor_handle = {
    let stores = ctx.read().await.stores.clone();
    let evt = event_tx.clone();
    let wm = ctx.read().await.worktree_manager.clone();
    let ic = ctx.read().await.config.integrator.clone();
    tokio::spawn(run_supervisor(stores, evt, wm, ic, SupervisorConfig::default()))
};
```

**On daemon shutdown:** Abort the supervisor task alongside the other agent tasks. The supervisor listens on `event_rx` so the `system.shutting_down` broadcast will cause `recv()` to return `Closed` after the sender drops, exiting naturally.

**Backoff strategy:** Exponential: 10s → 20s → 40s → 80s → 160s, capped at 300s. 5 restarts max. This prevents runaway restart loops while giving the coordinator a reasonable chance to recover from transient issues.

**Restart counter reset:** The supervisor watches for `AgentStatus::Running` transitions on coordinator sessions (visible in the code above). When a restarted coordinator successfully reaches `Running`, `restart_count` resets to 0. This means a coordinator that ran successfully before its next failure gets a fresh failure budget.

### Fix 4: Lifeguard Error Classification

**Overview:** Classify errors into `config` (tool not found, wrong project type) vs `agent` (sandbox escape, repeated bad action) categories. Only count `agent` errors toward escalation.

**Changes in `src/agents/lifeguard.rs`:**

Add error classification:

```rust
/// Classify an error to determine if it should count toward escalation.
fn is_config_error(error: &str) -> bool {
    // Patterns are intentionally narrow to avoid false positives.
    // "No such file or directory" is excluded — too broad (matches agent ReadFile errors).
    let config_patterns = [
        "unknown tool:",          // ToolRunner rejects unknown tool name
        "command not found",      // shell can't find the tool binary
        "cargo: not found",       // specific tool binary missing
        "npm: not found",
        "node: not found",
        "python: not found",
        "pytest: not found",
        "ruff: not found",
    ];
    config_patterns.iter().any(|p| error.contains(p))
}
```

Modify `record_error()` — the return type changes from `Verdict` to `(Verdict, Option<String>)` to carry config-error warnings back to callers:

```rust
/// Record an action error. Returns (verdict, optional_warning).
/// Config errors don't count toward escalation but accumulate a separate counter.
pub fn record_error(&mut self, error: &str) -> (Verdict, Option<String>) {
    if is_config_error(error) {
        self.config_error_count += 1;
        let warning = if self.config_error_count >= 10 {
            Some(
                "WARNING: Repeated tool configuration errors detected. \
                 The configured tools may not match this project type.".into()
            )
        } else {
            None
        };
        return (Verdict::Continue, warning);
    }

    let hash = hash_string(error);
    self.recent_errors.push_back(hash);
    if self.recent_errors.len() > self.error_window_size {
        self.recent_errors.pop_front();
    }

    let same_count = self.recent_errors.iter().filter(|h| **h == hash).count() as u32;
    if same_count >= self.error_threshold {
        return (Verdict::Escalate(format!(
            "same error repeated {} times: {}",
            same_count,
            truncate(error, 200),
        )), None);
    }

    (Verdict::Continue, None)
}
```

**Caller update required:** All call sites (`implementer.rs:224`, `coordinator.rs`, `researcher.rs`) currently do:
```rust
if let Verdict::Escalate(reason) = guard.record_error(&err_msg) { ... }
```
These must change to:
```rust
let (verdict, warning) = guard.record_error(&err_msg);
if let Some(w) = warning {
    // Inject into previous_summary or agent log
    self.ctx.warn(&w);
}
if let Verdict::Escalate(reason) = verdict { ... }
```

**Why pattern-based:** The alternative is a typed error hierarchy, but tool execution errors come back as strings from `sh -c`. Pattern matching is pragmatic. The patterns are conservative — they match well-known shell/tool error messages.

### Data Model

No new persistence structures. Changes are in-memory only:

- `Lifeguard` gains `config_error_count: u32` field
- `ToolRunner::detect_or_default()` returns a session-local instance
- `SupervisorConfig` added to `Config` struct under `[daemon.supervisor]`

### Fix Interaction

These four fixes form a defense-in-depth stack. In the observed live run:

- **Fix 1 alone** would have prevented coordinator death (the validator loop was the proximate cause)
- **Fix 2 alone** would have prevented implementer tool failures (but the coordinator would still have died from the validator loop)
- **Fix 3** is insurance — even if another bug kills the coordinator, the supervisor brings it back
- **Fix 4** is defense-in-depth — even if tools are misconfigured in a way we don't detect, the lifeguard won't kill the agent for config errors

The implementation order prioritizes the highest-bang fixes first (Fix 1), then structural resilience (Fix 3), then the broader tool problem (Fix 2), then the safety net (Fix 4).

### Implementation Plan

**Phase 1: Validator bypass** (Fix 1) — highest bang, smallest change
- Guard `build_generation_footer()` with `stores.validator.is_some()`
- Add "activate directly" footer for disabled validator
- Test: coordinator with `validator.enabled: false` transitions Drafts to Active without ValidateDocument

**Phase 2: Coordinator supervisor** (Fix 3) — structural resilience before tool changes
- Create `src/daemon/supervisor.rs`
- Add `SupervisorConfig` to `Config`
- Spawn supervisor in `daemon_main()`
- Test: kill coordinator session, verify restart with backoff

**Phase 3: Project-aware tools** (Fix 2) — broader fix, more files touched
- Add `detect_or_default()` to `ToolRunner`
- Create session-local `ToolRunner` in `run_agent_task()`
- Add presets for JS (package.json), Python (pyproject.toml), Rust (Cargo.toml)
- Test: worktree with `package.json` gets npm tools, worktree with `Cargo.toml` gets cargo tools

**Phase 4: Lifeguard error classification** (Fix 4) — defense-in-depth safety net
- Add `is_config_error()` classifier
- Split `record_error()` to skip config errors
- Add config-error counter with warning threshold
- Test: config errors don't escalate, agent errors still escalate at threshold 3

## Alternatives Considered

### Alternative 1: Remove lifeguard from coordinator entirely
- **Description:** Give the coordinator immunity — never trigger lifeguard on it
- **Pros:** Simple, coordinator never dies from lifeguard
- **Cons:** If the coordinator genuinely loops (non-config bug), it burns iterations until the hard cap. No safety net.
- **Why not chosen:** The supervisor (Fix 3) is more robust — it lets lifeguard do its job but adds recovery. Immunity masks real bugs.

### Alternative 2: Goal-level tool config instead of detection
- **Description:** Require the user (or coordinator) to specify tools per goal in the config
- **Pros:** Explicit, no detection heuristics
- **Cons:** Shifts burden to the user. Coordinator doesn't know what tools a project needs until it sees the codebase. Breaks the "just set a goal" UX.
- **Why not chosen:** Detection from marker files is reliable and zero-config. We keep the config override as an escape hatch.

### Alternative 3: Coordinator internal retry for NeedHelp
- **Description:** Remove the `NeedHelp` bypass in `run()` (coordinator.rs:1295), let the `max_restarts` loop handle it
- **Pros:** Zero new code, just delete 3 lines
- **Cons:** The coordinator restarts in the same process with the same lifeguard state (reset by the new `Lifeguard::new()` in the loop). If the underlying cause persists (e.g., validator disabled), it dies again 3 times then truly dies. No backoff visibility.
- **Why not chosen:** The daemon-level supervisor provides proper backoff, event-driven restart, and visibility. Internal retry is a band-aid.

### Alternative 4: Typed error enum instead of string pattern matching
- **Description:** Define `ToolError::ConfigError(...)` vs `ToolError::AgentError(...)` in the tool runner
- **Pros:** Type-safe, no fragile string matching
- **Cons:** Tool execution goes through `sh -c` — the error source is a process exit code + stderr text. We'd need to parse stderr into typed errors, which is more fragile than matching known patterns.
- **Why not chosen:** Pattern matching on well-known error strings is pragmatic. The patterns are conservative and easy to extend.

## Technical Considerations

### Dependencies

No new external dependencies. All changes use existing crates (tokio, serde_json, log).

### Performance

- **Supervisor:** One background task per daemon, event-driven (no polling). Negligible CPU.
- **Tool detection:** One `Path::exists()` per marker per agent start. Sub-millisecond.
- **Error classification:** One string scan per error. Negligible.

### Testing Strategy

**Unit tests:**
- `lifeguard.rs`: Config errors don't escalate, agent errors still escalate
- `tools/mod.rs`: `detect_or_default` returns correct presets for each marker file
- `coordinator.rs`: `build_generation_footer` returns activation footer when validator is `None`

**Integration tests:**
- Supervisor restart: spawn coordinator, force `Failed`, verify restart event
- Tool detection end-to-end: create worktree with `package.json`, verify implementer sees `npm test`

**Manual validation:**
- Full live run with `validator.enabled: false` and a JS goal
- Verify coordinator survives, implementers use correct tools, session completes or reaches a meaningful state

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Supervisor restart loop on persistent failure | Medium | Medium | Exponential backoff + max restart cap (5). Counter resets on success. |
| Marker-file detection picks wrong preset (e.g., both `Cargo.toml` and `package.json` exist) | Low | Low | Priority order in preset list. Config override escape hatch. |
| Config-error pattern miss-classifies agent errors | Low | Medium | Patterns are conservative (exact well-known strings). False negatives (agent error treated as config) are worse than false positives — we err on the side of escalation. |
| Supervisor races with manual `agent.start` | Low | Low | Check `has_active_coordinator` before restart. Existing dedup in `agent.start` handler rejects duplicates. |
| Tool detection wrong for greenfield projects in foreign repos | Medium | Medium | Acceptable for now — production use is against repos with existing markers. Document as known limitation. |
| Supervisor sleeps through daemon shutdown | Low | Low | Next `recv()` returns `Closed`, supervisor exits. At most one wasted restart attempt. |

## Open Questions

- [ ] Should the supervisor also restart the Integrator on failure, or only the Coordinator?
- [ ] Should tool presets live in a file (`tool-presets.toml`) for user extensibility, or are hardcoded presets sufficient for now?
- [ ] When both `Cargo.toml` and `package.json` exist (e.g., wasm-bindgen project), which preset wins? (Current answer: priority order in `MARKER_ORDER` — `package.json` wins.)
- [ ] For greenfield projects in foreign repos, should tool detection re-run when marker files appear, or should the coordinator specify tools in the goal?

## References

- `docs/design/2026-02-26-multi-level-rwl.md` — MVP4 design (promised supervisor, not implemented)
- `docs/design/2026-02-26-implementer-reviewer-agents.md` — MVP3 design (Implementer/Reviewer agents, tool execution)
- `docs/design/2026-03-01-manual-test-findings.md` — Manual e2e test findings
- `src/agents/lifeguard.rs` — Lifeguard loop detector
- `src/agents/coordinator.rs` — Coordinator FSM and footer generation
- `src/daemon/mod.rs` — Daemon main loop
- `src/config.rs` — Tool and validator configuration
