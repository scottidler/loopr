# Ralph Wiggum Loop — ONE TASK THEN EXIT

You are in a Ralph Wiggum loop. You have NO MEMORY of previous runs.
Your state persists ONLY in `progress.txt` and the git history.

## CRITICAL RULES

1. **CHECK THE INJECTED CONTEXT BELOW** — progress, validation output, and quality gate results from previous iterations are appended to this prompt automatically. You do NOT need to read progress.txt yourself.
2. **DO ONE LOGICAL UNIT OF WORK** — see "What is one task?" below. Batch similar/mechanical changes together, but don't mix unrelated work.
3. **EXIT IMMEDIATELY** — do not retry failures. Do not loop. Just exit.
4. **If validation failed last iteration, FIX THAT FIRST** — the validation output is injected below. Read it, fix the issue, done.
5. **WIRE CODE IN BEFORE MOVING ON** — dead code = compile failure (`-D dead-code`). If you write a function, it must be called somewhere. If you add a module, it must be `pub mod`'d.
6. **IF YOUR ITERATION IS GETTING LONG, STOP** — commit what you have and exit. The next iteration picks up where you left off. Timeouts waste everything.

The bash loop restarts you with fresh context. That's the whole point.
The bash loop runs validation EXTERNALLY — you do NOT run `otto ci`.

---

## Your Workflow

```bash
# 1. Check injected context (appended below this prompt automatically):
#    - Progress from previous iterations
#    - Last validation output (if it failed)
#    - Last quality gate output (if it failed)
#    You already have this — no need to cat files.

# 2. Optionally check current code state
git log --oneline -10
ls src/ 2>/dev/null || echo "src/ not created yet"

# 3. Do ONE small task (see "What is ONE task?" below)

# 4. Record what you did
echo "Iteration N: <what you did>" >> progress.txt

# 5. If ALL work is complete (ALL 6 phases done, ALL success criteria met):
echo "<promise>COMPLETE</promise>"

# 6. EXIT — do nothing else
```

---

## What is ONE task?

**Batch similar/mechanical work together.** If the changes follow the same pattern (same transformation applied to multiple types/handlers), do them all in one iteration. Don't waste iterations doing one type at a time — they're the same pattern with different type names.

**YES — do these (each is one iteration):**
- Add ALL strategy knob types (`StalePolicy`, `ConflictPolicy`, `TickCadence`, `BundleSizePolicy`, `ValidatorStrictness`, `PromotionPolicy`) to `StrategyConfig` in `config.rs`
- Create `src/agents/context.rs` — generic `ContextBuilder`, `TokenBudget`, `AssembledContext`, `select_learnings()`
- Enrich `Learning` model — add `applicable_roles`, `resource_tags`, `confidence` fields with `#[serde(default)]`, `recompute_confidence()`, auto-promotion in `reinforce()`
- Refactor `implementer.rs` to use new `ContextBuilder` (replace `load_context()` + `build_user_message()`)
- Refactor `reviewer.rs` to use new `ContextBuilder` (replace `load_review_context()` + `build_review_message()`)
- Create `src/agents/coordinator.rs` — system prompt, context loading, action parsing, long-lived loop
- Add `AgentType::Coordinator` + `CoordinatorConfig` + Coordinator actions to `AgentAction` enum
- Add Coordinator executor dispatch + `execute_action()` cases for all Coordinator actions
- Add `coordinator.set_goal` / `coordinator.clear_goal` IPC handlers + `CoordinatorGoal` record
- Create `src/agents/generation.rs` — Plan/Spec/Phase/WorkItem generation prompts
- Wire generation prompts into Coordinator with validate-iterate loop
- Create `src/agents/researcher.rs` — system prompt, search actions, codebase investigation
- Add `AgentType::Researcher` + `SearchCode`/`SearchFiles`/`ListDirectory` actions + path sandboxing
- Create `src/agents/integrator_task.rs` — deterministic Tick lifecycle (no LLM)
- Extend `IntegratorConfig` with `interval_secs` + `enabled`, extend crash recovery for stuck Ticks
- Add `AcquireLock`/`ReleaseLock` to Coordinator actions, wire lock checking into executor `WriteFile`
- Add `Role::Researcher` to role enum
- Extend `AgentSession` with `target_id` and `query` fields
- Add `role` field to `AgentAction::Transition`, wire role inference in executor
- Add TUI keybindings (`g` set goal, `p` pause, `r` resume, `x` stop) and CLI commands
- Fix a compile error or test failure

**NO — too much:**
- "Implement Phase 1" (context builder + learning enrichment + strategy knobs = multiple units)
- Add coordinator + researcher + integrator in one iteration
- Mix unrelated work (e.g., fix a bug AND add a new feature)

## On Previous Validation Failure

If the injected context below shows a FAIL or validation output with errors:
1. Read the validation/quality gate output (injected below — you already have it)
2. For more detail, read `logs/iter-NNN-validation.log` and `logs/iter-NNN-claude.log` for the failing iteration
3. Fix that ONE thing
4. Record what you fixed
5. EXIT immediately

---

## Project: Loopr v3 MVP4

### What You're Building

Loopr is a TUI-based "dev team in a box" orchestrator. MVP4 completes the full vision by extending the Ralph Wiggum Loop from code-level only (Implementer + Reviewer) to ALL four levels: Plan → Spec → Phase → Code. Four major additions:

1. **Context Builder** — Generic `build_context()` replacing hardcoded `load_context()` in implementer/reviewer. Per-role context slicing, token budgeting, enriched Learning selection.
2. **Coordinator Agent** — The meta-Ralph. LLM agent that operates at all 4 levels: generates Plans/Specs/Phases/WorkItems, assigns Implementers, triages Bundles, manages locks, spawns Researchers. Long-lived Tokio task with adaptive timer.
3. **Researcher Agent** — Codebase investigation. Searches code (ripgrep), reads files, produces Learnings. Also serves as Proposer for Spec/Phase generation (lightweight swarm). Read-only, path-sandboxed.
4. **Deterministic Integrator** — Automates Tick lifecycle (create/seal/validate/publish). NOT an LLM agent — pure state machine code. No prompt, no parser.

Plus: strategy knobs (StalePolicy, ConflictPolicy, TickCadence, etc.), advisory lock system, Learning auto-promotion, iteration records as structured Learnings.

### Starting Point

**MVP1 + MVP2 + MVP3 are complete.** The codebase has:
- ~22,500 lines of Rust across 49 `.rs` files, 762 tests
- 8 domain types with 3 FSMs (WorkItem, Bundle, Tick) + HierarchyStatus
- Daemon with IPC handlers over Unix socket (NDJSON)
- TaskStore persistence (JSONL + SQLite) via `scottidler/taskstore`
- Doc Validator (read-only LLM gating Draft→Active) using `ureq`
- Ratatui TUI with 6 views (Dashboard, WorkItems, Bundles, Ticks, Learnings, Agents) + CLI
- Git worktree management with staleness guards
- Implementer agent (RWL in worktrees, produces Bundles)
- Reviewer agent (single-shot Bundle review)
- Agent streaming (SSE via reqwest, broadcast channel)
- Tool execution system (subprocess with timeout/truncation)
- Crash recovery on startup

### Adding Dependencies

**ALWAYS use `cargo add` to add dependencies.** Never hand-write version numbers in `Cargo.toml`.

```bash
# For glob pattern matching (Researcher SearchFiles):
cargo add glob
```

Existing deps cover everything else: `reqwest` (async HTTP), `ureq` (sync), `tokio`, `serde`, `ratatui`, `taskstore`, etc.

**Read the design doc:** `docs/design/2026-02-26-loopr-v3-mvp4.md`
This is the single source of truth for MVP4. It contains all data models, prompts, strategy knobs, implementation phases, and success criteria.

**Previous design docs (reference):**
- `docs/design/2026-02-26-loopr-v3-mvp3.md` — Implementer + Reviewer agents
- `docs/design/2026-02-26-loopr-v3-mvp2.md` — TaskStore + Doc Validator
- `docs/design/2026-02-25-loopr-v3-mvp1.md` — Orchestration spine

### Architecture Summary

- **Single Rust binary** (`loopr`) operating in daemon or TUI/CLI mode
- **Daemon** — Tokio process, single authority for all state mutations, FSM validation, worktree management
- **Two planes:** Thinking (Coordinator, Researcher, Reviewer — Tokio tasks, no worktrees) and Changing (Implementer — worktrees, writes code)
- **Integrator** — deterministic Tokio task (no LLM), runs validation commands in repo root
- **AgentIpcBridge** — in-process channel for agent↔daemon communication. Same `dispatch()` as socket IPC.
- **Context Builder** — generic per-role context assembly with token budgeting and learning selection
- **Strategy Knobs** — configurable policies (StalePolicy, ConflictPolicy, TickCadence, BundleSizePolicy, ValidatorStrictness, PromotionPolicy)
- **Advisory Locks** — Coordinator acquires/releases locks on resources; executor checks locks on WriteFile per ConflictPolicy

### Key Technical Details

**Coordinator is a long-lived Tokio task** with an adaptive timer (5s active, 30s idle), NOT a fixed-iteration loop like Implementer. It runs until cancelled or NeedHelp.

**The Integrator is NOT an LLM agent.** It is deterministic Rust code. No system prompt, no response parser, no LLM client. Just a loop: check for Accepted Bundles → create Tick → seal → validate → publish/fail. The existing `IntegratorConfig.validation_commands` drives it.

**Coordinator goal persists in TaskStore** as a `CoordinatorGoal` record (not DaemonContext — survives daemon crashes).

**`AgentAction::Transition` gets a `role` field.** The executor infers role from agent_type when role is None. This fixes the bug where Integrator bundle transitions would fail.

**Per-role action sub-enums** (`CoordinatorAction`, `ResearcherAction`) convert `Into<AgentAction>` for execution. This prevents the god-type problem and enables compile-time enforcement.

**Researcher path sandboxing:** All file operations validate paths within repo root. Absolute paths rejected. Symlink following disabled. Denylist: `.env`, `*.key`, `*.pem`, `credentials.*`, `*secret*`.

**Learning auto-promotion:** `reinforce()` checks if reinforcements >= threshold AND contradictions == 0 → auto-promote to Policy. Contradiction after promotion → notify Coordinator, do NOT auto-demote.

**SealTick is validate-then-mutate:** Check all Bundle preconditions before any mutations. If any check fails, no mutations occur.

### New Modules (target)

```
src/
├── agents/
│   ├── mod.rs             # +AgentType::Coordinator/Researcher/Integrator, +new AgentActions
│   ├── context.rs         # NEW — ContextBuilder, TokenBudget, select_learnings()
│   ├── coordinator.rs     # NEW — Coordinator agent loop (long-lived, adaptive timer)
│   ├── generation.rs      # NEW — Plan/Spec/Phase/WorkItem generation prompts
│   ├── researcher.rs      # NEW — Researcher agent (codebase search, proposer mode)
│   ├── integrator_task.rs # NEW — Deterministic Integrator (no LLM)
│   ├── implementer.rs     # MODIFY — use ContextBuilder
│   ├── reviewer.rs        # MODIFY — use ContextBuilder
│   ├── executor.rs        # MODIFY — new action dispatch, role inference, lock checking
│   ├── bridge.rs          # (unchanged)
│   └── llm_client.rs      # (unchanged)
├── domain/
│   ├── learning.rs        # MODIFY — applicable_roles, resource_tags, confidence, auto-promotion
│   ├── role.rs            # MODIFY — +Role::Researcher
│   └── (others unchanged)
├── config.rs              # MODIFY — +StrategyConfig, +CoordinatorConfig, extend IntegratorConfig
├── daemon/
│   ├── handlers.rs        # MODIFY — +coordinator.set_goal, +coordinator.clear_goal, pool_size enforcement
│   └── context.rs         # MODIFY — +CoordinatorGoal, extend crash recovery for stuck Ticks
├── tui/
│   └── input.rs           # MODIFY — +keybindings (g, p, r, x)
├── cli/
│   └── dispatch.rs        # MODIFY — +coordinator commands
└── (existing modules unchanged)
```

### Implementation Phases

#### Phase 1: Context Builder + Learning Enrichment + Strategy Knobs (Foundation)
- `src/agents/context.rs` — ContextBuilder, TokenBudget, select_learnings()
- `src/domain/learning.rs` — enrich with applicable_roles, resource_tags, confidence, auto-promotion
- `src/domain/role.rs` — add Role::Researcher
- `src/config.rs` — add StrategyConfig with all knobs
- Refactor implementer.rs and reviewer.rs to use ContextBuilder

#### Phase 2: Coordinator Agent
- `src/agents/coordinator.rs` — long-lived loop, system prompt, context loading, action parsing
- `src/agents/mod.rs` — AgentType::Coordinator, extend AgentSession, extend AgentAction, add role to Transition
- `src/agents/executor.rs` — Coordinator dispatch, new action execution, role inference, cancellation check
- `src/config.rs` — CoordinatorConfig, coordinator field in AgentConfig
- `src/daemon/handlers.rs` — agent.start for Coordinator, coordinator.set_goal/clear_goal, pool_size enforcement
- `src/daemon/context.rs` — CoordinatorGoal record, crash recovery for stuck Ticks
- AcquireLock / ReleaseLock actions wired in

#### Phase 3: Document Generation Pipeline
- `src/agents/generation.rs` — level-specific generation prompts
- Wire into Coordinator with validate-iterate loop (max_validation_attempts cap)

#### Phase 4: Researcher Agent
- `src/agents/researcher.rs` — system prompt, search execution, path sandboxing
- AgentType::Researcher, SearchCode/SearchFiles/ListDirectory actions
- Dedup on scope_id, proposer mode via query convention

#### Phase 5: Integrator Task (Deterministic)
- `src/agents/integrator_task.rs` — deterministic loop, merge logic, validation runner
- Extend IntegratorConfig, crash recovery for stuck Ticks
- Validate-then-mutate for SealTick

#### Phase 6: TUI + CLI + Integration Tests
- TUI keybindings (g, p, r, x), CLI commands
- End-to-end integration tests

### Phase Dependencies

```
Phase 1 (Foundation)
  ├── Phase 2 (Coordinator) ── Phase 3 (Generation Pipeline)
  ├── Phase 4 (Researcher)
  └── Phase 5 (Integrator)
All ────────────────────────── Phase 6 (TUI + CLI + Integration)
```

Recommended sequence: Phase 1 → Phase 2 → Phase 3 → Phase 4 → Phase 5 → Phase 6

### Success Criteria (ALL must be met)

1. ContextBuilder produces correct per-role context for all 4 LLM agent types
2. Learning enrichment works: confidence, role filtering, auto-promotion
3. Strategy knobs are configurable via `loopr.yml`
4. Coordinator runs as long-lived Tokio task with adaptive timer
5. Coordinator generates Plans/Specs/Phases/WorkItems via generation prompts
6. Coordinator validates documents via Doc Validator before Draft→Active
7. Coordinator assigns Implementers and Reviewers, respecting pool_size
8. Coordinator manages advisory locks (acquire before assignment, release on completion)
9. Researcher searches codebase, produces Learnings, respects path sandboxing
10. Researcher serves as Proposer when query starts with "Propose..."
11. Integrator is deterministic code (no LLM) automating Tick lifecycle
12. Integrator performs validate-then-mutate for SealTick
13. Stuck Ticks (Sealing/Validating) recovered on daemon restart
14. `AgentAction::Transition` passes role correctly (no FSM rejections from role mismatch)
15. Pool_size enforced as hard guard in handle_agent_start
16. Human can set goal, pause/resume/stop Coordinator via TUI/CLI
17. All agents disabled by default, human-driven workflow unchanged
18. Code compiles, all tests pass, clippy clean, no dead code

---

## Rust Conventions

1. **Async everywhere** — tokio runtime, async fn
2. **Structured errors** — thiserror for types, eyre/anyhow for propagation
3. **Return data** — functions return `Result<T>`, minimize side effects
4. **Tests in every module** — `#[cfg(test)] mod tests { ... }` at the bottom of each file
5. **Use `cargo add`** for dependencies — never manually write version numbers in Cargo.toml
6. **Clippy is strict** — `-D warnings` means ANY warning is a compile failure. Check your work.
7. **Wire everything in** — dead code is a compile error. If you write it, something must call it.

### Testing Requirements

Every module must have tests. When you write code, write tests for it.

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_something() {
        // ...
    }
}
```

- Test public functions and key internal logic
- Use `#[tokio::test]` for async tests
- For LLM agents: use mock/trait-based interface, never hit real API in tests
- Meaningful coverage, not 100% coverage theater

---

## Completion

Output `<promise>COMPLETE</promise>` on its own line when you believe:
- ALL 6 phases are implemented
- ALL 18 success criteria are met
- Code compiles and tests pass
- No `#[allow(dead_code)]` remaining
- No underscore-prefixed unused variables

The bash loop verifies this externally. If validation fails, you'll be restarted.

---

## If You Get Stuck

1. Record the blocker in progress.txt
2. EXIT immediately
3. The next iteration sees the blocker and can try a different approach

**NEVER just exit.** Always update progress.txt first.

---

## Logs

Per-iteration logs are saved in `logs/`:
- `logs/iter-NNN-claude.log` — Full Claude output for iteration NNN
- `logs/iter-NNN-validation.log` — Full `otto ci` validation output for iteration NNN

If you need to investigate what happened in a previous iteration, read the relevant log files.

## Quick Reference

| File | Purpose |
|------|---------|
| `progress.txt` | Your memory between iterations |
| `logs/iter-NNN-claude.log` | Full Claude output for iteration NNN |
| `logs/iter-NNN-validation.log` | Full validation output for iteration NNN |
| `docs/design/2026-02-26-loopr-v3-mvp4.md` | **The MVP4 design doc** — single source of truth |
| `docs/design/2026-02-26-loopr-v3-mvp3.md` | MVP3 design doc (reference) |
| `docs/design/2026-02-26-loopr-v3-mvp2.md` | MVP2 design doc (reference) |
| `docs/design/2026-02-25-loopr-v3-mvp1.md` | MVP1 design doc (reference) |
| `docs/design/mvps.md` | MVP1/2/3/4 comparison matrix |
| `.otto.yml` | CI pipeline definition |

## Start of Iteration Checklist

1. [ ] Read injected context below (progress + validation + quality gates)
2. [ ] If last iteration failed: fix that failure. If not: determine next task.
3. [ ] Optionally run `git log --oneline -10` and `ls src/` for current state
4. [ ] Do one logical unit of work — batch similar/mechanical changes together
5. [ ] Make sure all new code is WIRED IN (no dead code)
6. [ ] `git add <specific files>` then `git commit`
7. [ ] `echo "Iteration N: <what you did>" >> progress.txt`
8. [ ] Check if ALL phases + success criteria complete → `echo "<promise>COMPLETE</promise>"`
9. [ ] EXIT
