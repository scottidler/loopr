# worktree

Sibling git worktree lifecycle. Creates worktrees under `<target-parent>/<target-name>-work-<work-id>/` outside the target repo, maintains the registry under `.loopr/worktree-registry.jsonl` inside it, and cleans up on completion or at daemon startup after an ungraceful shutdown.

## In scope

- `Worktree` struct: handle to a live worktree with guaranteed cleanup on `Drop` (happy path)
- `create(target_path, work_id)` → `Result<Worktree>` — provisions a sibling worktree on branch `loopr/wk-<work-id>`
- Registry: append to `<target>/.loopr/worktree-registry.jsonl` on create; mark entries terminal on successful cleanup
- **Crash recovery on daemon startup:** read the registry, reconcile each entry against git state (branch exists? worktree path exists? Work record in terminal state?), clean up orphans, delete orphaned `loopr/wk-*` branches
- Internal git invocations via `std::process::Command` (`git worktree add`, `git worktree remove`, `git branch -D`) — does NOT go through the `tools` crate (worktree is infrastructure, not an LLM-facing tool)
- Config: `WorktreeConfig` composed into the top-level `Config` by `loopr`

## Out of scope

- Tool execution inside the worktree — that's `tools`. Tools receive a worktree handle and run commands inside it.
- LLM calls — that's `llm`
- Branch naming conventions beyond `loopr/wk-<work-id>` — plan integration branches (`loopr/plan-<plan-id>`) are owned by `integrator`
- Push/pull operations — loopr's push policy is "never" (see vision.md "Git Posture")

## Rule

`Drop` handles the happy path. Crash recovery — the daemon startup reconciliation — handles SIGTERM, SIGKILL, and power loss. Both paths are required; without startup reconciliation, orphaned worktrees accumulate over time and eat disk.

The Architect's Round 1 finding — "Drop guards do not execute on SIGTERM, SIGKILL, or power loss" — is addressed by the registry + startup reconciliation pattern documented here and in `docs/vision.md`.

## Dependencies

`domain` (for `WorkId` and related types), `telemetry` (for span emission), workspace-shared (`serde`, `eyre`). Added via `cargo add`.

## See also

- [../../CLAUDE.md](../../CLAUDE.md): project-wide rules and crate map
- [../../docs/vision.md](../../docs/vision.md): "Target Repo Layout" and crash-recovery subsection
- [docs/CLAUDE.md](docs/CLAUDE.md): where this crate's design docs go
- [.otto.yml](.otto.yml): scoped CI for this crate (`otto ci` inside this dir)
