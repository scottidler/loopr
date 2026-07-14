# Implementation notes: Kill the log-capture flake class workspace-wide

Running record of decisions/deviations while executing
`2026-07-13-tracing-interest-cache-helper.md`. Append-only.

## Phase 0: baseline + installer inventory

### Design decisions
- Evidence loops run **sandbox-disabled**. `RUSTC_WRAPPER=sccache` fails
  `Operation not permitted` under the command sandbox on any (re)compile, which
  turns recompiling loop iterations into spurious `could not compile` failures.
  A first botched run reported loopr 200/200 purely from this. All loops (and
  `otto ci`) run with the sandbox off; mixing modes churns the build fingerprint
  and forces recompiles.
- Baseline loop counts failures over the full run (does not break on first
  failure) so the fail *rate* is captured, per the b1b076ed evidence standard.

### Deviations
- None from the spec. (The doc already specified sandbox-agnostic commands; the
  sandbox-off requirement is an environment detail, not a spec change.)

### Tradeoffs
- Ran one detached (`setsid`) loop rather than the skill's default background
  mechanism, because sandboxed status checks live in a different PID namespace
  and cannot observe sandbox-disabled processes — a heartbeat file + done-marker
  gives reliable observation.

### Open questions
- None. loopr's 0/200 baseline is a null result (accepted); the class is proven
  real there by a live-caught calibration flake and by b1b076ed.

## Phase 1: promote the helper into telemetry

### Design decisions
- New module `crates/telemetry/src/testing.rs` holds `InterestedDiscard` +
  `pub fn ensure_global_interested_default()`. Migrated `store`'s doc comment
  (the fuller of the two) and generalized its opening lines for telemetry's home
  (it no longer sits next to the tests it serves), plus a new `# Contract`
  section — `crates/telemetry/src/testing.rs`.
- Private `mod testing;` + crate-root `pub use testing::ensure_global_interested_default;`,
  mirroring how `subscriber` is a private module whose items are re-exported —
  `crates/telemetry/src/lib.rs`. Call sites use
  `telemetry::ensure_global_interested_default()`.
- `init_for_test` calls the helper as its **first** statement, before EnvFilter
  validation and the `set_default` install — `crates/telemetry/src/subscriber.rs:init_for_test`.

### Deviations
- None.

### Tradeoffs
- `pub fn`, not a `test-support` cargo feature — the Resolved Decision in the
  doc; symmetric with the already-un-gated `init_for_test`/`TestSubscriberGuard`.

### Open questions
- None.

## Phase 2: route llm + store, delete the copies

### Design decisions
- `llm/src/metered.rs` and both store `set_capturing_default` helpers
  (`bundles/tests.rs`, `works/tests.rs`) now call
  `telemetry::ensure_global_interested_default()`; comment refs updated from
  `crate::logcapture` to `telemetry::testing`.
- Deleted `crates/llm/src/logcapture.rs` and `crates/store/src/logcapture.rs`
  via `git rm`, plus their `#[cfg(test)] mod logcapture;` decls. `rg "mod
  logcapture" crates/` -> 0 hits (acceptance criterion).
- Added `telemetry` as a **dev-dependency** to `store` via
  `cargo add telemetry --dev --path ../telemetry` (llm already deps telemetry
  normally). store/CLAUDE.md Dependencies section updated.

### Deviations
- None.

### Tradeoffs
- Used `git rm` (not `rkvr rmrf`) to delete the two tracked files: git history
  is the recovery path for a tracked-file deletion, and `git rm` records the
  removal in the same commit.

### Open questions
- None.

## Phase 3: fix loopr + worktree HIGH-risk binaries

### Design decisions
- loopr: added a local `set_capturing_default` helper in
  `transition/tests.rs` (mirror of store's) that calls
  `telemetry::ensure_global_interested_default()` then `set_default`; routed all
  6 sites through it.
- worktree: routed via `json_subscriber()` in `tests.rs` (evaluated immediately
  before each `with_default`), covering both capture sites with one edit.
- Break-to-prove: removed the `error!` at `worktree/src/lib.rs:107`; confirmed
  `delete_branch_guard_refusal_logs_error` fails (ERROR==1 got 0), restored.

### Deviations
- **Evidence methodology deviated from the doc's raw loop.** The doc's loop
  counted exit codes only (output to /dev/null). That could not distinguish the
  tracing flake from unrelated failures, so I re-ran with per-failure output
  captured and classified. This surfaced that worktree's raw baseline was
  tracing + a separate seq-race. Clean classified result: worktree tracing flake
  31/400 (~7.8%) pre-fix -> 0/400 post-fix; loopr 0/300 (latent, as the doc
  anticipated with "null result accepted"). Recorded in the doc Evidence
  addendum.

### Tradeoffs
- Routed worktree via `json_subscriber` rather than adding a `set_capturing_default`
  wrapper, because `with_default` (scoped) can't use the guard-returning shape;
  calling the helper inside the subscriber-builder is the minimal DRY route that
  still runs before `with_default`.

### Open questions
- **Separate pre-existing flake discovered:** `worktree::handle::tests::
  concurrent_creates_for_same_work_id_all_get_distinct_seqs` races ~1%
  independently of this work (a `Worktree::create` seq-allocation race under
  10-way concurrency). OUT OF SCOPE for this doc; recommend a separate
  targeted-fix / design-doc triage. Not fixed here.

## Phase 4: total sweep of remaining sites

### Design decisions
- Re-verified the installer inventory at HEAD: `crates/llm/tests/span.rs` is the
  only in-process global-default installer; added the fixed-shape
  `// interest-cache exempt: ...` comment at its `.init()` site.
- Routed every remaining raw `set_default`/`with_default` capture site through
  `telemetry::ensure_global_interested_default()`: the 6 crate
  `tests/instrumentation.rs` binaries (tools, context, decomposer, integrator,
  store, agents, worktree), and telemetry's own `src/tests.rs` (12 sites, via a
  one-line prepend before each `with_default`). All 7 crates already dep
  telemetry, so no new dev-deps beyond store's (Phase 2).
- `init_for_test`-based capture files (`telemetry/tests/events_log_contract.rs`,
  `loopr/tests/work_plan_summary_visibility.rs`) get a comment noting they are
  covered via `init_for_test` (which calls the helper), satisfying the audit.

### Deviations
- None. The audit command exits 0; workspace `otto ci` green.

### Tradeoffs
- Routed rather than exempted the low-risk single-test instrumentation binaries,
  honoring the doc's "siblings behave identically" — cheap since every crate
  already deps telemetry.

### Open questions
- Workspace `otto ci` can still flake ~1% on the out-of-scope seq-race above; a
  green run confirms the tracing work, but the seq-race remains a separate
  follow-up.
