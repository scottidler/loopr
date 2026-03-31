# Design Document: First Autonomous End-to-End Run

**Author:** Scott Idler + Claude
**Date:** 2026-03-30
**Status:** Implemented
**Review Passes Completed:** 5/5
**Supersedes:** `2026-03-21-first-end-to-end-run.md`

## Summary

Loopr's orchestration pipeline (Chat -> Plan -> Coordinator -> Implementer -> Reviewer -> Integrator -> GoalComplete) is fully implemented but has never completed an autonomous run. This document defines the test protocol: a disposable target repo, a trivial task, and a fix-forward strategy for surfacing and resolving bugs in the orchestration machinery.

## Problem Statement

### Background

Loopr is an orchestrator that operates on external target repositories. Its pipeline is complete:
- **Chat funnel**: `/plan` -> `/draft` -> `/accept` -> Plan + CoordinatorGoal creation
- **Coordinator FSM**: Planning -> ActivatePhase -> Executing -> PhaseGate -> GoalComplete
- **Implementer**: picks up Ready Work, iterates with built-in tools, proposes Bundles
- **Reviewer**: single-pass LLM review (Approve / RequestChanges / Reject)
- **Integrator**: merges Accepted Bundles into Ticks, validates, publishes
- **Completion sweep**: Coordinator transitions Integrated -> Done

Every piece exists. But the system has never completed an end-to-end run on a real task.

### Problem

Without an end-to-end run, we don't know:
1. Whether the Coordinator's LLM-generated Plan decomposes into valid Specs/Phases/Works
2. Whether the Implementer can produce code that compiles and passes tests
3. Whether the Reviewer accepts valid Bundles
4. Whether the Integrator can merge Bundles and produce valid Ticks
5. Whether phase gating and goal completion logic fires correctly
6. What the actual failure modes are vs. the theoretical ones

Previous E2E attempts (7 bugs fixed so far) incorrectly targeted the Loopr repo itself. Loopr is an orchestrator - it must target an external repo.

### Goals

- **G1**: Complete one autonomous run from Chat to GoalComplete against an external target
- **G2**: Document every failure encountered and the fix applied
- **G3**: Establish a repeatable test protocol for future runs
- **G4**: Identify which roadmap items (P2-P6) are actually blocking vs. nice-to-have

### Non-Goals

- Interview funnel refinement - use `/plan` + manual chat, bypass interview quality
- Coverage evaluator bubble-up - test with a plan simple enough to not need regeneration
- Multi-pass Reviewer - single-pass review is sufficient
- Prompt audit - address after we have a working baseline
- Heavy runner lane architecture - tools run in-process for now
- Multi-repo or target-switching - one daemon instance per target repo is fine

## Proposed Solution

### Overview

Scaffold a disposable Rust CLI repo in `/tmp`, start the Loopr daemon from that directory, give it a trivial task, and let it run. Fix bugs as they surface. Repeat until GoalComplete.

### The Target Repository

A disposable scaffold created fresh before each run:

```bash
cargo init /tmp/loopr-e2e-target
cd /tmp/loopr-e2e-target && git add -A && git commit -m "init"
```

This produces a minimal `src/main.rs` (`fn main() { println!("Hello, world!"); }`).

**Why disposable:**
- No risk to any real project
- Recreatable for clean-slate retries
- Minimal codebase - Implementer reads everything in one shot
- Simplest possible target reduces debugging variables

### The Test Task

**Task**: "Add a `--version` flag that prints the crate version from `CARGO_PKG_VERSION` to stdout."

**Why this task:**
- Single file change (`src/main.rs`, ~5 lines of scaffold)
- Clear acceptance criteria (run with `--version`, see version string)
- Testable (verify output in a test)
- Verifiable via `cargo test`
- No external dependencies needed (manual arg parsing is sufficient)
- Small enough for one Phase with 1-2 Work items

### How Loopr Targets External Repos

The daemon operates on whatever repo `config.project.repo_path` points to, which defaults to the current working directory (`src/config.rs:524`). Worktrees are created under `config.project.worktree_dir` (defaults to `.worktrees/` relative to `repo_path`).

To target the scaffold repo, start the daemon from that directory:

```bash
cd /tmp/loopr-e2e-target
loopr daemon
```

In a second terminal, connect the TUI from the same directory:

```bash
cd /tmp/loopr-e2e-target
loopr
```

No `--target` flag or config changes needed. One daemon instance per target repo.

### Agent Configuration

From `src/config.rs`:

| Agent | Model | Max Iterations | Timeout | Notes |
|-------|-------|---------------|---------|-------|
| Coordinator | `claude-opus-4-6` | Unbounded (FSM) | None | `max_requeries: 3`; `max_validation_attempts: 3` |
| Implementer | `claude-sonnet-4-6` | 20 | 30 min | Budget-exhaustion prompt on iteration 19 |
| Reviewer | `claude-sonnet-4-6` | 5 | 10 min | Typically 1 iteration |
| Researcher | `claude-sonnet-4-6` | 10 | 10 min | Context gathering |
| Integrator | `claude-sonnet-4-6` | N/A | N/A | Agent + daemon handlers |

### Execution Flow

#### Pre-flight

1. Ensure `ANTHROPIC_API_KEY` is exported.
2. Scaffold the target: `cargo init /tmp/loopr-e2e-target && cd /tmp/loopr-e2e-target && git add -A && git commit -m "init"`.
3. Verify scaffold compiles: `cargo check`.
4. Kill any existing Loopr daemon: `loopr daemon stop` (a daemon running from the Loopr repo will conflict).
5. Start the daemon from the target directory: `cd /tmp/loopr-e2e-target && loopr daemon`.
6. Connect the TUI from the target directory: `cd /tmp/loopr-e2e-target && loopr`.
7. Verify Dashboard shows "Connected" and version matches.

#### Chat funnel (human + LLM)

7. Type the task prompt: "Add a --version flag that prints the crate version from CARGO_PKG_VERSION to stdout."
8. Type `/plan` - transitions to `FunnelState::Interview`.
9. Answer 1-2 clarifying questions.
10. Type `/draft` - LLM generates a structured Plan.
11. Review the plan. Ensure single-phase, 1-2 work items.
12. Type `/accept` - sends `IpcAction::AcceptPlan` to daemon.

#### Orchestration (Coordinator)

13. Daemon creates a Plan (Draft -> Active) and a CoordinatorGoal.
14. Coordinator auto-starts.
15. **Planning**: generates Spec, decomposes into Phase(s) and Work item(s).
16. **ActivatePhase**: activates first Phase, transitions Work items to Ready.
17. **Executing**: assigns Implementer to Ready Work.

#### Implementation (Implementer)

18. Daemon creates a git worktree at `.worktrees/<work_id>/` in the target repo on branch `agent/<work_id>`.
19. Implementer iterates (up to 20):
    - Reads `src/main.rs`
    - Edits to add `--version` handling
    - Runs `cargo check` and `cargo test`
    - Self-corrects on failures
20. Implementer calls `propose_bundle` - creates Bundle (Proposed).

#### Review and integration

21. Coordinator triages Bundle (Proposed -> Triaged).
22. Reviewer evaluates and renders verdict (Approve / RequestChanges / Reject).
23. On Approve: Coordinator transitions Reviewed -> Accepted.
24. Integrator validates and publishes: merges worktree branch, creates Tick.
25. Coordinator sweep: Integrated -> Done.
26. Phase complete -> PhaseGate -> **GoalComplete**.

#### Status transitions (happy path)

- **Plan**: Draft -> Active
- **Spec**: Draft -> Active -> Done
- **Phase**: Draft -> Active -> Done
- **Work**: Draft -> Ready -> InProgress -> InReview -> Integrated -> Done
- **Bundle**: Proposed -> Triaged -> Reviewed -> Accepted -> Integrating -> Merged
- **Tick**: Open -> Sealing -> Validating -> Published
- **Coordinator FSM**: Planning -> ActivatePhase -> Executing -> PhaseGate -> GoalComplete

### Monitoring

```bash
loopr diagnose dump      # Full diagnostic (FSM state, token usage)
loopr diagnose agents --failed  # Failed agents only
loopr diagnose state     # TaskStore snapshot
loopr diagnose log --tail       # Session log
```

**TUI views**: Chat (orchestration events), Dashboard (Work status), Agents (Implementer state).

**Logs**: `~/.local/share/loopr/sessions/{session_id}/`

### Headless Alternative: `loopr run`

```bash
cd /tmp/loopr-e2e-target
loopr run --plan "Add a --version flag..." "Add a --version flag that prints the crate version from CARGO_PKG_VERSION to stdout." --timeout 600
```

Use `--plan` to skip the interview (headless has no human to answer questions). Exit codes: 0=GoalComplete, 1=timeout, 2=NeedHelp.

### Fix-Forward Protocol

When a failure occurs:
1. Stop the system (`/stop` or `loopr agent stop <id>`).
2. Diagnose: `loopr diagnose dump`, inspect TaskStore, read agent logs.
3. Apply minimal fix in the **Loopr repo** (not the target).
4. `otto ci` to verify.
5. Commit: `fix(scope): description - first e2e run`
6. Rebuild and reinstall Loopr: `cargo install --path ~/repos/scottidler/loopr`
7. Re-scaffold the target (clean slate) and restart.

### Fix-Forward Log

Bugs discovered during previous E2E attempts. These fix Loopr's orchestration machinery and are valid regardless of target repo.

| # | Bug | Commit | Description |
|---|-----|--------|-------------|
| 1 | Coordinator action name mismatch | `489a5b4` | `coordinator.rs:468` used wrong action name |
| 2 | accept_plan missing CoordinatorState | `489a5b4` | `handlers.rs` didn't set state on accept |
| 3 | Poll loop wrong RPC method | `2131540` | `dispatch.rs:174` used `coordinator.goal` (not registered), fixed to `coordinator.get_state` |
| 4 | Missing Ready -> Blocked FSM transition | `2131540` | `domain/work.rs` only allowed `InProgress -> Blocked` |
| 5 | ReadFile no line cap | `2131540` | Entire file returned, format_action_summary truncated to 4000 bytes, causing retry loops |
| 6 | No edit_file action | `0675c64` | Implementer had no surgical edit tool, only full file write |
| 7 | ReadFile retry loop (no dedup) | `28f00ec` | Same file re-read returned same content; mtime-based dedup now returns "file unchanged" |
| 8 | Reviewer received work IDs as bundle IDs | `724b4d4` | Added bd-* prefix validation on TriageBundle, AcceptBundle, and AssignAgent(reviewer); guarded post-dispatch hook |
| 9 | Implementers produced zero code changes | `f565f9b` | ProposeBundle now auto-commits pending filesystem changes; force-propose logs failures instead of silently discarding |

| 10 | Integrator disabled by default | n/a (config) | `IntegratorConfig.enabled` defaults to `false`; e2e.sh now writes explicit config with `enabled: true` |

All 10 bugs fixed. The first clean run succeeded on 2026-03-30.

### First Successful Run (2026-03-30)

**Result: GoalComplete** - full pipeline completed autonomously.

| Stage | Agent | Iterations | Result |
|-------|-------|-----------|--------|
| Planning | Coordinator (`ag-qikke`) | FSM: Planning -> ActivatePhase -> Executing -> GoalComplete | 1 Work item created |
| Implementation | Implementer (`ag-d05om`) | 4 | Wrote `src/main.rs` + `tests/version_test.rs` |
| Review | Reviewer (`ag-0m9io`, `ag-ogwju`, `ag-2rzqv`) | 1 each | Approved with minor style notes |
| Integration | Integrator (`ag-any47`) | deterministic | Merged bundle branch into main |

**Target repo final state:**
- `src/main.rs`: early-exit guard using `std::env::args()` + `env!("CARGO_PKG_VERSION")`
- `tests/version_test.rs`: 2 integration tests (--version output + normal Hello World)
- `cargo test`: 2 passed, 0 failed
- `--version` output: `0.1.0`
- Git log: `init` -> `Add --version flag` -> `impl: ...` -> `Merge bundle branch agent/wk-639ed`

**Automated via `bin/e2e.sh`**: builds loopr, scaffolds target, writes config, starts daemon, runs headless, verifies results.

### State Recovery

For the first E2E run, prefer **clean restarts**: stop daemon, re-scaffold target, restart. State recovery (resuming from checkpoint) is itself untested.

## Alternatives Considered

### Alternative 1: Write integration tests first
- **Description:** Mock LLM responses and exercise the full pipeline.
- **Pros:** Repeatable, CI-able, no API costs.
- **Cons:** Testing mocks, not the system. Hardest bugs are in LLM interaction.
- **Why not chosen:** A real run with a real LLM finds real bugs faster.

### Alternative 2: Bottom-up component testing
- **Description:** Test each agent in isolation before combining.
- **Pros:** Isolates failures.
- **Cons:** Bugs we care about are at component boundaries. Component tests already exist.
- **Why not chosen:** Components work individually. The question is whether they work together.

### Alternative 3: Run Loopr on itself
- **Description:** Use Loopr to modify its own source code.
- **Pros:** No external repo needed.
- **Cons:** Fundamentally wrong. Loopr is an orchestrator - it operates on other repos. Conflates the tool with the workpiece. Shared git DB, build lock collisions, conceptual confusion.
- **Why not chosen:** Violates the core design principle.

### Alternative 4: Target an existing real repo (e.g. `scottidler/cidr`)
- **Description:** Run against a real project with real code.
- **Pros:** More realistic test.
- **Cons:** Adds risk to a real project. More code for the Implementer to navigate. Failures harder to attribute.
- **Why not chosen:** A disposable scaffold minimizes variables. Graduate to real repos after the pipeline works.

### Alternative 5: Start with a larger multi-phase task
- **Description:** 5+ Work items to exercise phase gating and parallel work.
- **Pros:** Tests more pipeline surface area.
- **Cons:** If phase 1 doesn't work, phases 2-5 don't matter.
- **Why not chosen:** Start small, succeed once, then scale.

## Technical Considerations

### Dependencies

- **LLM API**: Coordinator (opus), Implementer/Reviewer/Researcher (sonnet) - all require `ANTHROPIC_API_KEY`
- **Target repo**: `/tmp/loopr-e2e-target` scaffolded via `cargo init` with clean git state
- **Daemon**: must be started from the target directory so `config.project.repo_path` resolves correctly
- **Cargo**: Implementer invokes `cargo check`/`cargo test` via Shell tool in the target worktree

### TaskStore Location

The TaskStore opens relative to `repo_path` (`src/daemon/context.rs:203`). When the daemon runs from `/tmp/loopr-e2e-target`, the TaskStore JSONL and SQLite files are created there, not in the Loopr repo. Each target gets its own TaskStore.

### Testing Strategy

This IS the testing strategy. Success = one task goes from Chat to GoalComplete autonomously. Future runs reuse this protocol with different tasks and different target repos.

### Rollout Plan

1. Execute the test run (Pre-flight through GoalComplete).
2. Fix issues as they surface (fix-forward protocol).
3. Run a second time with a clean scaffold to verify fixes hold.
4. Run headless (`loopr run`) to verify the non-TUI path.
5. Update `docs/next-steps.md` to mark P1 as complete.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| LLM API rate limits or outages | Low | High | Run during off-peak; sufficient quota |
| Coordinator infinite loop | Medium | Medium | Lifeguard escalation; `/stop` available |
| Implementer exhausts iterations with no output | High | Medium | Fixed (Bug #9). ProposeBundle now auto-commits. Implementer completed in 4 iterations on successful run |
| Reviewer rejects valid work | Medium | Low | Tune prompt if too aggressive |
| Worktree cleanup fails in target repo | Low | Low | `git worktree prune` as fallback |
| First run reveals many bugs | High | Low | Expected. Each fix is progress |

## Success Criteria

All criteria met on 2026-03-30:

1. [x] The `loopr` process reached **GoalComplete** (exit code 0).
2. [x] Running the target binary with `--version` prints `0.1.0`.
3. [x] `cargo test` passes: 2 passed, 0 failed.
4. [x] Fix-forward log documents all 10 bugs and their fixes.

## Open Questions

- [x] Is there a way to resume a failed run? - **Answered**: prefer clean restarts for now.
- [x] How does Loopr target an external repo? - **Answered**: daemon uses CWD as `repo_path`. Start from the target directory.
- [x] Where are worktrees created? - **Answered**: `.worktrees/` relative to `repo_path` (the target repo).
- [x] Bug #9 root cause: why did implementers produce zero code changes? - **Answered**: write_file/edit_file only write to filesystem, never git commit. ProposeBundle only created a Bundle record without committing. Fixed: ProposeBundle now auto-commits pending changes.
- [ ] Does the Coordinator detect Implementer timeout (30 min) and retry, or does it hang?

## References

- Orchestration spine: `docs/design/2026-02-25-orchestration-spine.md`
- Chat-to-orchestration bridge: `docs/design/2026-03-17-chat-to-orchestration-bridge.md`
- Multi-level RWL: `docs/design/2026-02-26-multi-level-rwl.md`
- Native tool use: `docs/design/2026-03-04-native-tool-use.md`
- ReadFile dedup: `docs/design/2026-03-30-read-file-dedup.md`
- E2E test script: `bin/e2e.sh`
- Previous version: `docs/design/2026-03-21-first-end-to-end-run.md`
