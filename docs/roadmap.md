# Loopr v5 Roadmap

**Status:** living index of design docs that need to exist. Each stage has an exit criterion that must pass before the next stage begins. Design docs inside each stage are placeholders until their stage's time comes; actual content is written when the stage is ready to ship - whether that's motivated by a failing run, a known gap, or a coherent next-feature plan.

**Build direction: loopr outward.** Stage 1 gets a working binary (`loopr --version`). Each later stage teaches that binary one new trick. First gate (Stage 9) reproduces v4's single successful E2E with v5's typed pipeline.

---

## Stage 0: scaffold compiles

**Status:** done on orphan branch `v5`; tag `v0.5.0` cut at the initial 6-crate scaffold, additional crates landed after tag on branch.

**What shipped:** 13-crate workspace (`derive`, `telemetry`, `store`, `domain`, `llm`, `tools`, `worktree`, `ipc`, `context`, `decomposer`, `agents`, `integrator`, `loopr`), README + CLAUDE.md front-door language, `.loopr-source-guard` sentinel, full operational decisions in `vision.md` (prompts, errors, models/budgets, git posture, security, observability, target-repo layout), `[workspace.dependencies]` starter set (tracing, serde, eyre, clap, chrono; promoted via `cargo add`-then-move). Architect consultation rounds 1, 2, 3 reconciled: `runtime` junk-drawer split into `store`/`llm`/`tools`/`worktree`; `context` added as a shared prompt-assembly crate so `decomposer` and `agents` don't duplicate templating logic; `tools` stripped of `domain` coupling; `LOOPR_` env prefix restored to prevent subprocess env-var pollution.

---

## Stage 1: `loopr --version` works

**Goal:** the binary compiles, clap parses, `loopr --version` prints `GIT_DESCRIBE`, `loopr --help` shows a stub.

**Design docs:**
- `crates/loopr/docs/design/cli-skeleton.md` — clap structure, subcommand layout, `-C` flag semantics, source-guard check flow.

**Crates touched:** `loopr` only.

**Amendment:** the global `--log-level` / `-l` flag (mandated by `rules/rust.md`) was omitted from the Stage 1 doc and is caught up under Stage 2. See [`docs/design/2026-04-19-telemetry-stage-2.md`](design/2026-04-19-telemetry-stage-2.md).

**Exit criterion:** `cargo install --path crates/loopr && loopr --version` prints the current tag; `loopr -C /tmp plan "x"` errors cleanly (no subcommand implementation yet, but CLI parse works).

---

## Stage 2: telemetry initialized

**Goal:** tracing subscriber lives, writes structured JSON to `$XDG_DATA_HOME/loopr/sessions/<session-id>/targets/<target-slug>/runs/<process-id>/events.log`, pretty output to the sibling `loopr.log`, console mirror at INFO+ for interactive runs, and a `WorkFanoutLayer` that splits events into `runs/<process-id>/work/<work_id>.log` whenever a `work_id`-scoped span fires. A second `SessionFanoutLayer` aggregates session-scoped events at `sessions/<session-id>/targets/<target-slug>/session-fanout.log`. `loopr logs tail` shows the latest process's pretty log. Fanout layers ship in Stage 2 / Phase 7 and run inert until their routing-field spans fire. (XDG-rooted paths and the `session-id`/`process-id`/`target-slug` taxonomy were retrofitted in `docs/design/2026-04-24-loopr-layout.md`.)

**Design doc:** [`docs/design/2026-04-19-telemetry-stage-2.md`](design/2026-04-19-telemetry-stage-2.md) — consolidated subscriber + run-id + span-conventions + `loopr logs tail` body. Originally listed as two separate docs; collapsed because they cannot be reviewed independently (layers define where spans go; conventions define what spans look like).

**Scope caveat:** catches up a Stage 1 omission — the global `--log-level` / `-l` flag that `rules/rust.md` mandates but the CLI skeleton did not add. Documented in the Stage 2 design doc; Stage 1's doc gets a one-line amendment pointer.

**Crates touched:** `telemetry`, `loopr` (CLI flag + run() rewire + logs module).

**Exit criterion:** `loopr -C /tmp plan "x"` resolves a session-id and allocates a process-id, writes non-empty `events.log` + `loopr.log` under the XDG session/target/runs path, and returns `StageUnimplemented`. `loopr -C /tmp logs tail` prints the pretty log; `loopr -C /tmp logs runs` lists known sessions newest-first. (The originally-stated "`loopr --version` emits one span" criterion was unsatisfiable — clap short-circuits `--version` before `lib::run` — so it was replaced.)

---

## Stage 3: ipc messages round-trip

**Goal:** `Request`, `Response`, `Event` enums defined; serde framing chosen (length-prefixed JSON or newline-delimited JSON); round-trip tests pass for every variant.

**Design docs:**
- `crates/ipc/docs/design/protocol.md` — message taxonomy, tagging scheme, framing choice, versioning/compat rules.

**Crates touched:** `ipc`, and eventually `domain` (messages reference record types).

**Exit criterion:** `cargo test -p ipc` passes a round-trip test per message variant and a framing test (bytes in, message out, message in, bytes match).

---

## Stage 4: daemon fork + IPC transport

**Goal:** `loopr daemon start` forks-to-daemon, binds `.loopr/socket`, writes `.loopr/daemon.pid`. A client invocation (`loopr plan "x"`) finds the socket, connects, sends a `Plan` request, receives an ACK, exits. Daemon logs the receipt. No business logic yet.

**Design docs:**
- `crates/loopr/docs/design/daemon-lifecycle.md` — fork-to-daemon, PID lock, stale-socket detection, client connect-or-fork-daemon logic.
- `crates/loopr/docs/design/ipc-transport.md` — async socket acceptance, connection lifecycle, serde framing bridge to the `ipc` crate.
- [`design/2026-05-09-ipc-timeouts.md`](design/2026-05-09-ipc-timeouts.md) — bounded waits on every hangable path: client request, server read-idle, server write, daemon startup. Operator overrides via `transport:` section in `.loopr/config.yml`.

**Crates touched:** `loopr`, `ipc`, `telemetry`.

**Exit criterion:** two terminals: one runs the daemon, the other runs `loopr plan "x"`; the second terminal gets an ACK and the daemon's log shows the request arriving with the right `session_id` and `client_session_id`.

---

## Stage 5: daemon persists Plan via TaskStore

**Goal:** daemon accepts `Plan` requests, opens `.loopr/taskstore/` (taskstore git dep), persists the Plan record. `loopr plan "x"` returns the persisted Plan's ID. Second invocation sees the first Plan via `loopr list plans`.

**Design docs:**
- [`docs/design/2026-04-20-fsm-macro.md`](design/2026-04-20-fsm-macro.md) — `#[derive(Fsm)]` revived from v3's `loopr-derive` (v4 had deleted it for a YAML runtime that v5 also rejects). Located at the repo-root `docs/design/` rather than the crate's own `docs/design/` because it touches two crates (`derive` emits the macro; `domain` hosts the runtime support types `Transition`, `FsmError<S>`, `FsmErrorKind`, `TargetKind`, `Role`). Shipped in v0.5.8 (v0.5.9 followed with a validator tightening from Architect audit). Status: Implemented.
- [`crates/derive/docs/design/2026-04-20-record-macro.md`](design/2026-04-20-record-macro.md) — `#[derive(Record)]` that implements taskstore's `Record` trait (`id()`, `updated_at()`, `collection_name()`, `indexed_fields()`). Shipped in v0.5.10. Status: Implemented.
- [`crates/domain/docs/design/2026-04-20-records.md`](design/2026-04-20-records.md) — `Plan` record type (five fields), `PlanStatus` FSM (six states, v4's hierarchy.yml verbatim), `PlanId` typed newtype, `id_type!` macro_rules for stamping out future record IDs. Shipped in v0.5.11. Status: Implemented.
- [`crates/store/docs/design/2026-04-20-store.md`](design/2026-04-20-store.md) — `Store` wrapper + `PlansStore<'_>` async accessor over `taskstore_async::AsyncStore` (Stage 5 scope: plans only, `create` / `get` / `list`, plus `close()` for safe shutdown). Shipped in v0.5.12. Status: Implemented.

**Crates touched:** `derive`, `domain`, `loopr`.

**Exit criterion:** `loopr plan "x" && loopr plan "y" && loopr list plans` shows both plans; `.loopr/taskstore/plans.jsonl` has two lines.

**Init carry-over (Stage 6 dependency):** `loopr init` is still `StageUnimplemented` at the end of Stage 5 (v0.5.16). Stage 6's scope memo ([`docs/design/2026-04-20-stage-6-scope.md`](design/2026-04-20-stage-6-scope.md)) settles that `init` must write `.loopr/config.yml` (from a canonical template at `resources/config/default.yml`) and `.loopr/prompts/**/*.pmt` (materialized via `include_dir!()`). Stage 6 code falls back to baked-in defaults when the target has not been init'd so unit tests and ad-hoc commands do not require init, but the user-facing "edit your prompts" flow depends on init actually writing them. Land init's real body alongside or just before Stage 6's execution; either way, track it under Stage 5 scope so Stage 6 is not gated on a second feature.

---

## Stage 6: decomposer produces a Work DAG

**Goal:** daemon, on receiving a Plan request, runs the decomposer which produces a trivial Work DAG (single Work is fine for now). Work records land in `.loopr/taskstore/works.jsonl` with dependencies on the Plan.

**Scope memo:** [`docs/design/2026-04-20-stage-6-scope.md`](design/2026-04-20-stage-6-scope.md) — gates the design docs below. Architect consulted (round 1, 2026-04-20); final decision matrix locks records shape (`Work.parent_id: PlanId`), FSM shape (v4's 10-state `WorkStatus` matching v5 `PlanStatus` symmetry — NOT v3's asymmetric 9), LLM client shape (tool-use only, buffered, generics not `dyn`), and the v3→v5 lineage (v4's agent-harness decomposer rejected in favor of v3's standalone function).

**Design docs (3, not 4; `context-builder.md` deferred to Stage 7 per scope memo D11):**
- `crates/domain/docs/design/2026-04-20-hierarchy.md` — `Work` record + `WorkStatus` FSM (10 states, via `#[derive(Fsm)]`); flat `Plan → Work` scope, Spec/Phase deferred.
- `crates/llm/docs/design/2026-04-20-llm-client.md` — `LlmClient` trait (tool-use only, buffered), Anthropic backend, typed `Retryable` / `Fatal` error enum, `.loopr/config.yml`-sourced model/tokens/temperature, API-key precedence CLI > env > config-env-name.
- `crates/decomposer/docs/design/2026-04-20-plan-then-decompose.md` — `async fn decompose<L: LlmClient>(plan, target, llm) -> Result<Vec<Work>>`, tool-schema + text fallback, title-based deps with server-side id map, cycle detection, workspace-file-tree injection via `git ls-files`, zero-children bails.

**Crates touched:** `domain`, `store`, `llm`, `decomposer`.

**Exit criterion:** `loopr plan "Add --version flag to a Rust CLI"` produces at least one Work record persisted to `.loopr/taskstore/works.jsonl`.

---

## Stage 7: Implementer agent produces a Bundle

**Goal:** daemon spawns an Implementer in a sibling worktree, runs a ralph loop with the tool registry, produces a Bundle record referencing the commit on the worktree branch.

**Design docs:**
- `crates/tools/docs/design/tool-registry.md` — `Tool` trait with typed `Input`/`Output`, registry, first builtin set: `Read`, `Write`, `Edit`, `Bash`, `Grep`, `Glob`, plus the lane classification table.
- `crates/worktree/docs/design/lifecycle.md` — sibling worktree creation, cleanup, registry entry in `.loopr/worktree-registry.jsonl`, daemon-startup crash reconciliation.
- `crates/agents/docs/design/implementer.md` — ralph loop structure, retry strategy selection, bundle production.

**Crates touched:** `tools`, `worktree`, `agents`.

**Exit criterion:** on a toy target repo, a Work item produces a Bundle whose commit diff shows real file edits made by the Implementer.

---

## Stage 8: Reviewer + Integrator — Bundle becomes Tick

**Goal:** Reviewer agent reads Bundle, produces Verdict. Approved Bundles go through the Integrator (deterministic, non-LLM) which merges into the integration branch and produces a Tick.

**Design docs:**
- [`docs/design/2026-04-22-reviewer.md`](design/2026-04-22-reviewer.md) — Reviewer ralph loop (single turn + parse retry), typed `Verdict` with structured `ReviewIssue` reasons, OCC-aware `BundlesStore::update`, `build_for_reviewer` prompt assembly. **Implemented** 2026-04-22.
- [`docs/design/2026-04-22-integrator.md`](design/2026-04-22-integrator.md) — Integrator deterministic non-LLM merge path: pre-flight + Phase 2 git sequence with Phase 2 prologue (`Accepted -> Integrating`) + Phase 3 batched commit. Tick record in `domain`, `TicksStore` with `DuplicateTick` detection, crash-recovery via `Integrating` re-entry + `git merge-base --is-ancestor`. Validation deferred. **Implemented** 2026-04-22.
- [`docs/design/2026-04-22-stage-8-wiring.md`](design/2026-04-22-stage-8-wiring.md) — Stage 8 wiring capstone: handle_plan_create creates loopr/plan-<id> before any store write, spawn_reviewer_for_bundle / spawn_integrator_for_bundle chain inline via per-stage JoinSets, retry-with-circuit-breaker on Integrating Bundles, Bundle-FSM sweep in daemon startup reconcile. Adds `WorkStatus: InReview => Blocked by Coordinator` override edge to domain. **Implemented** 2026-04-22.

**Crates touched:** `agents`, `domain`, `store`, `context`, later `integrator`.

**Exit criterion:** on the toy target, an approved Bundle lands on the integration branch and produces a Tick record.

---

## Stage 9: FIRST GATE — E2E on rust-version target

**Status (2026-05-30): PASSED — with one caveat about `main`.** Run on v0.7.21 via the `/e2e` skill against a freshly scaffolded `rust-version` target. Fully autonomous, end-to-end in ~55s: the daemon decomposed Plan `pl-tsaql` into one Work (`wk-421n5`), the Implementer produced Bundle `bd-2a773`, the Reviewer approved it, and the Integrator merged it (merge commit `447bb21`) onto the **per-Plan integration branch `loopr/plan-pl-tsaql`**, producing Tick `tk-wrmlv`; the Plan reached `complete`. Verification ran against that integration branch (HEAD): the `loopr`-built binary prints `0.1.0` for `--version` and `cargo test` passes 2/2 (`version_flag_prints_cargo_pkg_version`, `no_args_prints_hello_world`).

**Caveat:** `main` itself stayed at `init` and was *not* advanced. v5's integrator deliberately terminates on the per-Plan integration branch (`crates/integrator/src/lib.rs:223` builds `loopr/plan-{id}`; the crate is "merge-publish Accepted Bundles into Tick") and has **no merge-to-main step** anywhere in the pipeline. So the Stage 9 **Goal** prose below ("produces a Tick that merges to main") is not literally satisfied — promoting the integration branch to `main` is an unimplemented (or deliberately out-of-pipeline) final step. The **exit criterion** below (a merged commit that adds the flag + `--version` prints the version + session indexes the Plan/Work/Bundle/Tick ids) is met; the "merges to `main`" clause in the Goal is the only gap. Decide whether main-promotion belongs in the pipeline or is a separate operator gate, and reconcile the Goal prose accordingly.

The Tier-1 "First Gate completion gaps" that earlier runs surfaced (the python-api run on non-trivial targets) all shipped first — dependency-DAG gate, Director Phase 1 routine orchestration, blocked-Work / rejected-Bundle recovery, and post-merge validation; see [deferred-roadmap.md](deferred-roadmap.md) Tier 1. `first-gate-e2e.md` was never written; the live procedure is the `/e2e` skill.

**Goal:** reproduce v4's single successful E2E. Target is a Rust CLI repo (create with `scaffold-rust-repo` skill) named `rust-version`. Run:

```
loopr -C ~/repos/scottidler/rust-version plan \
  "Add a --version flag that prints CARGO_PKG_VERSION to stdout"
```

The daemon decomposes, implements, reviews, integrates, and produces a Tick that merges to `main`. The target repo now has a working `--version` flag.

**Design docs:**
- `docs/design/first-gate-e2e.md` — end-to-end happy path, target repo setup, validation script, what counts as "passed."
- [`docs/design/2026-04-26-scoped-staging.md`](design/2026-04-26-scoped-staging.md) — scoped staging for `commit_changes` and `propose_bundle`. Replaces unconditional `git add -A` with `git status --porcelain --untracked-files=all` + scope-partition + `git commit --only`; adds per-Work `files` allow-list emitted by the decomposer; populates `bundle.paths` from the branch-vs-base diff. Defense-in-depth against the 2026-04-26 `python-api` `.venv/` regression. **Implemented** 2026-04-27.

**Crates touched:** all of them, integration only.

**Exit criterion:** running the above command on a fresh `rust-version` clone produces a merged commit that adds the flag; `rust-version --version` prints the crate version; and a `loopr sessions list` run against the target shows an active session whose `session-fanout.log` references the Plan, Work(s), Bundle, and Tick ids emitted by the pipeline (i.e., session-id indexes the E2E's records). Per the 2026-04-24 instrumentation-sweep doc, every non-trivial function in every crate touched by the run carries `#[tracing::instrument]` per `rules/log.md`; reviewing a failed run reads the events.log without restarting the daemon at `-l debug`.

---

## Cross-cutting exit criterion (every stage)

`#[tracing::instrument]` coverage on non-trivial public + crate-private functions ≥95%, scope-field discipline matches each crate's `CLAUDE.md` "Instrumentation" section, per-crate acceptance test asserts representative span names. Added by the 2026-04-24 instrumentation-sweep design doc; applies retroactively to every existing stage and forward to every future stage.

## Beyond First Gate (earned features)

Not scheduled. Earn each when a real run fails for lack of it. Cross-reference `docs/vision.md` "Deferred Enhancements" and "Explicitly Not in First Gate."

- **TUI** as its own crate. Ratatui app + widgets + event loop. Subscribes to the telemetry stream; can attach to or detach from a session.
- **Per-Work fanout subscriber.** Split `events.log` by `work_id` span into `<run-dir>/work/<work-id>.log`. (Shipped in Stage 2; listed here historically.)
- **Director agent. Shipped** (Phase 1 v0.7.11; Phase 2 v0.7.17–v0.7.20; test-hardening v0.7.21). A long-lived per-Plan Opus agent that owns the orchestration plane. **Phase 1** (routine orchestration): Blocked-Work recovery, Reviewed-Bundle acceptance policy, goal-completion audit, poll-based state summary via `context::build_for_director`, typed `DirectorAction` vocabulary, 3-restart story. **Phase 2** (judgment plane): deterministic stuck-state recovery sweep, cross-iteration pattern tracker, four-mode FSM (Normal / Conservative / NeedsOperator), operator chat (`loopr director chat`) + `OperatorNote`/`NotesStore`, `plan.override` + `director.status` verbs, and `NeedsOperator → Stalled` grace. Note: Phase 1 was actually a *gate-completion prerequisite* (deferred-roadmap Tier 1.2), not a post-gate feature — it is listed here because the original roadmap framed it as one. See [design/2026-05-08-director-phase-1.md](design/2026-05-08-director-phase-1.md), [design/2026-05-09-director-phase-2.md](design/2026-05-09-director-phase-2.md), and deferred-roadmap Tier 3.1. Before it shipped, escalation was "exit with error."
- **Researcher agent.** Tool/info discovery; before this ships, the Implementer does its own lookup.
- **Parallel worktrees.** Multiple Works running simultaneously.
- **AutoResearch harness.** Config sweeping + scoring, not YAML-composed orchestration.
- **Multi-level hierarchy.** Plan → Spec → Phase → Work, deeper than the first-gate flat list.
- **Typed event bus** (Anthropic leaked primitive #6). Subscribers react to state-change events instead of polling TaskStore.
- **Supersession over deletion** (Cloudflare pattern). Record revisions keep forward pointers.
- **Graph memory for record recall** (Cersei's Grafeo pattern). Indexed lookups at μs scale vs. LLM-based rank.
- **LLM response cache** at `~/.local/share/loopr/llm-cache/`. Cross-repo prompt-hash dedup.
- **Global runs-index** at `~/.local/share/loopr/runs-index.jsonl`. Cross-repo index enabling `loopr runs list --all`.
- **`.pmt`-file prompt migration** for every role's system prompt. Moves the current inline Rust constants (`context::implementer::render_system_prompt`, `context::REVIEWER_SYSTEM_PROMPT`, and whatever decomposer/researcher/director add) to `.pmt` files resolved through the three-layer override chain `.loopr/prompts/` → `~/.config/loopr/prompts/` → baked-in via `include_dir!()`, with handlebars-rust as the templating engine. Committed in principle by `crates/context/CLAUDE.md` and vision.md "Prompts" section; deferred in practice until real-run signal makes "edit prompt, cargo install, rerun" painful enough to earn the handlebars + loader + init-seeding subsystem. Motivating signal: you are iterating prompts across ≥2 targets and recompiling to tweak a word feels like a bug. Tracked per-role with `TODO(pmt-migration)` markers in the source. See the last Open Question in [design/2026-04-22-reviewer.md](design/2026-04-22-reviewer.md).

---

## See also

- [vision.md](vision.md): architectural shape, ABI contracts, process rules.
- [CLAUDE.md](../CLAUDE.md): project-wide rules and canonical crate map.
- `crates/<name>/docs/design/`: where each stage's design docs land when written.
