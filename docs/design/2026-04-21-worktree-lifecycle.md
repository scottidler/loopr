# Design Document: Stage 7 Worktree Lifecycle

**Author:** Claude (with Scott)
**Date:** 2026-04-21
**Status:** Draft
**Review Passes Completed:** 5/5 + Architect R1 + R2
**Crates touched:** `worktree`, `loopr` (cross-cutting; lives at repo-root `docs/design/` per `docs/CLAUDE.md`).

## Summary

The `worktree` crate provides primitives for creating, listing, and cleaning up per-attempt git worktrees that Implementer agents run inside. Every Work attempt gets a fresh worktree at `<target>/.loopr/worktrees/<work-id>-<seq>/` on branch `loopr/wk-<work-id>-<seq>`, where `<seq>` starts at `1` and increments globally per `work-id` across retries and across runs. Allocation is atomic via `git worktree add`'s own "already exists" errors as the EEXIST-equivalent — the seq loop inside `Worktree::create` retries on collision until it wins. The layout is intentionally flat (not per-run) because `git worktree add` registers each worktree by its path *basename* under `$GIT_DIR/worktrees/<name>/`; putting worktrees under per-run directories would still collide at the git-internal registration level when the same work_id appears across runs. The crate is **infrastructure-only**: it does not know about TaskStore, Plans, or Work status — it exposes primitives and a typed handle with a `Drop`-safety-net cleanup, and the `loopr` binary owns the crash-recovery reconcile routine that joins git state with TaskStore.

This doc deliberately overrides three items in `docs/vision.md`. (1) **No `.loopr/worktree-registry.jsonl`** (D5) — git's own registry (`git worktree list --porcelain`) plus the `loopr/wk-*` branch-name prefix plus TaskStore is a complete, race-free reconcile path; the JSONL is redundant state with its own corruption and locking surface. (2) **No sibling-path layout** (D3) — `<target-parent>/<target-name>-work-<work-id>/` introduces deployment regressions (CI paths, containers, read-only mounts, atomic-cleanup breakage); v5 worktrees live at the **flat** path `<target>/.loopr/worktrees/<work-id>-<seq>/` inside the target. (3) **Reconcile lives in `loopr` binary, not `worktree` crate** (D6) — reconcile requires joining git state with TaskStore + Work FSM mutations, which `worktree` (infrastructure-only) must not depend on. All three amendments originated from the Architect consultation (R1 findings #1, #2, #5); the companion vision-doc PR is listed in Rollout Plan.

## Decisions

Locked up front. Pre-design brief at `crates/worktree/docs/design/2026-04-21-pre-design-brief.md` captured v4 code archaeology + git-history evolution. Architect consultation (2026-04-21, R1) produced findings #1, #2, #4, #5 that this doc adopts as D3/D5/D6/D11. R2 (post-draft audit, 2026-04-21) produced four corrections adopted below (D2/D3/D11 edits + Phase 4 update): basename-collision reasoning correction, `LC_ALL=C` hardening on git subprocess calls, `Never`-policy memory-leak semantics, and a `spawn_blocking`-based cleanup pattern so Drop doesn't starve the tokio executor. R2 also confirmed D6 (reconcile in `loopr`) and validated the implicit adoption of R1 Finding #3 via D1+D8 (v4 branch-reuse retry trauma is structurally impossible in v5).

| # | Decision | Choice |
|---|---|---|
| D1 | Attempt identity | Every Work attempt gets a fresh worktree + fresh branch. No branch reuse across attempts; no retry-on-existing-branch code path. |
| D2 | Seq allocation | Monotonic `-1`, `-2`, `-3`, ... suffix per `(work-id)`. Allocated **internally by `Worktree::create`** by looping seq from 1 and attempting `git worktree add`; on "already exists" / "already checked out" / "is not an empty directory" errors (matched against stderr with `LC_ALL=C` forced on the subprocess to disable localization) it retries with seq+1. Git's own branch-creation refcount is the serialization primitive. First attempt is `-1`; cap 1000. Coordinator never sees or picks the seq; it reads it off the returned handle. **R2 hardening:** every `std::process::Command` for git sets `.env("LC_ALL", "C")` so stderr phrasing is stable across locales; exit code 128 is NOT used as a retry signal (it's git's generic `fatal:` code for disk-full / permission-denied, which would spin-loop). |
| D3 | Worktree location | `<target>/.loopr/worktrees/<work-id>-<seq>/` — **interior** to the target, **flat** (not nested under `runs/<run-id>/`). Flat because `git worktree add` derives the internal registry folder name (`$GIT_DIR/worktrees/<name>/`) from the path's basename; duplicate basenames do NOT hard-collide (git auto-disambiguates with its own integer suffixes like `wk-1` / `wk-11`) but that auto-disambiguation breaks deterministic reverse-mapping from `.git/worktrees/` back to our domain IDs — a debugging nightmare we avoid by making the basename globally unique in our own space (R2 correction; R1 originally implied hard collision). Provenance (which run created an attempt) is recorded in TaskStore's Work record, not in the filesystem path. **Vision amendment** (vision prescribed sibling `<target-parent>/<target-name>-work-<work-id>/`). |
| D4 | Branch naming | `loopr/wk-<work-id>-<seq>`. Deterministic from work-id + seq; never derived from git state. Prefix `loopr/wk-` is the provenance marker that distinguishes our branches from any user-created branch. |
| D5 | Registry file | **None.** Drop the vision's prescribed `.loopr/worktree-registry.jsonl`. Reconcile joins `git worktree list --porcelain` with branch-name-parse and TaskStore. **Vision amendment.** |
| D6 | Crash-recovery reconcile | Lives in the **`loopr` binary crate** (`loopr::daemon::startup::reconcile`), not in `worktree`. `worktree` is infrastructure-only; reconcile needs a store+domain dependency that `worktree` must not have. **Vision amendment** (vision placed `reconcile` inside `worktree`). |
| D7 | Three distinct lifecycle events | `cleanup_worktree` (end of implementer session, pre-Tick; **keeps branch**), `delete_branch` (after Tick publishes; **integrator's responsibility**), and implicit retry-via-new-attempt. Conflating these broke v4 twice (commits `86f3278` → `b78465b` rollback; `57ed30c` preserving implementer work). |
| D8 | Retry policy (implicit) | Retry is "spawn a fresh worktree at seq+1 from current integration tip." No branch reuse, no rebase, no commit preservation. v4's NO-OP-LOOP fix (commit `120c29b`) is made structural: there is no code path to preserve rejected commits because there is no reused branch. |
| D9 | `git worktree prune` once per `Worktree::create` call | Non-fatal on failure. Clears orphaned registrations left by crashed sessions. Called once at entry to `create`, NOT inside the seq-retry loop — pruning during the loop would create new race conditions. Ported verbatim from v4 commit `c4b4158`. |
| D10 | Base ref SHA resolved in repo context | `git rev-parse <base_ref>` runs with `.current_dir(repo_path)`, not inside the worktree. Prevents the "HEAD-inside-worktree resolves to the agent branch tip" gotcha v4 documented in commit `120c29b`. |
| D11 | `AttemptCleanupPolicy` enum | Four variants — `Immediate`, `OnWorkTerminal`, `OnRunEnd`, `Never`. Default `OnWorkTerminal`. Exposed via `.loopr/config.yml` (`worktree.cleanup-policy`), ENV (`LOOPR_WORKTREE_CLEANUP_POLICY`), and CLI (`--worktree-cleanup <mode>`). Precedence: CLI > ENV > config > default. Debugging-oriented knob: flip to `OnRunEnd` or `Never` when investigating "why did the implementer produce garbage" without a code change. **`Never` is strict debug-only (R2):** the coordinator parks handles in a long-lived `Vec`, which leaks memory and file descriptors on a multi-week daemon; changing the config mid-flight from `Never` → any other variant does NOT retroactively clean Works that already terminated under `Never` (cleanup fires only on the edge transition); a daemon restart is required to trigger `reconcile` and clear accumulated state. |
| D12 | Drop `Worktree::refresh` from v5 API | v4's `refresh(work_id, new_base_ref)` (rebase existing worktree onto new Tick) is obsolete: a retry spawns a new worktree at the current integration tip at seq+1. Smaller v5 API than v4. |
| D13 | `ensure_loopr_excludes` pattern list (v5-updated) | Idempotent append to `.git/info/exclude` with `# loopr-managed` marker (ported verbatim from v4). Patterns: `.loopr/runs/`, `.loopr/worktrees/`, `.loopr/socket`, `.loopr/daemon.pid`, `.loopr/config.yml`. `.loopr/taskstore/` is NOT excluded — per vision, TaskStore IS committed. |

## Problem Statement

### Background

Stage 7 (`docs/roadmap.md` lines 110-121) produces a Bundle from an Implementer agent running inside a sibling git worktree. The Implementer needs an isolated filesystem + branch so its edits don't mix with the target repo's main checkout, the Reviewer can read a clean diff, and the Integrator can merge the branch independently.

v3 and v4 shipped a byte-identical `WorktreeManager` (`~/repos/scottidler/loopr/src/worktree/manager.rs` == `~/repos/scottidler/loopr-v4/src/worktree/manager.rs`, 857 lines). Its git-history evolution (detailed in the pre-design brief) went through five distinct failure modes between Feb 28 and Apr 10, 2026:

1. `86f3278` → `b78465b` (30 min later): cleanup was deleting the branch too, breaking the integrator's merge.
2. `67758ea`: derived branch name from `git rev-parse --abbrev-ref HEAD` which returned `"main"` on detached HEAD.
3. `57ed30c`: retry deleted the branch, destroying uncommitted implementer work.
4. `c4b4158`: crashed sessions left git with "missing but already registered" → `git worktree prune` before create.
5. `0ce1226` → `120c29b` (2 hours later, same day): rebase-on-retry preserved rejected commits → **NO-OP LOOP** where the new implementer saw stale work as "already done" → replaced with unconditional hard-reset.

The surviving v4 shape encodes every lesson. v5's worktree module ports that shape, amends it for one new capability (crash recovery via daemon-startup reconciliation, explicitly absent in v3/v4 per vision.md line 316), and simplifies the retry model by making every attempt a fresh worktree rather than reusing a single branch across attempts.

### Problem

v5 Stage 7 needs a worktree crate that:

1. Provisions an isolated git worktree per Work attempt, including repeat attempts when a Bundle is rejected.
2. Survives daemon crashes without leaking worktrees or orphan branches.
3. Makes cleanup-timing policy user-tunable, for debugging the class of "implementer keeps producing wrong output" failures.
4. Stays infrastructure-only (no `store`, no `domain` reach-into): reconcile orchestration lives in the binary crate.
5. Does not collide with the target's own `.gitignore` semantics.
6. Does not require append-locking, JSONL corruption recovery, or tie-breaker rules across redundant state stores.

### Goals

- `Worktree::create(repo_path, worktree_root, work_id, base_sha) -> Result<Worktree>` provisions a worktree at `<worktree_root>/<work-id>-<seq>/` on branch `loopr/wk-<work-id>-<seq>`. **Seq is allocated internally** by looping seq from 1, attempting `git worktree add <path> -b <branch> <base_sha>`, and retrying with seq+1 on the "branch already exists" or "path already exists" error. Caller (`loopr` coordinator) computes `worktree_root = target.join(".loopr/worktrees")` (flat; no run-id) and resolves `base_ref` to a SHA in the repo context before calling. This keeps `worktree` ignorant of the `target` naming convention — it only sees the assembled root path — and the coordinator ignorant of seq allocation mechanics. `Worktree::create` calls `create_dir_all(worktree_root)` at entry if the directory doesn't exist.
- `Worktree` handle: owns `path`, `branch`, `work_id`, `seq` (the allocated value), `repo_path`, and a `consumed: bool` flag. `Drop` is a best-effort synchronous cleanup (safety net for handles that were never explicitly consumed).
- `Worktree::cleanup(self)` — explicit cleanup: runs `git worktree remove --force <path>`, **keeps the branch**, marks handle consumed.
- `worktree::delete_branch(repo_path, branch) -> Result<()>` — free function called by the integrator after a Tick publishes. `git branch -D <branch>`, idempotent on missing. Takes the full branch string (from a `Worktree.branch()` or `WorktreeInfo.branch`) rather than reconstructing from work_id + seq.
- `worktree::list(repo_path, worktree_root) -> Result<Vec<WorktreeInfo>>` — parses `git worktree list --porcelain`, filtered to entries whose path is under `worktree_root`. Reused by reconcile.
- `worktree::ensure_loopr_excludes(repo_path) -> Result<()>` — idempotent `.git/info/exclude` injection with `# loopr-managed` marker + v5 patterns.
- `WorktreeConfig { cleanup_policy: AttemptCleanupPolicy }` composed into top-level `Config` by `loopr`.
- `AttemptCleanupPolicy` enum exposed via config + ENV + CLI with precedence; default `OnWorkTerminal`.
- No dependency on `store`, `domain-beyond-ids`, or `serde_json` for a registry file.

### Non-Goals

- **Crash-recovery reconciliation lives in `loopr`, not here.** Reconcile needs TaskStore lookups and Work FSM mutations; both are forbidden in this crate. The `loopr` binary's `daemon::startup::reconcile` module calls `worktree::list` + TaskStore `get_work` + FSM-mutate.
- **Parallel worktrees** (multiple Works running simultaneously). Stage 7 is serial per vision.md line 598. The crate will handle parallel correctly (atomic seq allocation), but no parallel consumer exists yet.
- **Worktree-registry JSONL.** Dropped per D5.
- **Sibling-path layout.** Dropped per D3.
- **`refresh` (rebase existing worktree onto new Tick).** Dropped per D12.
- **Push/pull operations.** Loopr's push policy is "never" (vision.md "Git Posture"). The integrator merges locally; human pushes.
- **LLM-facing tool wrapping.** `worktree` is called directly by daemon/coordinator orchestration code (`std::process::Command`). It is NOT routed through the `tools` crate. This is explicit per `crates/worktree/CLAUDE.md` line 11.
- **Plan integration branches (`loopr/plan-<plan-id>`).** Owned by `integrator`, not `worktree`.
- **`git worktree lock` / `unlock`.** v4 didn't use them; v5 single-daemon constraint means no concurrency race to lock against.

## Proposed Solution

### Overview

`worktree::Worktree` is an RAII handle: construction provisions the worktree, `Drop` cleans up (safety net), and explicit `cleanup()` / `delete_branch()` calls give the coordinator fine-grained control. The coordinator (in `loopr` crate) decides when to drop the handle, based on `AttemptCleanupPolicy`:

- `Immediate`: coordinator drops the `Worktree` handle on Bundle rejection → `Drop` cleans synchronously.
- `OnWorkTerminal`: coordinator retains the handle until the Work reaches `Done` or `Abandoned` → drops all retained handles for that Work at that moment.
- `OnRunEnd`: coordinator retains handles in an in-memory per-run collection until the run completes, then drops all of them. (This is purely a Rust `Vec<Worktree>` in memory; no filesystem registry.)
- `Never`: coordinator moves the handle into a long-lived collection that outlives normal scope; worktrees persist until the daemon restarts and `reconcile` sweeps them.

Seq allocation is atomic via `git worktree add`'s own "branch already exists" / "path already exists" errors as the EEXIST-equivalent. `Worktree::create` loops seq from 1 upward; each iteration attempts a full `git worktree add <path> -b <branch> <base_sha>`; on the "already exists" class of git errors it retries with seq+1 up to 1000 attempts. This eliminates the need for a separate `next_seq` claim step (which introduced a TOCTOU window between claim and git-add). Git's own refcount on branch creation is the serialization primitive; two concurrent coordinators picking the same seq both run `git worktree add`, one succeeds, the other gets a clean rejection and retries.

Reconcile (in `loopr` crate) works off three sources:
1. `git worktree list --porcelain` → existing worktrees on disk, with branch names.
2. Branch name parse (`loopr/wk-<work-id>-<seq>`) → identify which Work and attempt.
3. TaskStore `get_work(work_id)` → Work's current status.

For each managed worktree:
- Work is terminal (`Done` / `Abandoned`): clean worktree (`worktree::cleanup` primitive, ignoring handle lifecycle), delete branch if Work is `Done` and no live integrator needs it, otherwise leave branch for integrator.
- Work is non-terminal AND no live coordinator session is handling it: mark Work as `FailureReason::CrashInterrupted`, leave worktree+branch for next attempt's disposal logic.
- Work is non-terminal AND a live coordinator session exists: noop, the session is handling it.

### Architecture

```
┌───────────────────────────────────────────────────────────────────┐
│ loopr (binary crate)                                              │
│                                                                   │
│  Coordinator (per Work attempt):                                  │
│    1. base_sha = git rev-parse (integration tip) in repo context  │
│    2. let wt = Worktree::create(repo_path, worktree_root,         │
│                                 &work_id, &base_sha)?             │
│       (seq allocated internally; wt.seq() exposes the value)      │
│    3. hand wt.path to the Implementer ralph loop                  │
│    4. on Bundle accepted/rejected/abandoned:                      │
│         match config.worktree.cleanup_policy {                    │
│           Immediate       => drop(wt),                            │
│           OnWorkTerminal  => retain until Work terminal, drop,    │
│           OnRunEnd        => move into in-memory Vec, drop@end,   │
│           Never           => leak (reconcile cleans on restart),  │
│         }                                                         │
│                                                                   │
│  daemon::startup::reconcile(target, store, live):                 │
│    for info in worktree::list(&target)?:                          │
│        parse branch → (work_id, seq)                              │
│        join TaskStore: is Work terminal? is session live?         │
│        act: cleanup / mark CrashInterrupted / noop                │
│                                                                   │
└─────────────────┬─────────────────────────────────────────────────┘
                  │
                  ▼
┌───────────────────────────────────────────────────────────────────┐
│ worktree crate (infrastructure)                                   │
│                                                                   │
│  Public API:                                                      │
│    Worktree::create / cleanup / Drop (RAII handle; seq internal)  │
│    worktree::delete_branch (free fn, integrator-callable)         │
│    worktree::list          (parse git worktree list --porcelain)  │
│    worktree::cleanup_at    (free fn, reconcile-callable)          │
│    worktree::parse_branch                                         │
│    worktree::ensure_loopr_excludes                                │
│    WorktreeConfig { cleanup_policy }                              │
│    AttemptCleanupPolicy enum                                      │
│                                                                   │
│  Internal:                                                        │
│    ops::try_create_at_seq(repo, root, work_id, seq, branch, sha)  │
│    ops::remove_worktree(repo, path)                               │
│    ops::delete_branch(repo, branch)                               │
│    ops::prune(repo)                                               │
│    parse::porcelain(output) -> Vec<WorktreeInfo>                  │
│                                                                   │
│  Shells out to git via std::process::Command (NOT through tools)  │
└───────────────────────────────────────────────────────────────────┘
```

### Data Model

#### `Worktree` handle

```rust
pub struct Worktree {
    path: PathBuf,
    branch: String,
    work_id: WorkId,
    seq: u32,
    repo_path: PathBuf,
    consumed: bool,  // true after explicit cleanup() call; Drop skips
}

impl Worktree {
    pub fn path(&self) -> &Path { &self.path }
    pub fn branch(&self) -> &str { &self.branch }
    pub fn work_id(&self) -> &WorkId { &self.work_id }
    pub fn seq(&self) -> u32 { self.seq }

    /// Explicit cleanup. Calls git worktree remove --force; keeps the branch.
    /// After this returns, Drop is a no-op.
    pub fn cleanup(mut self) -> Result<(), WorktreeError> {
        ops::remove_worktree(&self.repo_path, &self.path)?;
        self.consumed = true;
        Ok(())
    }
}

impl Drop for Worktree {
    fn drop(&mut self) {
        if self.consumed { return; }
        // Best-effort synchronous cleanup. Logs on failure; does not panic.
        if let Err(e) = ops::remove_worktree(&self.repo_path, &self.path) {
            tracing::warn!(
                path = %self.path.display(),
                error = %e,
                "Worktree Drop cleanup failed (non-fatal; reconcile will sweep on next startup)"
            );
        }
    }
}
```

`Drop` as a **crash safety net**, not the routine cleanup mechanism. Normal flow: the coordinator calls explicit `.cleanup()` from inside `tokio::task::spawn_blocking` (see Phase 4) so the synchronous `git worktree remove` doesn't starve the tokio runtime. The `consumed` flag prevents double-cleanup when `.cleanup()` is then followed by Drop at scope exit.

**Concurrency note:** `Worktree` is `Send` (all fields are `Send`); `!Sync` is deliberate — the handle represents ownership of the worktree, not a shared resource.

#### `WorktreeInfo` (from `git worktree list --porcelain`)

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WorktreeInfo {
    pub path: PathBuf,
    pub branch: String,  // without refs/heads/ prefix
    pub head: String,    // 40-char SHA
}
```

Unchanged from v4. The parser `parse::porcelain` matches v4's (manager.rs lines 289-330).

#### `AttemptCleanupPolicy`

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize, clap::ValueEnum)]
#[serde(rename_all = "kebab-case")]
#[clap(rename_all = "kebab-case")]
pub enum AttemptCleanupPolicy {
    /// Clean worktree + branch immediately when a Bundle is rejected.
    /// Minimum disk usage; no forensic artifact for failed attempts.
    Immediate,
    /// Keep rejected-attempt worktrees until the Work reaches Done/Abandoned.
    /// Then sweep all prior attempts. DEFAULT.
    OnWorkTerminal,
    /// Keep all attempts (including successful) until the run completes,
    /// then sweep. Most disk, best forensics.
    OnRunEnd,
    /// Never clean automatically. Strict debug-only.
    ///
    /// The coordinator parks handles in a long-lived `Vec` to keep them alive,
    /// which leaks memory and file descriptors over multi-week daemon uptime.
    /// Changing the config mid-flight from `Never` to any other variant does
    /// NOT retroactively clean Works that already terminated under `Never` —
    /// cleanup fires only on the edge transition to terminal. Daemon restart
    /// is required to trigger `reconcile` and clear accumulated state.
    Never,
}

impl Default for AttemptCleanupPolicy {
    fn default() -> Self { Self::OnWorkTerminal }
}
```

Consumer of the policy is the **coordinator in the `loopr` crate**. The `worktree` crate defines the enum (because it conceptually belongs to worktree lifecycle) but does not apply it. The coordinator reads `config.worktree.cleanup_policy` and decides when to drop `Worktree` handles accordingly. `Drop` itself always cleans when it fires — the policy only controls *when* the coordinator drops.

#### `WorktreeConfig`

```rust
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct WorktreeConfig {
    #[serde(default)]
    pub cleanup_policy: AttemptCleanupPolicy,
}
```

Composed into `loopr::Config::worktree: WorktreeConfig`. CLI flag `--worktree-cleanup` overrides `LOOPR_WORKTREE_CLEANUP_POLICY` overrides `worktree.cleanup-policy` overrides `Default`.

#### `WorktreeError`

```rust
#[derive(Debug, thiserror::Error)]
pub enum WorktreeError {
    #[error("git command failed: {0}")]
    GitCommand(String),

    #[error("worktree not found at {0}")]
    NotFound(PathBuf),

    #[error("failed to allocate seq after {attempts} attempts under {dir}")]
    SeqAllocExhausted { attempts: u32, dir: PathBuf },

    #[error("invalid branch name {0:?}: not a loopr-managed branch")]
    InvalidBranchName(String),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}
```

Closed enum. No `eyre::Report` escape; no `Other(String)`. Matches the tool-registry doc's error-hygiene pattern.

### API Design

```rust
// lib.rs re-exports
pub use config::{AttemptCleanupPolicy, WorktreeConfig};
pub use error::WorktreeError;
pub use handle::Worktree;
pub use info::WorktreeInfo;

// Free functions
pub fn list(
    repo_path: &Path,
    worktree_root: &Path,
) -> Result<Vec<WorktreeInfo>, WorktreeError>;

pub fn delete_branch(
    repo_path: &Path,
    branch: &str,
) -> Result<(), WorktreeError>;

pub fn cleanup_at(
    repo_path: &Path,
    worktree_path: &Path,
) -> Result<(), WorktreeError>;  // primitive reconcile uses when it has no handle

pub fn parse_branch(branch: &str) -> Option<(WorkId, u32)>;

pub fn ensure_loopr_excludes(repo_path: &Path) -> Result<(), WorktreeError>;

// Worktree associated fn — seq allocated internally via git-error EEXIST-retry
impl Worktree {
    pub fn create(
        repo_path: &Path,
        worktree_root: &Path,
        work_id: WorkId,
        base_sha: &str,
    ) -> Result<Self, WorktreeError>;
}
```

`Worktree::create` takes the caller-computed `seq` (from `next_seq`) rather than computing it internally, so the coordinator controls the two-step (allocate-then-create) and can log both moments independently.

### File Layout

```
crates/worktree/src/
├── lib.rs               # pub re-exports + free-fn entry points
├── handle.rs            # Worktree struct + Drop impl
├── handle/
│   └── tests.rs         # 2018+ submodule pattern
├── config.rs            # WorktreeConfig, AttemptCleanupPolicy
├── config/
│   └── tests.rs
├── error.rs             # WorktreeError
├── info.rs              # WorktreeInfo
├── ops.rs               # git command wrappers (create_branch, remove, delete_branch, prune)
├── ops/
│   └── tests.rs
├── parse.rs             # porcelain + branch-name parsers
├── parse/
│   └── tests.rs
└── excludes.rs          # ensure_loopr_excludes
```

One single-word file per module (per rules/rust.md). Tests in sibling `tests.rs` per 2018+ submodule pattern (per feedback memory).

### Implementation Plan

#### Phase 1: Scaffold types (error, config, info, handle skeleton)
**Model:** sonnet

- `error.rs`: `WorktreeError` closed enum with `From<std::io::Error>`.
- `config.rs`: `WorktreeConfig`, `AttemptCleanupPolicy` enum with `Default`, `clap::ValueEnum`, `serde` kebab-case.
- `info.rs`: `WorktreeInfo` struct (Serialize/Deserialize) matching v4 shape.
- `handle.rs`: `Worktree` struct with `path`/`branch`/`work_id`/`seq`/`repo_path`/`consumed` fields, getters, `cleanup()` method (placeholder delegating to `ops::remove_worktree`), `Drop` impl.
- `lib.rs`: public re-exports.
- `Cargo.toml`: add `serde`, `thiserror`, `clap` (derive + value-enum features). Domain + telemetry already present.
- Unit tests: enum default, config serde round-trip, error display, handle field accessors, Drop-with-`consumed=true` is a no-op.

#### Phase 2: Git operations + porcelain/branch parsers
**Model:** sonnet

- `ops.rs`: **Every `std::process::Command` for git is constructed via an `fn git_cmd(repo_path: &Path) -> Command` helper that sets `.current_dir(repo_path)` and `.env("LC_ALL", "C")`.** The `LC_ALL=C` is mandatory (R2 hardening): without it, `LANG=fr_FR.UTF-8` users get stderr phrases like "existe déjà" and our SeqTaken classifier silently misses the retry signal, bubbling out as a fatal `GitCommand` error.
  - `try_create_at_seq(repo_path, worktree_root, work_id, seq, base_sha) -> Result<CreateOutcome, WorktreeError>`: builds `path = worktree_root/<work_id>-<seq>/`, `branch = loopr/wk-<work_id>-<seq>`, runs `git worktree add <path> -b <branch> <base_sha>` via `git_cmd`. Returns `CreateOutcome::Created { path, branch }` on success, `CreateOutcome::SeqTaken` on git's "already exists" errors matched against English stderr fragments: `"already exists"`, `"already checked out"`, `"is not an empty directory"`. Propagates any other failure as `WorktreeError::GitCommand`. **Does NOT use exit code 128 as a retry signal** — it's git's generic `fatal:` exit code (disk-full, permission-denied, OOM), and retrying on it would spin-loop.
  - `remove_worktree(repo_path, path)`: runs `git worktree remove --force <path>` via `git_cmd`.
  - `delete_branch(repo_path, branch)`: runs `git branch -D <branch>` via `git_cmd`; "not found" → Ok, any other error → WorktreeError.
  - `prune(repo_path)`: runs `git worktree prune` via `git_cmd`; non-fatal on failure (logs warn!, returns Ok).
  - `resolve_sha(repo_path, base_ref)`: runs `git rev-parse <base_ref>` via `git_cmd`; returns the SHA string (D10 gotcha).
- `parse.rs`:
  - `parse::porcelain(output, worktree_root_prefix) -> Vec<WorktreeInfo>`: v4-verbatim parser (manager.rs 289-330), filtered to paths under `worktree_root_prefix`.
  - `parse::branch("loopr/wk-<work-id>-<seq>") -> Option<(WorkId, u32)>`: strict parser; rejects anything not matching the pattern (missing prefix, missing seq, non-numeric seq).
- `info.rs`: wire the parser.
- Unit tests: `try_create_at_seq` happy path + SeqTaken classification (inject scripted git-output fixtures); `parse::porcelain` fixture → expected output; `parse::branch` happy + negative cases.

#### Phase 3: Worktree::create (with internal seq allocation) + primitives
**Model:** sonnet

- `handle.rs::Worktree::create(repo_path, worktree_root, work_id, base_sha)`:
  1. `ops::prune(repo_path)?` — clear crashed-session registrations (non-fatal; one call per `create`, not per seq iteration).
  2. Loop `seq` from 1 to `MAX_SEQ = 1000`:
     - `match ops::try_create_at_seq(repo_path, worktree_root, &work_id, seq, base_sha)?`:
       - `CreateOutcome::Created { path, branch }` → proceed to verify step.
       - `CreateOutcome::SeqTaken` → continue to seq+1.
  3. If loop exhausted: return `Err(WorktreeError::SeqAllocExhausted { attempts: MAX_SEQ, dir: worktree_root.into() })`.
  4. Post-create verify: `git branch --show-current` inside path matches `branch` (v4 ships this defensive check at manager.rs lines 122-132).
  5. Return `Ok(Worktree { path, branch, work_id, seq, repo_path: repo_path.to_path_buf(), consumed: false })`.
- `delete_branch` (pub free fn): `ops::delete_branch(repo_path, branch)` — takes the full branch string from a `Worktree.branch()` or `WorktreeInfo.branch`.
- `cleanup_at` (pub free fn): `ops::remove_worktree(repo_path, path)`. For reconcile when no handle exists.
- `list(repo_path, worktree_root)`: `git worktree list --porcelain` → `parse::porcelain(output, worktree_root)`.
- `ensure_loopr_excludes(repo_path)` → `excludes.rs` module with v5 pattern list and `# loopr-managed` marker.
- Integration tests using `tempfile::TempDir` + real `git init` fixtures:
  - Fresh create → verify path exists, branch matches, HEAD matches `base_sha`.
  - Two sequential `Worktree::create` calls for the same work_id → seq=1, then seq=2; both coexist.
  - Drop cleans: `create` → `drop(wt)` → verify path gone, branch alive.
  - Explicit cleanup keeps branch: `create` → `wt.cleanup()?` → path gone, branch alive.
  - `delete_branch` on missing → Ok; on existing → branch gone.
  - `ensure_loopr_excludes` idempotent + correct pattern list.

#### Phase 4: Reconcile wiring + coordinator cleanup pattern in `loopr` binary
**Model:** sonnet

**R2 cleanup pattern.** The coordinator's routine worktree cleanup (policy-driven sweeps) must NOT rely on `Worktree::Drop` firing synchronously on the tokio worker. Under `OnRunEnd` with 20 retained handles, dropping the `Vec<Worktree>` would block one tokio worker for ~20 × 30ms = ~600ms of sequential `git worktree remove --force` calls, starving IPC heartbeats and other async tasks. Instead, the coordinator calls explicit `.cleanup()` from inside `tokio::task::spawn_blocking`:

```rust
// When policy says "clean these N handles now":
tokio::task::spawn_blocking(move || {
    for wt in handles {
        if let Err(e) = wt.cleanup() {
            tracing::warn!(work_id = %wt_work_id, error = %e, "routine cleanup failed");
        }
    }
}).await.ok();  // best-effort; reconcile catches leftovers
```

Synchronous `Worktree::Drop` remains in place as a **crash safety net** — it fires during panic unwinding or when a handle is accidentally dropped outside an async context, and blocking the current thread for ~30ms in those cases is the lesser evil compared to leaking the worktree entirely.

- `crates/loopr/src/daemon/startup/reconcile.rs` (new file):
  - `pub fn reconcile(target: &Path, store: &Store, live: &LiveSessions) -> Result<ReconcileReport>`:
    1. Compute `worktree_root = target.join(".loopr/worktrees")`.
    2. `worktree::list(target, &worktree_root)` → `Vec<WorktreeInfo>`.
    3. For each `WorktreeInfo`:
       - `parse::branch(&info.branch)` → `Some((work_id, seq))` or skip (not ours).
       - `store.get_work(&work_id)?` → `Some(work)` or log "orphan worktree with no TaskStore record", skip.
       - Match `(work.status.is_terminal(), live.has_session(&work_id))`:
         - `(true, _)` → `worktree::cleanup_at(target, &info.path)?`; if status is `Done`, also `worktree::delete_branch(target, &info.branch)?` — integrator should have done this but belt-and-suspenders.
         - `(false, false)` → `store.mark_crash_interrupted(&work_id)?`; leave worktree + branch for next attempt's disposal.
         - `(false, true)` → noop, the live session handles it.
    4. Return `ReconcileReport { cleaned: usize, marked_crashed: usize, orphans_logged: usize }`.
- `crates/loopr/src/config.rs`: extend top-level `Config` with `worktree: WorktreeConfig`.
- `crates/loopr/src/cli.rs`: add `--worktree-cleanup <policy>` (via `clap::ValueEnum` on `AttemptCleanupPolicy`).
- `crates/loopr/src/daemon.rs::DaemonContext::new`: call `reconcile(target, &store, &live)?` before accepting IPC connections. Also call `worktree::ensure_loopr_excludes(target)?`.
- Unit tests: reconcile with mock store + fake worktree list → correct actions; `DaemonContext` test path that reconcile does not crash on an empty `.loopr/runs/`.

#### Phase 5: Seam tests + integration + architect audit
**Model:** opus

- Seam test: real git repo fixture → `Worktree::create` (returns seq=1), drop (Drop should clean), verify path gone and branch alive. Re-run `create` for same work_id → seq=1 again (path & branch from prior attempt were both cleaned: path by Drop, branch implicitly by `cleanup()` NOT having run — actually branch was NOT deleted, so second `create` would hit SeqTaken on seq=1 and allocate seq=2; assert this).
- Concurrent allocation test: spawn 10 threads calling `Worktree::create` for the same work_id in parallel → all 10 succeed with distinct seq values 1..=10 (proves git's internal serialization of `worktree add` + our SeqTaken retry loop combine correctly).
- `AttemptCleanupPolicy` integration: config serde with each variant; CLI parse of `--worktree-cleanup never`; ENV precedence over config; CLI precedence over ENV.
- Reconcile integration: scripted scenario with stubbed store → inject stale `loopr/wk-*` worktrees → run `reconcile` → assert correct dispositions per Work status.
- Architect round: review vision-doc amendments (D3, D5, D6 drop or relocate vision-prescribed features), the `Drop`-as-safety-net contract, and the seq allocation atomicity story.

## Alternatives Considered

### Alternative 1: Keep `.loopr/worktree-registry.jsonl` (vision-literal)

- **Description:** Append a JSONL row on `create`, append a terminal-marker row on cleanup; reconcile reads and folds the log.
- **Pros:** Vision-compliant; explicit source of truth for "which worktrees belonged to loopr"; timestamps and metadata beyond what git carries.
- **Cons:** Redundant with `git worktree list --porcelain` + `loopr/wk-*` branch prefix. Adds three-source-of-truth split brain (git + JSONL + TaskStore). Needs `fcntl` or `fs2` append-locking to survive concurrent daemon edge cases. Partial-write corruption on SIGKILL; reconcile must parse-tolerantly skip bad lines. Terminal-marking requires fold-forward semantics over an append-only log. None of the complexity yields anything the branch-prefix join doesn't.
- **Why not chosen:** Architect R1 Finding #1. The JSONL is over-specification in the vision; git + branch-name-parse is a complete reconcile path. Explicit vision amendment (D5).

### Alternative 2: Sibling-path layout (vision-literal)

- **Description:** `<target-parent>/<target-name>-work-<work-id>/` — outside the target, in the parent directory.
- **Pros:** Vision-compliant; worktree files are entirely outside the target's `.gitignore` and exclude-file influence; cleanup trivially cannot orphan a path inside the target.
- **Cons:** Requires write permission on `<target-parent>`, which fails in CI environments (GitHub Actions `/home/runner/work/repo/repo` has restricted parent semantics), containers with read-only parent mounts, and any target checked out to a read-only root-owned directory. Pollutes the workspace namespace. Breaks atomic cleanup: deleting the target repo leaves sibling worktrees orphaned. The vision's justification ("Git's ignore rules inside the worktree don't accidentally exclude files the agent writes") is solvable via `.git/info/exclude` (which `ensure_loopr_excludes` already manages).
- **Why not chosen:** Architect R1 Finding #2. Introduces deployment regressions. `.loopr/worktrees/` is inside the target, inherits permissions, is covered by `ensure_loopr_excludes`, and is atomically cleaned when the target is deleted. Explicit vision amendment (D3). Layout is flat (not `.loopr/runs/<run-id>/worktrees/`) because `git worktree add` registers by path basename — Pass 4 edge-case find.

### Alternative 3: Reuse a single `loopr/wk-<work-id>` branch across attempts (v4 model)

- **Description:** Every attempt for work X uses the same branch and worktree; retry hard-resets to integration tip. One worktree dir per work_id, reused.
- **Pros:** v4-proven. Fewer directories on disk during a long retry chain. Branch lineage is preserved for Blame archaeology.
- **Cons:** The retry vs. reuse distinction is the single biggest source of v4 worktree bugs (five distinct failures between Feb 28 and Apr 10, 2026 per pre-design brief). The NO-OP LOOP class (rejected commits surviving retries → new implementer sees stale work as "already done") required a mid-refactor inversion (`0ce1226` → `120c29b` in 2 hours). Hard-reset fixes the loop but couples every retry to a "wipe rejected commits" step that is easy to forget. The mental model is "branch is mutable state; retry mutates it."
- **Why not chosen:** Structurally eliminating the class of bugs is worth the extra disk. Option 1 (fresh worktree per attempt) makes "commits from a rejected attempt" syntactically inaccessible — there is no shared branch to carry them. Debugging story also improves: each attempt's worktree is a preserved artifact (under `OnWorkTerminal` policy), not a mutating ghost.

### Alternative 4: `reconcile` lives inside `worktree` crate (vision-literal)

- **Description:** `worktree::reconcile(target)` takes a store handle or store-like trait and does the full reconcile including `FailureReason::CrashInterrupted` mutations.
- **Pros:** Vision-compliant; single-call API for the daemon.
- **Cons:** Forces `worktree` to depend on `store` or a store-shaped trait; `worktree` is documented as infrastructure-only in `crates/worktree/CLAUDE.md`. Dragging in TaskStore awareness makes `worktree` know about `Work` records, FSM transitions, and `FailureReason` variants — direct violation of blast-radius discipline. The trait-abstraction cost (define a reconcile-store-trait here, implement it in `store`) is pure bureaucracy to hide a dependency that shouldn't exist.
- **Why not chosen:** Architect R1 Finding #5. Reconcile is an orchestration concern: joining git state + TaskStore + live-session map. That's `loopr` binary's job. `worktree` exposes primitives (`list`, `cleanup_at`, `delete_branch`, `parse_branch`); `loopr::daemon::startup::reconcile` uses them. Explicit vision amendment (D6).

### Alternative 5: `AttemptId = Uuid` (v7) instead of monotonic `-<seq>`

- **Description:** Every attempt gets a UUIDv7 ID; path `.loopr/worktrees/<work-id>-<uuid-short>/`, branch `loopr/wk-<work-id>-<uuid-short>`.
- **Pros:** Globally unique regardless of sequencing; no race in allocation; no EEXIST dance.
- **Cons:** Opaque to human ops. `ls .loopr/runs/.../worktrees/wi-042/` shows `019283ab-fa21-7...` not "attempt 3". UUIDv7's timestamp prefix partially mitigates but doesn't match the clarity of a small integer. UUID-in-filenames is also slightly hostile to tab-completion.
- **Why not chosen:** User preference (Option 1 locked in 2026-04-21): `-1`, `-2`, `-3` beats UUIDs for debuggability. Atomic `create_dir` EEXIST-retry gives the same race-freeness without sacrificing readability.

## Technical Considerations

### Dependencies

Internal: `domain` (for `WorkId`), `telemetry` (for span emission). **Explicitly not** `store`, `serde_json`-as-registry-format, `fs2` / `fcntl` locking. No `tokio` — synchronous `std::process::Command` is appropriate (git commands are short; worktree setup is rare; async adds no benefit).

External (added via `cargo add`):
- `serde`, `serde_yaml` (workspace) — config deserialization
- `thiserror` (workspace) — `WorktreeError`
- `clap` (with `derive` + `env` features) — `ValueEnum` for `AttemptCleanupPolicy`
- `tracing` (workspace) — span emission per v5 tracing convention
- `tempfile` (dev-dep only) — test fixtures

### Performance

- `next_seq` is O(k) where k = actual seq in use (typically ≤ 5); EEXIST probe is cheap.
- `git worktree add` is the dominant cost — tens of ms on SSD. Async gives no benefit.
- `git worktree list --porcelain` scales with total worktree count under `<repo>/.git/worktrees/`; v5 first gate has 1-2 active worktrees per run, so sub-millisecond.
- `Drop`-path cleanup is synchronous (blocks current thread during `git worktree remove`); acceptable because the handle is always held by orchestration code, not the tokio event loop directly.

### Security

- **Branch-prefix provenance.** The `loopr/wk-` prefix is how we distinguish our branches from user/human-created branches. `parse::branch` returns `None` for non-matching input; `delete_branch` only deletes branches we created.
- **Path escape.** `Worktree::create` computes `worktree_root.join(format!("{}-{}", work_id, seq))`. `WorkId`'s Display impl must reject path-escape sequences (`..`, `/`, etc.); checked in `domain` crate. `parse::branch` rejects any branch whose suffix isn't `[0-9]+`. No user input flows into the shell commands directly — every git invocation uses `Command::arg` (not `sh -c`).
- **`git worktree remove --force`** handles dirty worktrees. Deliberately forceful — the worktree's contents are agent-produced output, not user data.
- **No sandbox** on git subprocess calls. `worktree` is infrastructure, not an LLM-facing tool. No LLM prompt flows into its arguments.
- **Reconcile race** with a live coordinator. If reconcile runs concurrently with a coordinator spawning a new attempt for the same work_id, both could contend. Mitigation: reconcile runs **once** at daemon startup before the IPC listener accepts; coordinators cannot spawn until reconcile returns. Single-threaded boot sequence; no TOCTOU.

### Testing Strategy

**Unit (per module):**
- `error.rs`: Display output for each variant.
- `config.rs`: serde round-trip each `AttemptCleanupPolicy`; default is `OnWorkTerminal`; `deny_unknown_fields` rejects typos.
- `info.rs`: `WorktreeInfo` serde round-trip.
- `parse.rs`:
  - `porcelain`: v4 fixture → expected `Vec<WorktreeInfo>`; filter by `worktree_root` drops out-of-scope entries.
  - `branch`: `loopr/wk-wi-042-3` → `Some((WorkId("wi-042"), 3))`; `main` → `None`; `loopr/wk-wi-042` (no seq) → `None`; `loopr/wk-wi-042-abc` → `None`.
- `seq.rs`: allocator returns 1 on fresh, 2 on repeat (dir already exists), exhausts after 1000 collisions.
- `handle.rs::Drop`: Drop with `consumed=true` calls nothing; Drop with `consumed=false` on a missing path logs warn but doesn't panic.

**Integration (crate-local, uses real git):**
- Fresh repo → `Worktree::create` → verify path exists, branch matches, HEAD is base_ref's SHA.
- Fresh repo → `Worktree::create` → explicit `cleanup()` → verify path gone, branch alive.
- Fresh repo → `Worktree::create` → `drop(wt)` → verify path gone, branch alive.
- Fresh repo → two sequential creates for same `work_id` → seq=1, then seq=2; both exist simultaneously.
- Fresh repo → create, commit, cleanup, create again → new attempt starts from base_ref, NOT from prior attempt's commit.
- Concurrent `next_seq` (10 threads, same work_id) → 10 distinct seq values.
- `delete_branch` on missing branch → Ok; on existing branch → branch gone.
- `ensure_loopr_excludes` on fresh `.git/info/` → creates file with marker and patterns; on existing file without marker → appends; on existing file with marker → idempotent no-op.

**Seam (across worktree + loopr):**
- `loopr::daemon::startup::reconcile` with scripted `store` + inject stale `loopr/wk-*` worktrees → correct actions per Work status.
- Config precedence: CLI `--worktree-cleanup never` overrides ENV `LOOPR_WORKTREE_CLEANUP_POLICY=on-run-end` overrides `worktree.cleanup-policy: immediate` in config.

**Out of scope for Stage 7:**
- Parallel coordinator / parallel worktrees (Stage 9+).
- E2E through the full Stage 7 pipeline (implementer design doc's problem).

### Rollout Plan

Single branch (`v5`), single `bump` per v5 branch versioning override. Ship as next available patch (`v0.5.x+1`). No feature flag. No migration (v5 branch has no production state).

**Before merge:**
1. Vision amendment PR (D3, D5, D6): edit `docs/vision.md` lines 137, 291, 302-316 to reflect interior-path + no-JSONL + reconcile-in-loopr. Keep the amendment small and surgical.
2. Update `crates/worktree/CLAUDE.md` to match (remove the reconcile-in-worktree language; emphasize "primitives only; reconcile lives in loopr binary").

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| `next_seq`'s `create_dir`-then-`remove`-for-git pattern races against another coordinator | Low (daemon is single-threaded accept-loop per vision.md line 598) | Med | EEXIST retry on `git worktree add` failure catches any leaked race. |
| Drop-time cleanup blocks the tokio worker for 10-50ms per handle (×N handles under `OnRunEnd`) | Med | Med | **R2 finding.** Coordinator's routine cleanup uses explicit `.cleanup()` inside `tokio::task::spawn_blocking` (see Phase 4), never relying on Drop for routine sweeps. Drop remains only as a crash/panic safety net. Documented in `handle.rs` module doc. |
| `Never` policy leaks memory + file descriptors on long-uptime daemons | Med (only if user sets `Never`) | Med | **R2 finding.** `Never` docstring explicitly flags as strict debug-only; requires daemon restart to clear accumulated state. `--help` mentions the constraint. |
| Downstream telemetry/UI assumes seq is strictly monotonic without gaps | Low | Low | **R2 finding.** Seq monotonicity holds *absent manual user intervention* — if a user runs `git branch -D loopr/wk-<id>-1` while the daemon is running, the next attempt reclaims seq=1, breaking monotonicity. Document the invariant as "seq is unique-at-a-time, not strictly monotonic across the lifetime of a work_id." |
| Vision amendment PR delayed → design doc and vision out of sync | Low | Low | Amend vision in the same PR as Phase 1. |
| Reconcile's TaskStore join crashes on missing records | Med | Low | `store.get_work` returns `Option`; missing → log orphan, skip. |
| `git worktree add` refuses to use a pre-claimed empty directory | Low (tested behavior: git accepts empty target dirs) | Med | Phase 2 integration test confirms; fallback is remove-empty-dir-then-add with a retry loop. |
| `AttemptCleanupPolicy::Never` creates unbounded disk pressure | Low (debug-only flag) | Low | Next daemon startup's reconcile sweeps orphans from prior runs. Document the constraint in `--help`. |
| Concurrent reconcile vs. coordinator | Very low | High | Reconcile runs ONCE at daemon startup before IPC listener accepts. Single-threaded boot sequence. |
| Branch name collision with a user branch named `loopr/wk-*` | Very low | Low | Document the `loopr/wk-` prefix as reserved. `parse::branch` on parse failure → log and skip, never act destructively on unrecognized branches. |
| Target with `.git/info/exclude` pointing elsewhere (bare clone semantics) | Low | Low | `ensure_loopr_excludes` writes to `.git/info/exclude` under the detected `.git` path; test fixture covers. |

## Open Questions

- [ ] **Git "already exists" error-message fragments stability across git versions.** `ops::try_create_at_seq` classifies `CreateOutcome::SeqTaken` by matching substrings in stderr (`"already exists"`, `"already checked out"`, `"is not an empty directory"`). Git's error messages are user-facing strings that COULD change across minor versions. Phase 2 tests pin the fragments against git 2.x; revisit if a distro ships git 3.x with different wording. Fallback: add an exit-code check (`128` for most "can't create" errors) as a coarser but more stable signal.
- [ ] **Does `git worktree add` serialize concurrent invocations via internal file locks on `.git/worktrees/`?** Practical answer is "yes, via `config.lock` / index locks" but we should confirm — two coordinators concurrently entering the seq-retry loop for the same `work_id` must not corrupt git state even when both pick the same seq initially. If not serialized internally, add a crate-level `std::sync::Mutex` around `ops::try_create_at_seq`. Low practical risk in the single-coordinator-per-daemon model of Stage 7.
- [ ] **Should `Worktree::create` take `&DaemonContext` instead of individual `repo_path`/`worktree_root`?** Creates a lighter call site but couples the `worktree` crate to a `DaemonContext` type that lives in `loopr`. Current draft keeps them separate; revisit if the call site gets unwieldy.
- [ ] **`AttemptCleanupPolicy::Never` + long-running daemon: how to expose current disk usage?** Out of scope for Stage 7; consider a `loopr worktrees ls` CLI subcommand later if the footgun matters.
- [ ] **Reconcile batch-size / pagination.** Flat layout means reconcile scans one directory with all worktrees across all runs. If the directory accumulates thousands of entries (rare; reconcile+cleanup_policy should prevent it), daemon startup becomes slow. Defer; revisit if observed in practice. Mitigation if needed: add a background GC pass that sweeps worktrees older than N days at startup, non-blocking.
- [ ] **`WorkId` Display contract.** `parse::branch` and path construction depend on `WorkId`'s Display being `[a-z0-9-]+` (no `/`, no `..`, no spaces). Confirm in `domain` crate's WorkId tests; add a `parse::branch` test that asserts rejection of malformed work_id segments. The parser splits on the **last** `-` to separate work_id from seq, so work_ids containing `-` (e.g., `wi-042`) are handled correctly; the parser rejects seq values `0` or non-numeric.
- [ ] **Orphan branches from manual user intervention.** If a user manually runs `git worktree remove` on one of our paths, the branch survives. Future `Worktree::create` for that work_id hits SeqTaken on the orphan branch and allocates seq+1. Acceptable but leaves a dangling branch. Future reconcile enhancement: sweep orphan `loopr/wk-<id>-<seq>` branches when the Work is terminal and no worktree references them. Defer to a follow-up.
- [ ] **`Immediate` cleanup for crash-interrupted attempts.** Under `AttemptCleanupPolicy::Immediate`, a Bundle rejection immediately cleans that attempt's worktree. But a *crash* doesn't produce a Bundle — reconcile just marks the Work `CrashInterrupted` and leaves the worktree for the next attempt. If a user never retries, the crashed worktree sits indefinitely under `Immediate`. Decide: should reconcile itself apply the cleanup policy to orphaned-but-nonterminal worktrees? Scope creep; defer.

## References

- `docs/vision.md`:
  - Lines 44-46 — crate role row
  - Lines 129-137 — worktree crate contract
  - Lines 287-316 — Target Repo Layout + Worktree crash recovery (the parts D3/D5/D6 amend)
  - Lines 510-512 — branch naming conventions
- `crates/worktree/CLAUDE.md` — scope rules (this doc amends the reconcile-location language)
- `crates/tools/docs/design/2026-04-21-tool-registry.md` — pattern template (Decisions table, per-phase model annotation, alternatives section shape)
- `crates/worktree/docs/design/2026-04-21-pre-design-brief.md` — pre-design Architect consultation input
- `crates/telemetry/src/runid.rs` — the atomic `create_dir` EEXIST-retry pattern D2 inherits
- v4 source (port origin, read-only reference):
  - `~/repos/scottidler/loopr-v4/src/worktree/manager.rs` — 857-line WorktreeManager (byte-identical to v3)
  - Key commits: `86f3278`, `b78465b`, `67758ea`, `57ed30c`, `c4b4158`, `0ce1226`, `120c29b` (full evolution in pre-design brief)
