# Design Document: Stage 7 Tool Registry

**Author:** Claude (with Scott)
**Date:** 2026-04-21
**Status:** Implemented
**Review Passes Completed:** 5/5 + Architect R1 + R2 + R3 + Scott post-R3 steer

## Summary

A `pub trait Tool` with typed `Input` / `Output` / `Error` associated types and a module-level `tools::dispatch(name, input, ctx)` function that translates Anthropic's tool-use JSON into the selected tool's typed call. Six first-gate builtins (`Read`, `Write`, `Edit`, `Bash`, `Grep`, `Glob`). Port v4's `spawn_with_process_group`, `bwrap_command`, and `LaneRouter` primitives verbatim; add the three pieces v3/v4 lacked: the `security.sandbox: required | preferred | off` posture knob, the Bash command denylist (tokenized via `shell-words`), and typed tool-site I/O.

This is the first of three Stage 7 design docs per `docs/roadmap.md:113–118`; `crates/worktree/docs/design/lifecycle.md` will add sibling worktrees + crash-recovery registry, and `crates/agents/docs/design/implementer.md` will consume this crate's dispatcher from the ralph loop. Shipping this doc's code is a prerequisite for Stage 7's exit criterion (a Bundle from the Implementer) but does not by itself satisfy it — the agent and worktree work must also land.

## Decisions

Locked up front. The doc defends each below; alternatives in their own section.

**Architect Round 1 (2026-04-21):** D1/D3/D6 upheld. D5/D8/D12 flipped (bypass vector, deadlock risk, env-allowlist breaks cargo). D4/D13 amended (stop using `tracing::Span::current()` as a data channel for the persist-path filename — pass `invocation_id` explicitly on `ToolContext`). D15/D16/D17/D18 added (UTF-8 lossy, bwrap signal propagation test, stdout/stderr interleave, CWD statelessness).

**Architect Round 2 (2026-04-21):** D5/D8/D12/D16/D17 all flipped again. Rust `shlex` crate doesn't do operator tokenization — commit to `conch-parser` for AST parsing (D5). Leading-token closure was broken (`cd x && cargo build` routed Net instead of Heavy; `RUST_LOG=debug cargo build` same bug) — rewrite to AST-walk via same `conch-parser` (D8). Env denylist too narrow — expanded prefixes and suffixes (D12). `killpg` on inner PGID doesn't survive bwrap's PID semantics even without `--unshare-pid` — kill bwrap's outer PID directly (D16). `[stderr]` inline tag ambiguous with real output — separate `stdout`/`stderr` fields on `Output` (D17).

**Architect Round 3 (2026-04-21):** D16 endorsed (`child.kill()` immediate SIGKILL on bwrap is correct). D8 list expanded by ~10 tools (apt, apt-get, brew, nix, nvm, tsc, jest, vitest, black, flake8, gem, bundle). D12 `_PASS` suffix dropped (false-positives on MULTIPASS/BYPASS/LOWPASS); constrained to `_PASSWORD` only. D17 flipped back toward chronology — added `combined_output` field (arrival-order interleave, 2>&1-equivalent) alongside separate `stdout`/`stderr`; chronological causation matters for build-error reasoning.

**Post-R3 Scott steer (2026-04-21):** Architect's R3 surfaced that `conch-parser` is POSIX-only, forcing a "write POSIX-sh" instruction in the agent prompt. Scott pointed at Claude Code's approach (`~/repos/anthropics/claude-code`); CHANGELOG line 504 confirms they moved to a "native module" for bash parsing and handle full Bash (heredocs, pipelines, `!` in jq commands, compound `cd x && npm test`). D5 flipped from `conch-parser` → **`tree-sitter-bash`** — full Bash grammar, no POSIX caveat, parity with Claude Code. YAML-file extraction of `HEAVY_EXECUTABLES` deferred; list stays in Rust source for now.

| # | Decision | Choice |
|---|---|---|
| D1 | Registry dispatch model | Trait + module-level `dispatch` fn; no `Box<dyn Tool>`, no enum |
| D2 | First-gate builtin cut | 6: Read, Write, Edit, Bash, Grep, Glob |
| D3 | Long subprocess output over IPC | Inline-truncate at 32K + persist full to `.loopr/runs/<run-id>/work/<work-id>/<invocation>.log`; chunked `DaemonEvent` events deferred |
| D4 | `ToolContext` shape | `working_dir`, `router`, `sandbox`, `path_deny_patterns`, `bash_denylist`, `persist_base`, `invocation_id`. Drop v4's `exec_id`. Tracing spans remain for log correlation only, **never** as a data channel for filename/path construction (Architect R1) |
| D5 | Bash denylist parsing | **`tree-sitter-bash`** via the `tree-sitter` crate — full Bash grammar (heredocs, `<(...)` process substitution, `foo=(a b)` arrays, `[[ ... ]]` conditionals, pipelines, subshells). Battle-tested (Neovim/Helix/GitHub syntax highlighting), incremental, error-tolerant. Parity with Claude Code's approach (CHANGELOG line 504: "switched to native module for bash command parsing"). **R3 flip**: original R2 pick was `conch-parser`, but R3 revealed it's POSIX-only and rejects common Bash constructs; the fix after Scott's "what does Claude Code do, can we steal" prompt. Cost: +~300KB for tree-sitter runtime + bash grammar vs. ~30KB for conch-parser; accepted as central to the tool layer. |
| D6 | `SandboxMode::Required` enforcement site | Daemon startup (every start, not just first `loopr init`) |
| D7 | `LaneRouter` lifecycle | Singleton, owned by `DaemonContext`, `Arc`-shared into `ToolContext` |
| D8 | Bash lane classification | **AST-walk on `conch-parser` output** (same parser as D5). Walks every simple command in the pipeline/list (skipping env-var assignments like `RUST_LOG=debug`); if ANY resolved command name is in `HEAVY_EXECUTABLES` → `Heavy`, else `Net`. Covers (R3-expanded, ~40 tools): `cargo`, `cargo-*` (subcommands via symlink), `npm`, `npx`, `pnpm`, `yarn`, `bun`, `deno`, `nvm`, `otto`, `make`, `cmake`, `pytest`, `black`, `flake8`, `go`, `tsc`, `jest`, `vitest`, `uv`, `pip`, `pipx`, `poetry`, `rustup`, `gradle`, `mvn`, `bazel`, `just`, `task`, `mise`, `docker`, `docker-compose`, `kubectl`, `terraform`, `terragrunt`, `apt`, `apt-get`, `brew`, `nix`, `gem`, `bundle`. `classify("bash") → Net` stays name-based per `docs/vision.md:542`; the per-command routing lives inside `Bash::execute`. R2 flipped this from a broken `find`-the-first-non-skip-token sniff; R3 expanded the list. |
| D9 | Per-call timeout override | Accepted, clamped to `lane.max_timeout_secs` |
| D10 | Async-fn-in-trait | Yes (Rust 2024); no `Pin<Box<dyn Future>>` boilerplate |
| D11 | Tool schema export format | JSON Schema via `schemars`; Anthropic-tool-use-compatible shape |
| D12 | Subprocess env scrubbing | **Denylist** with strict **prefix** + **suffix** match (not "contains" — would false-positive on `SSH_AUTH_SOCK`). Prefixes: `LOOPR_*`, `ANTHROPIC_*`, `AWS_*`, `GITHUB_*`, `GOOGLE_*`, `GCP_*`, `AZURE_*`, `OPENAI_*`, `GEMINI_*`. Suffixes: `*_API_KEY`, `*_SECRET`, `*_TOKEN`, `*_PASSWORD`, `*_CREDENTIALS`, `*_AUTH`. R3-dropped: `*_PASS` (false-positives on `MULTIPASS`, `BYPASS`, `LOWPASS`, `CLI_PASS_ARGS`); `*_PASSWORD` alone is sufficient for real passwords. `SSH_AUTH_SOCK` passes (ends in `_SOCK`); `SLACK_BOT_TOKEN` is stripped (ends in `_TOKEN`). Denylists are never complete — documented known gap. |
| D13 | Persist-path + invocation-id injection | `ToolContext.persist_base: Option<PathBuf>` and `ToolContext.invocation_id: Option<Uuid>`; agents set `Some` for production calls, unit tests leave `None` (persist falls back to `std::env::temp_dir().join("loopr-tool-output")`, invocation_id synthesized from timestamp). These are explicit fields, **not** read from `tracing::Span::current()` (Architect R1). |
| D14 | Startup sandbox telemetry | `tracing::info!` at `LaneRouter::new` (structured: `sandbox_mode`, `bwrap_available`, `bwrap_functional`); tool-call spans carry `sandbox_mode` by inheritance, not re-emission |
| D15 | Non-UTF-8 subprocess output | `String::from_utf8_lossy`, never `from_utf8`. ANSI escape sequences + raw bytes from `cargo`, `pytest`, etc. must not crash the Bash tool (Architect R1). |
| D16 | bwrap signal + kill strategy | Architect R2 clarified PID-namespace reasoning. **Do NOT add `--unshare-pid`** — keep v4's flag set (preserves grandchild semantics the agents rely on). **Add `--die-with-parent`** unconditionally as a safety net (bwrap exits if loopr daemon dies). **On timeout, kill bwrap's outer PID** (`child.kill()` on the `bwrap` process), NOT `killpg` on the inner PGID — killing bwrap cascades to every descendant regardless of `setsid` escape attempts inside. For non-bwrap spawns (Net/Heavy, no sandbox) keep the v4 `killpg` SIGTERM→5s→SIGKILL escalation. Phase 2 test: spawn `sleep 60 & wait` under bwrap at 1s timeout; assert `pgrep sleep` returns empty. |
| D17 | stdout/stderr on subprocess tools | **Three fields on `Output`**: `stdout: String`, `stderr: String`, `combined_output: String`, `exit_code: i32`. R2 dropped `[stderr]` inline tags (ambiguous with literal text); R3 flipped back toward chronology by adding `combined_output` (arrival-order interleave, equivalent to shell `2>&1`). Agents use `combined_output` for cause-and-effect reasoning about build errors ("this cargo error fired while compiling *that* file"); separate `stdout`/`stderr` available when an agent wants them disentangled. Spawn.rs reads both streams via separate `Stdio::piped()` handles through a `tokio::select!` loop, appending each chunk to its stream buffer AND to the combined buffer in arrival order. |
| D18 | Bash CWD statelessness | Each Bash invocation is a fresh process. `cd x && ls` within one call works; two consecutive `cd x` then `ls` calls do **not** persist. Documented in `builtin/bash.rs` module doc so the LLM prompt can surface the constraint. |

## Problem Statement

### Background

v3 (`~/repos/scottidler/loopr/src/tools/`) and v4 (`~/repos/scottidler/loopr-v4/src/tools/`) ship essentially the same tool layer: 14 builtins behind a `HashMap<String, Box<dyn Tool>>` registry, with `serde_json::Value` as both input and output, and `Pin<Box<dyn Future>>` return types for object safety. Lane classification, bwrap sandboxing, setsid+killpg subprocess hygiene, and Anthropic wire types all landed in v3 and survived v4 unchanged.

Three items never landed in v3/v4:
1. The `security.sandbox: required | preferred | off` posture knob — v3/v4 had a boolean with a startup `warn!` if bwrap was missing and nothing else.
2. The **Bash command denylist** — v3/v4's deny patterns only covered *file paths* (`.env`, `.key`, etc.), not commands.
3. **Typed `Input` / `Output` / `Error`** at tool sites — v3/v4 used `serde_json::Value` end-to-end.

Stage 7 adopts all three, per `docs/vision.md` lines 118–127 (the `tools` ABI contract) and lines 514–558 (the Security section with the three-lane table and denylist).

### Problem

Stage 7 needs a tool registry that:

1. Is callable from Anthropic's tool-use wire format (`{"name": "read", "input": {...}}`) with typed Input/Output/Error at the tool impl site.
2. Respects vision.md's "typed associated types" and user's no-`dyn`-for-DI rule simultaneously.
3. Enforces lane isolation (Local/Net/Heavy concurrency caps via tokio semaphores; Local bwrap --unshare-net when posture demands).
4. Refuses known-bad Bash commands before they reach the subprocess spawner.
5. Exposes each tool's JSON Schema for `agents::ContextBuilder` to render into prompts.
6. Handles subprocess output larger than IPC's 1 MiB `LinesCodec` cap without severing the client-daemon socket.

### Goals

- `pub trait Tool` with typed `Input: Deserialize + JsonSchema`, `Output: Serialize`, `Error: Into<ToolError>` associated types; async fn `execute(input, ctx) -> Result<Output, Error>`.
- Six zero-sized-type builtins implementing `Tool` (`Read`, `Write`, `Edit`, `Bash`, `Grep`, `Glob`), one per file under `src/builtin/`.
- Module-level dispatcher: `pub async fn dispatch(name: &str, input: serde_json::Value, ctx: &ToolContext) -> Result<serde_json::Value, ToolError>` matches on name and fans out to the right tool's typed `execute`.
- `LaneRouter` singleton with per-lane tokio semaphores; `LanePolicy::{local, net, heavy}` with vision-verbatim slot/timeout numbers.
- `bwrap_command` wrapping with the 3-value `SandboxMode` knob enforced at daemon startup.
- `BashDenylist` with base-hardcoded patterns + target tighten-only extensions; AST-based matching via `tree-sitter-bash` (full Bash grammar, walks simple-command / pipeline / list / subshell nodes). Same parser reused for D8 Bash lane routing.
- `tools::all_schemas() -> Vec<ToolSchema>` for prompt rendering.
- Subprocess primitives ported verbatim from v4: `spawn_with_process_group` (setsid + killpg SIGTERM→5s→SIGKILL, with `bwrap --die-with-parent` safety net per D16), inline-truncate at 32K + persist full output to `.loopr/runs/<run-id>/work/<work-id>/<invocation>.log`. Output captured via `String::from_utf8_lossy` (D15) so non-UTF-8 bytes don't crash the tool.
- Stdout + stderr captured and surfaced on `Output` as **three fields**: `stdout`, `stderr`, and `combined_output` (arrival-order interleave for chronological causation, per D17/R3). No inline tag markers — structural fields instead.
- Bash lane chosen per invocation by **AST-walk** over the parsed command (not string-sniffing) — any executable in `HEAVY_EXECUTABLES` anywhere in the pipeline/list routes the whole invocation to `Heavy`; else `Net` (D8).
- Closed typed `ToolError`; no `eyre::Report` escape.
- Subprocess environment scrubbed via **denylist** of secret-bearing variables (`LOOPR_*`, `ANTHROPIC_*`, `AWS_*`, `GITHUB_*`, `*_API_KEY`, `*_SECRET*`, `*_TOKEN`); all other vars pass through so cargo/npm/rustup can find their config (`SSH_AUTH_SOCK`, `XDG_*`, `RUSTUP_HOME`, `CARGO_HOME`, etc.).

### Non-Goals

- **Streaming tool output via `DaemonEvent::ToolOutputChunk` events.** No Stage 7 consumer (TUI explicitly beyond first gate per vision.md line 595). `crates/tools/CLAUDE.md:38–45` invites this as a design-doc call; deferring with rationale. See Alternative 4.
- **v4's other 8 builtins** (`list`, `tree`, `find`, `fetch`, `search`, `delegate`, `slash`, `todo`, `plan`). Vision line 122 says 6 for first gate; the others earn their way.
- **Background subprocess tasks** (v4's `shell tool run_in_background` param). No Stage 7 consumer. Deferred.
- **Dynamic tool registration via IPC** (v4's `tools.register` handler). Agent-harness scaffolding; v5 keeps builtins compile-time-fixed until a real need surfaces.
- **Permission tier UI / tool-approval prompts.** TUI-era concern.
- **Tool-schema OpenAI-compat / Gemini-compat normalizers.** v5 is Anthropic-only per the Stage 6 `llm` design.
- **`cargo`/`npm`/`otto` detection + auto-configuration of project tools.** v3/v4's `detect.rs` is part of the config-driven tool pattern, which v5 drops; builtins are fixed.

## Proposed Solution

### Overview

One public trait `Tool` with typed associated types. Each builtin is a zero-sized unit struct (`pub struct Read;`) with a `Tool` impl. The crate's public dispatch entrypoint is a module-level async function that matches on name and calls the selected tool's typed `execute`, deserializing LLM JSON into the tool's `Input` at the match arm and serializing the `Output` back out. No `HashMap`, no `Box<dyn Tool>`, no enum.

Lane routing and sandboxing sit beneath the trait, not inside it: `LaneRouter` is a singleton owned by `DaemonContext` and passed via `Arc` into every `ToolContext`. Each tool picks its lane statically via `fn lane() -> Lane`. Subprocess-spawning tools (`Bash`, `Grep`) build a pre-configured `tokio::process::Command`, hand it to the router, the router acquires a semaphore permit, wraps with `bwrap` for `Local`-lane commands when posture demands, spawns in its own process group, and enforces timeout via SIGTERM/SIGKILL escalation on the PGID.

### Architecture

```
┌─────────────────────────────────────────────────────────────────┐
│ agents (ralph loop)                                             │
│  LlmClient emits ToolCall { name, id, input: Value }            │
│       │                                                         │
│       ▼                                                         │
│  tools::dispatch(name, input, ctx) -> Result<Value, ToolError>  │
└─────────────────┬───────────────────────────────────────────────┘
                  │
                  ▼
┌─────────────────────────────────────────────────────────────────┐
│ tools crate                                                     │
│                                                                 │
│  ┌───────────────────────────┐                                  │
│  │ dispatch(name, val, ctx)  │                                  │
│  │   match name {            │                                  │
│  │     "read"  => run(Read,  │                                  │
│  │     "write" => run(Write, │                                  │
│  │     "edit"  => run(Edit,  │                                  │
│  │     "bash"  => run(Bash,  │                                  │
│  │     "grep"  => run(Grep,  │                                  │
│  │     "glob"  => run(Glob,  │                                  │
│  │     _       => Err(...),  │                                  │
│  │   }                       │                                  │
│  └────────────┬──────────────┘                                  │
│               │ generic run::<T: Tool>: Value -> T::Input,      │
│               │   T::execute, T::Output -> Value                │
│               ▼                                                 │
│  ┌──────────────────────────────────────────────┐               │
│  │ builtin/{read,write,edit,bash,grep,glob}.rs  │               │
│  └────────┬─────────────────────────────────────┘               │
│           │ Bash + Grep shell out:                              │
│           ▼                                                     │
│  ┌──────────────────┐  (Bash pre-flight)                        │
│  │  BashDenylist    │─► DenyReason? -> ToolError::BashDenied    │
│  │  (tokenized)     │                                           │
│  └────────┬─────────┘                                           │
│           ▼                                                     │
│  ┌──────────────┐    ┌──────────────────┐   ┌────────────────┐  │
│  │ LaneRouter   │───►│ bwrap_command    │──►│ spawn.rs       │  │
│  │ (tokio       │    │ (if Local lane + │   │ setsid +       │  │
│  │  semaphores) │    │  posture allows) │   │ killpg escalate│  │
│  └──────┬───────┘    └──────────────────┘   └────────────────┘  │
│         │                                                       │
│         │ on completion: SpawnResult (inline-truncate + persist)│
│         ▼                                                       │
│  ┌────────────────────────────────────────────────────────────┐ │
│  │ .loopr/runs/<run-id>/work/<work-id>/<invocation>.log       │ │
│  │   (full output when >32K inline cap)                       │ │
│  └────────────────────────────────────────────────────────────┘ │
└─────────────────────────────────────────────────────────────────┘
```

### Data Model

#### The `Tool` trait

```rust
pub trait Tool: Sized + Send + Sync {
    type Input:  for<'de> Deserialize<'de> + JsonSchema + Send;
    type Output: Serialize + Send;
    type Error:  Into<ToolError> + Send;

    fn name() -> &'static str;
    fn description() -> &'static str;
    fn lane() -> Lane;
    fn schema() -> ToolSchema;

    async fn execute(input: Self::Input, ctx: &ToolContext) -> Result<Self::Output, Self::Error>;
}
```

Associated items (`name` / `description` / `lane` / `schema`) are *static*, not `&self`-receiving, because every builtin is a zero-sized unit struct — all per-invocation state flows through `Input` / `Output`. The trait is *not* object-safe (`Sized` bound, generic-return associated types) and is never used as a trait object.

**AFIT Send-inference**: the async `execute` returns `impl Future<Output = Result<_, _>> + '_`. Whether that future is `Send` is inferred from the body. Stage 7 bodies only hold `Send` state across `.await` points (file I/O, subprocess spawning, semaphore acquisition — all `Send`); if a future builtin ever holds non-Send state across an `.await`, the call from `tokio::spawn` inside the agent ralph loop will fail to compile, surfacing at the agent crate rather than here. Known gotcha; no runtime workaround needed.

Each builtin is a unit struct:

```rust
pub struct Read;
pub struct Write;
pub struct Edit;
pub struct Bash;
pub struct Grep;
pub struct Glob;

impl Tool for Read {
    type Input  = read::Input;
    type Output = read::Output;
    type Error  = read::Error;

    fn name() -> &'static str { "read" }
    fn description() -> &'static str { read::DESCRIPTION }
    fn lane() -> Lane { Lane::Local }
    fn schema() -> ToolSchema { schema::for_tool::<Self>() }

    async fn execute(input: Self::Input, ctx: &ToolContext) -> Result<Self::Output, Self::Error> {
        read::execute(input, ctx).await
    }
}
```

#### Module-level dispatch

```rust
pub async fn dispatch(
    name: &str,
    input: serde_json::Value,
    ctx: &ToolContext,
) -> Result<serde_json::Value, ToolError> {
    match name {
        "read"  => run::<Read>(input, ctx).await,
        "write" => run::<Write>(input, ctx).await,
        "edit"  => run::<Edit>(input, ctx).await,
        "bash"  => run::<Bash>(input, ctx).await,
        "grep"  => run::<Grep>(input, ctx).await,
        "glob"  => run::<Glob>(input, ctx).await,
        other   => Err(ToolError::UnknownTool(other.to_string())),
    }
}

// Generic over any Tool; the trait's `type Error: Into<ToolError>` bound makes
// Into::into resolution automatic, no extra where-clause needed.
async fn run<T: Tool>(input: serde_json::Value, ctx: &ToolContext) -> Result<serde_json::Value, ToolError> {
    let typed: T::Input = serde_json::from_value(input)
        .map_err(|e| ToolError::InvalidInput(e.to_string()))?;
    let output = T::execute(typed, ctx).await.map_err(Into::into)?;
    serde_json::to_value(output).map_err(|e| ToolError::SerializeOutput(e.to_string()))
}

pub fn all_schemas() -> Vec<ToolSchema> {
    vec![
        Read::schema(),
        Write::schema(),
        Edit::schema(),
        Bash::schema(),
        Grep::schema(),
        Glob::schema(),
    ]
}

pub fn schema_for(name: &str) -> Option<ToolSchema> {
    all_schemas().into_iter().find(|s| s.name == name)
}
```

Adding a new builtin is a five-edit commit: unit struct, `Tool` impl, `dispatch` match arm, `all_schemas` entry, per-tool module file. Compile-time enforcement via the generic `run::<T>`.

#### Per-tool types (example: `Read`)

```rust
// builtin/read.rs

pub const DESCRIPTION: &str = "Read a file with line numbers. Default max 500 lines; \
                               use offset/limit to paginate.";

#[derive(Deserialize, JsonSchema)]
pub struct Input {
    pub path: PathBuf,
    pub offset: Option<u64>,
    pub limit:  Option<u64>,
}

#[derive(Serialize)]
pub struct Output {
    pub content:     String,
    pub lines_shown: usize,
    pub lines_total: usize,
    pub truncated:   bool,
}

#[derive(thiserror::Error, Debug)]
pub enum Error {
    #[error("path escapes sandbox: {0}")]
    SandboxViolation(String),
    #[error("failed to read {path}: {source}")]
    Io { path: PathBuf, source: std::io::Error },
    #[error("path matched deny pattern: {0}")]
    PathDenied(String),
}

impl From<Error> for ToolError {
    fn from(e: Error) -> Self {
        match e {
            Error::SandboxViolation(s) => Self::SandboxViolation(s),
            Error::Io { path, source }  => Self::Io(std::io::Error::new(
                source.kind(),
                format!("{}: {}", path.display(), source),
            )),
            Error::PathDenied(s)        => Self::PathDenied(s),
        }
    }
}

pub async fn execute(input: Input, ctx: &ToolContext) -> Result<Output, Error> { /* ... */ }
```

Similar shape for `write.rs`, `edit.rs`, `bash.rs`, `grep.rs`, `glob.rs`. Module-local types named `Input` / `Output` / `Error`; disambiguated by path (`read::Input`, `write::Input`).

#### `ToolContext`

```rust
pub struct ToolContext {
    pub working_dir: PathBuf,
    pub router: Arc<LaneRouter>,
    pub sandbox: SandboxMode,
    pub path_deny_patterns: Vec<String>,
    pub bash_denylist: Arc<BashDenylist>,
    /// Base directory for persisting subprocess output when inline-truncation fires.
    /// Agents set `Some(.loopr/runs/<run-id>/work/<work-id>/)`; unit tests leave `None`
    /// (spawn falls back to `std::env::temp_dir().join("loopr-tool-output/")`).
    pub persist_base: Option<PathBuf>,
    /// Unique identifier for this tool invocation. Used to name the persist-overflow
    /// file (`<invocation_id>.log`). `None` in unit tests — spawn synthesizes a
    /// timestamp-based fallback. Explicit field, **not** read from `tracing::Span::current()`:
    /// after Architect R1, tracing spans are never used as a data channel for path
    /// construction (brittle across `tokio::spawn` boundaries unless `.instrument()` is
    /// explicit, and conflates telemetry with business logic).
    pub invocation_id: Option<Uuid>,
}
```

Dropped from v4's `ToolContext`: `exec_id` as a string tag (replaced with `invocation_id: Option<Uuid>`; explicit, not span-derived), `read_files` tracking (v4's "Read-before-Edit" enforcement hit false negatives on symlinks; not required by vision).

`path_deny_patterns` and `bash_denylist` are composed by the daemon at startup from baked-in defaults + `.loopr/config.yml`. `invocation_id` and `persist_base` are set per-call by the agent's ralph loop before each `dispatch` invocation.

#### `ToolError`

```rust
#[derive(thiserror::Error, Debug)]
pub enum ToolError {
    #[error("unknown tool: {0}")]
    UnknownTool(String),

    #[error("invalid input: {0}")]
    InvalidInput(String),

    #[error("failed to serialize output: {0}")]
    SerializeOutput(String),

    #[error("sandbox violation: {0}")]
    SandboxViolation(String),

    #[error("bash command rejected by denylist: {reason}")]
    BashDenied { reason: String },

    #[error("path denied: {0}")]
    PathDenied(String),

    #[error("timed out after {timeout_secs}s: {tool}")]
    Timeout { tool: &'static str, timeout_secs: u64 },

    #[error("execution failed: {0}")]
    ExecutionFailed(String),

    #[error("lane semaphore closed: {0:?}")]
    LaneClosed(Lane),

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),
}
```

Closed enum. Per-tool errors convert through `From` impls at the dispatch edge. `ToolError` has no `Other(String)` or `eyre::Report` variant — vision-mandated.

#### Lane / LanePolicy / LaneRouter

Port v4 verbatim for the types and numbers (`docs/vision.md` lines 536–540):

```rust
pub enum Lane { Local, Net, Heavy }

pub struct LanePolicy {
    pub lane: Lane,
    pub max_slots: usize,
    pub default_timeout_secs: u64,
    pub max_timeout_secs: u64,
    pub sandbox_net: bool,  // only Local
}

// Local:  slots=10, default=30s,  max=60s,   sandbox_net=true
// Net:    slots=5,  default=60s,  max=120s,  sandbox_net=false
// Heavy:  slots=1,  default=600s, max=1800s, sandbox_net=false

pub fn classify(tool_name: &str) -> Lane { /* match arms; see Phase 1 */ }
```

```rust
pub struct LaneRouter {
    policies: HashMap<Lane, LanePolicy>,
    semaphores: HashMap<Lane, Arc<Semaphore>>,
    sandbox: SandboxMode,
    bwrap_available: bool,
}

impl LaneRouter {
    pub fn new(sandbox: SandboxMode) -> Result<Self, RouterInitError> {
        let bwrap_available = detect_bwrap_functional()?;
        match (sandbox, bwrap_available) {
            (SandboxMode::Required, false) => Err(RouterInitError::BwrapRequired),
            (SandboxMode::Preferred, false) => {
                tracing::warn!("bwrap not available; Local lane will run UNSANDBOXED (sandbox: preferred)");
                Ok(Self::build(sandbox, false))
            }
            _ => Ok(Self::build(sandbox, bwrap_available)),
        }
    }

    pub async fn spawn(
        &self,
        cmd: tokio::process::Command,
        lane: Lane,
        timeout_secs: Option<u64>,
    ) -> Result<SpawnResult, ToolError> { /* ... */ }
}
```

Change from v3/v4: `spawn()` takes a pre-built `tokio::process::Command`, not a shell string. This closes Grep's string-concatenation injection vector (v4: `grep -rn '{pattern}' '{path}'` via `sh -c`). Shell-wrapping becomes a helper (`shell::sh_command(cmd_str, cwd) -> Command`) used by `Bash`; `Grep` builds `Command::new("grep")` directly.

**Bwrap wrapping of a Command** (not a string) is the tricky bit. `bwrap_wrap(cmd: Command, cwd: &Path) -> Command` extracts `program`, `args`, and the configured `current_dir` via `Command::as_std().get_program()` / `get_args()` / `get_current_dir()`, then rebuilds as:

```
bwrap --unshare-net --ro-bind / / --dev /dev --proc /proc
      --bind /tmp /tmp --bind <cwd> <cwd> --chdir <cwd>
      -- <program> <args...>
```

No `sh -c`; the arg vector survives the bwrap boundary verbatim, preserving the shell-injection protection that the Command-based shape gives us.

One `LaneRouter` instance per daemon, owned by `DaemonContext`, `Arc`-shared into every `ToolContext`.

**Startup-refuse surfacing**: `LaneRouter::new(SandboxMode::Required)` returning `Err(RouterInitError::BwrapRequired)` is a fatal daemon-boot error. `loopr` surfaces it with the install command (`apt install bubblewrap`) and the config knob to downgrade (`.loopr/config.yml: tools.sandbox: preferred`). The daemon does NOT start in this state — a subsequent `loopr plan ...` will get "daemon not running" with the same actionable error via the CLI's connect-or-fork-daemon path.

#### `SandboxMode`

```rust
#[derive(Deserialize, Copy, Clone)]
#[serde(rename_all = "lowercase")]
pub enum SandboxMode {
    Required,   // default; daemon startup fails if bwrap missing
    Preferred,  // use if present; warn + run unsandboxed if not
    Off,        // skip sandbox; quiet
}

impl Default for SandboxMode {
    fn default() -> Self { Self::Required }
}
```

Enforced at **every daemon startup** (not just `loopr init`), so a later `apt remove bubblewrap` surfaces. Recorded via `tracing::info!` at `LaneRouter::new` (not on per-tool-call spans — the posture doesn't change between calls, so re-emitting is noise); see D14.

#### `BashDenylist`

Token-based (not substring; see D5 and Alternative 5). Patterns match contiguous shell-token subsequences:

```rust
pub struct BashDenylist {
    patterns: Vec<DenyPattern>,
}

pub struct DenyPattern {
    pub tokens: Vec<TokenMatcher>,
    pub reason: String,
}

pub enum TokenMatcher {
    Literal(Cow<'static, str>),  // exact token match
    Prefix(Cow<'static, str>),   // token starts with
    Any,                         // wildcard single token
}

impl BashDenylist {
    pub fn with_base() -> Self { /* vision.md base patterns */ }
    pub fn extend_from(&mut self, cfg: &ToolsConfig) {
        // Per-entry conversion: DenyEntryConfig (owned Strings from serde)
        // into DenyPattern (TokenMatcher enum). Literal tokens stay Literal;
        // tokens ending in `*` become Prefix; a bare `*` becomes Any.
        for entry in &cfg.bash_denylist_extend {
            self.patterns.push(DenyPattern::from_config(entry));
        }
    }
    pub fn check(&self, command: &str) -> Result<(), &DenyPattern> { /* parse via tree-sitter-bash, walk command nodes, match each simple command's argv against patterns */ }
}

/// Serde-facing config shape. Converted into `DenyPattern` at startup.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub struct DenyEntryConfig {
    pub tokens: Vec<String>,
    pub reason: String,
}
```

Base patterns (vision verbatim):

```rust
fn base() -> Vec<DenyPattern> {
    use TokenMatcher::*;
    vec![
        DenyPattern { tokens: vec![Literal("rm".into()), Literal("-rf".into()), Literal("/".into())],
                      reason: "deletes root filesystem".into() },
        DenyPattern { tokens: vec![Literal("rm".into()), Literal("-rf".into()), Literal("~".into())],
                      reason: "deletes home directory".into() },
        DenyPattern { tokens: vec![Literal("sudo".into()), Any],
                      reason: "privilege escalation".into() },
        DenyPattern { tokens: vec![Literal("|".into()), Literal("sh".into())],
                      reason: "piped shell execution".into() },
        DenyPattern { tokens: vec![Literal("|".into()), Literal("bash".into())],
                      reason: "piped shell execution".into() },
        DenyPattern { tokens: vec![Literal("git".into()), Literal("push".into())],
                      reason: "push policy is human-only".into() },
        DenyPattern { tokens: vec![Literal("gh".into()), Literal("repo".into()), Literal("delete".into())],
                      reason: "destructive github op".into() },
    ]
}
```

Target extension via `.loopr/config.yml`:

```yaml
tools:
  bash-denylist-extend:
    - tokens: ["./deploy.sh"]
      reason: "deploys are a human action"
```

**Tighten-only**: the deserializer has no `replace` key — only `extend` is accepted. Structurally impossible to remove a base pattern.

### API Design

Public surface of the crate:

```rust
// lib.rs re-exports
pub use config::{ToolsConfig, BashDenylistConfig};
pub use error::ToolError;
pub use lane::{Lane, LanePolicy, classify};
pub use router::{LaneRouter, RouterInitError};
pub use sandbox::SandboxMode;
pub use schema::ToolSchema;
pub use tool::{Tool, ToolContext};

pub use builtin::{Read, Write, Edit, Bash, Grep, Glob};

pub async fn dispatch(
    name: &str,
    input: serde_json::Value,
    ctx: &ToolContext,
) -> Result<serde_json::Value, ToolError>;

pub fn all_schemas() -> Vec<ToolSchema>;
pub fn schema_for(name: &str) -> Option<ToolSchema>;
```

Internal (`pub(crate)`):
- `spawn::spawn_with_process_group`, `spawn::SpawnResult`
- `sandbox::{detect_bwrap_functional, bwrap_command}`
- `denylist::BashDenylist`
- `shell::sh_command`

### File Layout

```
crates/tools/src/
├── lib.rs                      # pub re-exports + top-level dispatch/all_schemas
├── tool.rs                     # Tool trait + ToolContext
├── error.rs                    # ToolError
├── config.rs                   # ToolsConfig, BashDenylistConfig
├── schema.rs                   # ToolSchema + schemars bridge
├── lane.rs                     # Lane, LanePolicy, classify()
├── router.rs                   # LaneRouter, RouterInitError
├── sandbox.rs                  # SandboxMode, detect_bwrap_functional, bwrap_command
├── spawn.rs                    # spawn_with_process_group, SpawnResult
├── shell.rs                    # sh_command helper
├── denylist.rs                 # BashDenylist, DenyPattern, TokenMatcher
└── builtin/
    ├── read.rs
    ├── write.rs
    ├── edit.rs
    ├── bash.rs
    ├── grep.rs
    └── glob.rs
```

vision.md line 122 says "one per file under `src/tools/`" for builtins. We put them under `src/builtin/` — the crate is already `tools`, so the subdirectory distinguishes *role* (builtin) from *infrastructure* (lane, router, sandbox).

### Implementation Plan

#### Phase 1: Scaffold types (trait, errors, lane, config, schema)
**Model:** sonnet

- `tool.rs`: `Tool` trait with typed associated types, `ToolContext` struct.
- `error.rs`: closed `ToolError` enum, `From<std::io::Error>`.
- `lane.rs`: `Lane`, `LanePolicy::{local, net, heavy}`, `classify()` with verbatim mapping from v4.
- `config.rs`: `ToolsConfig { sandbox: SandboxMode, bash_denylist_extend: Vec<DenyEntryConfig>, path_deny_patterns: Vec<String> }` with `#[serde(rename_all = "kebab-case")]`.
- `schema.rs`: `ToolSchema { name, description, input_schema: serde_json::Value }`, `for_tool::<T: Tool>()` helper using `schemars`.
- `sandbox.rs`: `SandboxMode` enum only (detection lands in Phase 2).
- Unit tests: lane classification, error display, config serde round-trip, schema generation for a trivial dummy tool.

#### Phase 2: Subprocess + sandbox + denylist primitives
**Model:** sonnet

- `spawn.rs`: port `spawn_with_process_group` from v4 verbatim. `MAX_INLINE_OUTPUT = 32_000`. Change the persist path from `/tmp/loopr-tool-output/` to accept an injected base path (caller provides `.loopr/runs/<run-id>/work/<work-id>/`); daemon sets this up. Use `String::from_utf8_lossy` on stdout/stderr capture (D15) — subprocess output is not guaranteed UTF-8 and `from_utf8` would panic on a `cargo build` that emits ANSI escapes mixed with raw bytes. Capture stdout and stderr through separate `Stdio::piped()` handles; drive them via `tokio::select!` over line-readers on each, appending each chunk to its per-stream buffer AND to a third `combined_output` buffer in arrival order (D17/R3). `SpawnResult` returns all three.
- `sandbox.rs`: `detect_bwrap_functional()` actually *invokes* `bwrap --unshare-net --ro-bind / / -- /bin/true` to verify kernel support (not just binary presence, which `bwrap --version` checks). `bwrap_command(cmd: Command, working_dir)` ported verbatim, with `--die-with-parent` added unconditionally (D16 safety net). Phase 2 signal-propagation test: spawn `sleep 60` via router-with-bwrap-wrapping at 1s timeout, verify after escalation that `pgrep -f "sleep 60"` finds zero processes. If the test reveals bwrap's PID-1 swallows signals despite `--die-with-parent`, escalate to killing bwrap's outer PID directly from `spawn_with_process_group` rather than `killpg` on the inner PGID.
- `shell.rs`: `sh_command(cmd_str, cwd) -> tokio::process::Command` helper for `Bash`.
- `router.rs`: `LaneRouter::new(sandbox)` with startup posture enforcement per D6. `spawn(cmd: Command, lane, timeout_secs)` takes pre-built `Command`, acquires semaphore, wraps with `bwrap_command` for Local+posture-allows, calls `spawn_with_process_group`, releases permit. Records `tool_invocation_id`, `lane`, `timeout_secs` on a child span.
- `denylist.rs`: `BashDenylist::with_base()`, `extend_from(&ToolsConfig)`, `check(cmd)` that parses via `tree-sitter-bash` and walks the syntax tree: for each `command` node, extract the `command_name` + argv child nodes, match against each `DenyPattern`'s token sequence. Tree-sitter returns a CST so walking involves node-kind checks (e.g. `node.kind() == "command"`), not typed variants — slightly more verbose than an AST but tolerates partial/invalid input (tree-sitter's "ERROR" nodes preserved; we can still find the valid command nodes around them). Extensions over raw string-scan: env-var assignments (`FOO=bar cmd`) treated as metadata not command; pipelines / lists / redirections walked so `rm -rf /` inside `echo hi && rm -rf /` is caught.
- Tests: spawn echo, spawn timeout — under both plain-shell (`killpg`) AND bwrap (`child.kill()` on outer bwrap PID, per D16), bwrap wraps correctly, router slot enforcement serializes Heavy lane, denylist rejects every base pattern, denylist does NOT false-positive on `echo "git push is disabled"` (quoting in AST), denylist DOES catch `curl x\|sh` (no-space — AST gives us pipeline semantics regardless of whitespace), denylist catches `rm -rf /` buried inside `echo hi && rm -rf /` (D5 AST-walk benefit over v4 substring match), subprocess output with `\xff\xfe` bytes survives via `from_utf8_lossy` (D15).

#### Phase 3: First-gate builtins (6 tools)
**Model:** sonnet

One file per tool under `src/builtin/`, each module-local `Input` / `Output` / `Error` types + `pub async fn execute` + per-tool `impl Tool for X` block in a separate `impl.rs` aggregator or inlined in each file:

- `read.rs` — path + optional offset/limit, default 500-line cap, numbered output. Lane `Local`.
- `write.rs` — path + content, `create_dir_all(parent).await`, returns `bytes_written`. Lane `Local`.
- `edit.rs` — exact-match unique replacement (error if 0 or >1 matches). Lane `Local`.
- `bash.rs` — command + optional timeout. Per D8, routes via `tree-sitter-bash` AST walk (same parse as D5 denylist check):
  ```rust
  // Kept inline in Rust source for now (YAML extraction deferred per Scott 2026-04-21).
  // Matched against each parsed `command` node's command_name child.
  const HEAVY_EXECUTABLES: &[&str] = &[
      // Rust
      "cargo", "rustup",
      // Node / JS
      "npm", "npx", "pnpm", "yarn", "bun", "deno", "nvm",
      "tsc", "jest", "vitest",
      // Python
      "pytest", "black", "flake8", "uv", "pip", "pipx", "poetry",
      // Go
      "go",
      // Build systems
      "make", "cmake", "gradle", "mvn", "bazel", "just", "task", "otto",
      // Tool runners
      "mise",
      // Containers / infra
      "docker", "docker-compose", "kubectl", "terraform", "terragrunt",
      // Package managers
      "apt", "apt-get", "brew", "nix", "gem", "bundle",
  ];

  /// Prefixes: if a command head begins with any of these, route Heavy.
  /// Covers cargo subcommands installed as symlinked binaries (cargo-expand, cargo-nextest, ...).
  const HEAVY_PREFIXES: &[&str] = &["cargo-"];

  fn lane_for_command(tree: &tree_sitter::Tree, source: &str) -> Lane {
      // Walk every `command` node in the CST (tree-sitter-bash's grammar handles
      // pipelines, lists &&/||, subshells, subcommands). For each, read the
      // command_name child, skip env-var prefix nodes (variable_assignment), and
      // check against HEAVY_EXECUTABLES + HEAVY_PREFIXES.
      // Correctly routes: `cd x && cargo build`, `RUST_LOG=debug cargo build`,
      // `echo start && cargo build`, `(cd x; cargo build)`, `cargo-nextest run`.
      for node in iter_command_nodes(tree.root_node()) {
          let head = resolved_head(node, source);  // strips env assignments, ./ prefix
          if HEAVY_EXECUTABLES.contains(&head) || HEAVY_PREFIXES.iter().any(|p| head.starts_with(p)) {
              return Lane::Heavy;
          }
      }
      Lane::Net
  }
  ```
  Flow: parse command via `tree-sitter-bash` ONCE → `BashDenylist::check(&tree)` → `lane_for_command(&tree)` → `shell::sh_command` → `LaneRouter::spawn(lane, ...)`. Parsing once and reusing the tree avoids re-parsing. tree-sitter is error-tolerant: an unparseable fragment yields an `ERROR` node in the CST, but surrounding valid commands are still walkable — we find command nodes whose kind is `command` and skip error regions for matching purposes. A command that is *entirely* unparseable returns `ToolError::InvalidInput`. No POSIX caveat — full Bash (heredocs, `<(...)`, arrays, `[[ ]]`) parses cleanly. D18 module doc: each invocation is a fresh process; `cd x && ls` within ONE call works, but two consecutive calls do NOT share CWD. `Output` has `stdout`, `stderr`, `combined_output`, and `exit_code` fields per D17.
- `grep.rs` — pattern + optional path + optional glob. Builds `Command::new("grep").arg("-rn").arg(&pattern).arg(&path)` directly (no `sh -c`), routed via `LaneRouter::spawn(Local, ...)`.
- `glob.rs` — pattern. Uses `glob` crate directly (no subprocess). Strips `working_dir` prefix from results. Lane `Local` (convention; no subprocess emitted).

Each tool validates paths through `working_dir` boundary + `path_deny_patterns`.

Wire each into `dispatch` match arm and `all_schemas()`.

#### Phase 4: `DaemonContext` wiring in `loopr`
**Model:** sonnet

- `crates/loopr/src/daemon/context.rs`: add `pub router: Arc<LaneRouter>`, `pub path_deny_patterns: Vec<String>`, `pub bash_denylist: Arc<BashDenylist>`. Initialize in `DaemonContext::new` from loaded `Config`.
- `crates/loopr/src/config.rs`: add `tools: ToolsConfig` top-level field; compose default `path_deny_patterns` (`.env`, `.key`, `.pem`, `credentials`, `secret`) unless overridden.
- Helper `fn tool_context(daemon: &DaemonContext, work_id: WorkId, invocation_id: Uuid) -> ToolContext` that builds a context from daemon state + per-invocation IDs.

Stage 7's implementer design doc will consume this helper. This phase just wires it up so the `tools` crate has a live caller; no agent loop yet.

#### Phase 5: Seam tests + architect review
**Model:** opus

- Serde round-trip tests for every `Input` / `Output` / `Error` type (catches `deny_unknown_fields` regressions).
- Dispatch integration test: given a v4-style `ToolCall` JSON for each of the 6 builtins, drive through `tools::dispatch`, assert expected `Output` JSON shape.
- End-to-end denylist test: construct a `ToolContext` with a target extension (`./deploy.sh`), invoke `dispatch("bash", {"command": "./deploy.sh"}, ctx)`, assert `ToolError::BashDenied { reason }`.
- Sandbox posture seam: construct `LaneRouter::new(SandboxMode::Required)` on a machine where `detect_bwrap_functional()` returns false (via a `#[cfg(test)]` override) — assert `RouterInitError::BwrapRequired`.
- Long-output test: `dispatch("bash", {"command": "python3 -c \"print('x'*100_000)\""}, ctx)`, assert `Output.truncated == true` and persist file exists at expected `.loopr/runs/.../<invocation>.log` path with full content.
- Architect round: review enum-dispatch-alternative (vs. the trait+fn choice), denylist pattern-match rigor, and sandbox-mode enforcement paths.

## Alternatives Considered

### Alternative 1: `Box<dyn Tool>` registry (v3/v4 verbatim)

- **Description:** `HashMap<String, Box<dyn Tool>>` where `Tool` has `fn name(&self) -> &str` and `fn execute(&self, json: Value, ctx: &Ctx) -> Pin<Box<dyn Future<Output = Value> + Send>>`.
- **Pros:** Familiar from v3/v4. Less scaffolding in the dispatch fn. Trivial to extend at runtime (we don't need this).
- **Cons:** Conflicts with vision.md's "typed `Input`/`Output`/`Error`" — trait-object type-erases everything to `Value` at the boundary anyway. Conflicts with user's no-`dyn`-for-DI rule. Object-safety forces `Pin<Box<dyn Future>>` boilerplate at every tool site (unergonomic in Rust 2024 where async-fn-in-trait is native otherwise). No compile-time exhaustiveness for callers iterating all tools.
- **Why not chosen:** Trait + generic `run::<T>` dispatch function is the same LOC once you count the match arms against HashMap insertions, and it keeps the typed associated types that vision.md mandates.

### Alternative 2: Enum-dispatch (`enum Tool { Read(Read), ... }`)

- **Description:** A `Tool` enum holding unit-struct variants, with `impl Tool { pub async fn dispatch(&self, input, ctx) { ... } }` matching on `self`.
- **Pros:** Exhaustiveness via `match` at the dispatch site (new variant forces match-arm update). Reads like a table.
- **Cons:** The variants are zero-sized unit structs, so the enum carries no state — making the enum itself redundant. It's "an enum of Sized `()`s" that exists only so you can call `.dispatch` on an instance. The only value it adds over a free function + trait is exhaustiveness, and that's equally achieved by tagged match arms in the free `dispatch` fn.
- **Why not chosen:** Strictly more ceremony than the chosen trait + free-function shape. (Earlier drafts of this doc had the enum; Pass 2 removed it.)

### Alternative 3: `#[derive(Tool)]` proc macro (Cersei-style)

- **Description:** Annotate each tool struct with `#[derive(Tool)]`; the macro generates the `Tool` impl, `dispatch` match arm, and `all_schemas` entry.
- **Pros:** Less boilerplate. Vision.md line 613 specifically names this pattern as worth studying from Cersei.
- **Cons:** Introduces a second proc-macro alongside the nascent `#[derive(Fsm)]` + `#[derive(Record)]` before either has been exercised through one full pipeline pass. Debugging proc-macros mid-first-gate is exactly the trap v4 fell into (YAML FSM runtime). Each `Tool` impl is ~15 lines of boilerplate for 6 tools — not enough tax to justify the macro.
- **Why not chosen:** Premature. Earn it when the hand-written trait impls reach 15+ tools. Listed as a Deferred Enhancement.

### Alternative 4: Chunked `DaemonEvent::ToolOutputChunk` for long output

- **Description:** Tool captures full output; daemon splits into ~64KB chunks and emits `DaemonEvent::ToolOutputChunk { tool_invocation_id, seq, text }` events; clients reassemble.
- **Pros:** Client sees output as it arrives — useful for TUI. Matches `crates/tools/CLAUDE.md:38–45` stated user preference.
- **Cons:** Requires per-invocation buffering at the client, seq-ordering, completion signaling, backpressure, stdout/stderr interleaving semantics. ~200 LOC of new protocol surface. Stage 7 has no streaming consumer (TUI is beyond first gate per vision.md line 595). The 32K inline + persist-to-log approach is fully functional without any chunking machinery — the persist path string (<200 bytes) goes over IPC well under the 1 MiB cap.
- **Why not chosen for Stage 7:** No first-gate consumer; complexity without benefit. Raised on design-doc review per CLAUDE.md explicit invitation. Deferred to whenever TUI streaming lands.

### Alternative 5: Substring-matched denylist (v4-style)

- **Description:** `BASE_DENY: &[&str]` with `haystack.contains(pattern)` checks.
- **Pros:** Trivial to implement.
- **Cons:** False positives (`echo "git push is disabled"` triggers `git push` match — painful when agents write commit messages). False negatives (`git   push` with extra whitespace bypasses `"git push"` exact-substring). Agents get comfortable generating either shape.
- **Why not chosen:** CST-based match via `tree-sitter-bash` handles quoting, whitespace, metacharacters, pipelines, subshells, heredocs, process substitution, arrays, and `$(…)` / `` `…` `` correctly. Substring match was v4's baseline; `shell-words` (whitespace+quotes splitter) doesn't solve the problem; the Rust `shlex` crate doesn't either (R2); `conch-parser` is POSIX-only and rejects valid Bash (R3). Hand-rolling a pre-pad regex around any of these is worse than using an actual shell parser. tree-sitter-bash produces a CST we walk for the denylist AND reuse for the D8 lane-routing decision, so the parser cost is amortized across both mechanisms.

### Alternative 6: Bash lane = `Net` (v4 default) OR unconditionally `Heavy`

- **Description:** Either Net-for-all or Heavy-for-all. v4 shipped Net. Earlier drafts of this doc shipped Heavy.
- **Pros (`Net`-for-all):** Faster turnaround for short bash calls (`ls`, `pwd`, `cat`).
- **Cons (`Net`-for-all):** `cargo build` on Net runs into the 120s max timeout; five concurrent builds fight for resources under one 5-slot cap.
- **Pros (`Heavy`-for-all):** Long-build-safe; 1800s max.
- **Cons (`Heavy`-for-all, what flipped the Architect):** `Heavy` is a 1-slot global semaphore. Once Stage 9 parallel worktrees land, one agent's 5-minute `cargo build` in Worktree A globally blocks Agent B's 50ms `git status` in Worktree B. Deadlock-adjacent.
- **Why not chosen:** Neither. The per-invocation leading-token heuristic (D8) splits the difference: `cargo`/`npm`/`otto`/etc. → Heavy, everything else → Net. Adds ~15 lines vs. either extreme and removes a Stage-9 deadlock that would otherwise require a mid-stage re-architecture.

## Technical Considerations

### Dependencies

Internal: `telemetry` (for span emission). **Explicitly not** `domain` — `tools` is agnostic of loopr's pipeline types (subprocess executor doesn't know Plan/Work/Bundle; Architect Round 3). `invocation_id` and `persist_base` are passed as explicit `ToolContext` fields per D13, NOT read from `tracing::Span::current()` (Architect R1 flagged that as a brittle data channel).

External (added via `cargo add`, not hand-edited):
- `tokio` (workspace) — async runtime, `process::Command`, `sync::Semaphore`
- `serde`, `serde_json` (workspace) — Value shuttling at dispatch edge
- `thiserror` (workspace) — `ToolError` + per-tool errors
- `schemars` — `JsonSchema` derive + schema generation
- `tree-sitter` — incremental parsing runtime (C library, linked via cc-rs). Build-time C toolchain required. Accepted as central to the tool layer per Scott's 2026-04-21 direction.
- `tree-sitter-bash` — full Bash grammar for tree-sitter. Used by both D5 (BashDenylist match over `command` nodes) and D8 (Bash lane routing via CST walk). Chosen over `conch-parser` (POSIX-only, R3-flagged) and the Rust `shlex` crate (R2-confirmed: no operator tokenization). Parity with Claude Code's "native module for bash command parsing" (CHANGELOG line 504).
- `glob` — filesystem glob tool
- `libc` — `setsid`, `killpg`, `SIGTERM`, `SIGKILL`
- `tracing` (workspace) — per-call span instrumentation per `rules/rust.md` tracing-override
- `uuid` (workspace?) — `tool_invocation_id` generation via UUIDv7

### Performance

- `tools::dispatch` is O(1) on tool count (match arm).
- `serde_json::from_value` into `Input` dominates per-call overhead before the tool's own work.
- Semaphore acquisition on Heavy lane can block for minutes during a cargo build; intended.
- `spawn_with_process_group` adds one `fork` + `setsid` syscall per tool call; negligible.
- Output truncation at 32K is O(n) in output size; dominant cost for large builds is writing the persist file, which is async.

### Security

- **Sandbox posture**: `Required` is default. Unsandboxed run requires an explicit `.loopr/config.yml` edit. Recorded at router construction via `tracing::info!` with `sandbox_mode`, `bwrap_available`, `bwrap_functional` structured fields.
- **Functional bwrap detection** (not just `--version`): at daemon startup, invoke `bwrap --unshare-net --ro-bind / / -- /bin/true` to verify user namespaces work. A machine that has the binary but can't execute it (e.g., `user_namespaces=0`) surfaces at startup, not at first tool call.
- **Path escape**: `ToolContext` canonicalizes `working_dir` and each resolved tool path; rejects paths outside when `SandboxMode` ≠ `Off`. `path_deny_patterns` (`.env`, `.key`, `.pem`, `credentials`, `secret`) always apply regardless of sandbox mode.
- **Bash denylist**: pre-flight on every Bash invocation. Denied commands never reach subprocess spawn.
- **Target tighten-only**: config deserializer rejects any key other than `extend`. Structurally impossible to widen permissions.
- **Shell-injection mitigation**: `Grep` builds `Command::new("grep").arg(pattern)` directly rather than `sh -c "grep '{pattern}'"`. Closes v4's single-quote-escape vulnerability. Bash still uses `sh -c` (that's its job), but goes through the denylist first.
- **Environment scrubbing (denylist; R1 allowlist → denylist flip; R2 expansion; R3 `_PASS` removal)**: tool subprocess env strips variables matching known-provider **prefixes** (`LOOPR_*`, `ANTHROPIC_*`, `AWS_*`, `GITHUB_*`, `GOOGLE_*`, `GCP_*`, `AZURE_*`, `OPENAI_*`, `GEMINI_*`) OR known-secret-shape **suffixes** (`*_API_KEY`, `*_SECRET`, `*_TOKEN`, `*_PASSWORD`, `*_CREDENTIALS`, `*_AUTH`). Match is strict prefix/suffix (`var.starts_with("X_")` / `var.ends_with("_X")`), NOT substring — so `SSH_AUTH_SOCK` passes (ends `_SOCK`, not `_AUTH`), `SLACK_BOT_TOKEN` is stripped (ends `_TOKEN`). R3-dropped `*_PASS`: false-positives on legitimate vars (`MULTIPASS_*`, `BYPASS_*`, `LOWPASS_*`, `CLI_PASS_ARGS`); `*_PASSWORD` is the correct suffix for actual passwords. Everything else passes through so cargo/rustup/git/npm find their config. Denylists are never complete — a new provider's unanticipated env shape can leak; defense-in-depth is that the daemon's env shouldn't hold arbitrary-provider secrets anyway.
- **No background tasks**: Stage 7 drops v4's `run_in_background` input param, eliminating a class of lifetime-management bugs.
- **Bash denylist is a tripwire, not a sandbox.** bwrap + working_dir containment are the authoritative boundaries. Denylist catches common footguns.

### Testing Strategy

Per CLAUDE.md "Seam tests, not only unit tests":

**Unit (per module):**
- `lane::classify` → expected lane for every first-gate tool
- `error::ToolError` display + conversions
- `denylist::BashDenylist::check` — each base pattern rejected, extension patterns accepted, quoted-string false positive NOT triggered
- `config::ToolsConfig` serde round-trip with `deny_unknown_fields`
- Each builtin's `execute` on happy path + per-variant error path

**Seam (crate boundary):**
- `tools::dispatch` with JSON input for each builtin → expected Output JSON
- `tools::all_schemas()` returns 6 schemas with distinct names and valid JSON Schema
- `LaneRouter::spawn` with lane=Local wraps with bwrap when SandboxMode=Required
- Dispatch → ToolError variant preserved through JSON conversion boundary

**Integration (inside crate):**
- Temp-dir fixture: Read → Edit → Read through dispatch, assert file state.
- Long-output: `bash` runs `python3 -c "print('x'*100_000)"`, assert `Output.truncated`, persist file present with full content.
- Denylist target extension: `ToolsConfig { bash_denylist_extend: [{tokens: ["./deploy.sh"], reason: ...}] }` → dispatch bash with `./deploy.sh` → `BashDenied`.

**Out of scope for Stage 7:**
- E2E against a real target repo (Stage 9).
- Concurrent Heavy invocations (Stage 7 is serial).

### Rollout Plan

Single branch (`v5`), single tag bump per v5 branch versioning override. Ship as `v0.5.21` or next available. No feature flag; no migration (no production state to migrate).

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| Trait + free-fn dispatch becomes unwieldy at 15+ tools | Low (first-gate: 6) | Med | Revisit with `#[derive(Tool)]` proc macro (Alt. 3) |
| Denylist false negative via whitespace / encoding tricks | Med | High | `shlex` with operator tokenization handles quoting + metacharacters; document tripwire-not-sandbox boundary; bwrap + working_dir containment are the authoritative boundaries |
| `bwrap --ro-bind / /` fails on exotic filesystem layouts | Low | Med | Functional detection at startup surfaces the failure; fall back to `preferred` after manual config |
| 32K truncation cap too small for some LLM interpretations | Low | Low | Cap is tunable via `ToolsConfig`; persist path always carries full output |
| `Required` + missing bwrap blocks daemon startup with no escape | Low | Med | Error message includes `apt install bubblewrap` and the config knob to downgrade to `preferred` |
| `schemars` output incompatible with Anthropic tool-use schema | Low | Med | Phase 1 verifies via a known-good builtin's schema; fallback is hand-written JSON Schema in `schema.rs` |
| `tree-sitter` C toolchain build-time dep unavailable in minimal CI environments | Low | Low | `tree-sitter` + `cc-rs` is standard practice in the Rust ecosystem (used by `ripgrep`, `helix`, many others); any reasonable CI image has a C compiler. If it bites, add explicit `cc` system dep to the project's `.otto.yml`. |
| tree-sitter-bash grammar misses a Bash feature we depend on | Very low | Low | Grammar is actively maintained (GitHub/Neovim/Helix all depend on it); any gap is fixable upstream. For tripwire + routing purposes, imperfect fidelity is still fine — the AST parser only needs to find command heads + argv, not semantically interpret globs or brace expansions. |
| bwrap swallows signals or children escape via `setsid` inside the sandbox | Med | High | D16: kill bwrap's outer PID directly via `child.kill()`, not `killpg` on inner PGID. `--die-with-parent` as defense-in-depth. Phase 2 test validates. |
| Env denylist misses a new LLM provider's API-key var (e.g., a hypothetical `COHERE_API_KEY` — would match via `*_API_KEY` suffix; a hypothetical `COHERE_SESSION` would NOT) | Med | Low | Per-provider tests; `*_API_KEY` and `*_TOKEN` suffixes catch the common shapes. Revisit on every provider addition. Documented as known-incomplete. |
| `tokio::spawn` drops `tracing` context — previously load-bearing for filenames, now only for logs | N/A | N/A | D4/D13 amendment eliminated the data-flow path; remaining `tracing` usage is telemetry-only. Agents still wrap spawned futures with `.instrument(span)` for log correlation, but nothing breaks if they forget |
| Bash CST-walk mis-routes a command whose `command_name` doesn't match the string form (e.g., `"cargo"` vs. `cargo`) | Low | Med | Resolved-head helper normalizes: reads the node's text, strips quotes, `./` prefix, and path components. Tested against all ~40 HEAVY_EXECUTABLES. |
| A target repo uses a heavy build tool not in `HEAVY_EXECUTABLES` (e.g., `bake`, custom `./run.sh`) | Med | Low | Routes to `Net` with 120s max timeout; long builds will time out. Mitigation: target extends via `.loopr/config.yml: tools.heavy-executables-extend: ["bake", "./run.sh"]` (Open Question tracked). |
| UUIDv7 `tool_invocation_id` collides with an existing `.log` filename | Negligible | Low | UUIDs do not collide in practice; if two calls produce the same id, second append fails and tool errors |

## Open Questions

- [ ] **`tree-sitter-bash` node-kind names.** The CST uses node kinds like `command`, `command_name`, `variable_assignment`, `pipeline`, `list`. Phase 2 verifies exact node-kind strings in the grammar (the tree-sitter-bash `grammar.js` is the source of truth); the helper functions are straightforward but the node-kind names are worth confirming before baking into match arms.
- [ ] **`HEAVY_EXECUTABLES` YAML extraction.** Deferred 2026-04-21 per Scott. List stays inline in `builtin/bash.rs`. Revisit if the list exceeds ~60 entries or a target repo frequently needs extensions.
- [ ] **`schemars` version compatibility with Anthropic.** Phase 1 verifies; if a mismatch appears, hand-written schemas per builtin in `schema.rs`.
- [ ] **Edit on CRLF vs LF files.** Exact-match semantics make `old_string` with `\n` fail against a `\r\n` file. Document as known limitation; agents retry with different string. Fine for first gate.
- [ ] **Non-UTF-8 file handling (Read tool, not Bash output).** `Read` uses `read_to_string`, which fails loudly on binary files (v4 behavior). Separate from Bash's stdout handling (D15, which uses lossy). Fine for first gate.
- [ ] **`Heavy` leading-token list extensibility.** D8 bakes in 13 tokens. Target repos may have custom build commands (`./bake`, `task test`). Consider `.loopr/config.yml: tools.heavy-tokens-extend: ["./bake", "task"]` if Stage 9 first-gate demands it. Deferrable — not required by the `rust-version` target.

## References

- `docs/vision.md`:
  - Lines 118–127 — `tools` ABI contract
  - Lines 514–558 — Security section (lane model, sandbox posture, denylist)
  - Lines 626–633 — closed decisions on tool registry and security
- `crates/tools/CLAUDE.md` — scope rules, long-output TODO
- `crates/tools/docs/CLAUDE.md` — design-doc conventions
- `docs/roadmap.md` — Stage 7 goals + co-sibling docs
- `docs/design/2026-04-20-stage-6-scope.md` — the scope-memo pattern this doc's Decisions table mirrors
- v4 source (port origin):
  - `src/tools/spawn.rs` — `spawn_with_process_group` primitive
  - `src/tools/sandbox.rs` — `bwrap_command` + detection
  - `src/tools/router.rs` — `LaneRouter` with tokio semaphores
  - `src/tools/lane.rs` — `Lane`, `LanePolicy`, `classify()`
  - `src/tools/builtin/{read,write,edit,shell,grep,glob}.rs` — first-gate builtin logic
- v3 source (identical structure to v4 for this layer): `src/tools/*`
