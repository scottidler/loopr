# Loopr v5 Roadmap

**Status:** living index of design docs that need to exist. Each stage has an exit criterion that must pass before the next stage begins. Design docs inside each stage are placeholders until their stage's time comes; actual content is written motivated by the failing run that stage exists to fix.

**Rule #1 preserved.** This file enumerates *where design docs will go*, not their contents. A placeholder is not a spec.

**Build direction: loopr outward.** Stage 1 gets a working binary (`loopr --version`). Each later stage teaches that binary one new trick. First gate (Stage 9) reproduces v4's single successful E2E with v5's typed pipeline.

---

## Stage 0: scaffold compiles

**Status:** done on orphan branch `v5`; tag `v0.5.0` cut at the initial 6-crate scaffold, additional crates landed after tag on branch.

**What shipped:** 9-crate workspace (`derive`, `telemetry`, `domain`, `runtime`, `decomposer`, `agents`, `integrator`, `ipc`, `loopr`), README + CLAUDE.md front-door language, `.loopr-source-guard` sentinel, deferred-enhancements section in `vision.md`, observability + target-repo-layout sections in `vision.md`, `[workspace.dependencies]` starter set (tracing, serde, eyre, clap, chrono; promoted via `cargo add`-then-move).

---

## Stage 1: `loopr --version` works

**Goal:** the binary compiles, clap parses, `loopr --version` prints `GIT_DESCRIBE`, `loopr --help` shows a stub.

**Design docs:**
- `crates/loopr/docs/design/cli-skeleton.md` — clap structure, subcommand layout, `-C` flag semantics, source-guard check flow.

**Crates touched:** `loopr` only.

**Exit criterion:** `cargo install --path crates/loopr && loopr --version` prints the current tag; `loopr -C /tmp plan "x"` errors cleanly (no subcommand implementation yet, but CLI parse works).

---

## Stage 2: telemetry initialized

**Goal:** tracing subscriber lives, writes structured JSON to `.loopr/runs/<run-id>/events.log`, pretty output to `.loopr/runs/<run-id>/loopr.log`, and console mirror at INFO+ for interactive runs. `loopr logs tail` shows the latest run. Per-run fanout can be added later; the structured single-file layer must exist before anything else logs.

**Design docs:**
- `crates/telemetry/docs/design/subscriber-layers.md` — subscriber composition (file writer, console formatter), log layout on disk, run-id generation and collision policy (`YYYYMMDD-HHMMSS[-N]`).
- `crates/telemetry/docs/design/span-conventions.md` — standard span names (`stage.<name>`, `ralph.<role>`, `tool.<name>`), required context fields (`run_id`, `plan_id`, `work_id`).

**Crates touched:** `telemetry`, `loopr` (init call).

**Exit criterion:** `loopr --version` emits one span to `.loopr/runs/<run-id>/events.log`, round-trips through `tracing-subscriber::fmt::json`, and `loopr logs tail` can display it.

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

**Goal:** daemon accepts `Plan` requests, opens `.taskstore/` (taskstore git dep), persists the Plan record. `loopr plan "x"` returns the persisted Plan's ID. Second invocation sees the first Plan via `loopr list plans`.

**Design docs:**
- `crates/derive/docs/design/fsm-macro.md` — port `#[derive(Fsm)]` from `loopr-derive` (v4), adapt to v5 naming.
- `crates/derive/docs/design/record-macro.md` — new `#[derive(Record)]` that implements taskstore's `Record` trait (`id()`, `updated_at()`, `collection_name()`, `indexed_fields()`).
- `crates/domain/docs/design/records.md` — `Plan` record type, fields, FSM states, indexed fields.
- `crates/domain/docs/design/store.md` — thin wrapper over taskstore's `Store`, type-safe collection accessors.

**Crates touched:** `derive`, `domain`, `loopr`.

**Exit criterion:** `loopr plan "x" && loopr plan "y" && loopr list plans` shows both plans; `.taskstore/plans.jsonl` has two lines.

---

## Stage 6: decomposer produces a Work DAG

**Goal:** daemon, on receiving a Plan request, runs the decomposer which produces a trivial Work DAG (single Work is fine for now). Work records land in `.taskstore/works.jsonl` with dependencies on the Plan.

**Design docs:**
- `crates/domain/docs/design/hierarchy.md` — Plan/Spec/Phase/Work hierarchy and FSM states, deps semantics. (First-gate scope: flat, start with Plan→Work; Spec/Phase deferred.)
- `crates/runtime/docs/design/llm-client.md` — `LlmClient` trait + Anthropic Messages-API implementation, SSE streaming, cost accounting.
- `crates/runtime/docs/design/context-builder.md` — token-budgeted prompt assembly.
- `crates/decomposer/docs/design/plan-then-decompose.md` — `plan()` and `decompose()` function signatures, default strategy, validation.

**Crates touched:** `domain`, `runtime`, `decomposer`.

**Exit criterion:** `loopr plan "Add --version flag to a Rust CLI"` produces at least one Work record persisted to `.taskstore/works.jsonl`.

---

## Stage 7: Implementer agent produces a Bundle

**Goal:** daemon spawns an Implementer in a sibling worktree, runs a ralph loop with the tool registry, produces a Bundle record referencing the commit on the worktree branch.

**Design docs:**
- `crates/runtime/docs/design/tool-registry.md` — `Tool` trait with typed `Input`/`Output`, registry, first builtin set: `Read`, `Write`, `Edit`, `Bash`, `Grep`, `Glob`.
- `crates/runtime/docs/design/worktree.md` — sibling git worktree creation, cleanup, registry entry in `.loopr/worktree-registry.jsonl`.
- `crates/agents/docs/design/implementer.md` — ralph loop structure, retry strategy selection, bundle production.

**Crates touched:** `runtime`, `agents`.

**Exit criterion:** on a toy target repo, a Work item produces a Bundle whose commit diff shows real file edits made by the Implementer.

---

## Stage 8: Reviewer + Integrator — Bundle becomes Tick

**Goal:** Reviewer agent reads Bundle, produces Verdict. Approved Bundles go through the Integrator (deterministic, non-LLM) which merges into the integration branch and produces a Tick.

**Design docs:**
- `crates/agents/docs/design/reviewer.md` — reviewer ralph loop, verdict types, rejection handling.
- `crates/integrator/docs/design/integrate.md` — merge strategy, conflict surface as typed errors, Tick production.

**Crates touched:** `agents`, `integrator`.

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

---

## See also

- [vision.md](vision.md): architectural shape, ABI contracts, process rules.
- [CLAUDE.md](../CLAUDE.md): project-wide rules and canonical crate map.
- `crates/<name>/docs/design/`: where each stage's design docs land when written.
