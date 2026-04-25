# Design Document: Instrumentation Sweep

**Author:** Scott A. Idler
**Date:** 2026-04-24
**Status:** Implemented
**Review Passes Completed:** 5/5
**Crates touched:** derive, telemetry, store, domain, llm, tools, worktree, ipc, context, decomposer, agents, integrator, loopr

> **Amendment 2026-04-24 (layout):** `docs/design/2026-04-24-loopr-layout.md` landed immediately before this sweep begins and changes the on-disk foundation this doc is written against. This doc's body still references `<target>/.loopr/runs/<run-id>/` paths and a single `run_id` identifier; those are superseded. Read the current shape as:
>
> - **`run_id` is gone.** It split into `session_id` (user-facing, long-lived, `YYYYMMDD-HHMMSS[-N]`, resumable) and `process_id` (`pc-<6char>`, one per OS process). Anywhere below that this doc writes "run-id" or "run_id," substitute whichever concept fits the context; "daemon run" and "client run" both mean a process of that role, keyed by `process_id`, belonging to some `session_id`.
> - **Paths moved to XDG.** `events.log`, `loopr.log`, and the per-Work fanout live under `$XDG_DATA_HOME/loopr/sessions/<session-id>/targets/<target-slug>/runs/<process-id>/`. Only `<target>/.loopr/records/` stays under the target (see Q4/Q5 below — that decision survives).
> - **Correlation already landed.** Q2's proposed `client_run_id` handshake field shipped as `client_session_id` on the daemon's `ipc.connection` span (Phase 6 of the layout doc). Phase 10 of this sweep no longer needs to ship it.
> - **Session fanout already landed.** `SessionFanoutLayer` ships (Phase 7 of the layout doc), routing events carrying `session_id` / `client_session_id` to `sessions/<session-id>/targets/<target-slug>/session-fanout.log`. This supersedes the "gated follow-on" notion for role-wise fanout in Q1 only to the extent that the Session fanout now aggregates daemon + client events per session; per-role fanout is still a gated follow-on.
>
> The Per-Crate Scope Fields table, the Log-File Topology table, and Phase 10 below have been revised to the new shape. Policy and phase ordering are unchanged.

## Summary

The workspace sits at 1.6% function-level instrumentation coverage (4 of ~250 public functions, not counting decomposer's one golden example), in violation of `rules/rust.md`'s mandatory "Function-level instrumentation" section and the newly-added `rules/log.md`. A first attempt at Stage 9 E2E failed inside the Implementer's lifeguard with `"same action repeated 3 times"` and the operator could not determine *which* action without restarting the daemon at `-l debug` and rerunning - burning API dollars. This doc plans a crate-by-crate sweep that adds `#[tracing::instrument]` to every non-trivial function per the rules, settles per-crate field conventions, and decides the log-file topology questions (per-agent-role log? daemon and client sharing?).

## Problem Statement

### Background

`rules/rust.md` has contained a detailed "Function-level instrumentation (mandatory)" section for the life of the v5 rebuild. It spells out levels (entry-level orchestrators → `info`, per-iteration helpers → `debug`, tight-loop helpers → `trace`), field discipline (`skip_all` + explicit `fields(...)`), and required scope fields (`work_id`, `plan_id`, etc.) that downstream `warn!`/`error!` emissions inherit automatically. `crates/decomposer/src/decompose.rs:56` implements this pattern correctly and served as the pattern for the rest of the workspace.

The pattern did not propagate. Stages 5, 6, 7, 8, and the CLI-plumbing-shape amendment all shipped with "exit criterion met + tests green" as the review bar. `rules/rust.md` compliance was not part of any stage's exit criterion, no reviewer (human or agent) enforced it, and no CI check caught it. By the time Stage 9 began, the workspace was executing LLM calls, committing code in worktrees, and persisting records - all with no entry logs recording which work, which tool, which path, which hash.

`rules/log.md` was added 2026-04-24 as a shorter, auto-loaded, language-agnostic backstop to prevent further drift. Log.md does not retroactively instrument existing code.

### Problem

1. **Silent functions prevent post-mortem diagnosis.** A function that detects a failure, returns an error string, and emits nothing leaves the operator with the caller's lossy summary and no access to the parameters that caused the failure. The Stage 9 lifeguard incident is the canonical example.
2. **No scope-field inheritance.** Because `run_implementer` is not wrapped in `#[instrument(fields(work_id))]`, the `warn!("implementer escalated")` inside does not carry `work_id` as a structured field. Every subsystem below (tools, store, worktree) is in the same state: errors bubble up as strings with no structured context attached.
3. **No operator knob for "more detail."** Restarting at `-l debug` is the current knob. But if the functions emit nothing at DEBUG, the knob is a no-op. Debug level exists; debug *signal* does not.
4. **Ad-hoc log topology decisions loom.** The log-file layout (one per-run JSON log, one pretty log, a per-work fanout) is sound but has not been exercised against real multi-agent workloads. Open questions: per-agent-role fanout? daemon-vs-client log scoping? correlation between ephemeral client runs and the long-lived daemon run? Settling these up-front prevents ad-hoc evolution during the Stage 9 push.

### Goals

- Bring workspace `#[tracing::instrument]` coverage on non-trivial functions to ≥95%.
- Standardize per-crate scope fields so `warn!`/`error!` emissions anywhere in the workspace carry a predictable set of structured keys.
- **Treat logs as decision artifacts, not only debug traces.** A human or future LLM agent picking up a failed Work, a rejected Bundle, or a half-merged Plan must be able to read what's on disk and understand what happened well enough to decide what to do next - without running the daemon, without rerunning at `-l debug`, without grepping a 10,000-line JSON stream. This goal drives topology decisions (per-record summaries as a layer above raw events) and content decisions (every span field must name the thing it identifies, never rely on positional log order).
- Settle log-file topology: decide per-agent-role fanout yes/no, decide daemon-vs-client scoping, decide correlation between client processes and daemon processes (now solved: shared `session_id` + `client_session_id` on the connection span, plus session-fanout aggregate), decide whether summaries are written.
- Make the sweep diff-reviewable by landing one crate at a time, each as a commit small enough to eyeball.
- Produce a per-crate acceptance test that boots a subscriber, drives a canned workload, and asserts the expected span names appear. This is the forcing function that prevents the next regression.

### Non-Goals

- Replacing `tracing` / `tracing-subscriber` with another framework.
- Building a clippy lint or proc-macro that auto-applies `#[instrument]`. Picking good fields requires human judgment per function.
- Rewriting `llm`'s manual `info_span!()` calls into `#[instrument]` attributes where the manual form already carries correct fields and outcome variants. Convergent form matters; identical form does not.
- Instrumenting test code. `#[cfg(test)]` blocks are exempt per `rules/rust.md`.
- Backfilling a run-history analytics system. That's in `docs/roadmap.md`'s "Beyond First Gate" list. This sweep gives the analytics work something to read.

## Proposed Solution

### Overview

Ship the sweep as one commit per crate, ordered by operational blast radius. Each commit adds `#[tracing::instrument(level = "...", skip_all, fields(...))]` to every non-trivial function, adds one acceptance test that asserts a representative span name appears during a canned workload, and updates the per-crate `CLAUDE.md` with the crate's scope-field convention.

Three crates get special handling:
- **derive** - proc-macro internals are exempt (macros don't run in the hot path; their tests verify emitted code). The *emitted code* does need `#[instrument]` coverage - that's handled in the crates that consume derive (domain, store).
- **llm** - already uses manual `info_span!()` with correct outcome fields. Keep the manual spans; add `#[instrument]` only to functions that lack a span today.
- **domain** - mostly record types and FSM tables. Instrument only the FSM transition functions (they're state writes) and the typed-ID constructors if they do validation. Skip getters, Display impls, and pure ctors.

### The Policy: What Logs, and How Many?

Answering the framing question directly.

#### Per function

Every non-trivial function gets exactly **one** `#[tracing::instrument]` attribute. That attribute opens one span. Everything the function emits inside inherits the span's fields automatically - no hand-rolled `debug!("fn_name: a={a}")` at entry, that's what the attribute does.

Shape:
```
#[tracing::instrument(
    level = "<info|debug|trace>",
    skip_all,
    fields(<scope-keys>, <salient-params-as-length-or-preview>),
    [ret,] [err,]
)]
```

| Function tier | Level | Examples |
|---|---|---|
| Pipeline orchestrator (one call per Plan/Work/Bundle) | `info` | `run_implementer`, `run_reviewer`, `decompose`, `run_integrator`, `handle_plan_create` |
| Per-iteration / per-stage helper | `debug` | `check_action`, `dispatch_action`, `parse_actions`, `commit_changes`, `build_for_implementer`, tool `execute()` impls |
| Tight-loop body (dozens+ per call) | `trace` | per-item validators, per-record iterators, hash walks, per-token steppers |
| Trivial (getter, Display, ≤ 2 lines) | none | - |

`ret` and `err` are added where their payloads are small and their logging has diagnostic value:
- **`ret, err`** - store writes (`BundlesStore::create/update`, `WorksStore::update`), FSM transitions.
- **`err` only** - external call wrappers (`AnthropicClient::complete_*`, `Worktree::checkout`, `Bash::execute`, subprocess spawns). Their happy-path return bodies are large or uninteresting; their errors are always interesting.
- **Neither** - pure transformers, parsers, builders whose callers will log the result themselves.

Large payloads are summarized, never inlined:
- `goal_len = goal.len()`, not `goal = %goal`
- `prompt_chars = prompt.len()`, optionally `prompt_preview = &prompt[..100.min(prompt.len())]`
- `output_bytes = stdout.len()` rather than stdout itself
- `command = %cmd` is fine (commands are short and high-signal; not payloads)

#### Per run (how many events land in a log file)

Reasoning from function tiers and a typical E2E shape:

| Scenario | Entry-level (INFO) spans | Per-stage (DEBUG) events | TRACE (if enabled) | Total at DEBUG |
|---|---|---|---|---|
| Happy-path minimal E2E (1 Plan → 1 Work, 1 iteration) | ~6 | ~100 | ~0 | ~100 |
| Escalation (our Stage 9 failure, 4 iterations, no Bundle) | ~3 | ~150 | ~0 | ~150 |
| Realistic multi-retry (3 Bundles, 1 approved, integrator retry) | ~12 | ~400 | ~0 | ~400 |
| Same run with TRACE on `agents=trace` | ~12 | ~400 | ~500 | ~900 |

At ~150 bytes per JSON event, a 400-event run produces ~60 KB of `events.log`. Trivial storage-wise; easily consumed by `jq` for per-span filtering. The pretty `loopr.log` is ~2× the size.

This is the answer to "how many logs?" Concretely:

- **Raw event stream** (events.log / loopr.log): the sweep adds roughly **250-350 `#[instrument]` attributes** to the source tree (one per non-trivial function), and a representative DEBUG-level run emits **~100-400 events** depending on path length.
- **Per-work fanout**: one file per Work, appended to as events carry `work_id`; typically ~30-100 events per Work per run.
- **Per-record summaries** (see Q4): one markdown file per Plan, Work, Bundle, and daemon run. Rewritten (not appended) on each FSM transition. A first-gate run produces exactly 4 summary files. Each ≤ ~5 KB.
- **Per-record transcripts** (see Q5): one markdown file per LLM-using agent invocation. Append-only as the agent's Ralph loop iterates. Heavy: 5-50 KB per iteration, potentially multiple MB over a long loop. A first-gate happy-path run produces 3 transcripts: one Decomposer, one Implementer (1-10 iterations), one Reviewer.

Neither the attribute count nor the runtime event volume is a burden. Summaries are fixed-size per record. Transcripts are unbounded in principle but capped per-iteration (100 KB hard cap with truncation marker), so even a 50-iteration Ralph loop stays under 5 MB per Work.

### The Policy: Per-Crate Scope Fields

Every event inside an instrumented function inherits the span's fields. Each crate declares which keys its functions carry:

| Crate | Required scope fields (inherit to all events) |
|---|---|
| `agents` | `work_id`, `iteration`, and (for reviewer/integrator) `bundle_id` |
| `tools` | `tool_name`, `lane` (local/net/heavy); per-tool extras: `path` for file tools, `command` for bash, `pattern` for grep/glob |
| `store` | `record_kind`, `record_id`, `op` (create/update/get/list) |
| `worktree` | `work_id`, `branch`, `worktree_path` |
| `integrator` | `work_id`, `bundle_id`, `integration_branch` |
| `context` | `role` (implementer/reviewer/etc.), `tokens_budget`, `prompt_chars` |
| `decomposer` | `plan_id`, `goal_len`, `outcome` (already done; pattern) |
| `llm` | `model`, `system_chars`, `user_chars`, `outcome`, `duration_ms` (already done via manual spans; pattern) |
| `loopr` (daemon) | `session_id`, `process_id`, `target_slug`, `request_id` (for IPC handlers), `method` (for RPC dispatch), `client_session_id` (added to the `ipc.connection` span at handshake completion) |
| `loopr` (client) | `session_id`, `process_id`, `target_slug`, `subcommand` |
| `ipc` | none specific - IPC is transport, the handler on the loopr side owns the scope |
| `domain` | none specific - transitions carry `record_kind`/`record_id` via caller's span |
| `telemetry` | none - self |
| `derive` | n/a (compile-time) |

Convention: field names are `snake_case`. Never repeat a field across nested spans with different meanings (e.g., don't use `id` - use `work_id`, `bundle_id`, `plan_id`). On the daemon side, the process's own `session_id` (the daemon's session, set at boot) and `client_session_id` (the caller's session, recorded on the per-connection span at handshake completion) are distinct and both appear on nested spans; do not conflate them.

### Log-File Topology

Current state (post-layout-doc):

All per-process files live under `$XDG_DATA_HOME/loopr/sessions/<session-id>/targets/<target-slug>/runs/<process-id>/`. Session-scoped aggregate files live one level up at `sessions/<session-id>/targets/<target-slug>/`.

| File | Writer | Scope | Purpose |
|---|---|---|---|
| `<run-dir>/events.log` | `fmt::Layer::json()` | one per process (daemon gets one long-lived, each client call gets one short-lived) | machine-parseable source of truth |
| `<run-dir>/loopr.log` | `fmt::Layer` pretty | same as above | human `cat`/`grep` surface; what `loopr logs tail` reads |
| `<run-dir>/work/<work-id>.log` | `WorkFanoutLayer` | per Work, across whichever agent touched it | diagnose a single Work's full history |
| `<session-dir>/targets/<target-slug>/session-fanout.log` | `SessionFanoutLayer` | per session, across daemon + every client process of that session | diagnose "what happened in this session" without correlating across `<run-dir>`s by hand |
| stderr | `fmt::Layer` pretty | process-local (TTY only) | real-time operator mirror at INFO floor |

Open questions the user raised while drafting this:

#### Q1: Should each agent role get its own log file?

**Recommendation: no new file. Role is already a span field; grep is enough.**

Every agent emits events from its own `target` (e.g., `agents::implementer`, `agents::reviewer`, `agents::director`). At JSON-log time, filtering is `jq 'select(.target | startswith("agents::implementer"))' events.log`. That answers "what did the Implementer do across this run" without allocating a file handle per role per run.

The case *for* per-role files is ordering: if you want a single Implementer's timeline without interleaving from other agents on other Works, a dedicated file is easier than grep. But the per-work fanout already gives you ordering-within-a-Work, which is the question operators actually ask in practice. Per-role-across-all-works is rare.

**Gated follow-on:** if during the Stage 9 push we find ourselves repeatedly running the same `jq select .target=...` query, add `RoleFanoutLayer` as a parallel layer to `WorkFanoutLayer`. The implementation is ~50 lines and trivial to graft on. Ship only when that friction is empirically felt.

#### Q2: Should daemon and client logs be separate or shared?

**Recommendation: stay separate, correlate by `client_session_id`, aggregate via `SessionFanoutLayer`. (Both correlation and aggregation shipped with the layout doc; this question is historical.)**

Today: daemon opens `$XDG_DATA_HOME/loopr/sessions/<session-id>/targets/<target-slug>/runs/<daemon-process-id>/` at start and writes there for its whole lifetime. Each client invocation (`loopr plan`, `loopr works`, etc.) allocates its own `runs/<client-process-id>/` peer and writes there for the duration of one RPC call. An `ls .../runs/` reveals both: one dir per daemon boot, plus one dir per client call; all nested under the same `<session-id>` for work belonging to that session.

Separate is right because:
- Daemon lives for hours/days; client lives for milliseconds. Merging the per-process files would mean fsync contention or a shared mutex.
- A client-side failure ("server didn't respond") needs to log without depending on the daemon being reachable.
- Log-rotation policy differs: daemon logs rotate on size; client logs are write-once-throwaway.

Two correlation mechanisms ship to bridge the separation:

1. **`client_session_id` on the daemon's connection span.** The client's handshake message carries `session_id` (resolved by its own subscriber at startup). The daemon records it on the `ipc.connection` span at handshake completion so every request handled under that connection inherits `client_session_id`. A reader greps `client_session_id="20260424-150000"` against the daemon's `events.log` and gets exactly the daemon-side view of every request that client invocation issued.
2. **`SessionFanoutLayer` aggregating across all processes of a session.** Events carrying either `session_id` or `client_session_id` are appended to `sessions/<session-id>/targets/<target-slug>/session-fanout.log` regardless of which process emitted them. Daemon-side events handling a client's request, and the client-side events of the same request, both land in the same file, time-interleaved. This is the "what happened in this session" affordance that bare per-process files cannot provide.

Both shipped in the layout doc (Phases 6 and 7). Phase 10 below is correspondingly smaller - the handshake + span changes are done.

#### Q3: Does TRACE ever land in files, or is it opt-in at runtime only?

**Recommendation: opt-in via `-l` flag; never the default; never mixed into the default file.**

Default run: the subscriber filter is `info`. `events.log` + `loopr.log` contain INFO-and-above. A DEBUG rerun: `-l debug` widens the filter; the same files now contain DEBUG-and-above. A TRACE rerun: `-l trace` or `-l agents=trace` narrows further. The same files absorb the extra detail.

No separate "trace.log." Storage is not the problem. Operator cognitive load is - a file that *might* contain trace is confusing. A file that always matches the filter at the time the daemon started is predictable.

#### Q4: Should logs be readable by a future reader (human or LLM agent) without the daemon running?

**Recommendation: yes, and we add a summary layer above the raw event stream to make it tractable.**

The raw layer (`events.log`, `loopr.log`, `work/<work-id>.log`) is optimized for completeness - every structured event, time-ordered, losslessly captured. It's what you grep during a live debug session. It is not optimized for a reader asking "what happened to Bundle `bd-X`?" That reader has to jq-select by id across potentially hundreds of spans, mentally reconstruct the FSM walk, and correlate against the taskstore's final state. A human needs minutes; an LLM agent with a limited context window may not fit the raw stream at all.

We add a second layer: **per-record summaries**, written as markdown, keyed by the record id they describe, updated at FSM transition points. Summaries are derived - the taskstore JSONL is still the source of typed truth and `events.log` is still the source of the event stream - but they collapse what a reader most-often wants into one small file per record.

Layout (record-scoped, one directory per record, summary lives alongside any heavy artifacts for that record). Per the layout doc's "Alternative 3" (rejected), derived `records/` stays under the target so `cat .loopr/records/works/<id>/summary.md` remains a one-hop local affordance; per-process raw telemetry lives in XDG:

```
<target>/.loopr/
  records/
    plans/<plan-id>/
      summary.md                    # short digest
      decomposition.md              # Decomposer transcript (see Q5)
    works/<work-id>/
      summary.md                    # short digest
      transcript.md                 # Implementer ralph loop transcript (see Q5)
    bundles/<bundle-id>/
      summary.md                    # short digest
      review.md                     # Reviewer transcript (see Q5)

$XDG_DATA_HOME/loopr/
  sessions/<session-id>/
    summary.md                      # session-level digest, written on session end
    targets/<target-slug>/
      session-fanout.log            # per-session aggregate (ships; see Q2)
      runs/<process-id>/
        events.log
        loopr.log
        work/<work-id>.log
        summary.md                  # per-process digest, written at process shutdown
```

Each file is written by a small summary generator (one function per record kind, living in `crates/loopr/src/summary/`) hooked into the existing `BundleUpdateSink` / `WorkUpdateSink` / `PlanUpdateSink` transition callbacks. No new subscriber layer, no new background task - write-on-transition, idempotent, overwrite-safe (each write is a full re-render from taskstore + fanout log, not an append).

Template structure, baked in via `include_str!()`:

```
# Work <work-id>

**Parent Plan:** [<plan-id>](../plans/<plan-id>.md)
**Status:** <Done | Blocked | InProgress | ...>
**Iterations:** <N>
**Branch:** loopr/wk-<id>-<attempt>

## Goal
<work.title + work.content>

## Acceptance Criteria
<list>

## History
| Iter | LLM latency | Actions | Outcome |
| --- | --- | --- | --- |
| 1 | 3.2s | read src/main.rs | continue |
| 2 | 4.1s | edit src/main.rs, propose_bundle | Bundle <bd-X> |

## Outcome
<Bundle link OR escalation reason OR noop reason>

## Raw
- events: `$XDG_DATA_HOME/loopr/sessions/<session-id>/targets/<target-slug>/runs/<process-id>/work/<work-id>.log` (the process that last touched this Work; earlier processes listed by session fanout)
- session: `$XDG_DATA_HOME/loopr/sessions/<session-id>/targets/<target-slug>/session-fanout.log`
```

Why markdown: both humans and LLMs read it natively. Why record-scoped (not run-scoped) paths: a Work may span multiple runs (daemon restart, retry, attempt escalation); its summary should accumulate, not fragment.

Why derived, not primary: if the generator is buggy or a summary is deleted, rebuilding from taskstore + events.log is straightforward. The summary layer is a **view**, not a new FSM.

This sits between "what the log files capture at wire-level" (instrumentation sweep, Phases 1-11) and "what a future reader reaches for first" (Phase 8.5 below).

#### Q5: Where does the full LLM back-and-forth live?

**Recommendation: one transcript file per LLM-using agent invocation, alongside that record's summary, separate from both the event stream and the summaries.**

Agents are different from other functions. An Implementer iteration sends the LLM thousands of tokens of prompt (system prompt + work goal + acceptance criteria + accumulated iteration history + tool schemas) and receives back thousands of tokens of response (either text actions or tool-use calls). A Ralph loop with 10 iterations can accumulate hundreds of KB to multiple MB of conversation. Three properties matter:

1. **It's heavy.** Each LLM call's prompt+response is 5-50 KB. Mixing that into `events.log` (which is meant for structured events scannable by `jq`) breaks the scan affordance - a 30 KB JSON field makes the whole line unreadable. Mixing it into `loopr.log` (pretty text, line-oriented) bloats the line count of every grep.
2. **It accumulates.** The Ralph loop's defining feature is that each iteration's user message includes a summary of prior iterations. Capturing one iteration's prompt captures the state of accumulated knowledge at that moment. Reading iterations in order is how a debugger understands "what did the LLM know when it made that decision?" That's a transcript shape, not an event shape.
3. **It's what you reach for to debug agent behavior.** When the user needs to know why the Implementer repeated the same action three times, the first thing they want to see is: what did we ask the LLM, what did it return, what did the dispatcher do with the response, what changed for iteration N+1. A transcript is the only artifact that co-locates all four.

Layout (co-located with the record they describe):

```
<target>/.loopr/records/
  plans/<plan-id>/decomposition.md      # Decomposer: one LLM turn; prompt + response + parsed Work list
  works/<work-id>/transcript.md         # Implementer: N iterations, append-only as the Ralph loop runs
  bundles/<bundle-id>/review.md         # Reviewer: one LLM turn; prompt (diff + AC) + response (verdict) + parsed verdict
```

No transcript for Integrator - it's deterministic, non-LLM. No transcript for Director / Researcher yet - those agents don't ship in first gate.

Content per iteration (for Implementer's transcript.md; Decomposer and Reviewer use the same structure with one iteration):

```
## Iteration 3 - 2026-04-24T15:03:13

**Model:** claude-opus-4-7
**Latency:** 4.2s
**Tokens:** prompt=3_812, completion=247
**Session:** `20260424-150000`
**Process:** `pc-k3m9f2`
**Span:** events.log in the process run dir at `$XDG_DATA_HOME/loopr/sessions/20260424-150000/targets/<target-slug>/runs/pc-k3m9f2/events.log` (offset recorded in the event that emitted this iteration's `transcript_appended` debug line)

### Prompt (system)
<rendered system prompt - may be elided with a checksum if unchanged from iteration 1>

### Prompt (user)
<full rendered user message, including accumulated iteration history - this is the "accumulated knowledge" the loop carries>

### Response
<verbatim LLM response text>

### Parsed Actions
- `run_tool`: write src/main.rs (hash abc123)
- `propose_bundle`: claims=["added --version"]

### Dispatcher Outcome
- write: OK (215 bytes)
- propose_bundle: Bundle [bd-xyz](../../bundles/bd-xyz/summary.md) created at 698e22d
- Lifeguard: continue

---
```

Write semantics:
- **Append-only.** Unlike summaries (which are rewritten), transcripts grow as the Ralph loop runs. Partial writes survive a crash; the last iteration is complete or absent, never half-written.
- **Per-iteration commit.** Each iteration's block is flushed before the next iteration starts. A crash mid-iteration loses that iteration's block but preserves the record on disk up to that point.
- **Size cap per block.** Hard cap of ~100 KB per iteration; if a rendered prompt exceeds that (prompt engineering smell), truncate with a ">[truncated: original was N KB; see events.log span for checksum]<" marker. This prevents a single runaway prompt from bloating the transcript.
- **Never the source of truth.** The raw prompt bytes as-sent to the LLM are derived from prompt-assembly code (`crates/context/`), which is deterministic given the record state in taskstore. The transcript records what WAS sent as a convenience; the transcript going missing is recoverable in principle by re-rendering the prompt at the recorded taskstore commit. We don't build that recovery tool today, but the contract is: transcripts are deep-debugging artifacts, not audit/compliance artifacts.

Why this isn't the same as the summary:
- **Summary** = "what happened to this record, at a glance" - 1-5 KB, rewritten, digest.
- **Transcript** = "what text flowed between the dispatcher and the LLM" - 10 KB to multiple MB, append-only, raw.
- The summary *links* to the transcript. A reader who needs detail follows the link; a reader who needs the one-line overview stops at the summary.

Why separate files per agent invocation, not one per run or one per daemon:
- A Work may span many runs (daemon restarts, retries). The Work's transcript must accumulate across those, not fragment. Record-scoped paths match that.
- Different agents write different transcript shapes; co-locating keeps the shape consistent per-file.

Why not put transcripts in the taskstore: taskstore is git-committed truth. Transcripts can be 100× the size of the records they describe and are not truth - they're debugging artifacts. They live outside taskstore by design but still under the target, because the "Alternative 3" decision in the layout doc chose to keep derived `records/` target-local for `cat`-reachability. `.git/info/exclude` must cover `.loopr/records/**` explicitly; verify in Phase 8.6 that the exclude list installed by `loopr init` includes it.

### Implementation Plan

One phase per crate, ordered by operational pain. Each phase lands as one commit: code + per-crate acceptance test + per-crate `CLAUDE.md` scope-field note. `otto ci` must pass per-crate before moving on.

#### Phase 1: agents
**Model:** sonnet
Instrument `lifeguard::{check_action, record_parse_failure}`, `implementer::{run_implementer, force_propose, request_actions_with_retry}`, `reviewer::{run_reviewer, parse_verdict, build_messages, render_issue_summary, git_show}`, `dispatch::{dispatch_action, commit_changes, commit_partial_for_inspection, propose_bundle}`, `parse::{parse_actions, parse_one}`. Add `agents_smoke_spans` test that runs one Implementer iteration with a scripted LLM and asserts `check_action`, `dispatch_action`, `run_implementer` span names appear in the captured events. **This is the phase that fixes the Stage 9 debugging story.** Ship first, unblock next E2E run.

#### Phase 2: tools
**Model:** sonnet
Instrument every builtin's `execute()` (`bash`, `edit`, `read`, `write`, `grep`, `glob`) with `fields(tool_name, lane, ...tool-specific keys)`; instrument `router::{acquire, release}`, `spawn::spawn_sandboxed`. Add `err` on every external-subprocess call. Test: invoke each builtin through the registry and assert its span appears.

#### Phase 3: store
**Model:** sonnet
Instrument every public method on `PlansStore`, `WorksStore`, `BundlesStore`, `TicksStore`, plus `Store::{open, close}`. Use `ret` where the return is an ID or small enum; `err` everywhere. Test: open a tempdir store, do one write + read, assert `plans::create` span.

#### Phase 4: integrator
**Model:** sonnet
Instrument `run_integrator`, its phase-2 git sequence helpers, retry backoff, conflict detection paths. `fields(work_id, bundle_id, integration_branch, phase)`. Test: existing integrator seam tests plus one that captures `run_integrator` span.

#### Phase 5: worktree
**Model:** sonnet
Instrument `Worktree::{init, checkout, clean}`, registry ops, crash-recovery reconcile. `fields(work_id, branch, worktree_path)`. Test: create + clean a tempdir worktree, assert spans.

#### Phase 6: context
**Model:** sonnet
Instrument `build_for_implementer`, `build_for_reviewer`, and whatever private helpers assemble templates. `fields(role, tokens_budget, prompt_chars)`. Test: one call through each builder, assert span.

#### Phase 7: decomposer
**Model:** sonnet
Complete the work started by the existing `decompose` attribute: instrument `try_llm_once`, `validate_response`, `detect_cycles`, `build_prompt`, `scan_workspace_tree`. Keep `decompose` at `info`; demote helpers to `debug`.

#### Phase 8: llm
**Model:** sonnet
Keep the manual `info_span!("llm.anthropic")` on `complete_with_tool` and `complete_free`. Add `#[instrument]` to the non-spanned helpers (`new`, `build_headers`, error classification). Test: existing unit tests cover span emission when stubbed.

#### Phase 8.5: Summary Generators
**Model:** opus (template design + FSM-hook wiring requires judgment)
Add `crates/loopr/src/summary/` with one generator per record kind (`plan.rs`, `work.rs`, `bundle.rs`, `run.rs`). Each exposes `pub fn render_<kind>(record, taskstore_ref, events_ref) -> String` and a side-effecting `pub async fn write_<kind>(target, record, ...) -> Result<()>` that overwrites `<target>/.loopr/records/<kind>/<id>/summary.md` atomically (write-to-temp + rename). Templates are baked-in `include_str!()` constants; a future earned feature may move them to overrideable `.tmpl` files under `.loopr/templates/`, mirroring the prompt override chain.

Wire the writers into the existing FSM transition callbacks:
- `BundleUpdateSink` -> rerender bundle summary; if status changed, rerender work summary (Bundle status affects parent Work) and plan summary (Work status affects plan progress).
- `WorkUpdateSink` -> rerender work + plan.
- `PlanUpdateSink` -> rerender plan.
- Process shutdown (daemon or client) -> render `sessions/<session-id>/targets/<target-slug>/runs/<process-id>/summary.md` for the just-ended process.
- Session end (`loopr sessions end`) -> render `sessions/<session-id>/summary.md` (session-level digest that aggregates across targets and processes).

Acceptance: after a full happy-path E2E, the four summary files exist, cross-link correctly, and pass a snapshot test (`insta` or similar) against baked-in golden output.

Scope caveat: summaries must never mutate taskstore state or events.log. They are read-only views; if one errors, the transition callback logs a warn and continues.

#### Phase 8.6: Transcript Writers
**Model:** opus (agent-side integration, append-semantics edge cases)
Add `crates/loopr/src/transcript/` with one writer per LLM-using agent (`decomposer.rs`, `implementer.rs`, `reviewer.rs`). Each exposes `pub async fn append_iteration(target, record_id, iteration: TranscriptIteration) -> Result<()>` that appends a rendered iteration block to the record's transcript file at `<target>/.loopr/records/<kind>/<id>/<transcript-name>.md`. File is created on first append with a header block (model, start timestamp, record id); subsequent appends just add the iteration block.

Wire the writers into the agent modules (not into taskstore callbacks - transcripts capture LLM round-trips, which happen before any FSM transition):
- `crates/decomposer/src/decompose.rs` - after `try_llm_once` returns, append one iteration block (Decomposer is a single turn).
- `crates/agents/src/implementer.rs` - after each iteration's LLM response + action dispatch completes, append one iteration block.
- `crates/agents/src/reviewer.rs` - after the Reviewer's one LLM call + parse completes, append one iteration block.

Add a `TranscriptIteration` struct in `crates/loopr/src/transcript/model.rs` with fields: `model`, `started_at`, `latency`, `prompt_tokens`, `completion_tokens`, `system_prompt` (full, but elide-on-repeat via checksum), `user_prompt` (full), `response` (verbatim), `parsed_actions` (`Vec<ActionSummary>`), `dispatcher_outcomes` (`Vec<DispatchSummary>`), `lifeguard_decision`. The renderer consumes this struct; agents populate it.

Truncation: apply the 100 KB per-iteration cap at render time, not at append time. The struct holds full text; the renderer decides what to emit. Truncated regions get `>[truncated: N KB original; sha=...]<` markers so future recovery tools can validate.

Span fan-out: every iteration append emits a `debug!("transcript_appended", work_id=..., iteration=..., bytes=...)` event so the raw events.log shows each append and its size. This makes it trivial to spot abnormally large iterations without opening the transcript.

Acceptance: after a full happy-path E2E on `rust-version`, `records/works/<wk-id>/transcript.md` contains N iteration blocks matching N Implementer iterations, each block has the five subsections (prompts, response, parsed, outcomes, lifeguard), and the final block's outcome references a Bundle id that also has `records/bundles/<bd-id>/review.md` with one iteration showing the accept verdict. A snapshot test validates the shape (not the exact content, which depends on the stubbed LLM's scripted responses).

Scope caveat: transcripts are best-effort. If an append fails (disk full, permission error), the agent logs `warn!` and continues the Ralph loop. A missing transcript is a debug-time inconvenience, not a run-stopping error.

#### Phase 9: loopr (daemon side)
**Model:** sonnet
Instrument `handle_plan_create`, `spawn_implementer_for_work`, `spawn_reviewer_for_bundle`, `spawn_integrator_for_bundle`, reconcile sweep, `build_context`, `run_active_daemon`, `serve_core`. Also the IPC handlers for `record.list`, `record.get`, `system.status`. Top-level entries at `info`; per-connection handlers at `debug`.

#### Phase 10: loopr (client side)
**Model:** sonnet
Instrument the client-side `plan_command`, `list::run`, `show::run`, `connect_or_wait`, `IpcClient::{connect, handshake, request}`. The correlation-field plumbing (`session_id` on `system.handshake`, daemon recording `client_session_id` on the `ipc.connection` span) already shipped in Phase 6 of the layout doc; this phase only needs to ensure the client-side instrumentation emits `session_id` / `process_id` / `target_slug` on every outbound span so the daemon's inherited `client_session_id` has a matching client-side view.

#### Phase 11: ipc + domain + telemetry + derive
**Model:** sonnet
Minimal. `ipc`: nothing to instrument (types only). `domain`: instrument FSM transition functions (they're state writes). `telemetry`: self-host; nothing. `derive`: skip.

#### Phase 12: roadmap + rust.md + review-agent briefing
**Model:** sonnet
Update `docs/roadmap.md` to add "instrumentation coverage ≥95% on new functions" to every future stage's exit criterion. Add one paragraph to `rules/rust.md` pointing to `rules/log.md` as the short form. Update the `code-reviewer` agent's system brief (location TBD; check `~/repos/scottidler/claude/HOME/repos/.claude/agents/`) to specifically check instrumentation coverage as a mandatory pass. This is the forcing function that prevents the next regression.

### Acceptance Test Pattern (per phase)

Each per-crate phase ships with a test of this shape, adapted per crate:

```rust
// crates/<crate>/tests/instrumentation.rs
// Registers a span capture, drives one canonical code path, asserts the
// instrumented function names appear in the captured span stream. Failing
// this test means someone removed an #[instrument] attribute.
```

The test runs under the crate's own `otto ci` and catches both removal (directly: no span) and silent demotion (indirectly: filter level doesn't match). Together, these two failure modes cover the regression risks this sweep is addressing.

## Alternatives Considered

### Alternative 1: Surgical fix (instrument only lifeguard + dispatch)

- **Description:** Add `#[instrument]` to the two functions whose silence burned us in Stage 9. Defer the rest.
- **Pros:** Shortest path to unblocking the Stage 9 E2E rerun. Minimal diff. Minimal review load.
- **Cons:** The next E2E to hit a wall - in tools, store, integrator - will hit the same silent-functions problem in a different crate and force the same restart-at-debug cycle. Three more rounds of that pattern, if each takes 30 minutes and a few API dollars, is worse than doing the sweep once.
- **Why not chosen:** The cost of the sweep is bounded (roughly one review session per crate × 13 crates). The cost of silent functions during Stage 9 is unbounded and measured in API dollars.

### Alternative 2: Write a macro or clippy lint that forces instrumentation

- **Description:** A `#[require_instrumented]` attribute on a module that errors at compile time if a non-trivial function below it lacks `#[instrument]`.
- **Pros:** Mechanical enforcement - no reviewer judgment needed.
- **Cons:** Defining "non-trivial" in a lint is hard. The field list is human judgment (which scope keys matter, which payloads to summarize). A macro can force presence but not quality. A 5-line function wrapped in a bare `#[instrument]` satisfies the macro and still has useless fields.
- **Why not chosen:** Attribute quality is the real goal, not attribute presence. The forcing function is the code-reviewer agent's briefing (Phase 12), which can assess quality.

### Alternative 3: Single giant PR, all 13 crates at once

- **Description:** One commit, ~300 file changes, ~500 lines of attributes, merge to main in one gulp.
- **Pros:** Nothing to track across commits. Every crate's pattern stays synchronized.
- **Cons:** Unreviewable. The v5 working rules explicitly call out "The crate is the unit of blast radius" - a bad attribute choice in tools shouldn't force a revert that also touches agents.
- **Why not chosen:** Violates a v5 Working Rule.

### Alternative 4: Per-agent-role log files as a primary feature, not a gated follow-on

- **Description:** Build `RoleFanoutLayer` as Phase 1. Every agent role gets its own file.
- **Pros:** Obvious operator ergonomic: `tail -f $XDG_DATA_HOME/loopr/sessions/*/targets/<target-slug>/runs/*/role/implementer.log` across sessions.
- **Cons:** The same query is a one-line jq against `events.log` today. Building the feature speculatively is overengineering before evidence it's the right split.
- **Why not chosen:** The per-work fanout is the dimension operators actually ask about in practice. Role-wise is a nice-to-have. Ship when felt, not speculatively.

### Alternative 5: Skip the summary layer; rely on raw logs + taskstore for all reader queries

- **Description:** Cut Phase 8.5. Future readers reconstruct state by reading `events.log` and `taskstore/*.jsonl` directly.
- **Pros:** No new code, no new failure mode (summary generator bugs, drift from taskstore). Strict separation: taskstore = typed state, events.log = event stream. Nothing else.
- **Cons:** Every reader - human or LLM agent - pays the reconstruction cost on every query. An LLM agent asked "why did bundle bd-X fail?" without a summary must load potentially 400 events and the full Bundle / Verdict / Work records into context, then reason. With a summary, it loads one markdown file. The difference in context-window usage and latency is substantial for agentic consumers, which is the primary use case driving the "logs as decision artifacts" goal.
- **Why not chosen:** The user's goal (reframe above) explicitly names LLM agents as future readers. Summaries are the lever that makes raw logs usable by that reader. Skipping them preserves engineering simplicity but defeats the goal.

### Alternative 6: Write summaries as JSON, not markdown

- **Description:** Machine-parseable summaries. `summaries/works/<id>.json` instead of `.md`.
- **Pros:** Type-safe for machine consumers. Easier to diff programmatically.
- **Cons:** Worse for human reading (the co-primary consumer). `cat foo.json | jq` works, `less foo.json` doesn't. And LLM agents read markdown at least as well as JSON - markdown is native for most LLMs.
- **Why not chosen:** The typed truth lives in taskstore already. Summaries are the *readable* layer; their value is exactly in being prose, not schema. A reader who wants structured data reaches for taskstore.

## Technical Considerations

### Dependencies

No new crates. `tracing` + `tracing-subscriber` + `tracing-attributes` (the `#[instrument]` macro host) are already workspace deps.

### Performance

`#[instrument]` allocates a span structure at entry regardless of filter level (tracing's fast-path check is inside the span creation). Measured overhead on `tracing`'s own benchmarks: ~50ns per span entry at INFO/DEBUG when the filter drops the span, ~200ns when it fires. For our workload (low hundreds of spans per run) the overhead is unmeasurable.

The real perf concern is *tight-loop* instrumentation. A per-record iterator that fires 10,000 times at DEBUG emits 10,000 span events. This is why tight-loop helpers demote to `trace` - the filter drops them by default and they cost ~50ns each when gated. If someone opts into TRACE specifically, they're accepting the cost.

Field discipline is the other perf concern: `%x` / `?x` formats the value at span open, even if the event is dropped. `skip_all` + explicit small-payload fields keeps this cheap. Avoid `?full_prompt` on a 50 KB string - format once per open, 50 KB of work dropped to the floor.

### Security

`skip_all` prevents the default-capture-all-parameters path from accidentally recording API keys, user prompts, or full subprocess stdouts. Every field is explicit. Code review checks for:
- `%secret_value` anywhere - never
- `?llm_response_body` - never; use a length or preview
- `%api_key` - never

These already exist in `AnthropicClient::new` where the key is explicitly dropped from the span (it's not even a parameter to a spanned function).

### Testing Strategy

Per phase: the acceptance test described in "Acceptance Test Pattern" above. Each test:
1. Installs a capture-layer subscriber (not the production one; a test-only layer that accumulates span events into a `Vec`).
2. Drives one representative code path through the crate.
3. Asserts the expected span names are present and carry the expected field keys.

Workspace-wide: after Phase 12, run `bin/e2e rust-version` (once ported) or the equivalent manual invocation. Expected: ~400 DEBUG events for a happy path, no unexplained gaps in the trace.

### Rollout Plan

Per-phase:
1. Instrument the crate.
2. Add the per-crate acceptance test.
3. Run `otto ci` inside the crate dir until green.
4. Run `otto ci` at workspace root to catch cross-crate regressions.
5. Commit with message `feat(<crate>): instrument per rules/log.md`.
6. Bump workspace version once all phases land; ship as v0.5.X.

No feature flag. No dual-path. The work is additive - adding attributes changes no behavior, only observability.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Attribute on async fn changes a Send bound, breaks a spawn site | Medium | High (compile break) | `tracing` documents this; fix at the spawn site or use `in_current_span()`. Catch in `otto ci`. |
| Accidentally recording a large payload in a span field | Low | Medium (log bloat, possibly secret leak) | `skip_all` is default; field review is part of per-phase commit review. |
| Per-crate test becomes flaky due to subscriber-layer interactions | Low | Low (flake, not functional break) | Use `tracing_subscriber::fmt::test_writer()`-backed subscriber that's scoped to one test. |
| Sweep overlaps with concurrent Stage 9 work and creates merge conflicts | Medium | Medium (lost work) | Pause Stage 9 until Phase 1 (agents) lands; other phases can run in parallel to Stage 9 since agents is the only crate actively modified by Stage 9 work. |
| A crate we thought was cold (ipc/domain) actually has hot functions we should instrument | Low | Low (follow-up) | Per-phase review catches this. Add a follow-up phase if needed. |
| Summary generator errors mask real FSM transitions | Medium | High (if a write panics inside a transition callback, the Bundle/Work could stall) | Summary writes are best-effort: wrap in `try`, log `warn!` on failure, never propagate the error back to the transition path. Acceptance test covers the error path. |
| Summaries drift from taskstore reality (race between state write and summary render) | Medium | Medium (reader sees stale summary) | Render is synchronous inside the transition callback, after the taskstore write completes. Stale summaries are impossible if the callback order is `taskstore-write-first → summary-render-second`, which it is. |
| Summary file grows unbounded as iterations accumulate on a long-lived Work | Low | Low (≤ a few KB) | Iteration history is tabular; cap at last 50 iterations with a note "earlier iterations in raw log." Enforced at render time. |
| Transcript captures an API key or other secret that leaked into the rendered prompt | Low | High (secret persisted to disk, potentially committed) | Prompt assembly in `crates/context/` never interpolates env vars or file contents outside the work's scope. Transcript writer additionally redacts anything matching the workspace's `path_deny_patterns` (`.env`, `.key`, `credentials`, etc.) with a `[redacted: pattern=X]` marker. `.git/info/exclude` covers `.loopr/records/**` so transcripts never get committed. |
| A single runaway prompt (e.g., a tool dumped 1 MB of output into the next iteration's history) bloats one transcript to 100 MB | Low | Medium (disk pressure, slow reads) | Per-iteration hard cap of 100 KB enforced at render time with truncation marker. The appended event at DEBUG records the pre-truncation size so operators see the issue. |
| Transcript append fails silently mid-Ralph-loop, leaving incomplete debugging record | Medium | Low (debugging inconvenience, not run failure) | `warn!` on each failure with path and errno; agent continues. Acceptance test covers the failure path. |

## Open Questions

- [ ] **Should the `code-reviewer` agent be invoked automatically per phase, or manually?** Leaning manual - the user runs it after the commit lands, before signing off on the phase. Automatic would require a hook.
- [ ] **Does Phase 12 need `rules/rust.md` to shrink (with the long instrumentation section moved into `rules/log.md`), or should rust.md keep its detailed Rust-specific tracing section and log.md stay the short cross-language form?** Leaning keep-both: rust.md has Rust-specific mechanics (`#[instrument]` attribute shape, Send-bound caveats); log.md has the universal rule. Duplication is not bad here; rust.md should just explicitly defer to log.md for the philosophical "why."
- [ ] **Should tests that verify span presence live per-crate, or should a single workspace-level test exercise the whole pipeline and assert all span names across all crates?** Per-crate is more maintainable (each phase owns its own test); a workspace-level integration test is a nice-to-have Phase 13 that's not blocking first gate.
- [x] **Correlation field for client→daemon: is `client_run_id` the right key name, or should it be `client_invocation_id` or simply re-purposed `run_id`?** Resolved by layout doc Phase 6: the field is `client_session_id`, recorded on the daemon's `ipc.connection` span at handshake completion. Every request handled under that connection inherits it. The daemon's own span field `session_id` names the daemon's session; the two coexist on nested spans and are not conflated.
- [ ] **Should summary templates be user-overrideable (like prompt `.pmt` files with the three-layer fallback), or baked-in only?** Leaning baked-in for the first ship - customization is an earned feature, not a day-one requirement. Revisit if operators ask for different summary shapes per target repo.
- [ ] **Summary freshness semantics on crash:** if the daemon writes the taskstore record but crashes before rendering the summary, the summary is stale. On next boot, should the reconcile sweep rerender all summaries for records it touches, or lazily on next transition? Leaning: reconcile-time bulk rerender (cheap; one pass over the taskstore) and then transition-driven updates afterward. Document in Phase 8.5 acceptance.
- [ ] **Should a run summary include cost estimates** (token counts × model rates)? The raw spans already carry `system_chars` and `user_chars`; aggregating across a run to produce "this run cost approximately $X" is a small addition and high operator value. Leaning yes but optional per-phase - fold in if the plumbing makes it trivial.
- [ ] **Should transcripts be configurable off** for targets where persistence is undesirable (pre-release code, proprietary prompts, compliance-sensitive workflows)? Leaning: add a `persistence.transcripts: enabled | disabled` flag to `.loopr/config.yml`, default enabled. Disabling collapses the transcript writers to no-ops. Deferred to post-first-gate unless a specific need arises.
- [ ] **System prompt elision policy.** Implementer iterations 2..N typically share the same system prompt as iteration 1 (prompt assembly is deterministic across iterations of one Work). Should the transcript elide repeated system prompts and reference iteration 1 by checksum, or render them in full each time? Leaning elide - saves ~50% of transcript size for multi-iteration loops and preserves readability (readers rarely re-read the system prompt). Implementation: first iteration renders full + emits a sha256; subsequent iterations emit `**System Prompt:** same as iteration 1 (sha256:...)` unless the checksum differs, in which case they render full.
- [ ] **Transcript discoverability from summary.** Should `works/<id>/summary.md` link explicitly to `transcript.md`, or should the co-location in the same directory be sufficient? Leaning: explicit link in the summary's "Raw" section. Readers land on summary first; one line pointing at transcript.md is worth the characters.

## References

- `rules/rust.md` §"Function-level instrumentation (mandatory)" and §"Function-level instrumentation with `tracing`"
- `rules/log.md` - short-form universal rule added 2026-04-24
- `docs/design/2026-04-19-telemetry-stage-2.md` - telemetry crate + subscriber layout + WorkFanoutLayer (original); path layout superseded by the layout doc below
- `docs/design/2026-04-24-loopr-layout.md` - XDG-rooted session layout + `SessionId` / `ProcessId` / `target_slug` taxonomy + handshake `session_id` + `SessionFanoutLayer`; supersedes this doc's original path references and Phase 10 scope
- `docs/vision.md` §"Observability" - commits the workspace to `tracing` over `log`
- `crates/decomposer/src/decompose.rs:56` - the golden `#[instrument]` example this sweep propagates
- `crates/agents/src/lifeguard.rs:62` - the silent function whose failure motivated this doc
- `docs/roadmap.md` - Stage 9 (First Gate E2E), currently blocked on this sweep
