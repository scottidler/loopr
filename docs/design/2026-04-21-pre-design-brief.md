# Pre-Design Brief: Stage 7 Worktree Lifecycle

**Author:** Claude (with Scott)
**Date:** 2026-04-21
**Status:** Pre-design (no implementation; Architect consultation only)
**Target design doc:** `docs/design/2026-04-21-worktree-lifecycle.md` (cross-cutting: touches `worktree` + `loopr`, lives at repo root per `docs/CLAUDE.md`)

## Purpose

This is not a design doc. It is a consultation brief gathered before writing the design doc for `crates/worktree`. It captures (a) what v4's worktree module actually does and the git-history evolution that got it there, (b) the new v5-prescribed shape from `docs/vision.md`, and (c) the open design questions where I want the Architect to swing.

## Stage context

`docs/roadmap.md` Stage 7 has three design docs. The tool registry (`crates/tools/docs/design/2026-04-21-tool-registry.md`) is **Implemented**. Remaining:

1. `crates/worktree/docs/design/lifecycle.md` (this brief's target)
2. `crates/agents/docs/design/implementer.md` (consumes tools + worktree)

Stage 7 exit criterion: on a toy target, a Work produces a Bundle whose commit diff shows real file edits. Worktree blocks agents.

## v5 vision prescribes (confirmed from `docs/vision.md` + `crates/worktree/CLAUDE.md`)

- Sibling worktree path: `<target-parent>/<target-name>-work-<work-id>/` (OUTSIDE the target, not `.worktrees/` inside it)
- Branch name: `loopr/wk-<work-id>` (not v4's `agent/<work-id>`)
- Registry location: `<target>/.loopr/worktree-registry.jsonl`
- Crash recovery: `worktree::reconcile(target)` at daemon startup reads registry, reconciles against git state, cleans orphans, marks crashed Works as `FailureReason::CrashInterrupted`
- Internal git calls via `std::process::Command` directly, NOT through the `tools` crate (infrastructure, not an LLM-facing tool)
- `Drop` handles happy path; reconcile handles SIGTERM/SIGKILL/power-loss
- **Explicitly called out in vision line 316:** reconcile-at-startup is "absent in v3/v4"

## What v4 actually ships (read from `~/repos/scottidler/loopr-v4/src/worktree/manager.rs`, 857 lines, identical to v3)

### API surface
- `WorktreeManager { repo_path, worktree_dir }`
- `create_branch(work_id, base_ref) -> Result<PathBuf>`
- `get_or_create_branch(work_id, base_ref) -> Result<PathBuf>` — idempotent, TOCTOU-safe
- `refresh(work_id, new_base_ref) -> Result<()>` — `git rebase` with auto-`--abort` on conflict
- `cleanup(work_id) -> Result<()>` — `git worktree remove --force`, does NOT delete branch
- `delete_branch(work_id) -> Result<()>` — called by integrator AFTER Tick publishes
- `list() -> Vec<WorktreeInfo>` — parses `git worktree list --porcelain`, filtered by `worktree_dir` prefix
- `ensure_loopr_excludes(repo_path)` — idempotent `.git/info/exclude` injection with `# loopr-managed` marker

### Key git invocations
- Fresh: `git worktree add <path> -b <branch> <base_ref>` (run with `.current_dir(repo_path)`)
- Retry (branch exists): `git worktree add <path> <branch>` + `git -C <path> reset --hard <resolved_sha>`
  - Branch-exists probe: `git rev-parse --verify refs/heads/<branch>`
  - `base_ref -> SHA` resolved in **repo context** first (see gotcha below)
- Pre-create: `git worktree prune` (non-fatal) to clear crashed-session registrations
- Cleanup: `git worktree remove --force <path>`
- Delete branch: `git branch -D <branch>` (idempotent; "not found" suppressed)
- Refresh: `git -C <worktree> rebase <new_base>` with `rebase --abort` on failure

### HEAD-inside-worktree gotcha (lines 89-99)
`git rev-parse HEAD` run INSIDE a worktree resolves to the agent branch tip — not the intended base. `base_ref` must be resolved to a SHA in the **repo context** before being passed to the worktree's `reset --hard`. v4 ships this as a `rev-parse`-in-repo-then-pass-SHA pattern.

## Git-history evolution (v3/v4 identical; dates from Feb 28 → Apr 10, 2026)

This is the crucial part. v4's current shape is the *survivor* of five discovered failure modes:

| Commit | Date | Lesson |
|---|---|---|
| `ae6c4ef` | — | Initial `create/refresh/cleanup/list` (naïve happy-path) |
| `86f3278` | Feb 28 02:34 | `cleanup` needs `--force` (dirty worktrees); *also deleted branch after cleanup* |
| `b78465b` | **Feb 28 03:04** (30 min later) | **Undo branch-delete from `86f3278`** — integrator failed with "not something we can merge". **Cleanup and branch-delete are TWO lifecycle events:** cleanup ends implementer session, branch-delete happens AFTER Tick publishes. |
| `67758ea` | Feb 28 17:46 | TOCTOU: `get_or_create_branch`. Deterministic branch name `agent/<work_id>` — NEVER derive from `git rev-parse --abbrev-ref HEAD` (returns "main" on detached HEAD). |
| `57ed30c` | Apr 1 | Retry was deleting branch → destroying uncommitted implementer work. Fix: reuse branch on retry. |
| `c4b4158` | Apr 6 | `git worktree prune` before every create. Crashed session: git has "missing but already registered"; retry fails. |
| `0ce1226` | Apr 10 14:38 | Retry path: rebase existing agent branch onto new base_ref to catch up to integration tip. |
| `120c29b` | **Apr 10 16:49 (2 hours later)** | **Complete inversion of `0ce1226`**: rebase preserved rejected commits → **NO-OP LOOP** (new implementer saw stale work as "already done"). Replaced with unconditional `reset --hard <base_sha_resolved_in_repo_context>`. |

**What this forces into the v5 design doc:**

1. **Retry policy = unconditional hard-reset**, never rebase, never preserve. Rejected commits have zero value.
2. **Three distinct lifecycle events**: worktree-cleanup (pre-Tick, keep branch), branch-delete (post-Tick), retry (reuse branch, wipe commits).
3. **Branch naming derived from work_id, never from git state.**
4. **`git worktree prune` before every create**, non-fatal.
5. **HEAD-inside-worktree gotcha**: explicit `rev-parse base_ref` in repo context.

## v5 differences from v4

| | v4 | v5 vision |
|---|---|---|
| Worktree dir | `<target>/.worktrees/<id>` (inside) | `<target-parent>/<target-name>-work-<id>/` (sibling) |
| Branch name | `agent/<id>` | `loopr/wk-<id>` |
| Excludes | `.taskstore/`, `.worktrees/`, `loopr.yml` | `.loopr/runs/`, `socket`, `daemon.pid`, `config.yml`, `worktree-registry.jsonl` |
| Registry | **None** — git's own registry + `git worktree list --porcelain` | `.loopr/worktree-registry.jsonl` (vision-prescribed) |
| Reconcile | `git worktree prune` on each create only | daemon-startup sweep: parse JSONL → check git state → clean orphans + mark crashed Works `CrashInterrupted` |

## The big open question

**Does `.loopr/worktree-registry.jsonl` carry information that `git worktree list --porcelain` + branch-name parsing cannot produce?**

My read: barely. Given the v5 branch name `loopr/wk-<work-id>` encodes the work_id, reconcile can join:
- `git worktree list --porcelain` → (path, branch, head) tuples
- branch name `loopr/wk-<id>` → work_id
- TaskStore → Work status (terminal? live session?)

...without a JSONL. The JSONL would add `created_at`, `target_path`, and a last-known-status hint — none load-bearing.

Two options on the table:

**Option A (vision-literal):** JSONL is source of truth. Needs append-locking (`fcntl` or `fs2`), row-versioning, terminal-marking on cleanup. More complexity; more failure surface (corrupt JSONL → reconcile crashes).

**Option B (git-native):** JSONL is telemetry-only or dropped entirely. Reconcile works off `git worktree list --porcelain` + branch-name parse + TaskStore join. Closer to v4's proven shape. Drops a JSONL that might never be read.

My instinct is Option B but vision.md is literal about the JSONL. The Architect's opinion matters here.

## Other open questions

1. **Sibling-path directory conflict.** v5's sibling path `<target-parent>/<target-name>-work-<id>/` collides with any existing sibling named identically. v4 used an interior `.worktrees/<id>` which is namespaced. Does v5 need conflict detection or an override mechanism (e.g., configurable `worktree-base-dir`)?
2. **Target-parent write permission.** v4's interior path inherits target permissions. Sibling paths require writing in the parent directory — might not be writable in some deployments (e.g., read-only mounts, certain CI checkouts). Fall back to interior path?
3. **Concurrent-daemon safety.** v4's `get_or_create_branch` handles TOCTOU within a single daemon process. v5 vision says first-gate is serial (one Work at a time). But the daemon itself — if two daemons get started against the same target accidentally — the registry JSONL (if we keep it) needs append-ordered writes.
4. **`ensure_loopr_excludes` patterns.** v4 writes three patterns. v5 has more transient siblings (`runs/`, `socket`, `daemon.pid`, `config.yml`, `worktree-registry.jsonl`). Straight port of the idempotent injection pattern with a new pattern list.
5. **Reconcile's interaction with TaskStore.** v4 never did this. v5's reconcile has to read Work records (which crate owns that lookup? `store`? `domain`?) to decide "terminal → clean worktree" vs "non-terminal + no live session → mark CrashInterrupted". Creates a dependency from `worktree` onto `store`/`domain` that v4's equivalent didn't have.
6. **`Drop` and async runtimes.** `Drop` impls running `std::process::Command::output()` block the current thread. In a tokio runtime, that's fine for short cleanup calls but surprises some runtime configs. Alternative: spawn cleanup into a detached task. The trade-off is: detached task can be killed on process exit before it completes (defeating the purpose); synchronous Drop blocks briefly but finishes. v4 is synchronous-Drop; v5 probably same.

## Architect: questions I most want answered

1. Is **Option B (no JSONL)** defensible against the vision? Or is there a reconcile scenario I'm missing where the JSONL is load-bearing?
2. Does the **sibling-path** vs v4's **interior-path** swap introduce problems I haven't anticipated?
3. Is there a lesson from the v4 git-history evolution I've summarized incorrectly, or a lesson I've missed entirely?
4. The "three lifecycle events" framing (cleanup / branch-delete / retry) — does this belong as an explicit section in the design doc's Data Model, or as a Decisions-table row, or both?
5. Anything else a skeptical outside reviewer would call out before the design doc is written?
