# worktree

Flat-interior per-attempt git worktree lifecycle. Infrastructure-only: primitives, not orchestration.

Every Work attempt gets a fresh worktree at `<target>/.loopr/worktrees/<work-id>-<seq>/` on branch `loopr/wk-<work-id>-<seq>`. Seq is monotonic per `(work-id)`, allocated internally by `Worktree::create` via an EEXIST-retry loop against git's own "already exists" stderr phrases (with `LC_ALL=C` forced for locale stability). No branch reuse across attempts; no rebase-on-retry; no commit preservation.

## In scope

- `Worktree` RAII handle: owns `path` / `branch` / `work_id` / `seq` / `repo_path` / `consumed`. `Drop` is a crash safety net; routine cleanup is explicit `.cleanup()` inside `tokio::task::spawn_blocking` (sync `git worktree remove` must not starve the tokio executor).
- `Worktree::create(repo_path, worktree_root, work_id, sha)` — provisions the worktree + branch; caller passes a pre-resolved base SHA (D10: resolved in repo context, never inside the worktree).
- `Worktree::cleanup(self)` — removes the worktree, **keeps the branch** (integrator merges it later).
- `worktree::delete_branch(repo_path, branch)` — integrator calls this after a Tick publishes; idempotent.
- `worktree::list(repo_path, worktree_root)` — parses `git worktree list --porcelain`, filtered to paths under `worktree_root`.
- `worktree::cleanup_at(repo_path, path)` — free function for reconcile when no handle exists.
- `worktree::parse_branch(branch) -> Option<(WorkId, u32)>` — strict parser for `loopr/wk-<work-id>-<seq>`.
- `worktree::ensure_loopr_excludes(repo_path)` — idempotent append to `.git/info/exclude` with `# loopr-managed` marker.
- `WorktreeConfig { cleanup_policy }` + `AttemptCleanupPolicy` enum (`Immediate` / `OnWorkTerminal` / `OnRunEnd` / `Never`; default `OnWorkTerminal`). Composed into top-level `Config` by `loopr`.
- Internal git invocations via `std::process::Command` directly; `worktree` does NOT go through the `tools` crate (infrastructure, not an LLM-facing tool).

## Out of scope

- **Crash-recovery reconciliation.** Lives in the `loopr` binary (`loopr::daemon::startup::reconcile`) because it joins git state with TaskStore + `Work`-FSM mutations + live-session map. `worktree` exposes the primitives; `loopr` orchestrates.
- **No registry file.** Git's own worktree registry (`git worktree list --porcelain`) + the `loopr/wk-*` branch prefix + TaskStore is a complete, race-free reconcile path. The vision's prior `.loopr/worktree-registry.jsonl` was redundant state with its own locking / corruption surface.
- **No sibling-path layout.** Vision's prior `<target-parent>/<target-name>-work-<work-id>/` broke deployment (CI, containers, read-only parent mounts, atomic-cleanup) without adding value beyond what `ensure_loopr_excludes` already covers.
- **No `Worktree::refresh` (rebase onto new tick).** v4's API dropped: a retry spawns a new worktree at the current integration tip at seq+1.
- Tool execution inside the worktree — that's `tools`. Tools receive a worktree handle and run commands inside it.
- LLM calls — that's `llm`.
- Branch naming conventions beyond `loopr/wk-<work-id>-<seq>`. Plan integration branches (`loopr/plan-<plan-id>`) are owned by `integrator`.
- Push/pull operations — loopr's push policy is "never" (see `docs/vision.md` "Git Posture").

## Rule

`Drop` is a safety net, not the routine cleanup mechanism. The coordinator in `loopr` dictates *when* to clean based on `AttemptCleanupPolicy` and executes `.cleanup()` inside `tokio::task::spawn_blocking`. The Round 1 Architect finding — "Drop guards do not execute on SIGTERM, SIGKILL, or power loss" — is addressed by `loopr::daemon::startup::reconcile`, which runs ONCE at daemon startup before the IPC listener accepts.

Seq allocation is atomic via git's own "branch/path already exists" errors as the EEXIST-equivalent. No separate claim-then-create step; git's refcount on branch creation is the serialization primitive.

Design doc: [`docs/design/2026-04-21-worktree-lifecycle.md`](../../docs/design/2026-04-21-worktree-lifecycle.md) (cross-cutting; lives at repo root because it also touches the `loopr` binary).

## Dependencies

`domain` (for `WorkId`), `telemetry` (for span emission), workspace-shared (`serde`, `thiserror`, `clap`, `tracing`). Added via `cargo add`.

## Instrumentation

`Worktree::create` opens `worktree.create` at `info` with `err`. `work_id`, `repo_path`, `worktree_root`, `base_sha` set at span open; `seq` and `branch` filled via `Span::current().record()` once the seq-allocation loop succeeds. Reading the closing span answers "what worktree, on which branch, at which base SHA, on which seq."

`Worktree::cleanup` opens `worktree.cleanup` at `info` with `err` carrying `work_id`, `branch`, `worktree_path`, `seq`. The free functions `worktree::list`, `worktree::cleanup_at`, `worktree::delete_branch`, `worktree::resolve_sha`, `worktree::ensure_loopr_excludes` each open spans at `info`/`debug` with their own scope keys; `list` also records `count` post-parse.

Internal git wrappers (`ops::try_create_at_seq`, `ops::remove_worktree`, `ops::delete_branch`, `ops::prune`, `ops::resolve_sha`, `ops::list_porcelain`) open `worktree.ops.<op>` at `debug` with `err`. The pre-existing `Worktree::Drop` `tracing::warn!` stays as the safety-net signal.

Acceptance test: `tests/instrumentation.rs::worktree_smoke_spans_create_then_cleanup` creates and cleans up a real worktree, asserts both span names and the post-creation `seq` + `branch` fields.

## See also

- [../../CLAUDE.md](../../CLAUDE.md): project-wide rules and crate map
- [../../docs/vision.md](../../docs/vision.md): "worktree" crate contract (amended 2026-04-21, a1/a2/a3/a4/a5)
- [../../docs/design/2026-04-21-worktree-lifecycle.md](../../docs/design/2026-04-21-worktree-lifecycle.md): this crate's Stage 7 design doc
- [.otto.yml](.otto.yml): scoped CI for this crate (`otto ci` inside this dir)
