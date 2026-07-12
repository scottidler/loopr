# Design Document: Verified Swarm — executed checks, evidence records, and the operator surface

**Author:** Scott A. Idler (drafted with Claude)
**Date:** 2026-07-11
**Status:** Implemented (code Phases 1-19 landed + verified on branch `v5`; live bookends Phase 0 spike and Phase 20 real-target gate remain, and ACs 6 and 9 are live-pending on them)
**Review Passes Completed:** 5/5 (+ review panel + consensus round, all findings dispositioned)
**Crates touched:** domain, derive, store, llm, agents, context, decomposer, integrator, tools, worktree, ipc, telemetry, loopr

## Summary

v5 has the harness half of the verified-swarm design (typed FSMs, bounded loops, scoped commits, OCC persistence, crash recovery) and is missing the verification half: nothing re-executes work before merge, verdicts and evidence are not persisted, model tiers are not wired to workers, and the operator cannot watch a run. This doc builds all of it: the verification spine, the defect sweep from the 2026-07-11 four-agent code audit, the babysitting surface, and a real-target gate at the end. When this doc is Implemented, loopr runs against real repos and every Tick carries executed proof.

## Problem Statement

### Background

- Inspiration: Nate B Jones's verified-agent-swarm recipe (https://www.youtube.com/watch?v=suY66oTDn0s, vault note `notes/claude-fable-5-bossed-20-cheap-ai-agents-the-whole-site-cost-8.md`). Core ingredients: tiered model routing (frontier boss, cheap workers), independent checkers that re-execute instead of trusting self-reports, no rank above verification, standards enforced every round.
- 2026-07-11 audit: four parallel read-only review agents covered all 13 crates (every source file). Findings converge: harness solid, verification absent, plus a defect list with file:line citations. This doc is the response to that audit; the audit reports are the research brief.
- 2026-07-11 review panel (Architect | Staff Engineer): both reviewers verified the audit citations against code ("100% accurate"), answered the two open questions (now in Resolved Decisions), and surfaced the accept-site gate hole folded into Phase 11.
- Ground truth on usage: loopr has never run against a real repository. Every session under `~/.local/share/loopr/sessions/` targets a test harness or a `/tmp` scaffold. First gate passed 2026-05-30 on a scaffolded toy; last commit 2026-06-11.

### Problem

The pipeline's only quality gate is an un-tooled LLM opinion:

- `run_reviewer` is one LLM turn over a (possibly truncated) diff string. `ReviewerDeps` has no executor (`crates/agents/src/reviewer.rs:87-110`). The Implementer's `claims: ["tests pass"]` render into the prompt as trusted metadata.
- The Director accepts Bundles from status tables alone; it never sees the diff, verdict text, or claims (`crates/context/prompts/agents/director/user.pmt:7-23`). And the accept SITE is status-only: `spawner.rs` matches `BundleStatus::Reviewed => {}` with no evidence read. Rubber stamp, twice over.
- Integrator validation exists but ships empty and optional (`crates/integrator/src/config.rs:49`, `crates/loopr/src/config.rs:54`). When empty, the phase is skipped entirely.
- Verdict reasons evaporate when the review future returns (`crates/domain/src/verdict.rs:11-16`); `Bundle.verification` is a free-text String. No record distinguishes a self-reported claim from an executed check.
- FSM enforcement stops at the in-memory `transition()` method; `Store::update` checks OCC only, so any code path can persist an illegal status jump (`crates/store/src/works.rs:206-241`).
- Model tiers exist (`crates/llm/src/tier.rs:20-57`) but Implementer and Reviewer share one model; the cheap-worker | expensive-checker economics are unconfigurable.
- Operator surface for a live run does not exist: one-shot IPC clients, discarded events, 15s idle timeout that would kill any subscriber, thin `daemon status`, no work-level abort, no concurrency cap.
- Plus a poison-defect list (Phases 1-6) that would corrupt or kill real runs: action-key rewrite corrupts written files, one 429 kills a Work, cost brake prices with the wrong model, shutdown spawn-leak, client startup wait shorter than the daemon startup budget.

Because the gate is an opinion, real work was never trusted to it. Because nothing runs, the earned-features doctrine has no fuel. That is the stall.

### Goals

- Configured reviewer checks gate Accept in code; the LLM never overrides an exit code. Integration validation is mandatory by default: no Tick without executed green validation. (Owner ask, 2026-07-11: "build it all")
- Accepting a Reviewed Bundle requires persisted evidence (latest Review record: Accept, zero red checks), enforced at the accept site, not in a prompt.
- Reviews, check runs, and per-criterion AC results persist as records: the audit trail exists.
- FSM legality enforced at the store chokepoint, not by convention.
- Per-role model routing wired end-to-end (cheap implementer | expensive reviewer/director), opt-in per target.
- The defect sweep from the 2026-07-11 audit lands (all poison items).
- Operator can watch, cap, and intervene in a live multi-plan run.
- Final gate: loopr completes real work on a real repo, with reviewer checks AND validation configured and green.

### Non-Goals

Parked with revisit conditions (not silently dropped; each has a trigger):

- **Constitution mechanism** (per-Plan standards injected into every role). Revisit: after ≥3 real-repo runs show prompt-override churn.
- **Dispute escalation tier** (worker appeals a checker verdict). Revisit: first observed false-reject on a real run.
- **Sandbox hardening** (heavy lane bwrap, narrowing `--ro-bind / /`, file-secret denies). Revisit: before pointing loopr at any repo Scott does not own. Until then: own code, own machine.
- **Merge-to-main promotion.** Ticks land on `loopr/plan-<id>`; the human merges. This is the "no rank above verification" final gate for now. Revisit: after the real-target gate passes and manual merges feel like toil.
- **Spec/Phase hierarchy, JSONL compaction, ID widening, per-id store locking, TUI.** Revisit per deferred-roadmap triggers.

## Proposed Solution

### Overview

Four workstreams, ordered deterministic-cheap -> LLM-expensive:

1. **Defect sweep** (Phases 1-6): mechanical fixes, no design risk, de-risks everything after.
2. **Verification spine** (Phases 7-14): evidence records -> structured AC -> store-level FSM enforcement -> reviewer executed checks -> persisted reviews + accept gate -> validation-by-default -> model routing -> programmatic scope enforcement.
3. **Operator surface** (Phases 15-19): concurrency brake, fat status, `loopr watch`, intervention verbs, branch reaping.
4. **Real-target gate** (Phase 20): zero new features; run it for real.

### Architecture

The pipeline shape is unchanged: Plan -> Work DAG -> Implementer -> Bundle -> Reviewer -> Review -> Director accept -> Integrator -> Tick. What changes is what each arrow requires:

- Implementer -> Bundle: `bundle.paths` checked against `work.files` in code at propose time.
- Bundle -> Review: Reviewer runs a check suite (subprocess, exit codes) in a checkout of the bundle head before the LLM turn; Accept is code-gated on green.
- Review: persisted record with structured reasons and per-criterion AC status; CheckRun executions persisted alongside.
- Director accept: the accept site (`spawner.rs`) refuses `Reviewed -> Accepted` unless the persisted latest Review is Accept with zero red checks. The Director prompt additionally carries the evidence, but the gate is code.
- Integrator -> Tick: validation commands execute by default, each persisted as a CheckRun; red -> IntegrationFailed, no Tick.
- Every status write: store validates the FSM edge + role.

### Data Model

New records (all `#[derive(Record)]`, own JSONL collections). Naming: the persisted record of a review round is a `Review`; its outcome field reuses the existing `domain::Verdict` enum (`verdict.rs:28`), avoiding the name collision the panel flagged.

```
CheckRun {
    id: CheckRunId,
    bundle_id: BundleId,
    work_id: WorkId,
    command: String,          // as executed
    exit_code: i32,
    output_digest: String,    // sha256 of combined output
    output_excerpt: String,   // tail, capped
    executor: Role,           // Reviewer | Integrator
    duration_ms: u64,
    created_at, updated_at,
}

Review {
    id: ReviewId,
    bundle_id: BundleId,
    round: u32,               // review round for this bundle
    verdict: Verdict,         // existing enum: Accept | ChangeRequested | Escalate
    summary: String,
    reasons: Vec<ReviewIssue>,   // existing type, now persisted
    criteria: Vec<CriterionResult>,  // per-criterion status, Phase 8
    check_run_ids: Vec<CheckRunId>,
    model: String,
    created_at, updated_at,
}
```

Changed (Phase 8, its own phase; ripples across decomposer/context/agents):

```
AcceptanceCriteria: Vec<Criterion>      // was Vec<String>
Criterion { id: u32, text: String }
CriterionResult { criterion_id: u32, status: CriterionStatus, evidence: Option<String> }
CriterionStatus { Unmet | Met }         // Waived dropped: nothing writes it yet; earn it
```

`Bundle.verification: String` stays for back-compat display; the truth moves to Review/CheckRun records.

### API Design

- `store`: `CheckRunsStore`, `ReviewsStore` (create/get/list-by-bundle). `WorksStore::update` and siblings gain FSM validation (see Phase 9 for shape).
- `agents`: `ReviewerDeps` gains `check_runner: Arc<dyn CheckRunner>` plus store handles for Review/CheckRun persistence (same in-crate pattern as its existing OCC Bundle updates; `run_reviewer` persists, the daemon does not wrap). `CheckRunner::run(checkout_path, commands) -> Vec<CheckOutcome>`.
- `llm`: no trait change; `complete_free(..., Some(model))` now receives per-role resolved models.
- `ipc`: `system.status` result fattened; new `events.subscribe` long-lived method; new `work.override` method; `plan.override` gains Abandoned.
- CLI: `loopr watch`, `loopr work override`, `loopr budget reset`.
- Config (`.loopr/config.yml`):

```yaml
agents:
  implementer:
    model: lightweight        # tier name or literal id; default = primary (today's behavior)
  reviewer:
    model: primary
    check-commands: []        # executed at bundle head before verdict; opt-in per target
integrator:
  validation-commands: []     # empty + require-validation=true -> daemon refuses to start
  require-validation: true
budgets:
  max-concurrent-implementers: 4
```

### Implementation Plan

21 phases (0-20), four groups. Each phase: one commit, otto ci green, independently committable. Deterministic/cheap first.

| # | Phase | Group | Model | Crates |
|---|-------|-------|-------|--------|
| 0 | Prove executed-check plumbing | spike | sonnet | none (config only) |
| 1 | Parse-layer fixes | defects | sonnet | agents |
| 2 | Director session fixes | defects | sonnet | agents |
| 3 | LLM retry + cost sink | defects | opus | llm, agents |
| 4 | Cost/budget attribution | defects | sonnet | telemetry, llm, loopr |
| 5 | Daemon lifecycle fixes | defects | opus | loopr, agents |
| 6 | Store/domain hardening | defects | sonnet | store, domain, loopr |
| 7 | Evidence records (Review, CheckRun) | spine | opus | domain, store |
| 8 | Structured acceptance criteria | spine | opus | domain, decomposer, context, agents, store |
| 9 | FSM at store chokepoint | spine | opus | store, domain, loopr |
| 10 | Reviewer executed checks | spine | opus | agents, context, tools, worktree, loopr |
| 11 | Persist reviews + deterministic accept gate | spine | opus | agents, store, context, loopr |
| 12 | Validation on by default + integrator evidence | spine | sonnet | integrator, loopr, store |
| 13 | Per-role model routing | spine | sonnet | llm, agents, loopr |
| 14 | Programmatic scope enforcement | spine | opus | agents, decomposer, tools |
| 15 | Concurrency + budget brakes | operator | sonnet | loopr |
| 16 | Fat status | operator | sonnet | ipc, loopr |
| 17 | `loopr watch` | operator | opus | ipc, loopr |
| 18 | Intervention verbs | operator | opus | ipc, loopr, domain, tools |
| 19 | Failure-path reaping | operator | sonnet | integrator, loopr, worktree |
| 20 | Real-target gate | gate | opus | none (runs, not code) |

#### Phase 0: Prove the executed-check plumbing with zero new code
**Model:** sonnet
- On a scaffolded rust target: set `integrator.validation-commands: ["cargo test"]` in `.loopr/config.yml`, run a plan end-to-end (exercises the shipped 2026-05-08-validation.md plumbing).
- Then set `validation-commands: ["false"]`, rerun, confirm IntegrationFailed and no Tick.
- **Success criteria:** green run produces a Tick with validation executed (events.log shows the command); red run produces `BundleStatus::IntegrationFailed` and zero Ticks.

#### Phase 1: Parse-layer fixes (agents)
**Model:** sonnet
- `normalize_action_key` (`crates/agents/src/parse.rs:137-139`): replace blind string `"action":` -> `"type":` rewrite with a JSON-aware rewrite (parse to Value, rename top-level discriminator only). Today it corrupts any file the implementer writes containing `"action":`.
- `parse_director_actions` (`crates/agents/src/director.rs:372-390`): add the same fence-strip + balanced-bracket extraction `parse_actions` and `parse_verdict` already have.
- Regression tests: implementer writes a file containing `"action":` verbatim; director response wrapped in ```json fences parses.
- **Success criteria:** named tests fail on old code (break-to-prove), pass on new; `cargo test -p agents` green.

#### Phase 2: Director session fixes (agents)
**Model:** sonnet
- Restart-budget reset off-by-one (`crates/agents/src/director.rs:699-710`): healthy-run reset must apply on the final-restart arm too.
- Operator-note loss (`director.rs:945-963`, `1040-1050`): only mark-read the notes actually rendered; notes beyond the render cap stay unread for the next round.
- **Success criteria:** test: session at max_restarts with ≥10 healthy iterations survives one more transient error; test: 9 unread notes -> 8 rendered+read, 1 still unread.

#### Phase 3: LLM retry policy + cost sink hygiene (llm, agents)
**Model:** opus
- Shared bounded-retry helper honoring `RetryableReason::RateLimited { retry_after }` + exponential backoff; wrap the `complete_free` call sites in implementer (`implementer.rs:268`), reviewer (`reviewer.rs:249`), director. Today one 429 kills a Work run; the typed retry taxonomy has zero consumers.
- `CostSink::append` (`crates/llm/src/metered.rs:77-86`): move sync file I/O off the async path (spawn_blocking or tokio fs).
- Validate `per_work_cost_cap_usd` at config load (negative/NaN rejected); fix the `(cap * 1e6) as u64` cast (`implementer.rs:46`).
- **Success criteria:** test with scripted 429-then-200 client: work completes; retry count bounded; config with negative cap fails load with a typed error.

#### Phase 4: Cost/budget attribution (telemetry, llm, loopr)
**Model:** sonnet
- Pass `usage.model` into `ProcessSnapshot::record_llm_call` (`crates/llm/src/metered.rs:185-197`, `crates/telemetry/src/digest/process.rs:88-110`); price per-call by actual model. Today Director Opus calls are priced at the config's single `llm.model` rate and unknown models price at $0: the budget brake reads the wrong gauge.
- Warn once per unknown model id (both the brake and `costs.jsonl` paths).
- Wire the dead pipeline counters (`plans_created`, `works_*`, `bundles_*`, `ticks_created` in `digest/process.rs:31-52`) at the `transition_and_persist_*` and Tick-persist sites.
- **Success criteria:** test: two-model run prices each call by its own model; unknown model emits one WARN and a nonzero-usage ledger row; process digest shows nonzero counters after a scripted e2e.

#### Phase 5: Daemon lifecycle fixes (loopr, agents)
**Model:** opus
- Shutdown spawn-leak: add the `shutting_down` entry guard to `spawn_implementer_for_work` (`crates/loopr/src/daemon/context.rs:489-529`) and a pre-spawn check in `promote_unblocked_siblings` (`context.rs:1037-1040`). Restores the documented drain invariant (`daemon.rs:833-852`).
- Startup wait mismatch: make the client socket-wait a `transport:` knob defaulting to the daemon startup budget (60s, `config.rs:98`) instead of the hard 3s (`transport.rs:87`); poll the startup-error sentinel while waiting so real failures surface early.
- Delete duplicate `#[instrument]` attributes (`context.rs:482-488`, `744-750`, `integration.rs:33-39`); fix span-guard-across-await (`crates/agents/src/dispatch.rs:770` -> `.instrument(span)`).
- **Success criteria:** shutdown test: integrator completing during drain does not spawn into a drained pool and `Arc::try_unwrap` succeeds; client connecting to a daemon with a 10s-slow reconcile succeeds; capture-layer test asserts exactly one `daemon.spawn_implementer` span per spawn in events.log.

#### Phase 6: Store/domain hardening (store, domain, loopr)
**Model:** sonnet
- `Store::open` version-mismatch paths (`crates/store/src/store.rs:96-111`): `close()` the just-opened AsyncStore before returning Err; give unparseable `.version` its own variant carrying the raw string.
- `Tick::new` 1:1 bundles/merge_commits invariant: `debug_assert_eq!` -> `Result` (`crates/domain/src/tick.rs:62-68`).
- `record.get` oversize response: length-check before send; typed RpcError instead of a dropped connection (`crates/loopr/src/transport/handler.rs:463-513`).
- **Success criteria:** tests for each: bad `.version` file yields the new variant and no reactor stall; mismatched Tick construction returns Err; oversized record.get returns a typed error, connection survives.

#### Phase 7: Evidence records (domain, store)
**Model:** opus
- `CheckRun` and `Review` records per Data Model; `id_type!` ids; `CheckRunsStore` + `ReviewsStore` with create/get/list_by_bundle.
- `Review.criteria` lands as an empty Vec until Phase 8 defines `CriterionResult` writers (field present, typed, unwritten for one phase; documented in the field docstring).
- Seam tests: round-trip serde per record; store integration test per collection.
- **Success criteria:** round-trip + store tests green; `cargo check --workspace` green with zero consumers changed (proves the phase is additive and independently committable).

#### Phase 8: Structured acceptance criteria (domain, decomposer, context, agents, store)
**Model:** opus
- `AcceptanceCriteria` -> `Vec<Criterion>` (id + text). Back-compat: custom deserializer accepts the old `Vec<String>` shape (all entries become Criteria with sequential ids).
- Writers: the decomposer mints criteria ids at decompose time; the Reviewer produces `Vec<CriterionResult>` per round (persisted on the Review record, Phase 11 wires the write); nothing else mutates criteria text.
- Ripple sites updated in this phase: `context/src/implementer.rs:149`, `agents/src/reviewer.rs:430` (the fuzzy AC-event matcher now keys on criterion ids instead of substring matching), `decomposer/src/decompose.rs:532`, summary renderers, tests.
- `CriterionStatus` is `Unmet | Met` only. Waived is dropped until something writes it.
- **Success criteria:** old-format works.jsonl rows deserialize (back-compat test); reviewer AC events carry criterion ids, not fuzzy word-matches; full workspace green.

#### Phase 9: FSM enforcement at the store chokepoint (store, domain, loopr)
**Model:** opus
- `WorksStore::update`, `BundlesStore::update`, `PlansStore::update` gain FSM validation: after the OCC read, validate `current.status -> incoming.status` via the derived `validate_transition`/`validate_override` tables. Caller passes `Role` + override intent. Illegal jump -> typed `StoreError::IllegalTransition`.
- Same-status writes (field-only updates) bypass edge validation.
- Audit all callers; production paths already use `transition()` so this should be behavior-neutral; test-only direct assignments updated.
- Reconcile discipline: startup/crash-recovery writes go through `validate_override` with the daemon's role. If reconcile needs an edge the tables lack, the fix is an explicit override table entry in domain, never a store-level bypass flag.
- **Success criteria:** test: persisting a Work whose status jumped an illegal edge returns IllegalTransition; full scripted e2e still green (proves behavior-neutral for legal flows).

#### Phase 10: Reviewer executed checks (agents, context, tools, worktree, loopr)
**Model:** opus
- `CheckRunner` trait in agents; production impl shells the configured `reviewer.check-commands` via the existing tools spawn infrastructure (heavy lane semantics, existing subprocess timeout + bounded output).
- Execution site: the Work's implementer worktree, whose lifetime is extended to outlive review (cleanup deferred until the Bundle reaches a terminal state) so caches stay warm: cold-recreating a worktree per round means full rebuild + dep fetch and trips timeouts. Fallback only if the worktree is missing (crash): recreate ephemeral from the bundle branch, flagged in the CheckRun excerpt.
- Run checks BEFORE the LLM turn; append command + exit code + output tail to the reviewer prompt as executed evidence (fenced with the existing dynamic-fence helper).
- Code-gate: an `Accept` verdict while any check is red is overridden to ChangeRequested before the FSM transition (`reviewer.rs:212-220`), with a synthesized ReviewIssue naming the red command. The LLM never gets the final word over an exit code.
- Failure taxonomy: a SPAWN-level system error (command not found, spawn failure) is an environment problem, not a code problem -> Work goes `Blocked` with `blocked_reason` naming the command; no LLM turn, no ChangeRequested. A clean spawn with nonzero exit is a code signal -> ChangeRequested as above. Asking the LLM to fix infra burns `max_work_attempts` at max cost.
- Persist a `CheckRun` record per command (via the deps store handles).
- Fix the noop-bundle path reading file contents from `deps.target` instead of the bundle's checkout (`reviewer.rs:165`, `681-716`).
- Empty `check-commands` + this phase: checks skipped, verdict proceeds LLM-only (the merge gate arrives in Phase 12; reviewer checks stay opt-in per target).
- Environment-broken checks that pass spawn but fail on toolchain state still bound: `max_work_attempts` + the Director's pattern tracker (identical failures trip NoProgress); the output tail in the synthesized issue is what lets the Director or operator spot it.
- **Success criteria:** test with scripted LLM returning Accept + a failing check command: Bundle lands ChangeRequested with the synthesized issue; CheckRun records persisted with real exit codes; spawn-error test lands the Work Blocked with no LLM call; noop-bundle review reads from the bundle checkout (test).

#### Phase 11: Persist reviews + deterministic accept gate (agents, store, context, loopr)
**Model:** opus
- `run_reviewer` persists a `Review` record (round = prior review count for the bundle + 1) referencing its CheckRuns and carrying `Vec<CriterionResult>`. Crash between review-persist and bundle-transition is benign: the reconcile sweep re-reviews, which appends a new round; review rows are append-only history, no dedup needed.
- **Accept-site gate (panel must-fix #1):** the daemon's accept path (`spawner.rs`, today `BundleStatus::Reviewed => {}`) refuses `Reviewed -> Accepted` unless the persisted latest Review for the bundle is `Accept` with zero red referenced CheckRuns. Missing, stale (round mismatch), or red evidence -> no accept; the Bundle is flagged to the Director as evidence-broken. The prompt is not the gate; this is.
- Rejected-bundle retry feedback (`StateSummary.rejected_bundle_reason`) assembled from the persisted Review's structured reasons, not the one-line `Bundle.verification` string. Cap rendered reasons; full record remains on disk.
- Director state summary (`context::build_for_director`) includes last verdict kind + red-check count per Reviewed bundle: the Director sees evidence, and the code gate backstops it.
- **Success criteria:** test: a Bundle hand-set to Reviewed with no persisted Review is NOT accepted and the Director summary flags it; test: rejected bundle -> retried implementer prompt contains the structured reasons; reviews.jsonl has one row per review round with criterion results.

#### Phase 12: Validation on by default + integrator evidence (integrator, loopr, store)
**Model:** sonnet
- `require-validation: true` default. `validation-commands` empty + require-validation -> daemon REFUSES TO START for that target with a config error naming the knob. A config problem is not a per-Bundle terminal failure; fail closed at startup, before any LLM spend.
- Red validation (non-zero exit) keeps today's semantics: Bundle -> IntegrationFailed, no Tick, Director sees it.
- **Integrator persists a `CheckRun` per validation command** (executor: Integrator), success and failure both (`validation.rs:35` today returns `Ok(())` with no record). "Every Tick carries executed proof" must be literally true: the Tick's bundles reference CheckRuns.
- Operator escape hatch: explicit `require-validation: false` (startup WARN every boot).
- No autodetection magic (config drives behavior): `loopr init` template documents the knob with commented examples (`cargo test`, `otto ci`, `npm test`).
- **Success criteria:** daemon start on a target with no validation config fails with the named error; with `["cargo test"]`: Tick as before AND check-runs.jsonl has an Integrator-executed row per command; with `["false"]`: IntegrationFailed, no Tick, red CheckRun persisted; with `require-validation: false`: legacy behavior + startup WARN.

#### Phase 13: Per-role model routing (llm, agents, loopr)
**Model:** sonnet
- `model: String` on `ImplementerConfig` and `ReviewerConfig` (tier name or literal id), resolved through the existing `ModelTiers` table in `resolve_model_tiers` (`crates/loopr/src/config.rs:284-288`).
- Pass `Some(model)` at the two `complete_free` call sites (`implementer.rs:268`, `reviewer.rs:249`). Decomposer keeps `llm.model`.
- Defaults are behavior-neutral: both roles default to today's `llm.model` (tier `primary`). The cheap-worker split (`implementer: lightweight`) is opt-in config, shown in the init template. Defaults that silently change fleet behavior are an infection; opt-in per target.
- **Success criteria:** unconfigured e2e: every costs.jsonl row carries the same model id as before this phase (asserted, not eyeballed); configured split shows implementer calls on lightweight and reviewer on primary in costs.jsonl; unknown tier name fails config load.

#### Phase 14: Programmatic scope enforcement (agents, decomposer, tools)
**Model:** opus
- `files` becomes required in the decomposition tool schema (`crates/decomposer/src/tool.rs:78`); empty-`files` Work rejected at validation with the existing retry path.
- `propose_bundle` compares `bundle.paths` against `work.files`: out-of-scope paths -> propose rejected with a typed reason fed back to the implementer (same shape as the force-propose guards; loop bounded by the existing lifeguard + attempt budgets). The Reviewer's scope criterion becomes defense-in-depth, not the only line.
- Scope-match semantics: a `files` entry is an exact repo-relative path, or a directory prefix when it ends with `/`. New files are in-scope when they match an entry (the check is against intended paths, not existence). `.loopr/**` remains always-excluded.
- Add the history/index-mutating git set to the default `BashDenylist` (`crates/tools/src/denylist.rs:280-304`): `add`, `commit`, `checkout`, `switch`, `reset`, `rebase`, `merge`, `cherry-pick`, `stash`. The only mutation path is the scoped dispatcher; read-only git (`log`, `diff`, `status`, `show`, `blame`) stays allowed.
- Update the implementer system prompt (`implementer/system.pmt:8`) and `action.rs:32-34` docstrings: they currently tell the model `commit_changes` does `git add -A`, which is false since scoped staging shipped.
- **Success criteria:** test: bundle touching a file outside `work.files` is rejected at propose with the typed reason; `bash: git commit -m x` denied; decomposition omitting `files` fails validation and retries.

#### Phase 15: Concurrency + budget brakes (loopr)
**Model:** sonnet
- Global implementer semaphore: `budgets.max-concurrent-implementers` (default 4), permit acquired at the top of `spawn_implementer_for_work`, released when the implementer run returns (before reviewer spawn; reviewer and integrator are not semaphore-bound). Bounds N-plans × M-works LLM fan-out.
- `loopr budget reset` verb (IPC `budget.reset`): clears the one-shot `budget_event_sent` soft-pause (`context.rs:314`) so a budget-tripped daemon can resume after the operator raises the cap, without a restart.
- **Success criteria:** test: 6 ready works, cap 2 -> at most 2 InProgress concurrently; budget-tripped daemon resumes dispatch after `budget reset` + raised cap.

#### Phase 16: Fat status (ipc, loopr)
**Model:** sonnet
- `system.status` result gains per-plan rollups: works by state, bundles by state, director mode, attempt counts, cost-so-far (from the now-correct snapshot), stuck flags.
- `loopr daemon status` renders it; `--output json` for scripts. Read verbs stop auto-forking a daemon when no daemon is running (report "no daemon" instead, `lib.rs:118-134`).
- **Success criteria:** status on a mid-run daemon shows per-plan works-by-state + cost > 0; `loopr plans` on a quiet repo with no daemon does not fork one.

#### Phase 17: `loopr watch` (ipc, loopr)
**Model:** opus
- New `events.subscribe` long-lived IPC method: exempt from the server read-idle timeout (heartbeat frames), replays nothing (live tail), sends a typed gap marker on `RecvError::Lagged` (`server.rs:318`).
- `loopr watch [--plan <id>]`: renders the DaemonEvent stream (work/bundle/plan transitions, budget events, director mode changes) as one line per event; exits on daemon shutdown.
- Bump `EVENTS_CAPACITY` (`context.rs:60`) to something sane for a slow terminal (e.g. 1024).
- Client disconnect (ctrl-c, dropped socket) tears down the server-side subscription task cleanly: write error -> unsubscribe -> task exits. No leaked forwarders.
- **Success criteria:** watch survives >60s idle (heartbeats); events for a full plan lifecycle render in order; forced lag produces a visible gap marker, not silence; killing the client leaves no orphaned server task (asserted via task count or log).

#### Phase 18: Intervention verbs (ipc, loopr, domain, tools)
**Model:** opus
- `work.override` IPC method + `loopr work override <id> --status <target>`: operator-role FSM override (Blocked -> Ready retry, InProgress -> Blocked abort). Abort requires keyed cancellation: JoinSet has no per-task abort by key, so the daemon keeps a `work_id -> AbortHandle` map (populated at spawn, cleaned at join); abort fires the handle and the reconcile/completion path stamps `FailureReason::OperatorAbort`.
- **Cancellation-safe subprocess reaping (panel must-fix #5):** `tools/src/spawn.rs` does not set `kill_on_drop`, so a task abort mid-tool-call orphans the subprocess tree today (`integrator/validation.rs:60` sets it; the tools path does not). This phase adds `kill_on_drop(true)` plus drop-path process-group kill to the tools spawn path so an AbortHandle fire reaps every live child. Immediate abort is only acceptable with this in place.
- `plan.override` gains Abandoned (noted missing at `cli.rs:189-196`).
- **Success criteria:** aborting an InProgress work mid-bash-tool-call kills the task AND its subprocess tree (asserted: no surviving pid), lands Blocked with `OperatorAbort`; overridden-to-Ready work re-dispatches; plan abandon reaches the terminal state and the Director exits.

#### Phase 19: Failure-path reaping (integrator, loopr, worktree)
**Model:** sonnet
- Extend post-Tick cleanup and startup reconcile to delete `loopr/wk-*` branches + worktrees for `IntegrationFailed` and Abandoned/terminal-failed Works, not only Done (`crates/integrator/src/lib.rs:594-610`, `crates/loopr/src/daemon/startup.rs:246`).
- Retention interacts with Phase 10's warm-cache extension: worktrees for Works in flight (including awaiting/under review) are retained; reaping happens ONLY at terminal states. A failed review's worktree therefore survives until its Work goes terminal: that is the evidence an operator inspects for env failures.
- **Success criteria:** e2e forcing an IntegrationFailed: after the Work goes terminal, its branch and worktree are gone; reconcile on restart reaps any missed ones; a Work in InReview retains its worktree (test).

#### Phase 20: Real-target gate (zero new features)
**Model:** opus (for run-babysitting judgment, not code)
- Re-run the `python-api` e2e target (the one that broke v5 in April; its Tier-1 fixes shipped but were never re-tested against it) with validation-commands AND reviewer check-commands set (non-empty reviewer checks are a gate requirement, per panel).
- Run loopr against a real scottidler repo with a real backlog item. Target: owner picks at gate entry and it is recorded here in this doc before the phase starts (decision owner: Scott; not a build blocker for Phases 0-19).
- Watched via `loopr watch`; every failure -> a filed issue or a follow-up design doc; no silent patching.
- **Success criteria:** python-api plan completes with a Tick whose reviewer checks and validation executed green (CheckRun rows for both executors); a real-repo plan produces a merged, validated integration branch whose diff Scott merges to main by hand; costs.jsonl shows tiered attribution (≥2 models).

## Acceptance Criteria

- [x] A Bundle cannot reach Accepted while any of its configured checks has exit code != 0: test proves an LLM Accept over a red check lands ChangeRequested. (Phase 10 `reviewer.rs::apply_code_gate`; `accept_over_red_check_is_overridden_to_change_requested`.)
- [x] A Bundle in Reviewed with no persisted Review record (or a red/stale one) cannot be Accepted: the accept-site gate refuses and flags it. (Phase 11 `spawner.rs` + `domain::decide_accept`; `accept_gate.rs` suite.)
- [x] With default config, a target without validation-commands cannot start a daemon, and an integration without executed validation cannot produce a Tick; every Tick's bundles reference Integrator CheckRun rows. (Phase 12 `validation_gate.rs` + `validation_wiring.rs`.)
- [x] `reviews.jsonl` and `check-runs.jsonl` exist after any reviewed run; a rejected bundle's retry prompt quotes the persisted structured reasons; review rows carry per-criterion results keyed by criterion id. (Phases 7/8/11. Shipped collections are `reviews`/`checkruns`, i.e. files `reviews.jsonl`/`checkruns.jsonl` — not `check-runs.jsonl` as originally written here.)
- [x] `Store::update` on any of works/bundles/plans rejects an FSM-illegal status jump with `IllegalTransition`. (Phase 9, all three update methods.)
- [ ] One e2e run shows implementer and reviewer calls on different models in costs.jsonl, each priced by its own model. (Code-complete: Phase 4 per-model pricing + Phase 13 per-role routing wired; **live-pending** — verified on the Phase 20 real-target run.)
- [x] With cap N, no more than N Works are InProgress concurrently; `loopr watch` renders a full plan lifecycle and survives idle; aborting an InProgress Work reaps its subprocess tree. (Phase 15 semaphore test, Phase 17 idle-exempt/render tests, Phase 18 `abort_reaps_subprocess_tree_no_surviving_pid`; full *live* lifecycle render exercised on the Phase 20 run.)
- [x] Terminal-failed Works leave no branches or worktrees behind after reconcile. (Phase 19 e2e reap test + startup sweep.)
- [ ] The python-api e2e target completes end-to-end with reviewer checks and validation green. (**Phase 20 live gate** — owner-deferred to a dedicated session.)

## Resolved Decisions

- 2026-07-11 (Scott): scope is everything from the 2026-07-11 audit synthesis: verification spine + full defect sweep + operator surface + real-target gate. Constitution, dispute escalation, sandbox hardening, merge-to-main stay parked (they were suggested as deferrals; Non-Goals records the triggers).
- 2026-07-11 (audit consensus, all four reviewers): executed checks gate in code, not prompts; the LLM never overrides an exit code.
- 2026-07-11 (panel, Architect + Staff Engineer, converged): reviewer check-commands default empty (opt-in per target); integrator validation required by default. Rationale: the merge gate is universal and fails closed at startup before LLM spend; a full check suite per review round is per-target economics. Goal wording updated accordingly; non-empty reviewer checks required at the Phase 20 gate.
- 2026-07-11 (panel, converged): work abort is immediate (AbortHandle), contingent on cancellation-safe subprocess reaping in the tools spawn path (Phase 18). Waiting out a stalled tool call defeats the point of an operator abort.
- 2026-07-11 (panel must-fix, verified against code): the Director accept site is status-only (`spawner.rs` `Reviewed => {}`); prompt evidence is not a gate. Deterministic accept-site guard added to Phase 11.
- 2026-07-11 (panel + author): persisted review record named `Review` (existing `domain::Verdict` enum reused as its outcome field), resolving the name collision the panel flagged; more literal than the panel's `ReviewVerdictRecord` suggestion.
- 2026-07-11 (author, on panel finding): `CriterionStatus::Waived` dropped; nothing writes it. Earn it when an operator waive verb exists.
- 2026-07-11 (author pushback, consensus sought): only SPAWN-level system errors map to Blocked; nonzero exit codes stay ChangeRequested. A failing test cannot be reliably classified env-vs-code from the exit code; the bounded loop + Director pattern tracker + output tail cover the residual.
- 2026-07-11 (author disposition on panel #11): the Phase 20 real target is the owner's pick, recorded in this doc at gate entry. Pinning a repo now would invent a backlog item; the pick blocks nothing before Phase 20.
- 2026-07-11 (consensus round, closed): panel accepted both modified fixes (`Review` record naming verified collision-free; Waived dropped) and both pushbacks (env-vs-code split at the spawn boundary; Phase 20 target owned-not-pinned). No remaining objections from either reviewer.

## Alternatives Considered

### Alternative 1: Reviewer stays LLM-only; all executed checks live in the Integrator
- **Description:** keep review as opinion; make post-merge validation the sole executed gate.
- **Pros:** smaller diff; one execution site.
- **Cons:** failures surface after merge instead of before; the Reviewer keeps trusting self-reports; rework loop is longer and costlier (merge -> fail -> reject -> re-implement vs reject at review).
- **Why not chosen:** the video's core mechanism is the checker re-measuring the worker's output at review time. Post-merge-only keeps the "trust the worker" hole open through the whole review stage.

### Alternative 2: Enforce FSM in a wrapper type instead of the store
- **Description:** newtype `Fsm<Work>` whose only mutator is `transition()`.
- **Pros:** compile-time-ish; no store signature change.
- **Cons:** does not stop a caller constructing a fresh record with an arbitrary status and persisting it; store is the actual chokepoint; wrapper adds friction everywhere.
- **Why not chosen:** the store is the last write barrier; validation there catches every path including reconcile and tests.

### Alternative 3: Autodetect validation commands (cargo/npm/pytest sniffing)
- **Description:** integrator guesses the check suite from repo contents.
- **Pros:** zero-config.
- **Cons:** magic that can't be made predictable; wrong guesses fail confusingly; violates "config drives behavior."
- **Why not chosen:** require-validation + explicit commands + a documented init template is boring and predictable. Autodetection can be revisited as a suggestion printed by `loopr init`.

### Alternative 4: Ephemeral review worktrees (recreate per round)
- **Description:** Reviewer checks always run in a fresh worktree created from the bundle branch.
- **Pros:** no lifetime coupling between implementer and reviewer.
- **Cons:** cold `target/` / `node_modules` per round: full rebuild + dep fetch, trips subprocess timeouts, burns compute (panel top risk).
- **Why not chosen:** extend the implementer worktree's lifetime through review (Phase 10); ephemeral recreate survives only as the crash fallback.

## Technical Considerations

### Dependencies
- Internal only; no new external crates expected (CheckRunner reuses tools spawn infra; records reuse derive/store machinery). Single repo, no cross-repo blast radius. Ship order within the repo is the phase order; Phases 7-9 must land before 10-12 consume them.

### Performance
- Reviewer checks add a build/test execution per review round: that is the point. Bounded by existing subprocess timeout + output caps; warm-cache worktree reuse (Phase 10) keeps rounds incremental. Global implementer semaphore (Phase 15) is the fan-out brake.
- Store FSM validation adds one in-memory table lookup per update: negligible.

### Security
- Phase 14 closes the bash commit bypass; scope enforcement becomes code. Sandbox hardening explicitly parked (Non-Goals) with a trigger.
- No new secret surfaces; check commands come from operator-owned config, same trust level as validation-commands today.

### Testing Strategy
- Per-phase named regression tests, break-to-prove for the defect fixes (Phases 1-6).
- Seam tests for new records (Phase 7) per repo rule.
- Scripted-LLM e2e extended: red-check Accept override, evidence-missing accept refusal, validation-refusal, scope rejection, tiered-model attribution, abort-reaps-subprocess.
- Phase 20 is the live test.

### Rollout Plan
- Single repo, phase-per-commit on a v5 branch, otto ci green per phase, per-phase implementation notes. bump + tag at the end of each coherent group (post-6, post-14, post-19, post-20). No deployment surface; `cargo install --path crates/loopr` is the rollout.
- On Implemented: flip this doc's Status, update `docs/roadmap.md` (this doc absorbs deferred-roadmap items 1.4-adjacent validation-by-default, parts of 2.3/3.2/3.3 scope) and mark the absorbed entries.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Phase 9 store enforcement breaks a hidden illegal-transition path | Med | Med | full scripted e2e in the phase's gate; IllegalTransition error names the edge; reconcile paths audited in-phase |
| Reviewer checks make review slow/expensive on big targets | Med | Low | opt-in per-target commands; warm-cache worktree reuse; subprocess timeout + output caps already exist |
| require-validation default breaks existing toy targets | High | Low | that is the intent; startup error names the knob; explicit `false` escape hatch with WARN |
| Worktree lifetime extension (Phase 10) leaks disk on long runs | Med | Low | Phase 19 reaps at terminal states; in-flight retention is bounded by active Work count |
| 21 phases stall like the last roadmap | Med | High | phases are independently committable; the real-target gate is the finish line, not a new feature tier; owner runs `/how-to-execute-a-plan` per phase |
| AcceptanceCriteria migration breaks old JSONL | Low | Med | back-compat deserialization test in Phase 8 gate |

## Open Questions

None. (Two draft questions resolved by the 2026-07-11 review panel; see Resolved Decisions.)

## References

- Vault note: `notes/claude-fable-5-bossed-20-cheap-ai-agents-the-whole-site-cost-8.md` (Nate B Jones, https://www.youtube.com/watch?v=suY66oTDn0s)
- 2026-07-11 four-agent code audit (this session): domain/derive/store, agents/context/llm, decomposer/integrator/tools/worktree, loopr/ipc/telemetry reports.
- 2026-07-11 review panel synthesis (Architect via Gemini, Staff Engineer via Codex).
- `docs/roadmap.md`, `docs/deferred-roadmap.md`, `docs/three-tiers-of-broken-implementation.md`
- `docs/design/2026-05-08-validation.md`, `docs/design/2026-04-22-reviewer.md`, `docs/design/2026-04-22-stage-8-wiring.md`
