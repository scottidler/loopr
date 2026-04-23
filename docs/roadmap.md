# Loopr v5 Roadmap

**Status:** living index of design docs that need to exist. Each stage has an exit criterion that must pass before the next stage begins. Design docs inside each stage are placeholders until their stage's time comes; actual content is written motivated by the failing run that stage exists to fix.

**Rule #1 preserved.** This file enumerates *where design docs will go*, not their contents. A placeholder is not a spec.

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

**Goal:** tracing subscriber lives, writes structured JSON to `.loopr/runs/<run-id>/events.log`, pretty output to `.loopr/runs/<run-id>/loopr.log`, console mirror at INFO+ for interactive runs, and a `WorkFanoutLayer` that splits events into `.loopr/runs/<run-id>/work/<work_id>.log` whenever a `work_id`-scoped span fires. `loopr logs tail` shows the latest run. Fanout is built in Stage 2 but stays inert until Stage 7's first `work_id`-bearing span fires.

**Design doc:** [`docs/design/2026-04-19-telemetry-stage-2.md`](design/2026-04-19-telemetry-stage-2.md) — consolidated subscriber + run-id + span-conventions + `loopr logs tail` body. Originally listed as two separate docs; collapsed because they cannot be reviewed independently (layers define where spans go; conventions define what spans look like).

**Scope caveat:** catches up a Stage 1 omission — the global `--log-level` / `-l` flag that `rules/rust.md` mandates but the CLI skeleton did not add. Documented in the Stage 2 design doc; Stage 1's doc gets a one-line amendment pointer.

**Crates touched:** `telemetry`, `loopr` (CLI flag + run() rewire + logs module).

**Exit criterion:** `loopr -C /tmp plan "x"` allocates a run-id, writes non-empty `events.log` + `loopr.log`, and returns `StageUnimplemented`. `loopr -C /tmp logs tail` prints the pretty log; `loopr -C /tmp logs runs` lists known runs newest-first. (The originally-stated "`loopr --version` emits one span" criterion was unsatisfiable — clap short-circuits `--version` before `lib::run` — so it was replaced.)

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

**Crates touched:** `loopr`, `ipc`, `telemetry`.

**Exit criterion:** two terminals: one runs the daemon, the other runs `loopr plan "x"`; the second terminal gets an ACK and the daemon's log shows the request arriving with the right `run_id`.

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
- Stage 8 wiring capstone — how the daemon connects Implementer -> Coordinator triage -> Reviewer -> Integrator, and honors the "retry Integrating Bundles" contract from the Integrator design (design doc not yet written).

**Crates touched:** `agents`, `domain`, `store`, `context`, later `integrator`.

**Exit criterion:** on the toy target, an approved Bundle lands on the integration branch and produces a Tick record.

---

## Stage 9: FIRST GATE — E2E on rust-version target

**Goal:** reproduce v4's single successful E2E. Target is a Rust CLI repo (create with `scaffold-rust-repo` skill) named `rust-version`. Run:

```
loopr -C ~/repos/scottidler/rust-version plan \
  "Add a --version flag that prints CARGO_PKG_VERSION to stdout"
```

The daemon decomposes, implements, reviews, integrates, and produces a Tick that merges to `main`. The target repo now has a working `--version` flag.

**Design docs:**
- `docs/design/first-gate-e2e.md` — end-to-end happy path, target repo setup, validation script, what counts as "passed."

**Crates touched:** all of them, integration only.

**Exit criterion:** running the above command on a fresh `rust-version` clone produces a merged commit that adds the flag; `rust-version --version` prints the crate version.

---

## Beyond First Gate (earned features)

Not scheduled. Earn each when a real run fails for lack of it. Cross-reference `docs/vision.md` "Deferred Enhancements" and "Explicitly Not in First Gate."

- **TUI** as its own crate. Ratatui app + widgets + event loop. Subscribes to the telemetry stream.
- **Per-Work fanout subscriber.** Split `events.log` by `work_id` span into `.loopr/runs/<run-id>/work/<work-id>.log`.
- **Director agent.** Escalation handling; before this ships, escalation is "exit with error."
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
