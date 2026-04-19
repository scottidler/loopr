# Design Document: CLI Skeleton

**Author:** Scott A. Idler
**Date:** 2026-04-19
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

Turn `crates/loopr` from a one-line scaffold into a real CLI binary. `loopr --version` prints `GIT_DESCRIBE`, `loopr --help` shows the full subcommand shape, `loopr -C <path>` changes the effective target before anything else runs, and `loopr` refuses to operate on a directory carrying the `.loopr-source-guard` sentinel. No subcommand does real work yet; each returns a typed "not implemented - earned at Stage N" error. This is Stage 1 of `docs/roadmap.md`.

## Problem Statement

### Background

The v5 workspace compiles (Stage 0) and all 13 crates are wired together, but the `loopr` binary is still `fn main() { println!("loopr v5 scaffold"); }`. To move forward against `docs/roadmap.md`, we need a real CLI shell: clap-driven parsing, a frozen subcommand shape, git-style target selection, and the source-guard that `.loopr-source-guard` at the repo root is already pre-positioned to enforce.

v3 and v4 both landed this layer early and kept it stable through every pivot. v4's `src/cli.rs` and `src/main.rs` are the proven patterns to port, but the v5 scope is narrower - no CRUD subcommands, no `--as` role, no TUI launcher yet.

### Problem

Stage 1 of the roadmap has no exit criterion to measure against until the binary actually parses args, prints a version, rejects source-tree targets, and returns typed errors for stubs. Further, every later stage ("daemon forks," "decomposer runs," "integrator merges") presupposes a CLI subcommand surface it can attach its logic to. Without that surface declared up front, each stage ends up inventing ad-hoc dispatch, and the subcommand shape drifts.

### Goals

- `loopr --version` prints `GIT_DESCRIBE` (tag + commit count + short-sha) from a `build.rs`, falling back to `CARGO_PKG_VERSION` when git is absent
- `loopr --help` shows every Stage 1–9 subcommand by name so users see the shape even though bodies come later
- `-C <path>` (global, git-style) changes the effective target before any subcommand dispatch
- `LOOPR_TARGET` env var acts as fallback per `docs/vision.md` (CWD wins when neither `-C` nor env is set)
- Source-guard walk-up: from the effective target, walk toward `/` looking for `.loopr-source-guard`; if found, exit with a clear error naming the sentinel path
- Every subcommand stub returns a typed `StageUnimplemented { stage: u8, subcommand: &'static str }` error so callers see exactly which stage will light it up
- Exit criterion: `cargo install --path crates/loopr && loopr --version` prints the tag; `loopr -C /tmp plan "x"` parses, passes source-guard, and errors with `StageUnimplemented { stage: 5, ... }` (the `plan` subcommand is owned by roadmap Stage 5, not Stage 6)

### Acceptance Criteria

Each item is an assertable check. The design is Done when every assertion below is true.

- `loopr --version` exits 0 and stdout matches the current `git describe --tags --always` output (or `CARGO_PKG_VERSION` when git is absent)
- `loopr --help` output contains one line per Stage 1-9 subcommand (`init`, `plan`, `decompose`, `execute`, `integrate`, `daemon`, `score`, `logs`, `list`)
- Every subcommand in the `Subcommand -> Stage` mapping table returns `Err(LooprError::StageUnimplemented { stage, subcommand })` with the stage number and label from the table
- `loopr -C /tmp plan "x"` parses, passes source-guard, exits non-zero, stderr contains `"Stage 5"`
- `loopr plan "x"` run from any directory at or below the loopr-v5 checkout exits non-zero with stderr mentioning the `.loopr-source-guard` sentinel
- `loopr -C <nonexistent-path> plan "x"` exits non-zero with `LooprError::TargetInvalid`
- `loopr -C <path-to-a-file> plan "x"` exits non-zero with `LooprError::TargetIsFile` whose message hints to pass the parent (`try -C <parent>`)
- `LOOPR_TARGET=""` is treated identically to an unset env var (CWD resolves)
- Precedence test: with `-C /a`, `LOOPR_TARGET=/b`, CWD=`/c`, the resolved target is `/a`
- Git-root test: `loopr plan "x"` invoked from `<git-repo>/src/foo/bar/` resolves to `<git-repo>/` (the `git rev-parse --show-toplevel` answer)
- Fall-through test: `loopr -C /tmp plan "x"` (not a git repo) resolves to `/tmp` and proceeds (source-guard walk applies as before)
- `otto ci` at the `crates/loopr` root is green; workspace-level `otto ci` is green
- `cargo install --path crates/loopr` installs the binary and the installed binary reproduces all of the above

### Non-Goals

- Daemon forking, PID lock, socket creation - Stage 4
- IPC client-side request/response correlation - Stage 4
- Telemetry subscriber initialization - Stage 2 (CLI today uses `eprintln!` for user-facing errors; `tracing::init` calls come next stage)
- TaskStore open, `loopr init` body - Stage 5
- TUI launcher - deferred past First Gate entirely
- `--as <role>` flag from v4 - v5 has no per-command role override yet
- Per-subcommand arg parsing beyond what Stage 1 needs to reject cleanly (e.g. `plan "x"` takes a goal string and nothing else, for now)
- Any way for a developer to run `loopr` against the `loopr-v5` repo itself. The source-guard blocks that absolutely. If that ever becomes a real need, we add an override (env var or flag) at that point. Not a Stage 1 concern.

## Proposed Solution

### Overview

Thin `main.rs` parses the `Cli` struct, resolves the effective target via `-C` → `LOOPR_TARGET` → CWD, runs the source-guard check, and dispatches on `Command`. Every command arm currently returns `Err(LooprError::StageUnimplemented { ... })`. The clap derive tree is modeled on v4's `src/cli.rs` but trimmed to the Stage 1–9 surface named in `docs/vision.md` ("§loopr: CLI subcommands").

`lib.rs` re-exports `Cli`, `Command`, and the `LooprError` enum so tests can exercise parse and source-guard logic without going through the binary. `build.rs` produces the `GIT_DESCRIBE` env var using v4's exact shell-out pattern.

### Architecture

```
crates/loopr/
├── build.rs                 GIT_DESCRIBE via `git describe --tags --always` (v4-verbatim; see note on workspace paths)
├── src/
│   ├── main.rs              thin shell: parse, resolve target, source-guard, dispatch
│   ├── lib.rs               pub use of Cli, Command, LooprError, guard, target
│   ├── cli.rs               clap derive structs (Cli, Command enum)
│   ├── guard.rs             source_guard::check(&Path) -> Result<(), LooprError>
│   ├── target.rs            resolve + git-root discovery via `git rev-parse --show-toplevel`
│   └── error.rs             LooprError enum (thiserror); surfaced via eyre::Termination (exit 1 on any error)
```

File names all single-word per `rules/general.md` / `rules/rust.md`. No `mod.rs` (Rust 2018+ style throughout, per `rules/rust.md`).

### Data Model

```rust
// src/cli.rs
#[derive(clap::Parser)]
#[command(
    name = "loopr",
    version = env!("GIT_DESCRIBE"),
    about = "Agent orchestrator; operates on a target repo via decompose/implement/integrate.",
    long_about = None,
)]
pub struct Cli {
    /// Change to <path> before doing anything (git-style).
    /// Falls back to $LOOPR_TARGET, then CWD.
    #[arg(short = 'C', long = "chdir", global = true, value_name = "PATH")]
    pub chdir: Option<PathBuf>,

    #[command(subcommand)]
    pub command: Command,
}

#[derive(clap::Subcommand)]
pub enum Command {
    /// Initialize loopr state (.loopr/, .taskstore/, git hooks) at the target. [Stage 5]
    Init,

    /// Submit a Plan goal to the daemon. [Stage 5]
    Plan {
        /// One-sentence goal to plan for.
        goal: String,
    },

    /// Decompose an existing Plan into a Work DAG. [Stage 6]
    Decompose {
        plan_id: String,
    },

    /// Run agents against ready Work items. [Stage 7]
    Execute {
        #[arg(long)]
        work_id: Option<String>,
    },

    /// Integrate accepted Bundles into the integration branch. [Stage 8]
    Integrate,

    /// Daemon lifecycle (start, stop, status). [Stage 4]
    Daemon {
        #[command(subcommand)]
        cmd: DaemonCmd,
    },

    /// Score a completed run from its taskstore directory. [Stage 9 body; stub parses now]
    Score {
        #[arg(long, short)]
        dir: PathBuf,
        #[arg(long, default_value_t = 0)]
        duration_secs: u64,
    },

    /// Inspect run logs. [Stage 2 body; stub parses now]
    Logs {
        #[command(subcommand)]
        cmd: LogsCmd,
    },

    /// List records of a given kind. [Stage 5]
    List {
        /// One of: plans, specs, phases, works, bundles, ticks.
        kind: String,
    },
}

#[derive(clap::Subcommand)]
pub enum LogsCmd {
    /// Show the latest run's pretty log.
    Tail {
        #[arg(long, default_value_t = 100)]
        lines: usize,
    },
    /// List known runs.
    Runs,
}

#[derive(clap::Subcommand)]
pub enum DaemonCmd {
    /// Fork-to-daemon (default) or run in-foreground with `--foreground`.
    Start {
        #[arg(long)]
        foreground: bool,
    },
    /// Stop the running daemon (SIGTERM, escalate to SIGKILL after 3s).
    Stop,
    /// Query the running daemon's status via IPC.
    Status,
}
```

```rust
// src/error.rs
#[derive(thiserror::Error, Debug)]
pub enum LooprError {
    #[error("target {path} is a loopr source tree (sentinel found at {sentinel})")]
    SourceGuardTripped { path: PathBuf, sentinel: PathBuf },

    #[error("target {path} does not exist or is not a directory")]
    TargetInvalid { path: PathBuf },

    #[error("target {} is a file, not a directory (try -C {} to use its parent)", path.display(), parent_hint(path))]
    TargetIsFile { path: PathBuf },

    #[error("subcommand `{subcommand}` is not yet implemented (earned at Stage {stage})")]
    StageUnimplemented { stage: u8, subcommand: &'static str },
}
```

Note: `thiserror` is allowed here because `loopr` is both a binary and a library crate, and the error enum is part of its public API so downstream tests / integration code can match on variants. `main.rs` wraps with `eyre::Result` as per `rules/rust.md` ("CLIs: `eyre::Result` ... libraries: `thiserror`"). The top-level `main` returns `eyre::Result<()>` and converts `LooprError` via `?`.

### API Design

```rust
// src/guard.rs
/// Walk from `start` toward `/`, returning `Err(SourceGuardTripped)` if
/// `.loopr-source-guard` is found at any ancestor (including `start` itself).
/// Returns `Ok(())` if the walk reaches `/` without finding the sentinel.
pub fn check(start: &Path) -> Result<(), LooprError> { ... }
```

```rust
// src/target.rs
/// Resolve the effective target directory.
///
/// Step 1: pick the starting path via precedence: `-C` > `LOOPR_TARGET` env > CWD.
///   (Empty env string is treated as unset.)
/// Step 2: canonicalize the starting path. Canonicalize failures or
///   non-directory resolutions map to `LooprError::TargetInvalid { path }`.
///   Canonicalization follows symlinks: `-C ~/link-to-project` resolves to
///   the physical target directory. This matches `git -C` behavior.
/// Step 3: three-tier root discovery, first match wins.
///   (a) Shell out to `git -C <canonicalized> rev-parse --show-toplevel`.
///       Success = that path is the root. (Handles worktrees, submodules,
///       symlinks for free; most common case.)
///   (b) If git fails (not a git repo, git binary missing), walk ancestors
///       of the canonicalized path (including itself) looking for `.loopr/`
///       or `.taskstore/`. First ancestor containing either marker is the
///       root. Covers `loopr init`-ed targets that aren't git repos, and
///       git repos where git is somehow unavailable at runtime.
///   (c) If neither (a) nor (b) finds anything, fall through to the
///       canonicalized start path. This is the legitimate "fresh target,
///       never initialized" state; `loopr init` run there creates the
///       state; other subcommands error with "run loopr init first" at
///       their own stage. Also covers `/tmp`-style smoke-test paths.
pub fn resolve(
    chdir: Option<&Path>,
    env: Option<&str>,
    cwd: &Path,
) -> Result<PathBuf, LooprError> { ... }
```

**Rationale for three tiers**: `git rev-parse` is the fast path for the common case (every target repo Stage 9 wants to hit is a git repo). The marker walk is there so `loopr init`-ed targets still resolve correctly if git is absent. The fall-through is there so `loopr init` itself has a well-defined place to put new state, and so `/tmp`-style smoke tests behave.

**Git invocation shape**: `std::process::Command::new("git").args(["-C", &canonicalized, "rev-parse", "--show-toplevel"]).output()`. Trim stdout. Non-zero exit → proceed to tier (b). Missing `git` binary → same, plus a one-time `tracing::warn!` emitted when telemetry lights up at Stage 2.

Per `rules/rust.md` §Architecture: Shell/Core Split, all orchestration lives in `lib::run`; `main.rs` is a six-line shell whose only job is parsing argv, calling the library, and returning an `eyre::Result`.

```rust
// src/main.rs
use clap::Parser;
use loopr::Cli;

fn main() -> eyre::Result<()> {
    let cli = Cli::parse();
    loopr::run(cli)?;
    Ok(())
}

// src/lib.rs
pub fn run(cli: Cli) -> Result<(), LooprError> {
    let cwd = std::env::current_dir().map_err(|_| LooprError::TargetInvalid {
        path: PathBuf::from("."),
    })?;
    let env_target = std::env::var("LOOPR_TARGET").ok();
    let effective = target::resolve(cli.chdir.as_deref(), env_target.as_deref(), &cwd)?;
    guard::check(&effective)?;
    dispatch(cli.command)
}

fn dispatch(command: Command) -> Result<(), LooprError> {
    match command {
        Command::Init => Err(LooprError::StageUnimplemented { stage: 5, subcommand: "init" }),
        Command::Plan { .. } => Err(LooprError::StageUnimplemented { stage: 5, subcommand: "plan" }),
        // ... one arm per command, each mapping to its roadmap stage
    }
}
```

`dispatch` is the unit-test seam (one `Command` in, one `Result<(), LooprError>` out), deliberately kept separate from `run` so tests don't have to stub target resolution or source-guard. The `LOOPR_TARGET=""`-as-unset normalization lives inside `target::resolve` so it's testable in isolation too - `lib::run` passes whatever `std::env::var` returned.

`clap` handles `--version` and `--help` internally before `Command` dispatch runs - meaning neither target resolution nor source-guard fires for those two flags. This is intentional: `--version` / `--help` are pure metadata queries and should work anywhere, including inside the loopr source tree.

Subcommand-to-stage mapping lives in code (one `stage: u8` per match arm). The `--help` summary strings end with `[Stage N]` so users reading `loopr --help` see the roadmap without reading a doc.

#### Subcommand → Stage mapping (reference table)

| Subcommand | Stage (roadmap) | `StageUnimplemented.subcommand` label |
|---|---|---|
| `init` | 5 | `"init"` |
| `plan <goal>` | 5 | `"plan"` |
| `decompose <plan_id>` | 6 | `"decompose"` |
| `execute [--work-id]` | 7 | `"execute"` |
| `integrate` | 8 | `"integrate"` |
| `daemon start [--foreground]` | 4 | `"daemon-start"` |
| `daemon stop` | 4 | `"daemon-stop"` |
| `daemon status` | 4 | `"daemon-status"` |
| `score --dir <path>` | 9 | `"score"` |
| `logs tail [--lines]` | 2 | `"logs-tail"` |
| `logs runs` | 2 | `"logs-runs"` |
| `list <kind>` | 5 | `"list"` |

The `subcommand: &'static str` field of `StageUnimplemented` is a stable label tied one-to-one with the `Command` variant (nested commands get a hyphenated label, e.g. `"logs-tail"`). Labels are matched on in tests and printed in error messages; they do not need to equal the literal CLI token.

### Implementation Plan

#### Phase 1: Build script + error enum + skeleton crate wiring
**Model:** sonnet

- Add `build.rs` copied from v4 (`git describe --tags --always`, fallback to `CARGO_PKG_VERSION`). **Workspace adjustment**: v4's `build.rs` sits at the repo root, so `cargo:rerun-if-changed=.git/HEAD` works as written. In v5, `build.rs` lives at `crates/loopr/build.rs` (Cargo requires it next to the package's `Cargo.toml`), so `rerun-if-changed` paths resolve relative to `crates/loopr/` and need `../../.git/HEAD` and `../../.git/refs/` to reach the actual `.git/` at the workspace root. Without this fix, incremental builds won't trigger when new commits land and `loopr --version` will silently report stale metadata between clean builds. Shell-out for `git describe` itself is unaffected: it uses the CWD's git repo, which is the workspace (same repo).
- Create `src/error.rs` with `LooprError` enum (thiserror).
- Replace `src/main.rs` with a two-line stub that calls `loopr::run()` and maps errors; move logic to `lib.rs`.
- Dependencies: `clap` and `eyre` are already declared in `crates/loopr/Cargo.toml` via `workspace = true`. Add `thiserror` (this crate only; `loopr` is a lib-and-bin so thiserror is appropriate per `rules/rust.md`) via `cargo add thiserror` at the crate root.
- `.otto.yml` already exists at `crates/loopr/.otto.yml`; no changes this phase.
- `cargo check -p loopr` green.

#### Phase 2: Cli struct + subcommand tree + stub dispatch
**Model:** sonnet

- Write `src/cli.rs` with `Cli` + `Command` + `LogsCmd` exactly as in "Data Model" above.
- Write `src/lib.rs`: re-export `Cli`, `Command`, `LooprError`; define `pub fn run(cli: Cli) -> Result<(), LooprError>` containing the dispatch match that returns `StageUnimplemented` for every arm. `src/main.rs` wraps this in `eyre::Result` via `?` (thiserror types auto-convert into `eyre::Report`), preserving the `rules/rust.md` "CLIs use eyre" surface while keeping the lib boundary typed.
- Unit tests for clap: one test per subcommand verifying `Cli::parse_from(["loopr", ...])` produces the right `Command` variant (port the relevant subset of v4's `test_cli_parses_*` tests).
- `Cli::command().debug_assert()` test to catch malformed clap trees.
- `cargo test -p loopr` green.

#### Phase 3: Source-guard walk-up + target resolution + integration
**Model:** sonnet

- `src/guard.rs`: `check(start: &Path)` walks ancestors via `Path::ancestors()`, stops at the first existing `<ancestor>/.loopr-source-guard`, returns `SourceGuardTripped { path: start.to_owned(), sentinel: ancestor.join(".loopr-source-guard") }`. Reaches root without a match → `Ok(())`.
- `src/target.rs`: `resolve(chdir, env, cwd)` applies precedence (treating `LOOPR_TARGET=""` as unset), canonicalizes via `Path::canonicalize` (returns `TargetInvalid` for non-existent paths, `TargetIsFile` with a "try parent" hint for paths that exist but are files), then does three-tier root discovery: (a) `git -C <path> rev-parse --show-toplevel`; on failure (b) walk ancestors looking for `.loopr/` or `.taskstore/`; on failure (c) fall through to the canonicalized start path.
- Wire into `lib::run`: resolve, guard, then dispatch.
- Tests: source-guard hit (at exact path), hit at ancestor, no hit to root, `/tmp` target clean; target resolution precedence with all four combinations (-C vs env vs CWD).
- Use `tempfile::TempDir` for filesystem-dependent tests.

#### Phase 4: Exit-criterion smoke test + `--help` polish
**Model:** sonnet

- `tests/smoke.rs` integration test: invokes the compiled binary via `assert_cmd` (`cargo add --dev assert_cmd predicates`), asserts:
    - `loopr --version` stdout matches `/^v?\d+\.\d+\.\d+/` and contains the current tag substring
    - `loopr --help` stdout contains each Stage-N subcommand name
    - `loopr -C /tmp plan "x"` exits non-zero with stderr containing `Stage 5`
    - From the loopr-v5 checkout root (which carries `.loopr-source-guard`), `loopr plan "x"` exits with stderr containing `source tree`
- `otto ci` inside `crates/loopr` green.
- **Bump to `v0.5.1`** on the `v5` branch once Phases 1-4 are all green (repo-local override of `rules/git.md`; `v5` is long-lived). Workspace-inherited version means one `Cargo.toml` edit + one annotated `v0.5.1` tag.

### Phase Model Summary

All Stage 1 work is mechanical: CLI scaffolding, tests per subcommand, source-guard walk-up, and a `git rev-parse` shell-out for target resolution. Sonnet is the right fit. No phase here calls for Opus.

## Alternatives Considered

### Alternative 1: Define only `--version` and `--help`, no subcommands

- **Description:** Stage 1 ships a `Cli` with no `Command` enum at all. Subcommands are added per-stage as each earns its keep.
- **Pros:** Minimal surface area; matches the "one design doc per failing run" ethos at the sharpest possible point.
- **Cons:** Every later stage's design doc has to re-argue the subcommand name, parent-command shape, and `-C` semantics. Users get no preview of the CLI from `loopr --help` until the First Gate lands. The Stage 1 exit criterion explicitly requires `loopr -C /tmp plan "x"` to parse, which means we need at minimum a `plan` subcommand declared now.
- **Why not chosen:** The roadmap exit criterion pins the shape-minimum at "subcommand layout"; defining all seven up front costs ~40 lines of clap and buys a stable `loopr --help` surface the user can read to understand the whole roadmap. The cost is negligible compared to the discovery benefit.

### Alternative 2: Port v4's full `src/cli.rs` verbatim, then prune per-stage

- **Description:** Copy v4's `Cli` struct, all 12 top-level subcommands, all CRUD / BundleCmd / TickCmd / LearningCmd enums. Stub the bodies.
- **Pros:** Maximum fidelity to v4's proven shape; nothing to re-derive later.
- **Cons:** V4's CLI includes `learning`, `lock`, `diagnose`, `agent`, `coordinator`, `worktree` CRUD, and an `--as <role>` global - none of which v5 has in first-gate scope (`docs/vision.md` "Explicitly Not in First Gate"). Importing them now means either stubbing them as `StageUnimplemented` with no stage ever scheduled to light them up, or inventing fake stage numbers. Both are lies to `--help` readers.
- **Why not chosen:** V4's CLI grew organically over four paradigm shifts. V5's roadmap has nine labeled stages; the Stage 1 CLI should reflect that roadmap exactly, not v4's accreted shape. Features earn subcommands by being earned in a later stage, per Process Rule #1.

### Alternative 3: Inline source-guard into `lib::run` without a `guard` module

- **Description:** Put the walk-up logic as ten lines inside `lib.rs`.
- **Pros:** Fewer files; marginally simpler.
- **Cons:** The guard is the single most-tested unit of Stage 1 (negative case, positive case, root-reached case). Inline code is harder to test in isolation and the "single-word module per concept" rule from `rules/rust.md` points straight at a `guard.rs` file.
- **Why not chosen:** Testability plus the single-word-module convention.

## Technical Considerations

### Dependencies

Internal: `crates/loopr/Cargo.toml` already declares path dependencies on all 12 other workspace crates (`derive`, `telemetry`, `store`, `domain`, `llm`, `tools`, `worktree`, `ipc`, `context`, `decomposer`, `agents`, `integrator`). Stage 1 does not import any of them; they compile but are unused in the CLI skeleton and remain wired for later stages to activate. Rust's default `unused_crate_dependencies` lint is off, so no warnings.

External (added via `cargo add`):
- `clap` (workspace) - CLI parsing, derive feature
- `eyre` (workspace) - `main.rs` result type
- `thiserror` - typed `LooprError` (loopr is library + binary; thiserror is correct per `rules/rust.md`)
- `assert_cmd`, `predicates` (dev) - smoke-test invocation of the compiled binary
- `tempfile` (dev) - filesystem fixture for guard / target tests

### Performance

Not a concern at this scale. Clap parse is microseconds; the walk-up loop runs ≤20 iterations on any reasonable filesystem. Canonicalization does one `stat` per ancestor, bounded by tree depth.

### Security

- Source-guard prevents the v3/v4 recurring failure where an agent treated the loopr source tree as its target, potentially committing agent-produced changes into the loopr repo itself. Walking to `/` (rather than stopping at the target) defends against `loopr -C crates/loopr plan "x"` - if the guard lives at the v5 repo root, any subdirectory below it also trips.
- `-C` canonicalizes the path, so `-C ../../../etc/passwd` can't slip by as a relative directory; `TargetInvalid` fires for non-existent paths, `TargetIsFile` fires for files (with a hint pointing at the parent directory).
- No subprocess execution in Stage 1 (that's Stage 7 via `tools`); no user-supplied strings interpolate into shell commands.

### Testing Strategy

- **Unit tests (in each module):**
    - `cli.rs`: one `test_cli_parses_*` per `Command` variant (including nested `DaemonCmd` and `LogsCmd` variants); `test_cli_verify` calling `Cli::command().debug_assert()`.
    - `guard.rs`: sentinel at target, sentinel at ancestor, sentinel at root, no sentinel reaches `/`, `/tmp` is clean.
    - `target.rs`: all four precedence combinations (-C wins over env, env wins over CWD, CWD default, all-unset defaults to CWD), `LOOPR_TARGET=""` treated as unset, invalid-path case, `-C` to a file (not dir), plus three-tier discovery cases:
    - (a) start inside `<git-repo>/src/foo/bar/` resolves to `<git-repo>/` via `git rev-parse`
    - (b) start inside a non-git directory `<target>/sub/` where `<target>/.loopr/` exists resolves to `<target>/` via marker walk
    - (b2) same for `.taskstore/` marker
    - (c) start at `/tmp` (no git, no markers) falls through to `/tmp`
    - Tests use `tempfile::TempDir` fixtures: one with `git init`, one with a `.loopr/` dir and no git, one with `.taskstore/` dir and no git, one with neither.
- **Integration test (`tests/smoke.rs`):** exercises the compiled binary via `assert_cmd`; covers the four roadmap exit-criterion assertions end to end.
- **Seam test:** the `Cli::run(cli)` entry point is the seam between clap-land and the rest of `loopr`. Calling `run(Cli { chdir: Some(...), command: Command::Plan { goal: "x".into() } })` and asserting the exact `StageUnimplemented` variant is the seam test per Process Rule #2.

### Rollout Plan

- Stage 1 ships on branch `v5` (long-lived orphan branch) as commits on top of `37e02f2`.
- `otto ci` at workspace root passes on each phase commit; phases land incrementally, not as one mega-commit.
- **Versioning**: `v5` is a long-lived branch with an explicit repo-local override of `rules/git.md`'s "tags-only-on-main" constraint. Bump walks `v0.5.x` as Stage-1 work lands (e.g. `v0.5.1` after Phase 4 passes `otto ci`). Single flat `v*` workspace tag per release; every crate inherits via `version.workspace = true` (same pattern as `git-tools` / `aws-tools`).
- No coexistence: the one-line `main.rs` scaffold is replaced in one commit, not dual-pathed.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Subcommand names chosen here drift as later stages fill bodies | Medium | Medium | Each subcommand is named from `docs/vision.md`'s explicit list, not invented. Renames require a workspace-wide refactor via `replace`, which is cheap; the risk of needing one is real but bounded. |
| `git describe` in `build.rs` fails in sandbox / shallow clones | Medium | Low | Fallback to `CARGO_PKG_VERSION` (v4 pattern). Integration test runs from a proper clone, so the primary path is exercised. |
| Source-guard trips on a genuine target that happens to contain a `.loopr-source-guard` file (e.g. someone copies our repo) | Low | Low | The sentinel filename is unusual and namespaced. Error message prints the sentinel's absolute path so the user sees immediately where it is and can delete it if it's a stray copy. |
| `-C` canonicalization behaves differently on macOS vs Linux (symlink handling) | Low | Low | We use `std::path::Path::canonicalize`, which resolves symlinks on both platforms the same way. Docs the behavior in `loopr --help` for `-C`. |
| `thiserror` on a binary crate is unconventional | Low | Low | Justified by `loopr` being `[[bin]]` + `[lib]` simultaneously, and `LooprError` being part of the lib's public API. `main.rs` uses `eyre::Result` as the user-facing surface, so the `rules/rust.md` spirit ("CLIs: eyre") is preserved. |
| Clap's built-in `--version` / `--help` bypass source-guard | Low | Low | Intentional and standard clap behavior. Users may query version/help from anywhere, including inside the loopr source tree. Documented in the main.rs sketch comment so implementers don't add a redundant pre-clap guard. |

## Open Questions

- [x] **Does `-C` accept only directories, or also files (with the parent taken as target)?** Decided: directories only. A distinct `TargetIsFile` error variant fires for files, with the message hinting to pass the parent (`try -C <parent>`). Matches `git -C` semantics exactly.
- [x] **Where does `LOOPR_TARGET` env var fit?** Decided: between `-C` (highest) and CWD (lowest), per `docs/vision.md` CLI targeting section.
- [x] **Does the source-guard walk stop at the target's git root, or continue to `/`?** Decided: walk to `/`. A guard in an ancestor still counts, because the same issue (agent-writes-into-loopr-tree) applies if the target is a subdirectory of the v5 checkout.
- [x] **Should `loopr --version` include the workspace tag or each crate's inherited version?** Resolved: workspace `GIT_DESCRIBE` from `build.rs`, identical to `git-tools` / `aws-tools` pattern. All 13 crates inherit `version.workspace = true`; one tag per release; CLI prints `git describe --tags --always` output.
- [x] **Does `experiment` belong in Stage 1's shape at all, given it's post-First-Gate?** Resolved: **removed from Stage 1 CLI**. `experiment` was pulled forward from `vision.md`'s AutoResearch aspiration; v4 shipped `loopr score` (scores an existing run dir) + `bin/e2e` (bash harness). Keep that pattern. If AutoResearch earns its keep later, `experiment` is a natural add.

## References

- [`docs/vision.md`](../../../../docs/vision.md) §Target Repo Layout, §loopr ABI, §Git Posture
- [`docs/roadmap.md`](../../../../docs/roadmap.md) §Stage 1
- [`crates/loopr/CLAUDE.md`](../../CLAUDE.md) - scope rules for this crate
- v4 reference: `~/repos/scottidler/loopr/src/main.rs`, `~/repos/scottidler/loopr/src/cli.rs`, `~/repos/scottidler/loopr/build.rs`
- v2 proven patterns: `~/repos/scottidler/loopr/docs/v2-proven-patterns.md` §1 (client fork-to-daemon - applies at Stage 4, not Stage 1, but the CLI shape here is the hook it later extends)
- `rules/rust.md` - CLI conventions, thiserror-vs-eyre split, single-word modules
- `rules/general.md` - naming conventions
