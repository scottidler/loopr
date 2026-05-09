# Design Document: Comprehensive telemetry — make every stage diagnosable from events.log alone

**Author:** Scott Idler
**Date:** 2026-05-09
**Status:** Implemented
**Crates touched:** telemetry, agents, tools, integrator, worktree, decomposer, loopr
**Review Passes Completed:** 4/4 + Architect Rounds 1 + 2 (architecturally sound)

## Summary

Today's e2e python-api run exposed a class of diagnosability gap: the `#[instrument]` attributes are pervasive in the source, but successful spans emit nothing in `events.log` because `#[instrument]` alone does not produce a log line — only nested macro calls and `err` (on `Err`) do. The success paths of `integrator.integrate`, `tool.<name>`, `lifeguard.check_action`, every `worktree.*` op, and most decomposer inner spans run silently. This doc proposes a two-track fix: (1) emit explicit lifecycle events inside every "must-be-diagnosable" span so the span context lands on a real event, and (2) extend the schema with a small set of fields that today's runs proved we needed (per-AC reviewer verdict, bundle composition manifest, Director per-Bundle acceptance reasoning).

## Problem Statement

### Background

#### Process lesson

The 2026-04-24 instrumentation sweep added `#[instrument]` attributes pervasively but shipped without a test asserting that the resulting `events.log` actually contained the documented spans. The CI smoke tests at the time (`tests/instrumentation.rs::*_smoke_spans_*`) used custom in-memory subscribers, not the production `compose()` pipeline writing to a real on-disk JSONL file. A function with `#[instrument]` and an empty body passes those tests; the same function emits nothing into `events.log` in production. Today's gap is that the sweep's diagnostic value never reached operators reading the file. The Phase 1 contract test in this proposal directly addresses that: it uses the real `compose()` subscriber, writes to a tempdir's `events.log`, and re-parses the JSONL — which is the only thing that proves a span is *visible*, not just *declared*.

#### State of v5 observability

Loopr v5 was built with observability as a first-class concern. The `telemetry` crate owns subscriber composition, XDG-rooted session/process layout, fanout layers, and the typed `SessionId`/`ProcessId` surface. The 2026-04-24 instrumentation sweep added `#[tracing::instrument]` to every non-trivial function across `agents`, `tools`, `integrator`, `worktree`, `decomposer`, and `llm`. The `tools` crate's `CLAUDE.md` explicitly documents the contract: every builtin's `execute()` opens a `tool.<name>` span with `tool_name`, `lane`, `path`/`pattern`/`command_chars`, `working_dir`. The `integrator` crate's `CLAUDE.md` documents `integrator.integrate` carrying `phase=preflight|git_sequence|commit`. The `agents` crate's `CLAUDE.md` documents `lifeguard.check_action` with `action_hash`/`action_count`/`max_repeat`.

A spot-check of today's events.log against the latest python-api run confirmed:

```
=== span names actually present in events.log ===
daemon.build_context, daemon.reconcile, daemon.serve_core,
daemon.spawn_implementer_for_work, daemon.spawn_integrator_for_bundle,
daemon.spawn_reviewer_for_bundle, decompose, director.run,
ipc.connection, ipc.dispatch, ipc.plan_create, run_implementer,
run_reviewer, spawn_implementer_for_work, spawn_integrator_for_bundle,
spawn_reviewer_for_bundle
```

Missing (verified by grep against the live events.log): `integrator.integrate`, `integrator.transition_bundle`, `integrator.fail_all`, `integrator.git.*`, `tool.read`, `tool.glob`, `tool.bash`, `tool.write`, `tool.edit`, `tool.grep`, `router.spawn`, `spawn.process_group`, `check_action` (lifeguard), `record_parse_failure` (lifeguard), `worktree.create`, `worktree.ops.*`, `try_llm_once` (decomposer), `detect_cycles` (decomposer), `collect_workspace_tree` (decomposer), and most `agents::dispatch::*` + `agents::parse::*` spans.

Naming note: the source uses two conventions side-by-side. Stage-level spans declare an explicit `name = "integrator.integrate"` / `tool.read` / `director.run`. Inner helpers default to the function name (so `check_action`, `try_llm_once`, `detect_cycles` are bare). The per-crate `CLAUDE.md` files document the dotted form for some spans that the source declares bare; that drift is its own small reconciliation task tracked in Phase 9.

The source has the `#[instrument]` attributes. The subscriber config is correct (json layer at the user-supplied directive, daemon was started at `-l debug`). What's missing is that `tracing-subscriber`'s `fmt::layer().json()` does not synthesize span enter/exit events by default — only events emitted via `tracing::event!` (or the `info!`/`warn!`/`error!`/`debug!`/`trace!` macros) and the synthetic `err` event on `Err`. A function whose body never calls a logging macro and returns `Ok` is invisible.

### Problem

A failure mode like today's wk-k1oz5 lifeguard loop has a one-line ERROR string carrying the action_hash, but no:

- `tool.<name>` span recording WHICH tool was called with WHAT inputs in the iterations leading up to the escalation.
- `lifeguard.check_action` debug events with structured `action_hash`/`action_count` over time, so "did the same hash hit twice?" is a one-grep question.
- `integrator.integrate` phase progression, so a stalled integration shows where it stopped.
- `worktree.create` events naming which seq/branch was allocated to which Work.
- `agents.dispatch` per-iteration trail showing which actions were proposed by the LLM, which were dispatched, which failed.

Today's wk-7xtad reviewer-too-thin failure has no structured per-AC verdict either; the reviewer's `summary=...` is a freeform sentence, not a checklist of which ACs were verified vs skipped. A reviewer that rubber-stamps a too-thin Bundle is currently invisible to events.log.

### Goals

- Every "lifecycle" span (one per stage transition, one per tool call, one per FSM transition, one per LLM call, one per agent iteration) emits at least one log event from inside its scope so the span context lands on a real event in `events.log`.
- The fields documented in each crate's `CLAUDE.md` (action_hash, lane, phase, etc.) actually appear on a grep-able event line, not just the span ancestry of unrelated events.
- New schema for things we now know we need but never instrumented: per-AC reviewer verdict, bundle composition manifest, Director per-Bundle acceptance reasoning, Director restart_count + restart_reason as first-class fields, tool input args on the success path with secret redaction.
- A snapshot test in `crates/telemetry/tests/` (or per-crate) drives a representative path and asserts that `events.log` contains every documented span name with its required fields. If a future change removes an `#[instrument]` or stops emitting an event, the test fails.
- The fix is incremental and verifiable per phase: each phase ends with a green snapshot test for its scope.

### Non-Goals

- Switching subscribers, layers, or formatters. The composition in `crates/telemetry/src/subscriber.rs` is correct; the fix is in what emits events, not how they're formatted.
- Adding a metrics or OpenTelemetry export pipeline. Out of scope per the `telemetry` `CLAUDE.md`'s explicit non-goal.
- Changing `events.log`'s on-disk JSON shape. New fields are added; the line-oriented JSON-per-event format is unchanged.
- Removing the existing `err`-synthesized error events. They are useful and correct; the fix is to add complementary events on the success and progress paths.
- Sandbox / permission audit events. Those belong wherever permission decisions are made; they may layer on later.
- Replacing or rewriting any agent or stage logic. This is a pure observability change.

## Proposed Solution

### Overview

Two tracks, executed as ordered phases.

**Track A — Make existing instrumentation visible.** For every `#[instrument]`-decorated function whose body currently contains no logging macro, add a single explicit event at function entry or at success-path exit that names the operation and emits the same fields the span carries. This guarantees the span ancestry lands on a real event in `events.log`, and gives operators a grep-friendly handle.

Level discipline:
- `info!` — stage lifecycle (one per Plan, one per Work transition, one per Bundle FSM transition, one per integration phase).
- `debug!` — per-iteration / per-tool / per-action / per-AC. Default for the body of an agent loop.
- `trace!` — sub-helper noise (per-character, per-byte, per-loop-step). Hot-path-only.

Where the success path emits a duration is non-trivial (an entire LLM call, an entire tool execution, an iteration), the event also records `elapsed_ms` so a post-hoc latency analysis answers "which iteration was slow?" without subtracting timestamp pairs.

**Track B — Add fields and spans we proved we need.** Extend `Verdict` with structured per-AC results. Extend the implementer's "produced bundle" event with a paths/diff manifest. Add `director.accept_bundle` / `director.reject_bundle` spans for Director per-Bundle decisions. Add `restart_count` and `restart_reason` to `director.run`. Add success-path arg fields to tool spans (with secret redaction).

### Architecture

The fix is per-crate and additive. There is no new module, no new abstraction layer, no protocol change.

- **Stage crates** (`agents`, `tools`, `integrator`, `worktree`, `decomposer`): each `#[instrument]`-decorated function whose body currently contains no logging macro adds one inline event at the appropriate level (`info!` for stage lifecycle, `debug!` for per-iteration / per-tool, `trace!` for sub-helpers). Span ancestry on that event makes the span visible in `events.log`. Track A.
- **`agents::reviewer`**: parser improvement to extract per-AC results from the LLM response and emit one `reviewer.ac` event per criterion plus a roll-up. `domain::Verdict` is unchanged. Track B.
- **`agents::dispatch::propose_bundle`**: the existing "implementer produced bundle" emission gains paths/diff-sha manifest fields. Track B.
- **`agents::director`**: new `director.accept_bundle` / `director.reject_bundle` `#[instrument]` functions with their own inline events. The existing `director.run`'s `restart` field is preserved; new `restart_reason` field added. Track B.
- **`telemetry/tests/`**: new contract test harness asserting events.log produces every documented span name with its required fields after a representative scenario. It is the keystone of the work — Phase 1 ships it as scaffolding so every later phase can extend it.

### What events.log looks like AFTER the fix

A successful 3-tool implementer iteration today produces zero `tool.*` lines and one `implementer iteration start` line. After the fix the same iteration produces:

```jsonc
{"level":"INFO","fields":{"message":"implementer iteration start","iteration":3,"work_id":"wk-abc"},"target":"agents::implementer","span":{"name":"run_implementer","work_id":"wk-abc"}}
{"level":"DEBUG","fields":{"message":"tool: ok","elapsed_ms":12,"bytes":4096},"target":"tools::builtin::read","span":{"name":"tool.read","tool_name":"read","lane":"local","path":"/tmp/.../src/main.rs","working_dir":"/tmp/.../wk-abc-1"}}
{"level":"DEBUG","fields":{"message":"tool: ok","elapsed_ms":3,"bytes":120},"target":"tools::builtin::edit","span":{"name":"tool.edit","tool_name":"edit","lane":"local","path":"/tmp/.../src/main.rs","working_dir":"/tmp/.../wk-abc-1"}}
{"level":"DEBUG","fields":{"message":"lifeguard: action observed","action_kind":"run_tool","action_hash":"0xcf0a...","action_count":1,"max_repeat":3},"target":"agents::lifeguard","span":{"name":"check_action","work_id":"wk-abc"}}
{"level":"INFO","fields":{"message":"implementer produced bundle","bundle_id":"bd-xyz","head_commit":"...","paths_added":["src/foo.rs"],"paths_modified":["src/main.rs"],"paths_deleted":[],"patch_id":"a3f1...","diff_bytes":840},"target":"agents::dispatch","span":{"name":"propose_bundle","work_id":"wk-abc"}}
```

A grep `grep '"name":"tool.read"' events.log | jq -r .fields.path` now answers "which paths did the implementer read on this run." Today that question requires opening the transcript markdown file and reading prose.

### Data Model

#### Track B schema additions

`Verdict` is an enum in `crates/domain/src/verdict.rs` with three variants (`Accept` / `ChangeRequested` / `Reject`) — not a flat struct. Per-AC results are added as a SEPARATE structured payload that flows through the reviewer's events, NOT by extending the `Verdict` enum itself. This keeps `domain` unchanged and avoids a serde-shape break; the AC verification is observability data, not a domain record.

The reviewer emits a synthesized event per AC:

```rust
// inside agents::reviewer::run_reviewer, after parsing the LLM response
for ac in parsed_ac_results {
    debug!(
        target: "reviewer.ac",
        bundle_id = %bundle_id,
        work_id = %work_id,
        criterion = %ac.criterion,           // verbatim AC text from Plan
        status = %ac.status,                  // verified | skipped | failed | not_applicable
        evidence = %ac.evidence_or_empty(),   // optional reviewer note (path:line, snippet)
        "reviewer: ac evaluated"
    );
}

// then a roll-up event the existing accept-bundle path can read:
info!(
    bundle_id = %bundle_id,
    ac_count = parsed_ac_results.len(),
    ac_verified = verified_count,
    ac_skipped = skipped_count,
    ac_failed = failed_count,
    "reviewer: ac roll-up"
);
```

Parser shape (lives in `agents::reviewer::parse_verdict` or a sibling): the LLM's response already enumerates ACs in today's prompt. The parser extracts a `Vec<AcResult>` opportunistically and emits it as events; if parsing fails it logs a `warn!` and falls back to the existing freeform `summary` path. The parsed results live in `events.log` only — they do not round-trip back to `Verdict` (Verdict is unchanged per Phase 5's non-goal) and they do not feed `summary.md` (deferred to a separate follow-up doc; see Open Questions). The contract test asserts the events appear; that is the only consumer in scope for this doc.

The implementer's "produced bundle" emission gains a manifest. The event fires from inside `agents::dispatch::propose_bundle` (where `head_commit` and the dirty-paths set are already computed); the existing `loopr::daemon::context` "produced bundle" log line is removed and replaced by this richer event. The visible target/span ancestry shifts from `loopr::daemon::context` to `agents::dispatch` — that's the correct location and the contract test asserts the new shape.

```rust
// inside agents::dispatch::propose_bundle, after the bundle commit lands
info!(
    bundle_id = %bundle_id,
    work_id = %work_id,
    head_commit = %head_commit,
    paths_added = ?paths_added,
    paths_modified = ?paths_modified,
    paths_deleted = ?paths_deleted,
    patch_id = %patch_id,
    diff_bytes = diff_byte_len,
    "implementer produced bundle"
);
```

Where `patch_id` is the first whitespace-delimited token returned by `git show <commit> | git patch-id` (stable across context-line and whitespace settings — `git patch-id` reads a patch from stdin and emits `<patch-id> <commit-id>`; we keep the first token). For commits whose diff exceeds a configurable cap (default 1 MiB), `patch_id` is recorded as the literal string `"oversize"` and `diff_bytes` carries the length so a runaway implementer is visible without paying the patch-id compute cost.

#### Track B span additions

`director.accept_bundle` and `director.reject_bundle` (in `agents::director`):

```rust
#[instrument(name = "director.accept_bundle", level = "info", skip_all,
    fields(plan_id, work_id, bundle_id, retry_budget_remaining), err)]
fn accept_bundle(...)
```

`director.run` already declares a `restart` field (today's events show `"restart":3` after three retries). What's missing is a `restart_reason` so a single span entry distinguishes "LLM transient error" from "LLM fatal error" from "internal panic":

```rust
#[instrument(name = "director.run", level = "info", skip_all,
    fields(
        plan_id,
        iteration = tracing::field::Empty,
        restart = tracing::field::Empty,           // existing
        restart_reason = tracing::field::Empty,    // NEW
    ),
    err,
)]
```

The restart loop uses `tracing::Span::current().record("restart", n)` and `record("restart_reason", reason_str)` at each restart boundary. Reasons are a fixed-ish enum surface (`llm_retryable`, `llm_fatal`, `parse_failure`, `internal_panic`, ...) emitted as `&'static str` so grepping by reason is exact.

Tool success-path arg fields use the existing `path`/`pattern`/`command_chars` fields (already declared on the `#[instrument]` macro per `tools/CLAUDE.md`); Track A makes those visible by having each tool's `execute()` emit a `debug!("tool: ok", path = ..., bytes = ..., elapsed_ms = ...)` on the success branch before returning.

### API Design

This is observability, not API. The only externally-visible surface change is:

- `Verdict` JSON shape is **unchanged** (per-AC data lives in `events.log`, not in the persisted Verdict).
- `events.log` JSONL gains new event types; consumers grep by `name` or `target`, no schema migration needed.
- `record.get` IPC method's Verdict return shape is unchanged.

### Implementation Plan

Each phase ends with a green snapshot test that asserts the new fields and span names appear in `events.log` after a representative run. Test scaffolding lives in `crates/telemetry/tests/events_log_contract.rs` and is shared across phases via a builder helper.

#### Phase 1: Test harness for events.log assertions
**Model:** sonnet
- Add `crates/telemetry/tests/events_log_contract.rs`. Helper: `run_and_capture_events(scenario_fn) -> Vec<JsonValue>` that initializes a subscriber writing to a tempdir, runs the scenario, and returns the parsed JSONL.
- Helper: `assert_event(events, name = "...", required_fields = &[...])` — fails with a clear diff if the named event is absent or any required field is missing.
- Two initial scenarios as smoke tests: `decompose_smoke` (already-visible — sanity), `daemon_serve_core_smoke` (already-visible — sanity). These pass on day one; their function is to exercise the harness.

#### Phase 2: Track A — make tool.* spans visible
**Model:** sonnet
- Each builtin's `execute()` in `crates/tools/src/builtin/` adds a `debug!("tool: ok", elapsed_ms = ..., bytes = ...)` (or equivalent) on the success branch. The existing `#[instrument]` attribute already names the span and declares the right fields; the inline event makes them land on a real line.
- `crates/tools/src/router.rs::spawn` and `crates/tools/src/spawn.rs` each add a `debug!("router: dispatched", lane = ..., timeout_secs = ...)` and `debug!("spawn: process started", invocation_id = ...)` respectively.
- Add `tool_*_visible` scenarios to the contract test: dispatch each tool through the router and assert `events.log` contains `"name":"tool.<name>"` with the documented fields.

#### Phase 3: Track A — make integrator.* spans visible
**Model:** sonnet
- `integrator::integrate` adds `info!` events at each phase transition (already records the field; now emits a line). Pattern: `info!(phase = "preflight", "integrator: phase begin"); ... info!(phase = "git_sequence", "integrator: phase begin"); ...` so a stalled integration's last visible phase is unambiguous.
- `transition_bundle`, `fail_all`, and the `git.*` helpers each add a single `debug!` on the entry/success path.
- Add `integrator_happy_path_phases_visible` and `integrator_fail_all_visible` scenarios.

#### Phase 4: Track A — make lifeguard, dispatch, parse, worktree, decomposer-inner visible
**Model:** sonnet
- `lifeguard::check_action`: emit a `debug!(action_kind, action_hash, action_count, "lifeguard: action observed")` after the count update, before the early return. (The escalation message already carries the hash; this gives the non-escalation path a record.)
- `lifeguard::record_parse_failure`: same shape on the success path.
- Every `agents::dispatch::*` `#[instrument]` function emits a `debug!` at entry naming the action_kind / path / etc.
- `agents::parse::parse_action`: success-path `debug!(action_count = ...)` after parsing.
- `worktree::create`: success-path `info!(seq, branch, "worktree: allocated")`. The other `worktree::ops::*` helpers each get one `debug!`.
- `try_llm_once` (in `decomposer::decompose`), `detect_cycles` (`decomposer::cycles`), `collect_workspace_tree` (`decomposer::tree`), and the prompt-builder helpers in `decomposer::prompt` each add a single `debug!` at success. Span names are bare function names today; reconciliation with the `CLAUDE.md` claims that name them `decomposer.*` happens in Phase 9.
- Add per-area scenarios to the contract test (one assertion block per area, all in the same test file).

#### Phase 5: Track B — reviewer per-AC observability (events-only, no domain change)
**Model:** opus
- `agents::reviewer::parse_verdict` (or a sibling parser) extracts a `Vec<AcResult>` from the LLM's response. Today's reviewer prompt already enumerates ACs; the parser is the new work.
- For each parsed AC, the reviewer emits a `debug!(target: "reviewer.ac", criterion, status, evidence)` event.
- `ReviewerDeps` gains a `path_deny_patterns: Vec<String>` field (mirroring the implementer dispatcher's), wired through `ContextBuilder` setup at daemon startup. The reviewer applies `redact_paths(evidence, &deps.path_deny_patterns)` before emitting the event so caller-supplied deny patterns hit the evidence field. This is the only Reviewer-side dep change in this doc.
- The existing `reviewer accepted bundle` info event gains `ac_count`, `ac_verified`, `ac_skipped`, `ac_failed` counts.
- Partial extraction is acceptable: if the LLM produces 7 of 10 expected ACs in parseable form, emit the 7 events plus a `warn!` naming the count and the missing-AC indices. The roll-up event records `ac_count = 7` (parsed only). The reviewer's accept/reject decision is independent of parse success — the boolean transition still happens regardless.
- Total parse failure (zero structured ACs extractable): `warn!`, no `reviewer.ac` events, no roll-up. The freeform `summary` still lands on the existing accept-bundle event.
- `domain::Verdict` is **not modified**; per-AC data is observability, not a record.
- The Bundle's `summary.md` renderer is **not modified** by this phase. Rendering AC results into the domain artifact would require either I/O in a pure renderer or a Verdict shape change; both are out of scope here. Tracked as a follow-up: `docs/design/<future>-verdict-ac-results-domain-promotion.md` (to be authored when the work is wanted).
- Snapshot test asserts a happy-path review emits `reviewer.ac` events plus the four roll-up counts.

#### Phase 6: Track B — Bundle composition manifest
**Model:** sonnet
- The "implementer produced bundle" event moves into `agents::dispatch::propose_bundle` (where `head_commit` and the dirty-paths set are already computed). The existing emission from `loopr::daemon::context` is removed; the daemon-context site no longer logs a duplicate. The visible target/span ancestry shifts to `agents::dispatch`; that's correct because the function deciding *what's in the bundle* is the function that should report it.
- Manifest fields: `paths_added` / `paths_modified` / `paths_deleted` from git status; `patch_id` computed by piping `git show <commit>` into `git patch-id --stable` (stable across user `diff.context`, `diff.algorithm`, and whitespace settings — unlike `sha256(unified diff)`); `diff_bytes` length.
- For diffs above a configurable cap (default 1 MiB), `patch_id` is recorded as `"oversize"` with `diff_bytes` carrying the byte count so a runaway implementer is visible without paying the patch-id compute cost.
- Snapshot test asserts the manifest fields appear on the produced-bundle event for normal-size diffs and on a synthetically oversized one. Also asserts the duplicate `loopr::daemon::context` emission is gone.

#### Phase 7: Track B — Director acceptance and restart fields
**Model:** opus
- Add `director.accept_bundle` and `director.reject_bundle` `#[instrument]` functions in `agents::director`. The accept path gates the Bundle Reviewed → Integrating transition; the reject path gates the Bundle → Failed transition with reason.
- `director.run` gains `restart_reason` recorded field; the existing `restart` count field is preserved. The restart loop `span.record(...)`s the reason as a stable `&'static str` at each restart boundary.
- Snapshot test asserts both spans emit on the appropriate paths.

#### Phase 8: Cross-cutting — per-Work and per-Plan summary aggregation in events.log
**Model:** sonnet
- The Work's terminal-state transition (Done / Failed / Cancelled) emits an `info!` summary event with `total_iterations`, `lifeguard_fires` (count + last action_hash), `director_override_count`, `terminal_state`. This complements the existing `summary.md` written by the integrator.
- The Plan's Active → Done transition emits an `info!` summary event with `total_works`, `ticks`, `bundles_accepted`, `bundles_rejected`, `total_input_tokens`, `total_output_tokens`, `total_cost_usd`.
- Snapshot test asserts both summary events on a representative end-to-end run.

#### Phase 9: Documentation reconciliation
**Model:** sonnet
- Update each affected crate's `CLAUDE.md` Instrumentation section to reflect what now actually emits.
- Update `docs/vision.md` Observability section if its commitments have shifted.
- Add a short `docs/telemetry-grep-cookbook.md` (or similar) listing common grep patterns operators reach for ("which tool calls did the implementer make on wk-XXX?", "did the lifeguard fire on action_hash 0xYYY?", "what was the integrator's last phase before failure?").

## Alternatives Considered

### Alternative 1: Configure `FmtSpan::CLOSE` on the json layer
- **Description:** Change `crates/telemetry/src/subscriber.rs` so the json layer emits synthetic enter/close events for every span via `.with_span_events(FmtSpan::CLOSE)`.
- **Pros:** One-line change; instantly makes every existing `#[instrument]` visible; no per-function edits.
- **Cons:** Massive event volume — every helper, every trace-level span, every loop iteration emits two extra events. Token cost on log search and disk pressure both balloon. The synthetic events also lack the human-meaningful `message` field, so they're harder to grep by intent. Cannot selectively pick "lifecycle spans" vs "internal helpers."
- **Why not chosen:** Loses the curation that makes events.log readable. The project's whole observability philosophy is structured + selective; firehose-of-spans contradicts it.

### Alternative 2: Annotate spans with a "lifecycle" trait that the subscriber filters on
- **Description:** Introduce a custom marker (a span field like `lifecycle = "true"`) on every span we want visible, and add a custom subscriber layer that emits enter/close events only for those.
- **Pros:** Single declarative knob per span; easy to audit.
- **Cons:** Custom subscriber layer is real maintenance cost; adds a v5-specific tracing extension that future contributors must understand. The same outcome is achievable with one inline `info!`/`debug!` per span and zero new abstractions.
- **Why not chosen:** Not enough leverage to justify a new tracing-subscriber layer; the explicit-event approach scales linearly with intent and doesn't introduce a new mechanism.

### Alternative 3: Fix only the spans that bit us today (lifeguard, tool, integrator)
- **Description:** Skip the broad visibility sweep; only address the three spans the most recent e2e flagged.
- **Pros:** Smaller PR, less surface area to review.
- **Cons:** Punts the same issue forward. Every future stage that lands without explicit lifecycle events will be silently invisible until someone tries to debug a failure that happens inside it. The fix per-span is mechanical (one `debug!` line); doing them as a sweep is cheaper than doing them one at a time at incident-response speed.
- **Why not chosen:** The task today is "comprehensive telemetry," not "patch the three spans that bit us." Doing the sweep avoids future re-prosecutions of this same bug.

### Alternative 4: Move all instrumentation to manual `info_span!` blocks instead of `#[instrument]`
- **Description:** Each function opens a span explicitly with `let _enter = info_span!(...).enter();` and emits a `info!` inside the same scope.
- **Pros:** Removes the implicit "attribute that does nothing on success" footgun.
- **Cons:** Every function gets longer; scope-key inheritance via the attribute is lost; the existing `#[instrument]` attributes work fine when they're used by an internal log macro.
- **Why not chosen:** The pattern that works (`#[instrument]` + one inline log macro) is mechanically cheaper and reads better than the manual-span alternative. The attribute is the right tool; the missing piece is the inline event.

## Technical Considerations

### Dependencies

No new crate dependencies. The work is entirely within existing tracing/tracing-subscriber surface area.

### Performance

- Event volume increase: roughly +1 event per `#[instrument]` function per call. Empirically the python-api run produced ~600 events.log lines for a 213s run; this proposal adds order-of ~2-5x more events on a similar run (per-tool calls dominate). At ~500 bytes/event JSONL, an additional 10-50 KB per agent iteration is acceptable for the diagnostic value.
- Hot paths (per-iteration tool dispatch, per-action lifeguard checks) emit at `debug` not `info`, so production runs at `-l info` see only the lifecycle-level events. The verbose telemetry is opt-in via `-l debug`.
- No tokio-runtime impact; events are non-blocking via `tracing-appender`.

### Security

- Tool args (paths, patterns, command bytes) on the success path go through the same secret redaction the existing error-path events use. The `command_chars` field on `tool.bash` records LENGTH and a TRUNCATED preview, never the full command.
- Reviewer's `ac_results.evidence` may contain code snippets. The existing `telemetry::transcript::redact_paths` matches **only against caller-supplied patterns** (verified by reading `crates/telemetry/src/transcript/render.rs`); it does NOT have built-in secret-substring detection for `password`/`token`/`secret`/`api_key`. The reviewer's evidence field gets the same redaction treatment as the implementer's transcripts: caller-supplied `path_deny_patterns` are applied, nothing more. Designing a generic secret-substring redaction is out of scope for this doc; tracked as an Open Question. The wiring of `path_deny_patterns` into `ReviewerDeps` (which today does not carry it; only the implementer's dispatcher does) is part of Phase 5 and is the only Reviewer-side dep change.
- No headers or API keys are added to any span. The existing `llm.anthropic` rule (NEVER tool schemas, ToolCall.input, headers, or API key) is unchanged.

### Testing Strategy

- One snapshot test per phase, exercising the new visibility in isolation against a wiremock-or-tempdir scenario.
- A meta-test in `crates/telemetry/tests/events_log_contract.rs` enumerates the full set of "must-be-visible" span names and required fields. A future change that removes or renames a span fails this test.
- The contract test uses the real `compose()` subscriber pipeline, not a stripped-down test subscriber — that's the whole point of the Phase 1 keystone. The harness writes to a tempdir's `events.log` and re-parses the JSONL.
- Failure messages from the contract test name the missing event explicitly: "expected an event with `name=tool.read` and field `path` but found N events on span `tool.read` and none had `path`." This UX is mandatory; a generic "assertion failed" failure is unhelpful when the contract has 30+ spans.
- Coverage: every phase's snapshot test is a positive assertion. We do NOT add a "no extra events" assertion (too brittle); the test asserts presence, not exclusivity.

### Rollout Plan

The work is unobservable from the outside (no protocol or schema break). Each phase ships as its own commit / PR. Verification is `otto ci` plus a `bin/e2e python-api --build` to confirm the new fields appear in the resulting events.log. No staged rollout, no feature flag.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Events.log volume balloons enough to slow grep / search | Med | Low | Hot-path events at debug, not info. Production runs (`-l info`) unaffected. |
| Snapshot tests get brittle as fields are added | Med | Med | Contract test asserts presence of required-fields only; new fields are additive. Test fails only if a required field disappears. |
| Reviewer per-AC parsing fails on irregular LLM output | Med | Low | Fall back to today's freeform `summary` path with a `warn!`. The structured payload is best-effort; the FSM transition still happens on the boolean `accepted`. |
| `git patch-id` compute on every Bundle adds latency | Low | Low | Single `git show \| git patch-id` invocation per bundle; sub-millisecond on typical scopes. Oversize-cap fallback (default 1 MiB) protects against runaway diffs. |
| `agents::dispatch` becomes the new emission site for "implementer produced bundle" — does any external consumer grep for `loopr::daemon::context`? | Low | Low | The contract test asserts only the new emission. A workspace-wide grep for `"target":"loopr::daemon::context"` + `"implementer produced bundle"` confirms no internal consumer depends on the old shape; external consumers (TUI/dashboards) do not exist yet. |
| Director restart_count + restart_reason recording races with the restart loop | Low | Low | `span.record()` on the existing span is the standard pattern (already used by `integrator.integrate` for `phase`). |
| The bin/e2e script's "Tick landed at 40s — goal complete" exit condition doesn't change | n/a | n/a | Out of scope for this doc. Surfaced as a separate observation; the e2e exit-policy belongs in a different design. |

## Open Questions

- [ ] Should `tool.bash`'s `command_chars` preview length be tunable per-target (different security postures want different preview sizes)? Default 256 chars is the proposed value.
- [ ] Should the per-Plan summary event in Phase 8 also live under the `summary.md` digest, or only in events.log? Current digest already exists at `runs/<pid>/summary.md`; events.log emission is additive.
- [ ] Generic secret-substring redaction (e.g., regex match for `password=`/`token=`/`AKIA[0-9A-Z]{16}`) is currently absent from `telemetry::transcript::redact_paths`. Should we design that as a follow-up, or rely on caller-supplied `path_deny_patterns` to enumerate sensitive paths? Default assumption: defer; this doc does not propose adding it.
- [ ] Promoting `ac_results` into the domain layer (extending `Verdict::Accept` and `Verdict::ChangeRequested` variants with a `Vec<AcResult>` field, then rendering into `summary.md`) is the architecturally clean path the Architect's Round 1 hardest question pointed at. Tracked as a follow-up doc rather than expanded into this one. Default assumption: a future doc will work that question through; nothing in this doc blocks it.
- [ ] Phases 2-4 (Track A) are mechanical and parallelizable. Should they ship as one PR (mass sweep) or per-crate PRs? Recommend per-crate so each phase's snapshot test gates that crate's progress, but open to a single sweep if reviewer prefers.
- [ ] **Phase 5 implementation drift (post-implementation, 2026-05-09):** the prompt at `crates/context/prompts/agents/reviewer/system.pmt` asks the LLM for one Verdict JSON object — not per-AC verification — so the design doc's "parser extracts a `Vec<AcResult>` from the response" had no parseable input on the wire. The implementation synthesizes per-AC results from the parsed `Verdict` plus `Work.acceptance_criteria` (heuristic substring match on `ChangeRequested.reasons`). Honoring the design strictly requires a prompt change, which Phase 5 explicitly disallows ("events-only, no domain change"). Decide whether to (a) amend Phase 5 to acknowledge synthesis as the canonical path, or (b) author a follow-up doc that widens the prompt and lifts the parser to parse real per-AC output.
- [ ] **Phase 7 implementation drift (post-implementation, 2026-05-09):** the design specifies `director.accept_bundle` AND `director.reject_bundle` `#[instrument]` functions in `agents::director`. `DirectorAction` does not currently have a `RejectBundle` variant — the Director's only Bundle-touching action is `accept_bundle`; rejections are sourced from the Reviewer's `Verdict::Reject` / `Verdict::ChangeRequested` (already covered by Phase 5/6 events) or from `OverrideWork(target=Failed/Abandoned)` (a Work-level action, not Bundle-level). The implementation shipped `director.accept_bundle` only. Decide whether to (a) widen `DirectorAction` with a `RejectBundle` variant in a follow-up doc and add the span then, or (b) amend Phase 7 to acknowledge that Bundle rejections are observed via Phase 5's reviewer events and the Director only emits accept events.
- [ ] **Phase 8 deferred fields (post-implementation, 2026-05-09):** the Work summary lacks `total_iterations`, `lifeguard_fires`, and `director_override_count` — these are agent-runtime counters local to `run_implementer`'s loop and the Director's spawner state, not store-resident. Surfacing them would require either domain changes (add iteration/lifeguard counters to `Bundle` or `Work`) or threading counts through `transition_and_persist_work`'s 17+ call sites. The Plan summary lacks `total_input_tokens`, `total_output_tokens`, `total_cost_usd` — these live on `MeteredLlmClient`'s `ProcessSnapshot` and would require threading a snapshot handle into `transition_and_persist_plan` (or splitting summary emission off from the FSM helper). All five fields are tractable in a follow-up; the Phase 8 commit notes the deferral inline with file references.

## References

- Today's e2e python-api run with the post-temperature-fix verification: `/tmp/loopr/e2e/python-api/20260509-095044/`
- Spot-check that confirmed the gap: grep on `~/.local/share/loopr/sessions/20260509-095045-2/.../events.log` for the documented span names returns 0 hits across `integrator.integrate`, `tool.<name>`, `lifeguard.check_action`.
- The 2026-04-24 instrumentation sweep: `docs/design/2026-04-24-instrumentation-sweep.md`
- Per-crate Instrumentation contracts: `crates/{telemetry,agents,tools,integrator,worktree,decomposer,llm}/CLAUDE.md`
- Telemetry subscriber composition: `crates/telemetry/src/subscriber.rs::compose`
