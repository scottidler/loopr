# Agent Transparency Gaps (2026-04-02)

Observed during lua-todo E2E run. Four gaps identified, with owner responses:

## 1. Scope Drift - agents touching out-of-scope files

Git worktrees and atomic locks exist and provide isolation guarantees.
The question is whether we're actually using them.

Finding: `touched_paths` was `[]` on every bundle in the run. The field existed in the
schema and the handler accepted it, but `handle_propose_bundle` never computed or sent it.

**Fix applied:** `src/agents/executor/action/bundle.rs` now runs
`git diff --name-only main...HEAD` at proposal time and includes the result as
`touched_paths`. Scope enforcement at the handler (Layer 3) was already written - it was
just never triggered because `touched_paths` was always empty.

The scope enforcement at Layer 3 is currently warn-only. Promoting to hard-reject is
pinned pending confirmation that `resource_tags` covers all legitimately needed paths.

## 2. Tool Spawn Failures / Resilience

Shit happens. The system must be resilient.

The current `is_config_error()` / `CONFIG_PATTERNS` approach in `lifeguard.rs` is wrong.
A string whitelist will never cover all failure modes. See `docs/2026-04-02-error-classification-debt.md`.

**Fix needed:** Type errors at the source (`AgentErrorKind::ToolFailure`), not pattern-match
at the sink.

## 3. Integrator Rejects Without Reasons

When the integrator rejects a bundle (stale tick, merge conflict, validation failure) it
transitions to `Rejected` with no `verification` set. The reviewer always sets a reason.
The integrator must do the same.

Downstream agents (coordinator, next implementer) look at `verification` to understand why
a bundle failed. Empty `verification` from the integrator means they have nothing to act on.

**Fix needed:** Add `bundle.update` with `"Integrator rejected: <reason>"` before every
`bundle.transition` to `Rejected` in `src/agents/integrator.rs`.

## 4. Repeated Failures With No Detection

If the same work item fails for the same reason N times and nothing escalates, the system
will spin forever. The lifeguard detects loops within a single session. There is no
cross-session detection - the coordinator dispatches a new implementer each time without
looking at the pattern of prior failures.

**Fix needed:** The coordinator's `last_error_kind_for_work()` already exists
(`src/agents/coordinator.rs:1477`). It needs to also count repeated failures of the same
kind, and escalate to NeedHelp instead of re-dispatching when a threshold is crossed.
