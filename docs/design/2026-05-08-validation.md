# Design Document: Post-Merge Validation

**Author:** Scott A. Idler
**Date:** 2026-05-08
**Status:** Implemented
**Review Passes Completed:** 4/4
**Crates touched:** `integrator`, `domain` (Tick), `loopr` (config wiring)

## Summary

After `integrate()` merges a Bundle's branch into the integration branch, it currently persists the Tick immediately with no check that the integration branch is in a working state. This doc adds a post-merge validation step: run a configured list of shell commands against the target repo; if any command exits non-zero, roll back the merge and return `IntegrationError::ValidationFailed`. The Tick is only written on a clean validation pass.

## Problem Statement

### Background

The python-api E2E run shipped a Tick on commits `b04af23` and `fe250f1` while `main.py` contained only a stub. The Reviewer approved the Bundle because the implementation satisfied the acceptance criteria as written, but the actual test suite would have caught the regression. The `integrator` CLAUDE.md explicitly notes this deferral: "Validation will be earned via its own design doc when a real run shows a Reviewer-approved Bundle breaking the integration branch."

`v3` had `IntegratorConfig.validation_commands: Vec<String>` (default: `cargo fmt --check`, `cargo clippy`, `cargo test`) run synchronously via `sh -c`. The pattern was proven. v5 dropped it when the integrator crate was rewritten to be non-LLM and lean.

### Problem

`integrate()` in `crates/integrator/src/lib.rs` merges the bundle branch into `loopr/plan-<id>` and immediately writes a Tick. If the merge produces a broken build, broken tests, or a formatting violation, no one finds out until the Plan is Complete and a human reviews the integration branch. The first gate target (`rust-version`) expects `cargo test` to pass after integration; without validation, a failing test silently becomes part of the permanent integration.

### Goals

- After a successful merge, run each command in `IntegratorConfig.validation-commands` sequentially in the target repo.
- If any command exits non-zero or times out, roll back the merge (`git reset --hard <pre_merge_sha>`) and return `IntegrationError::ValidationFailed`.
- If all commands pass, persist the Tick as today.
- Empty `validation-commands` (the default) skips validation entirely - zero behavior change for existing runs.
- The `integrator` crate retains its no-`llm`, no-`agents` dependency invariant.

### Non-Goals

- Auto-detection of validation commands from `Cargo.toml`, `package.json`, etc. Commands are explicit in config.
- Per-Bundle or per-Work validation commands. Validation runs once per integration pass on the merged branch.
- `ToolExecutor` integration. Validation runs via `tokio::process::Command` directly, same as git ops. No agent tooling, no sandboxing, no path deny lists.
- Streaming validation output to the TUI. Captured as a log string; TUI is Tier 4.
- Post-merge branch cleanup on failure. The integration branch is reset; the bundle branch is untouched (Integrator never modifies bundle branches).

## Proposed Solution

### Overview

Insert a validation pass between the git merge phase and the Tick persistence phase inside `integrate()`. The validation pass runs each command in `IntegratorConfig.validation-commands` sequentially via async `tokio::process::Command` with the target directory as the working dir. On any failure, `git reset --hard` to the pre-merge SHA under the `git_lock`, then return `IntegrationError::ValidationFailed`. On success, persist the Tick as today.

### Architecture

```
integrate()
  Phase 1: preflight
  Phase 2: git sequence (checkout, merge loop)
    - pre_merge captured at lib.rs:262 (git rev-parse HEAD after checkout)
    - merge loop ends at lib.rs:358
  Phase 3: [NEW] validation  ← inserts between lib.rs:358 and lib.rs:360
    if config.validation_commands.is_empty() → skip (no-op)
    for cmd in config.validation-commands:
      run cmd in target (tokio::process::Command, .kill_on_drop(true), working_dir = target)
      on failure: git reset --hard pre_merge_sha; git clean -fd → return ValidationFailed
  Phase 4: commit (sha capture at lib.rs:360, create Tick, persist)
```

The pre-merge SHA (`pre_merge`) is already in scope at the insertion point: it is captured at `lib.rs:262` and lives until `integrate()` returns. No new variable needed.

### Data Model

#### `IntegratorConfig` additions

```rust
/// Shell commands to run after the git merge, before Tick persistence.
/// Each string is passed to `sh -c`. Commands run sequentially; the
/// first non-zero exit aborts with `ValidationFailed` and rolls back
/// the merge. Empty list (default) skips validation.
pub validation_commands: Vec<String>,

/// Wall-clock cap for each individual validation command. Default 300s.
pub validation_timeout: Duration,
```

Config file (`.loopr/config.yml`) surface:
```yaml
integrator:
  validation-commands:
    - "cargo fmt --check"
    - "cargo test"
  validation-timeout-secs: 300
```

#### `IntegrationError` new variant

```rust
#[error("validation command failed: `{command}` exited {exit_code:?}\n{log}")]
ValidationFailed {
    command: String,
    exit_code: Option<i32>,
    log: String,
}
```

#### `Tick` - no changes

Validation is a gate, not a field. A Tick only exists on success. `ValidationFailed` leaves no Tick record.

`spawn_integrator_for_bundle` in `context.rs` must handle `ValidationFailed` explicitly by calling `fail_all` to transition the Bundle to `IntegrationFailed`. Leaving the Bundle in `Integrating` is incorrect: the retry loop only retries `Update(Stale)` and `Store` errors; `ValidationFailed` is deterministic and would loop indefinitely without a state transition. This is a daemon-layer change (Phase 3) not an integrator-crate change.

### Implementation Plan

#### Phase 1: Config and error extension
**Model:** sonnet

- Add `validation_commands: Vec<String>` (default `vec![]`) and `validation_timeout: Duration` (default 300s) to `IntegratorConfig`.
- Add `#[error(...)] ValidationFailed { command: String, exit_code: Option<i32>, log: String }` to `IntegrationError`.
- Add `VALIDATION_TIMEOUT_SECS_DEFAULT: u64 = 300` const.
- Update `IntegratorConfig::default()` and any test fakes.
- Add `validation-commands` and `validation-timeout-secs` deserialization to whatever config loading exists in `loopr` for `IntegratorConfig` (check `build_context` in `daemon.rs`).
- Tests: unit tests for the config defaults and the new error variant's Display.
- `otto ci` green.

#### Phase 2: Validation runner
**Model:** sonnet

- Add `crates/integrator/src/validation.rs`.
- Implement `pub(crate) async fn run_validation(commands: &[String], timeout: Duration, target: &Path) -> Result<(), ValidationError>` where `ValidationError` carries the failing command, exit code, and captured stdout+stderr log.
- Each command runs via `tokio::process::Command::new("sh").arg("-c").arg(cmd).current_dir(target).kill_on_drop(true)` with stdout+stderr captured (`output().await`). `.kill_on_drop(true)` is mandatory: without it, dropping a timed-out future does not kill the OS process, leaving rogue child processes running in the worktree.
- Timeout wraps each command independently via `tokio::time::timeout`.
- Captured output stored in `ValidationError.log` is capped at 64 KiB by truncating the combined stdout+stderr buffer after `output()` returns. This bounds the error record size. An infinite-output command is killed by the timeout (via `kill_on_drop`), not this cap.
- On success (all commands exit 0 within timeout): `Ok(())`.
- On failure: `Err(ValidationError { command, exit_code, log })`.
- `ValidationError` is a module-private type that `integrate()` maps to `IntegrationError::ValidationFailed`.
- Tests in `crates/integrator/tests/validation.rs`: run a command that succeeds, run a command that fails, run a command that times out, empty command list succeeds immediately.
- `otto ci` green.

#### Phase 3: Wire into `integrate()`
**Model:** sonnet

- In `integrate()` in `lib.rs`, capture `pre_merge_sha` from `git rev-parse HEAD` after the integration branch checkout but before the merge.
- After Phase 2 git sequence completes successfully (all merges landed), call `run_validation(&deps.config.validation_commands, deps.config.validation_timeout, &deps.target).await`.
- On `Err(validation_err)`: run `git reset --hard pre_merge_sha` then `git clean -fd` under `deps.git_lock`, map to `IntegrationError::ValidationFailed`, return. `git clean -fd` removes untracked build artifacts, test caches, and log files generated by the validation command that `git reset --hard` would leave behind.
- On `Ok(())`: proceed to Phase 3 commit (create Tick, persist) as today.
- `IntegratorDeps` already has `target: PathBuf` (line 143 of `lib.rs`). No struct change needed.
- In `spawn_integrator_for_bundle` in `crates/loopr/src/daemon/context.rs`, add `IntegrationError::ValidationFailed { .. }` to the match arm that calls `fail_all`, transitioning the Bundle to `IntegrationFailed`. Do not add it to the retry arm.
- Tests: extend `tests/integrate_seam.rs` with a case where validation is configured and fails; assert the merge is rolled back (branch returns to pre-merge SHA), untracked files are gone, and `IntegrationError::ValidationFailed` is returned.
- `otto ci` green.

#### Phase 4: Integration test and summary
**Model:** sonnet

- Add `crates/integrator/tests/validation_wiring.rs`: full round-trip test using a real git repo in a tempdir. Configure one passing and one failing validation command. Assert: passing command alone → Tick created; failing command → no Tick, integration branch reset to pre-merge SHA.
- Verify `otto ci` at repo root passes.
- `otto ci` green.

## Alternatives Considered

### Alternative 1: ToolExecutor in IntegratorDeps

- **Description:** Add `T: ToolExecutor` to `IntegratorDeps` and run validation through the tool sandbox pipeline.
- **Pros:** Consistent with how agents run bash; respects path deny lists and lane routing.
- **Cons:** `tools` depends on `domain`; `integrator` already depends on `domain`. Adding `tools` as a dep is architecturally defensible but introduces agent machinery into a deterministic crate. The `integrator` CLAUDE.md says "no LLM" - `ToolExecutor` isn't LLM but it is agent infrastructure. Validation commands run in the target's root context with no sandboxing needed (they're the target's own test suite).
- **Why not chosen:** `tokio::process::Command` with `current_dir` is the exact same thing `ToolExecutor` does for Bash, minus the overhead. The integrator's simplicity invariant wins.

### Alternative 2: ValidationFailed Tick state (no rollback)

- **Description:** Write the Tick but mark it as `ValidationFailed` in a new FSM state. The integration branch stays merged.
- **Pros:** Preserves the merge as an artifact; easier to debug what broke.
- **Cons:** The integration branch is broken. Subsequent merges compound the breakage. Any human who checks out the branch sees a failing build. The recovery path is more complex (need to revert the Tick, unmerge, re-run).
- **Why not chosen:** A broken integration branch is worse than a rolled-back one. The merge never happened as far as the store is concerned; the Bundles stay in `Integrating` for the retry circuit-breaker to handle.

### Alternative 3: Per-target `validate.yml` file

- **Description:** Validation commands live in `<target>/.loopr/validate.yml` instead of `.loopr/config.yml`.
- **Pros:** Discoverable by looking in the target's `.loopr/` dir; natural place for target-specific config.
- **Cons:** Introduces a second config file with its own format. v5 already has `.loopr/config.yml` as the single per-target config surface. Per-target overrides of global config live inside `config.yml` (per vision.md "Config defines WHAT rules look like").
- **Why not chosen:** `integrator.validation-commands` in `.loopr/config.yml` follows the existing pattern.

## Technical Considerations

### Dependencies

No new crate dependencies. `tokio::process::Command` is already in scope via `tokio` (workspace dep).

### Performance

Validation commands run sequentially, blocking the `spawn_integrator_for_bundle` task and holding `git_lock` for their full duration. For a `cargo test` that takes 2 minutes, the `git_lock` is held for 2 minutes, serializing all other integrator tasks against the same target.

**Validation commands should be kept fast (seconds, not minutes).** `cargo fmt --check` and `cargo clippy` are appropriate first-gate commands. `cargo test --workspace` on a large repo is not. The timeout (default 300s) is a safety cap, not a suggested command duration. Users who configure slow commands are explicitly trading throughput for quality gates.

### Security

Commands run via `sh -c` with `current_dir = target`. The target dir is trusted (it's the repo the user pointed `loopr -C` at). No additional sandboxing.

### Testing Strategy

- **Unit (Phase 1):** Config defaults, error variant Display.
- **Unit (Phase 2):** Validation runner with real process execution in a temp dir (pass, fail, timeout, empty-list).
- **Integration (Phase 3):** `integrate_seam.rs` extended with a validation-failure case.
- **Integration (Phase 4):** Full git-round-trip test: merge lands, validation fails, branch reset confirmed.

### Rollout Plan

Single branch on `v5`. Four phases, each committed separately. No feature flag needed: `validation-commands` defaults to empty, so no behavior change until a user adds commands to their config.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| `git reset --hard` on rollback races with another concurrent integration | Low | High | Validation runs inside the same `git_lock` guard that covers Phase 2 (line 231 of `lib.rs`). The lock is held until `integrate()` returns, so no concurrent integration can touch the integration branch during validation or rollback. |
| Validation command needs `ANTHROPIC_API_KEY` or other env var not present in daemon env | Medium | Low | User's problem to configure; daemon inherits the shell env it was started with. Document in config's comment |
| Validation timeout too short for slow test suites (e.g., full `cargo test` in a large workspace) | Medium | Medium | Configurable per target via `validation-timeout-secs`; default 300s is generous for first-gate targets |
| `pre_merge_sha` capture fails (git error) | Low | Medium | Already fatal in current git sequence; if rev-parse fails, integrate returns a `Git` error before reaching validation |
| Running validation with a dirty worktree (index has staged-but-uncommitted changes from the merge) | Low | Low | The git merge in Phase 2 produces a clean commit; the integration branch HEAD is the merge commit; `cargo test` etc. see a clean tree |
| Cargo `target/` collision: implementer worktrees and integrator validation share the default build cache | Medium | Medium | Git worktrees share a single `target/` directory unless `CARGO_TARGET_DIR` is overridden. A `cargo test` in the root worktree will collide with `cargo build` in an implementer worktree, causing spurious lock errors. Mitigation: document that users running Rust targets should set `CARGO_TARGET_DIR` per worktree in their validation commands, e.g. `CARGO_TARGET_DIR=/tmp/loopr-integration cargo test`. Full fix (auto-injecting CARGO_TARGET_DIR) is deferred. |

## Open Questions

- [x] **Log size in `ValidationFailed`.** Capped at 64 KiB by truncating the combined stdout+stderr buffer after `output()` returns. An infinite-output command is killed by `kill_on_drop` when the per-command timeout fires — the cap bounds the stored error record, not in-memory accumulation during the run. Sufficient for first-gate commands.
- [ ] **Should the daemon log a structured `warn!` with the failing command and exit code, or rely on the error propagation?** Lean: structured `warn!` at the `spawn_integrator_for_bundle` call site in `context.rs` - same pattern as existing error paths.
- [ ] **Is `git reset --hard` the right rollback, or `git revert`?** Reset is right: the merge commit was never pushed (v5 git posture is never-push); hard reset discards it cleanly. Revert would add an extra commit that undoes the merge, which is unnecessary noise on a branch no one else has seen.

## References

- `docs/deferred-roadmap.md §1.4` - this doc's stub
- `crates/integrator/CLAUDE.md` - "Validation will be earned via its own design doc"
- `crates/integrator/src/lib.rs:4` - "First-gate scope is merge-only; validation is deferred"
- `crates/integrator/src/error.rs` - `IntegrationError` variants
- `crates/integrator/src/config.rs` - current `IntegratorConfig`
- `~/repos/scottidler/loopr/src/agents/integrator.rs:1707-1760` - v3 `run_validation_commands` (reference impl)
- `~/repos/scottidler/loopr/src/config.rs:342` - v3 `validation_commands: Vec<String>` config field
- `bin/e2e-targets/rust-version.md` - "Final Validation: `cargo test`"
