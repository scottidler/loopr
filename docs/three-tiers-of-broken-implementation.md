● Inventory. Organized by severity — Tier 1 = code claims something's done that isn't, Tier 2 =
  code is wired but next-stage callers missing, Tier 3 = explicitly deferred but with cascading
  consequences in the current product.

  Tier 1 — Lying-by-omission. Code doesn't do what its name/doc suggests.

  loopr init — scope mismatch

  - crates/loopr/CLAUDE.md:18 says: "loopr init: idempotent per-target setup (create .loopr/, open
   TaskStore, install taskstore git hooks, append to .git/info/exclude, verify source-guard)".
  - What I actually implemented in Phase 4 (crates/loopr/src/commands/init.rs): only seeds
  .loopr/prompts/. None of the other promised work happens. The docstring on Init in cli.rs:51-59
  explicitly admits "Future scope: .loopr/, .loopr/taskstore/, git hooks." So the doc is a lie and
   the user-facing loopr init only does ~20% of what its name promises.

  LooprError carries dead-but-shaped error variants

  - crates/loopr/src/error.rs:16-17, 46-47: StageUnimplemented { stage, subcommand } and
  NotYetImplemented { feature }. StageUnimplemented is now actively used only for daemon-start
  (which is a fork-hoist quirk, not really unimplemented). NotYetImplemented is used only for tui.
   Two error variants whose entire purpose is to gate work that "will be earned later."

  Decomposer transcripts are explicitly NOT wired

  - crates/loopr/CLAUDE.md:69 (rewrote in last commit, accurate now): "decomposer is NOT yet wired
   — decomposer::decompose does not call append_iteration; plans/<plan-id>/decomposition.md will
  not be written until that follow-up lands."
  - Consequence: when decomposition fails or produces weird Works (today's e2e: 1 Work for what
  should be 2), there's no transcript to debug from.

  System-prompt elision is documented as a feature but isn't built

  - crates/loopr/CLAUDE.md:71: "System-prompt elision (the design doc's "leaning yes" open
  question) is not implemented yet; iterations 2..N currently re-render the full system prompt."
  Costs money on every iteration after the first (system prompts are ~2-3 KB redundant tokens
  every iteration).

  Per-record summaries: writers exist, callers don't

  - crates/loopr/CLAUDE.md:79: "the renderer + atomic-write surface ships in this phase;
  per-transition callsite wiring at spawn_implementer_for_work, spawn_reviewer_for_bundle, and
  spawn_integrator_for_bundle remains a focused follow-up." Three fanout sinks needed; only
  BundleUpdateSink exists. So <target>/.loopr/records/works/<id>/summary.md and
  plans/<id>/summary.md never appear.

  Process / session digests: not built at all

  - crates/loopr/CLAUDE.md:81: "Process / session digests (runs/<process-id>/summary.md and
  sessions/<session-id>/summary.md) are not yet built." loopr sessions end and the daemon shutdown
   hook don't write them.

  Daemon dead-code helpers

  - crates/loopr/src/daemon/context.rs:749, 773: transition_and_persist_bundle and
  transition_and_persist_plan are #[allow(dead_code)]. They exist for sweep_bundles to call during
   crash recovery but the call site doesn't exist.
  - crates/store/src/error.rs:15: StoreError::Corruption variant is #[allow(dead_code)]. Designed
  in but never used.

  Tier 2 — Wired but with callers missing or behavior partial.

  v5 has 5 action types; v3 had 8

  - crates/agents/src/action.rs. Missing in v5: read_file, write_file, create_learning. The first
  two are now subsumed by run_tool against builtin tools (read, write, edit). create_learning has
  no replacement — Learning records exist nowhere in v5. So agents that needed to record
  discoveries have nowhere to put them.

  Researcher and Director agents are crate-scoped but absent

  - crates/agents/CLAUDE.md:7-10 lists run_researcher, run_director as in-scope. Neither function
  exists in crates/agents/src/. Only implementer and reviewer are real.
  - docs/roles-and-states.md:59-60: Researcher and Director both marked "deferred." So the
  agents/CLAUDE.md is forward-looking; the docs/roles doc admits it.
  - crates/agents/CLAUDE.md:7: run_director(event: &Event, deps) -> Result<Action> — but Event and
   Action types referenced here don't exist in domain either.

  Reviewer is wired but its role-FSM has no Reject path that recovers

  - docs/roles-and-states.md:182, 207: Two separate "Deferred from First Gate. ... escalations in
  First Gate runs result in exit-with-error" paragraphs. The Director agent is supposed to handle
  reviewer-rejection-then-iterate; without it, a reject verdict from the Reviewer is terminal.

  Multi-tier decomposition (Plan → Spec → Phase → Work) — all the slots, none of the impl

  - crates/context/prompts/decompose/{plan,spec,phase}/.gitkeep — empty dirs reserved by Phase 1.
  - The decomposer crate's decompose is single-tier (Plan → Work); v3 had four-tier. Per
  docs/roadmap.md:174: "Multi-level hierarchy. Plan → Spec → Phase → Work, deeper than the
  first-gate flat list." Beyond first gate.
  - Consequence: complex goals can't be broken into properly-sized chunks. Today's e2e produced 1
  Work for a multi-criterion goal because that's all single-tier supports.

  chat/ and partials/ skeletons exist, no content

  - crates/context/prompts/chat/.gitkeep — empty. v4 had 5 chat prompts (refine, interview,
  default, draft, executing). v5 has none. The TUI / chat-funnel UI is deferred entirely.
  - Only one partial (partials/tools-list.pmt); v4 had more.

  loopr daemon start has a special-case fork hoist

  - crates/loopr/src/lib.rs:181-189: DaemonCmd::Start { .. } => Err(LooprError::StageUnimplemented
   { stage: 4, subcommand: "daemon-start" }). Comment says: "Start is handled above in run
  (pre-telemetry); it never reaches dispatch." So the dispatch arm exists only to satisfy
  exhaustiveness; the real handler is hoisted to a different code path.

  Worktree cleanup policy is configured but partially honored

  - crates/worktree/src/handle.rs — has the policy, but crates/loopr/CLAUDE.md doesn't mention any
   sweep-on-shutdown hook. Sweep happens on reconcile passes only.

  Tier 3 — Explicitly deferred but with present-day fallout.

  Validation in the integrator

  - crates/integrator/CLAUDE.md:25: "Validation command execution. Originally in scope; deferred
  per docs/design/2026-04-22-integrator.md. Reviewer already validates against acceptance
  criteria, so an integration produces a typed Tick as the exit criterion, not a green build."
  - Consequence: a Reviewer can accept a Bundle whose code doesn't compile (the Reviewer is
  text-only, doesn't run cargo check). The integrator merges anyway. v3 didn't have this gap
  because the implementer ran cargo test before proposing.

  Work-only crash recovery missing

  - docs/design/2026-04-22-stage-8-wiring.md:58: "Work-only crash recovery. sweep_bundles recovers
   any Bundle in an intermediate FSM state, but a Work crashed at InProgress before producing a
  Bundle (Implementer crashed mid-run) is not re-enqueued." Today's e2e Blocked Work won't
  auto-recover on next daemon start.

  Plan.decomposition_attempts and bubble_up_count

  - docs/design/2026-04-20-hierarchy.md Open Question, never resolved. These fields exist or don't
   — but the retry-loop logic that would consume them isn't built either way.

  LlmError::Retryable carries a String, not Duration

  - docs/design/2026-04-20-llm-client.md Open Q. Anthropic's 429 responses include a retry-after
  header; v5 ignores it.

  WorkUpdateSink and PlanUpdateSink traits don't exist

  - crates/loopr/CLAUDE.md:79 and Phase 8.5 design doc reference these. Only BundleUpdateSink is
  real. Without the other two, work-status and plan-status changes can't fan out to summary
  writers, telemetry observers, or future event bus consumers.

  TUI — entire frontend is absent

  - loopr (no subcommand) → Command::Tui → LooprError::NotYetImplemented { feature: "tui" }.
  Vision references a future TUI crate; nothing exists.

  E2E success-pattern automation

  - bin/e2e is a shell harness; success/failure detection is heuristic ("watching for a Tick on
  main"). The e2e skill itself documents (docs/design/2026-04-24-prompts-on-disk.md:347) that
  "no-prose-preamble" / "no-double-reads" are observable in transcripts but verified manually. So
  the fix to today's regression cannot be detected automatically; we only know an old-style
  escalation didn't fire.

  Daemon shutdown hook

  - Daemon stops via SIGTERM + escalation to SIGKILL. No graceful "write final summaries / flush
  transcripts" path. The "process / session digests" gap (Tier 1, item 6) flows from this.

  prompt cache / mtime invalidation

  - Phase 2 ships strict-mode handlebars with no cache invalidation; long-running daemons keep the
   same compiled templates until restart. Documented but not built.

  experiment / score / AutoResearch CLI verbs

  - docs/design/2026-04-19-cli-skeleton.md:437: "experiment" was scoped out of Stage 1. Vision
  references it. Nothing exists.

  tools / lane configuration ergonomics

  - docs/design/2026-04-21-tool-registry.md:625: "HEAVY_EXECUTABLES. ... Kept inline in Rust
  source for now (YAML extraction deferred per Scott 2026-04-21)."
  - Per-target config can't override which executables go through the heavy lane.
