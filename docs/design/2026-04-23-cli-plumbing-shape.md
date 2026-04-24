# Design Document: CLI Plumbing Shape

**Author:** Scott A. Idler
**Date:** 2026-04-23
**Status:** Implemented
**Review Passes Completed:** 5/5 + 2 architect rounds
**Crates touched:** loopr, ipc

## Summary

Rework the `loopr` CLI into a minimal plumbing surface: nine one-shot verbs useful from scripts, plus bare `loopr` as the porcelain TUI launcher. Delete the stage-manual subcommands (`decompose`, `execute`, `integrate`, `score`) that the auto-chaining daemon made vestigial. Add `--output {json|yaml}` with TTY-based auto-detection for every data-returning verb. Add two generic IPC methods (`RecordList`, `RecordGet` with a `RecordKind` enum) so every taskstore read goes through the daemon, guaranteeing read-after-write consistency. `RecordList` returns lightweight summary projections (id, title, status, updated_at) to keep list responses well under the IPC 1 MiB frame cap; `RecordGet` returns the full record for detail views. Establish plumbing-vs-porcelain as an explicit Process Rule: the CLI is plumbing; the TUI (when it lands) is porcelain.

## Problem Statement

### Background

The Stage 1 CLI skeleton (`docs/design/2026-04-19-cli-skeleton.md`, shipped 2026-04-19) froze a twelve-subcommand surface: `init`, `plan`, `decompose`, `execute`, `integrate`, `daemon {start,stop,status}`, `score`, `logs {tail,runs}`, `list`. The mental model at the time was user-driven stage gating: the user would submit a Plan, then later invoke `decompose`, then later invoke `execute`, and so on.

By Stage 7-8 (shipped 2026-04-22, `docs/design/2026-04-22-stage-8-wiring.md`), the daemon was built around a different model. `PlanCreate` auto-chains: one IPC call submits a goal; the daemon runs decomposer, spawns Implementer per Work, spawns Reviewer per Bundle, spawns Integrator per approved Bundle, and produces a Tick. No stage-by-stage user intervention.

The CLI was never reconciled. `decompose`, `execute`, `integrate`, `score`, and most `list` kinds are still `StageUnimplemented` stubs - and under the auto-chain model they will never have sensible bodies. They are architectural zombies from a mental model that got replaced.

A separate but related issue: the TUI (a Ratatui app) is planned as a future crate (`docs/vision.md` §Explicitly Not in First Gate; `docs/roadmap.md` §Beyond First Gate). The TUI will be the primary user experience. No principle currently distinguishes what belongs in the CLI from what belongs in the TUI, and the existing CLI carries artifacts of a "CLI is the UI" mental model that predates the TUI decision.

### Problem

1. **Dead verbs.** Five subcommands and three `list` kinds will never gain bodies under the daemon's auto-chain model. They clutter `--help`, confuse new readers, and force every later design doc to decide whether to address them or route around them.
2. **No plumbing-vs-porcelain principle.** Without an explicit rule, each future CLI decision ("should `loopr retry <id>` exist?") is argued from scratch. The TUI crate, when it lands, will inherit the confusion.
3. **Ergonomics were not a v5 goal.** The v5 rebuild focused on crate separation; the CLI shape was assembled from Stage 1 plus amendments (`--log-level` from Stage 2, `--worktree-cleanup` from worktree lifecycle). The accumulation reads as disjoint, with globals of varying importance at the top level and subcommands of varying body-completeness mixed together.
4. **No machine-parseable output.** `list plans` prints a human-friendly two-column format. Scripting consumers (CI, git hooks, future AutoResearch harness) have no stable parseable form.
5. **No typed read seam to the daemon.** Only `PlanList` exists on the IPC wire, and the CLI's other `list` kinds are stubs. Reading any record type from a script today requires shelling out to `jq` on the raw JSONL. A proper plumbing surface needs a typed query path.

### Goals

- Reduce the CLI to verbs that have a real user story *outside* the TUI: scripts, cron jobs, CI, git hooks, one-off shell invocations.
- Codify "CLI is plumbing, TUI is porcelain" as a Process Rule governing this and all future CLI/TUI decisions.
- Add `--output {json|yaml}` as a global flag with TTY-aware default (JSON when stdout is a pipe, YAML when stdout is a TTY) for every data-returning verb.
- Make bare `loopr` (no subcommand) the porcelain entry point: launches the TUI when the TUI crate lands, returns a clear `NotYetImplemented` error until then. `loopr tui` is the explicit sibling form.
- Route all CLI commands that need taskstore state (`plans`, `works`, `bundles`, `ticks`, `show`, `plan`, `daemon status`) through IPC. Route commands that only inspect filesystem artifacts (`logs tail`, `logs runs`) directly against the filesystem; they do not touch the taskstore and do not need daemon coordination.
- Add two generic IPC methods: `RecordList(RecordKind)` returning summary projections, and `RecordGet(id)` returning full records, both with sum-type results so every record kind (Plan today; Spec/Phase later) is served by the same typed seam.
- Keep `RecordList` responses safely under the IPC `MAX_LINE_BYTES` 1 MiB frame cap at mature-repo scale by returning compact summary structs (id, title/goal, status, updated_at) instead of full records.
- Delete the `--worktree-cleanup` global CLI flag (it is already dead code; nothing consumes it).
- Drop the `[Stage N]` markers from subcommand help text (teaching-tool artifacts from the Stage 1 skeleton).

### Non-Goals

- **Building the TUI.** The TUI is a separate crate, a separate design doc, and earned after first gate. This doc commits only to the launcher seam: bare `loopr` and `loopr tui` both return `NotYetImplemented` until the TUI crate ships.
- **Adding Spec/Phase support.** Those records do not exist in v5 yet (`docs/vision.md` §Non-Goals; Stage 6 scope memo). When they land, the only CLI surface changes are two new `RecordKind` variants, two new arms in each result sum, and two new verb names (`specs`, `phases`). No structural work.
- **Adding interactive management verbs** (`retry`, `approve`, `reject`, `cancel`). Those are TUI keybindings, not CLI plumbing.
- **Building the `loopr init` body.** `init` still returns `StageUnimplemented` as of this doc's baseline (roadmap Stage 5 carry-over). Lighting it up is a separate design doc; this doc only removes the `[Stage N]` marker from its help text.
- **Falling back to direct taskstore reads when the daemon is down.** Post-crash forensics use `jq` on `.loopr/taskstore/*.jsonl` directly; that path already exists and needs no loopr code.
- **Streaming-response IPC methods.** `logs tail` would naturally want one but goes direct-FS instead; introducing streaming framing to the IPC protocol is out of scope here.
- **Pagination on `RecordList`.** Summary projections (see Architecture) keep the payload well under the 1 MiB frame cap through any realistic v5 repo scale. Pagination becomes a real need only when repos routinely hold thousands of records of a single kind; deferred until that signal arrives.
- **Backwards compatibility for removed verbs.** v5 has no coexistence migrations (`docs/vision.md` §Process Rules #3). Deleted verbs are deleted in one commit.
- **Changing crate boundaries.** All changes live inside `crates/loopr` and `crates/ipc`.

## Proposed Solution

### Overview

The final CLI surface is nine plumbing verbs plus bare `loopr`:

```
loopr init                               # bootstrap target (body is separate design doc)
loopr daemon {start|stop|status}         # lifecycle + health check
loopr plan "<goal>"                      # submit; daemon auto-chains
loopr plans | works | bundles | ticks    # list snapshots via IPC
loopr show <id>                          # prefix-typed detail via IPC
loopr logs {tail|runs}                   # pretty log stream + run index, direct filesystem
loopr tui                                # explicit porcelain entry
loopr                                    # bare: same as `loopr tui`
```

Globals: `-C <path>`, `-l <level>` / `--log-level`, `-o <fmt>` / `--output`. The `--worktree-cleanup` flag is deleted; its current definition in `cli.rs` is already dead code (parsed by clap, never read by any consumer), so its removal is a factual cleanup, not a behavior change. Its resolution chain (`.loopr/config.yml` < `LOOPR_WORKTREE_CLEANUP_POLICY` env) is unchanged.

### Architecture

Two architectural rules govern the surface.

**Plumbing vs. Porcelain.** A verb belongs in the CLI only if it has a real user story outside an interactive UI: scripts, cron, CI, git hooks, one-off shell invocations. Interactive management flows (retry, approve, reject, inspect-with-context, live-watch) live in the TUI as keybindings, not in the CLI as verbs. Without this rule, every future CLI addition restarts the debate from zero; with it, the question is always "what's the scripting case?" and an absent answer is the answer.

**Sub-principle: one seam for state, direct for artifacts.** Any command that reads or writes taskstore state goes through the daemon over IPC. This gives read-after-write consistency (the same process owns the SQLite cache, the JSONL append, and the in-memory record layer) and avoids the concurrent-writer hazard of two `AsyncStore` instances against the same `.loopr/taskstore/`. Commands that only touch filesystem artifacts independent of the taskstore (pretty log files under `.loopr/runs/`, the runs directory listing) read direct from disk; they are not state, they are artifacts, and the concurrency concern does not apply.

A practical consequence: the daemon must be running for `plans`, `works`, `bundles`, `ticks`, `show`, and of course `plan`. If it is not, those verbs error cleanly with "daemon not running at `.loopr/socket`; run `loopr daemon start`." No auto-start, no fallback, no magic. `logs tail`, `logs runs`, and `init` work without a daemon.

#### File layout inside `crates/loopr/src/`

```
cli.rs                   clap structs; shrunk subcommand set
output.rs                new: Format enum + tty auto-detect + render helpers
lib.rs                   dispatch match; all state-ful verbs go through transport
commands/                new module dir: one file per verb body
  init.rs                bootstrap target (stubbed until init's design doc)
  plan.rs                IPC PlanCreate
  list.rs                IPC RecordList for plans/works/bundles/ticks
  show.rs                IPC RecordGet with prefix-typed id
  logs.rs                moved from existing logs.rs; direct-FS bodies
  daemon.rs              start/stop/status
  tui.rs                 NotYetImplemented stub until TUI crate lands
```

Module extraction is motivated by `lib.rs` currently being 335 lines with only two real command bodies; adding four list verbs plus `show` pushes it past the single-word-module principle from `rules/rust.md`.

#### Global flag handling

`Cli` gains one field and loses one:

```rust
// Added
#[arg(short = 'o', long = "output", global = true, value_name = "FORMAT")]
pub output: Option<Format>,

// Deleted (was already dead code)
// pub worktree_cleanup: Option<AttemptCleanupPolicy>,
```

`AttemptCleanupPolicy` remains a config-layer concern. Its precedence becomes env > config > default (the dead CLI layer is dropped).

#### Bare invocation

`Cli::command` changes from required to `Option<Command>`. Dispatch normalizes `None` to `Command::Tui`:

```rust
let command = cli.command.unwrap_or(Command::Tui);
```

Clap continues to short-circuit `--help` and `--version` before dispatch, so they print help/version without attempting to launch the TUI.

### Data Model

#### Output format

```rust
// crates/loopr/src/output.rs
use std::io::IsTerminal;
use serde::Serialize;

#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum Format {
    Json,
    Yaml,
}

impl Format {
    /// Resolves the effective format: explicit flag wins, else TTY-based default.
    /// TTY (interactive) => Yaml; pipe/redirect (scripting) => Json.
    pub fn resolve(explicit: Option<Format>) -> Format {
        explicit.unwrap_or_else(|| {
            if std::io::stdout().is_terminal() {
                Format::Yaml
            } else {
                Format::Json
            }
        })
    }
}

pub fn render<T: Serialize>(value: &T, fmt: Format) -> Result<String, OutputError> {
    match fmt {
        Format::Json => serde_json::to_string_pretty(value).map_err(OutputError::Json),
        Format::Yaml => serde_yaml::to_string(value).map_err(OutputError::Yaml),
    }
}

#[derive(thiserror::Error, Debug)]
pub enum OutputError {
    #[error("json render: {0}")]
    Json(#[from] serde_json::Error),
    #[error("yaml render: {0}")]
    Yaml(#[from] serde_yaml::Error),
}
```

`Format` uses clap's `ValueEnum` derive so `--output json` and `--output yaml` parse automatically. The YAML crate choice is whatever `cargo add serde_yaml` resolves to at implementation time (pick the currently maintained fork, per `rules/rust.md`'s "never add deps from training memory" rule).

#### IPC additions

Two new methods, plus the `RecordKind` discriminator, summary projections for list responses, and two sum-type results. Adjacent tagging (`#[serde(tag, content)]`) on the result enums so tuple variants destructure cleanly:

```rust
// crates/ipc/src/method.rs additions

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RecordKind {
    Plan,
    Work,
    Bundle,
    Tick,
    // Spec, Phase added when those records land
}

pub enum Method {
    // existing
    Handshake(HandshakeParams),
    Status,
    PlanCreate(PlanCreateParams),
    // added
    RecordList(RecordListParams),
    RecordGet(RecordGetParams),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordListParams {
    pub kind: RecordKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecordGetParams {
    pub id: String,
}
```

**Summary projections (new; list-only).** `RecordList` responses do NOT return full records. They return lightweight summaries built by projection in the daemon's handler. This keeps even pathological repos (hundreds of mature Works or Bundles) well under the 1 MiB frame cap:

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanSummary {
    pub id: PlanId,
    pub goal: String,
    pub status: PlanStatus,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkSummary {
    pub id: WorkId,
    pub parent_id: PlanId,
    pub title: String,
    pub status: WorkStatus,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BundleSummary {
    pub id: BundleId,
    pub work_id: WorkId,
    pub status: BundleStatus,
    pub updated_at: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TickSummary {
    pub id: TickId,
    pub plan_id: PlanId,
    pub commit_sha: String,
    pub updated_at: i64,
}
```

Summaries live in `crates/ipc/src/method.rs` (or a sibling `summaries.rs` module if `method.rs` grows too large). Per-summary size is roughly 200-400 bytes after JSON encoding, so a list of 2500 summaries comfortably fits the 1 MiB frame.

**Result enums:**

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "records", rename_all = "kebab-case")]
pub enum RecordsResult {
    Plans(Vec<PlanSummary>),
    Works(Vec<WorkSummary>),
    Bundles(Vec<BundleSummary>),
    Ticks(Vec<TickSummary>),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "record", rename_all = "kebab-case")]
pub enum RecordResult {
    Plan(Plan),
    Work(Work),
    Bundle(Bundle),
    Tick(Tick),
}
```

Wire format:

```json
{"kind": "plans", "records": [{"id": "pl-...", "goal": "...", ...}, ...]}
{"kind": "plan",  "record":  {"id": "pl-...", "goal": "...", ...full fields...}}
```

**List vs. detail idiom.** This split mirrors every mature REST-ish data API (Linear, Jira, GitHub): list returns summaries for browsing ("which records exist, what are they, what's their state"), detail returns the full record for reading ("show me this Bundle's verification text, AC list, commit diff"). Users wanting the full content of every record in a list loop `show <id>` over the list; shell-side, this is `loopr works -o json | jq -r '.records[].id' | xargs -I {} loopr show {}`.

Adding Spec/Phase later is mechanical: two new `RecordKind` variants, two new summary structs, two new arms in each result enum, two new handler branches, two new verb names on the CLI.

Naming note: `RecordsResult` and `RecordResult` (not `RecordListResult` and `RecordGetResult`) per `rules/rust.md` §Collection type names.

### API Design

#### Subcommand enum (after change)

```rust
#[derive(clap::Subcommand, Debug)]
pub enum Command {
    /// Initialize loopr state at the target: .loopr/, taskstore, git hooks, config.
    Init,

    /// Daemon lifecycle (start, stop, status).
    Daemon {
        #[command(subcommand)]
        cmd: DaemonCmd,
    },

    /// Submit a Plan goal to the daemon.
    Plan {
        /// One-sentence goal to plan for.
        goal: String,
    },

    /// List all Plans in the target's taskstore.
    Plans,
    /// List all Work items in the target's taskstore.
    Works,
    /// List all Bundles in the target's taskstore.
    Bundles,
    /// List all Ticks in the target's taskstore.
    Ticks,

    /// Show a single record by ID (prefix-routed).
    Show { id: String },

    /// Inspect run logs.
    Logs {
        #[command(subcommand)]
        cmd: LogsCmd,
    },

    /// Launch the TUI. Same as bare `loopr`.
    Tui,
}
```

`DaemonCmd` and `LogsCmd` keep their current shape. `Score`, `Decompose`, `Execute`, `Integrate`, `List` are deleted. `Command::label()` updates accordingly.

#### Output defaults

| Verb | Data payload | TTY default | Pipe default |
|---|---|---|---|
| `init` | status line only | n/a | n/a |
| `daemon start` | status line | n/a | n/a |
| `daemon stop` | status line | n/a | n/a |
| `daemon status` | `DaemonStatus` record | YAML | JSON |
| `plan "<goal>"` | `Plan` record | YAML | JSON |
| `plans` | `Vec<PlanSummary>` via `RecordsResult::Plans` | YAML | JSON |
| `works` | `Vec<WorkSummary>` via `RecordsResult::Works` | YAML | JSON |
| `bundles` | `Vec<BundleSummary>` via `RecordsResult::Bundles` | YAML | JSON |
| `ticks` | `Vec<TickSummary>` via `RecordsResult::Ticks` | YAML | JSON |
| `show <id>` | `RecordResult` sum | YAML | JSON |
| `logs tail` | pretty text stream | n/a | n/a |
| `logs runs` | `Vec<RunInfo>` | YAML | JSON |
| `tui` | launches TUI | n/a | n/a |

Status-line verbs emit free-form human text (`daemon started at .loopr/socket`, `initialized at /path`). `--output` is accepted by clap on all verbs (it is global) but has no effect on status-line verbs; this keeps the flag's behavior uniformly permissive rather than forbidding it per-verb.

#### Prefix-routed `show`

`show <id>` uses the first two ASCII characters of the id to pick a `RecordKind`, then issues a single `RecordGet` IPC call. The prefix literals match the `$prefix` arguments to the `id_type!` macro invocations in `crates/domain/src/id.rs`; they are the on-wire discriminator and the stable contract:

```rust
pub async fn run(
    target: &Path,
    id: &str,
    fmt: Format,
) -> Result<(), LooprError> {
    let kind = match id.split('-').next().unwrap_or("") {
        "pl" => RecordKind::Plan,
        "wk" => RecordKind::Work,
        "bd" => RecordKind::Bundle,
        "tk" => RecordKind::Tick,
        _ => return Err(LooprError::UnknownIdPrefix { id: id.into() }),
    };
    // Prefix literals above mirror the `id_type!` invocations in
    // `crates/domain/src/id.rs`; the macro exposes them via `PlanId::prefix()`
    // (a fn, not a const), so we match on the literal string here.
    let mut client = transport::connect_or_wait(target).await?;
    client.handshake().await?;
    let params = ipc::RecordGetParams { id: id.into() };
    let (resp, _events) = client
        .request(ipc::MethodName::RecordGet, serde_json::to_value(&params)?)
        .await?;
    // ... handle resp, decode as RecordResult, render via output::render
    let _ = (kind, fmt); // kind is used for defensive check vs RecordResult discriminant
    Ok(())
}
```

`show` performs exact ID match. Fuzzy / partial matching is a TUI behavior. When Spec/Phase land, the match gets two new arms.

An optional tightening (out of scope here, recommended for the follow-up): extend the `id_type!` macro to also emit `pub const PREFIX: &'static str = $prefix;` alongside the existing `pub fn prefix()`. That would let `show` match against `PlanId::PREFIX` constants instead of string literals. Until then, the literal-match form is the right compromise.

#### Handler side: `RecordList` dispatch

The handler lists full records from the store, then projects each into its summary type before assembling the result. Projection is a straight field-copy:

```rust
// crates/loopr/src/transport/handler.rs (sketch)
async fn handle_record_list<L>(
    id: u64,
    params: RecordListParams,
    ctx: &Arc<DaemonContext<L>>,
) -> DaemonResponse
where L: LlmClient + Send + Sync + 'static,
{
    let result = match params.kind {
        RecordKind::Plan => {
            let plans = ctx.store.plans().list().await?;
            RecordsResult::Plans(plans.iter().map(PlanSummary::from).collect())
        }
        RecordKind::Work => {
            let works = ctx.store.works().list().await?;
            RecordsResult::Works(works.iter().map(WorkSummary::from).collect())
        }
        RecordKind::Bundle => {
            let bundles = ctx.store.bundles().list().await?;
            RecordsResult::Bundles(bundles.iter().map(BundleSummary::from).collect())
        }
        RecordKind::Tick => {
            let ticks = ctx.store.ticks().list().await?;
            RecordsResult::Ticks(ticks.iter().map(TickSummary::from).collect())
        }
    };
    DaemonResponse::ok(id, serde_json::to_value(result)?)
}
```

Each summary gets a `From<&FullRecord>` impl in the `ipc` crate (pure field copy; no allocation beyond the struct itself). `handle_record_get` takes a different shape: it uses the id prefix to pick the right store accessor, fetches the full record, and wraps it in `RecordResult` without projection.

### Implementation Plan

All phases are mechanical CLI and IPC reshaping. Sonnet throughout.

#### Phase 1: Output module + `--output` flag
**Model:** sonnet

- `cargo add serde_yaml` under `crates/loopr/Cargo.toml` (or whichever YAML crate `cargo add` resolves to today)
- Create `crates/loopr/src/output.rs` with `Format`, `resolve`, `render`, `OutputError`
- Add `--output` / `-o` global flag to `Cli`
- Unit tests: explicit-flag-wins; YAML round-trip on a sample `Plan`; JSON round-trip on a sample `Plan`; TTY auto-detect via a small `OutputStream` indirection trait for testability

Exit: `cargo test -p loopr` green; module compiles, unused.

#### Phase 2: Delete dead subcommands
**Model:** sonnet

- Remove `Command::Decompose`, `Command::Execute`, `Command::Integrate`, `Command::Score`, `Command::List` from `cli.rs`
- Remove corresponding arms from `dispatch` in `lib.rs`
- Remove corresponding clap-parse tests in `cli/tests.rs` and integration tests in `tests.rs`
- Keep `LooprError::StageUnimplemented` (still used by `init`)

Exit: `cargo test -p loopr` green; `loopr --help` shows only surviving verbs.

#### Phase 3: IPC RecordList + RecordGet + summary projections
**Model:** sonnet

- In `crates/ipc/src/method.rs` (or a sibling `summaries.rs` if method.rs grows): add `RecordKind`, `PlanSummary`, `WorkSummary`, `BundleSummary`, `TickSummary`, `RecordListParams`, `RecordGetParams`, `RecordsResult`, `RecordResult`
- Add `Method::RecordList` and `Method::RecordGet` variants; add `MethodName::RecordList` and `MethodName::RecordGet`
- Add `From<&Plan> for PlanSummary`, `From<&Work> for WorkSummary`, etc. impls in the `ipc` crate (pure field copy)
- Add serde round-trip tests: one per `RecordKind` variant for both result enums; params round-trip; summary round-trip
- Add a frame-size test: a `RecordsResult::Bundles` with 1000 `BundleSummary` entries serializes to under 512 KiB (well clear of the 1 MiB cap with headroom)
- In `crates/loopr/src/transport/handler.rs`: add `handle_record_list` (with projection) and `handle_record_get`; wire into the method dispatch
- Remove `handle_plan_list` and `Method::PlanList` - now superseded by `RecordList(RecordKind::Plan)`. Delete `PlanListResult` and its tests.
- Unit tests for handlers against a seeded `DaemonContext` fixture

Exit: `cargo test -p ipc` and `cargo test -p loopr` green; IPC surface = Handshake, Status, PlanCreate, RecordList, RecordGet; the 1000-summary frame test passes.

#### Phase 4: Flat list verbs via IPC
**Model:** sonnet

- Add `Command::Plans`, `Command::Works`, `Command::Bundles`, `Command::Ticks` to `cli.rs`
- Create `crates/loopr/src/commands/` module dir; add `list.rs` with four list bodies
- Each body: `transport::connect_or_wait(target).await?` -> handshake -> `RecordList(kind)` -> decode `RecordsResult` -> render via `output::render` (picking the right sum arm defensively)
- Delete the old `list_plans` function in `lib.rs` (now supplanted by `Command::Plans`'s body)
- Unit tests per verb via an in-process `DaemonHandle` fixture (already built in the test harness landed 2026-04-22) with seeded records; assert count and format round-trip

Exit: `loopr plans` and siblings work against a seeded target with a running daemon.

#### Phase 5: `show <id>`
**Model:** sonnet

- Add `Command::Show { id: String }` to `cli.rs`
- Add `crates/loopr/src/commands/show.rs` with prefix routing (string-literal match; comment pointing at `id_type!` as source of truth)
- Add `LooprError::UnknownIdPrefix { id: String }` and `LooprError::RecordNotFound { id: String }`
- Tests: each prefix happy path; unknown prefix; valid prefix, unknown id; truncated prefix (`pl` alone)

Exit: `loopr show pl-<id>`, `loopr show wk-<id>`, `loopr show bd-<id>`, `loopr show tk-<id>` all work.

#### Phase 6: Bare `loopr` -> TUI stub + explicit `tui`
**Model:** sonnet

- Change `Cli::command` to `Option<Command>`
- Add `Command::Tui` variant
- Add `LooprError::NotYetImplemented { feature: &'static str }` (distinct from `StageUnimplemented`; the TUI has no roadmap stage number)
- Dispatch: `None` maps to `Command::Tui`; `Command::Tui` returns `LooprError::NotYetImplemented { feature: "tui" }` with a message like "TUI launcher not yet implemented (earned post-first-gate)"
- Tests: bare invocation returns `NotYetImplemented`; `loopr tui` returns the same; `loopr --help` and `loopr --version` still short-circuit through clap

Exit: bare invocation reaches the dispatch arm; help/version unchanged.

#### Phase 7: Delete dead `--worktree-cleanup`, drop `[Stage N]` markers
**Model:** sonnet

- Remove the `worktree_cleanup` field from `Cli` (it is already dead code; no consumer exists)
- Remove any test that parsed the flag
- Remove `[Stage N]` substrings from the `///` doc comments on all surviving subcommands
- Add a one-line amendment pointer at the top of `docs/design/2026-04-19-cli-skeleton.md` citing this doc

Exit: `loopr --help` is clean of stage markers and the worktree flag; env + config paths for `AttemptCleanupPolicy` untouched (they already carry the full weight).

#### Phase 8: Vision amendment + bump
**Model:** sonnet

- Add amendment `a6` to `docs/vision.md` §Amendments summarizing the CLI reshape and the added Process Rule
- Update `docs/vision.md` §loopr bullet (currently "CLI subcommands mirror stage boundaries: `loopr plan`, `loopr decompose`, `loopr execute`, `loopr integrate`, `loopr experiment`") to the new surface
- Add Process Rule #5 to §Process Rules: "CLI is plumbing, TUI is porcelain. CLI verbs require a real user story outside interactive UI (scripts, cron, CI, hooks, one-off invocations). Interactive management lives in the TUI as keybindings, not in the CLI as verbs."
- `bump` patch; workspace version + tag

Exit: `otto ci` at workspace root green; tag pushed.

### Phase Model Summary

All phases sonnet. No algorithmic or novel work. IPC additions, surface reshaping, module extractions, deletions, doc amendments. Opus would be wasted.

## Alternatives Considered

### Alternative 1: Keep dead verbs as StageUnimplemented forever

- **Description:** Leave `decompose`, `execute`, `integrate`, `score` as stubs forever. Document that they will not be wired.
- **Pros:** Zero deletion; forward-compat if someone decides to re-expose manual stage control.
- **Cons:** `--help` stays cluttered. Every future design doc has to keep addressing them. The daemon's auto-chain model is the committed direction; pretending otherwise is a lie to the user.
- **Why not chosen:** The point of the rework is to make the surface honest.

### Alternative 2: Direct taskstore reads, bypassing the daemon

- **Description:** CLI list/show verbs call `Store::open(target)` themselves and read the taskstore directly, no IPC.
- **Pros:** Reads work when the daemon is down (post-crash forensics, pre-start audit). No new IPC methods. Lower latency for each read.
- **Cons identified by architect review (2026-04-23):**
    - Two `AsyncStore` instances against the same `.loopr/taskstore/` would each spin up a writer thread and each sync against the SQLite cache. Concurrent-writer semantics risk `SQLITE_BUSY` errors or `UNIQUE` constraint violations.
    - `AsyncStore::list` queries the SQLite cache index, not the JSONL; the earlier atomicity argument ("PIPE_BUF-sized appends are atomic") was beside the point because readers do not read the JSONL.
    - Read-after-write consistency is lost: `loopr plan "x" && loopr plans` can race because the CLI reads its own SQLite snapshot, not the daemon's.
    - `AsyncStore::open_at` calls `fs::create_dir_all`, so a read command in an uninitialized target would silently create `.loopr/taskstore/` as a side effect.
- **Why not chosen:** The consistency hazard alone disqualifies the approach for scripting. Post-crash forensics are already served by `jq` on the raw JSONL; the CLI does not need to replicate that.

### Alternative 3: Per-type IPC methods (`PlanList`, `PlanGet`, `WorkList`, ...)

- **Description:** One method per record kind per operation: `PlanList`, `PlanGet`, `WorkList`, `WorkGet`, `BundleList`, `BundleGet`, `TickList`, `TickGet`. Eight methods at first gate; twelve when Spec/Phase land.
- **Pros:** Each method is symmetric with `PlanCreate`. Each returns a typed result without a sum arm at the call site.
- **Cons:** Sprawl. Twelve near-identical handlers at maturity. Every record-type addition requires two new methods, two new result structs, two new handler functions. Does not buy "more typed" - `RecordKind` + sum-type results are equally type-checked; the wire contract is just as strict.
- **Why not chosen:** `RecordList` + `RecordGet` with sum-type results is equally typed and an order of magnitude less boilerplate. Adding Spec/Phase later becomes two enum-variant additions in three places, not four new methods.

### Alternative 4: `--output text` as a third format

- **Description:** Add a compact text format. TTY default is `text`; explicit `-o json` or `-o yaml` for structure.
- **Pros:** More compact for scanning.
- **Cons:** Requires per-verb column alignment, truncation logic. YAML already serves as a readable human format with zero extra code. `yq` can project to columns downstream for any user who wants that.
- **Why not chosen:** Two formats halves the code and test matrix. YAML is the readable default.

### Alternative 5: Bare `loopr` prints help (git-style)

- **Description:** Keep current behavior. Bare invocation prints help; explicit `loopr tui` required to launch.
- **Pros:** Discoverable. Matches git convention.
- **Cons:** Friction for the primary action. Loopr is a 10-verb daemon-with-UI, not a 150-verb toolbox. Tools in loopr's shape (lazygit, gitui, htop, btm) all launch their TUI on bare invocation.
- **Why not chosen:** Ergonomic optimization for the expected primary action. `--help` still works for discovery.

### Alternative 6: Internal-tag serde with struct variants

- **Description:** `#[serde(tag = "kind")] enum RecordsResult { Plans { records: Vec<PlanSummary> }, ... }`. Struct variants, not tuple variants, because internally tagged enums do not accept a tuple variant whose inner type serializes as a JSON array.
- **Pros:** Slightly smaller JSON envelope (the tag lives inside the content object).
- **Cons:** Destructuring requires a named-field pattern: `RecordsResult::Plans { records } => ...` where `records` has a redundant name. Ergonomics are worse than adjacent tagging's `RecordsResult::Plans(plans) => ...`.
- **Why not chosen:** Adjacent tagging (`#[serde(tag, content)]`) gives tuple variants and cleaner destructuring at zero wire cost.

## Technical Considerations

### Dependencies

- `serde_yaml` (or current maintained fork) added to `crates/loopr/Cargo.toml`
- `std::io::IsTerminal` from std; no crate needed
- No workspace-level dependency changes

### Performance

- Each list/show verb pays one socket round-trip to the daemon. Negligible for interactive use; measurable in tight shell loops. If it matters, a future `loopr watch` streaming endpoint covers the hot-loop case.
- YAML serialization is marginally slower than JSON. Negligible at the record scale we write (dozens to low hundreds per target).
- TTY detection is a single syscall per invocation.

### Security

- No new attack surface. IPC handshake and permissions on `.loopr/socket` are unchanged.
- `--output` values are validated by clap's `ValueEnum`; only `json` and `yaml` accepted.
- `RecordGet` by id is read-only; no authorization model needed beyond socket-file permissions.

### Testing Strategy

- **Per-command unit tests.** TempDir-built target, `DaemonHandle` fixture (in-process), seeded records; assert output parses as declared format and content matches.
- **IPC round-trip tests.** `RecordList` and `RecordGet` method shapes, every `RecordKind` variant, both result enums, params. Each round-trips bytes -> struct -> bytes and matches byte-for-byte.
- **Handler dispatch tests.** `handle_record_list` for each kind (including empty results) and `handle_record_get` for happy path, unknown id, and cross-kind id (e.g., requesting a `pl-`-prefixed id when no such plan exists).
- **Output module seam test.** `Format::resolve(Some(Format::Json))` and `Format::resolve(Some(Format::Yaml))` for explicit-flag-wins. The real TTY-auto behavior is covered by a smoke test.
- **Smoke test updates.** `tests/smoke.rs` adds: bare invocation returns `NotYetImplemented`; removed verbs fail with clap's unknown-subcommand error; `--output` flag parses; `show` with invalid id prefix fails cleanly.
- **Daemon-down behavior test.** Invoking `loopr plans` with no daemon running returns `LooprError::ClientIo` with a clear "daemon not running" message (existing transport error surface; verify the message is user-facing).

### Rollout Plan

- Ship on branch `v5` as incremental commits per phase.
- `otto ci` at workspace root green after each phase.
- Bump `v0.5.x` once all phases green (single flat workspace tag per `rules/git.md`).
- No coexistence migration: dead verbs removed in one commit per `docs/vision.md` §Process Rules #3.
- Vision amendment `a6` and Process Rule #5 land with the final bump.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| External user has a script invoking `loopr decompose` or `loopr list plans`; breaks | Low | Low | v5 is orphan-branch pre-first-gate; no external users. `list plans` -> `plans` is a one-word edit. |
| `serde_yaml` or its current fork adds transitive deps that bloat build | Low | Low | YAML crates are small. Already indirectly in the tree via taskstore. |
| TTY detection misbehaves on unusual terminals (CI runners, tmux-inside-docker, `less -F`) | Low | Low | `IsTerminal` is the std-library answer; `--output` override covers edge cases. |
| IPC round-trip for every read is a latency regression vs direct-disk reads | Low | Low | Single socket hop, single serde encode/decode. Dwarfed by any human-scale interaction or LLM round-trip. |
| `RecordList` response exceeds 1 MiB `MAX_LINE_BYTES` frame cap | Low | High | Summaries cap per-record payload at ~200-400 bytes JSON-encoded. 1000 summaries = ~300 KiB, well under 1 MiB. Frame-size regression test in Phase 3 pins this. Pagination is the follow-up if that ceiling is ever threatened. |
| A future record type (e.g., a hypothetical `Artifact` record with blob fields) has a summary that also grows large | Low | Medium | Summary definition is a deliberate design choice per record type. Reviewer checks on record-add design docs enforce the "summary fits in ~400 bytes" discipline. |
| `RecordGet` defensive kind-check (CLI asked for Plan, daemon returned a Work-kind result) is unreachable in practice but bloats code | Low | Low | Keep the defensive check as a `ProtocolError` variant rather than `unreachable!()`; compiler forces the CLI to handle the impossible branch, protecting against a future daemon bug. |
| Users of `plans`/`works`/etc. surprised that output lacks full record content | Low | Low | `--help` on each list verb names the summary fields. `show <id>` is the advertised path to full content; error messages on `show` reinforce this. |
| Daemon-down error message is confusing for users expecting reads to "just work" | Medium | Low | Error text explicitly names `.loopr/socket` and tells the user to run `loopr daemon start`. Matches how `git` behaves when not in a repo. |
| Adding two `RecordKind` variants for Spec/Phase later cascades into many files | Low | Low | Three files total: `ipc/method.rs` (variant in `RecordKind` and both result enums), `loopr/transport/handler.rs` (two new arms), `loopr/commands/list.rs` + `show.rs` (two new arms and two new `Command` variants). Contained. |
| Process Rule #5 interpreted too strictly, blocking legit CLI additions | Medium | Low | Rule says "real user story outside interactive UI," not "never add CLI verbs." Additions remain possible when justified; the rule forces the justification. |

## Open Questions

All primary design decisions are now settled. Remaining items are small follow-ups worth tracking but do not gate implementation:

- [ ] **Extend `id_type!` to emit `pub const PREFIX` alongside `pub fn prefix()`.** Would let `show` match on `PlanId::PREFIX` instead of string literals. One-line macro addition. Suggested follow-up; not in scope here.
- [ ] **Streaming `logs tail` via IPC.** Today `logs tail` reads the log file directly. When the TUI lands and wants live log streaming, an IPC streaming-response design becomes real work. Out of scope; revisit when the TUI design doc lands.
- [ ] **Filters on list verbs.** `loopr works --plan <id>`, `loopr bundles --status Accepted`. Deferred. `jq`/`yq` cover the scripting case today; add real filters when a concrete scripting story demands them.
- [ ] **Pagination on `RecordList`.** Summaries keep the payload well below 1 MiB through expected v5 scale. If a real repo accumulates thousands of Works/Bundles and the frame cap is threatened, add `limit`/`offset` to `RecordListParams`. Not in scope.

## References

- [`docs/vision.md`](../vision.md) §loopr, §Target Repo Layout §CLI targeting, §Process Rules, §Amendments
- [`docs/roadmap.md`](../roadmap.md) §Beyond First Gate (TUI entry)
- [`docs/design/2026-04-19-cli-skeleton.md`](2026-04-19-cli-skeleton.md) - the Stage 1 CLI skeleton this amends
- [`docs/design/2026-04-19-telemetry-stage-2.md`](2026-04-19-telemetry-stage-2.md) - added `--log-level` global flag
- [`docs/design/2026-04-19-protocol.md`](2026-04-19-protocol.md) - IPC framing and method dispatch, the contract this extends
- [`docs/design/2026-04-21-worktree-lifecycle.md`](2026-04-21-worktree-lifecycle.md) - added `--worktree-cleanup` global flag (being deleted here as dead code)
- [`docs/design/2026-04-22-stage-8-wiring.md`](2026-04-22-stage-8-wiring.md) - where daemon auto-chain behavior completed
- [`docs/design/2026-04-22-daemon-test-harness.md`](2026-04-22-daemon-test-harness.md) - `DaemonHandle` fixture used by Phase 4/5 tests
- `rules/rust.md` §CLI conventions, §Naming and Style §Collection type names (added in this round), §Architecture Shell/Core Split
- `rules/general.md` §Naming conventions
- v3 reference: `~/repos/scottidler/loopr` - git plumbing/porcelain antecedent
- v4 reference: `~/repos/scottidler/loopr-v4` - pre-auto-chain CLI ancestor
- Prior art: `lazygit`, `gitui`, `htop`, `btm` - bare-invocation-launches-TUI convention
