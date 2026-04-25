# Design Document: Prompts on Disk (themed `.pmt` tree, three-layer override, `loopr init` seeding)

**Author:** Scott Idler
**Date:** 2026-04-24
**Status:** Draft
**Crates touched:** context, decomposer, loopr
**Review Passes Completed:** 5/5

## Summary

Move every LLM-bound prompt out of inline Rust strings and into `.pmt` files on disk, organized in a v4-style themed tree (`agents/`, `chat/`, `decompose/`, `partials/`). The tree is baked into the binary via `include_dir!()`, written to `<target>/.loopr/prompts/` by `loopr init`, and resolved at runtime through a three-layer override chain (`<target>/.loopr/prompts/` → `~/.config/loopr/prompts/` → baked). Templating is handlebars; v3's implementer-prompt discipline language is ported verbatim, with the action-shape adapted to v5's vocabulary (5 action types, dynamic tool list, structurally different JSON examples).

## Problem Statement

### Background

v3 shipped 17 `.pmt` files in a flat `prompts/` directory loaded via `include_str!()`. v4 shipped 26 `.pmt` files in a themed `resources/{agents,chat,decompose}/` tree with the same `include_str!()` baking and a filesystem override hook. v5 shipped zero `.pmt` files. All prompts live as inline string literals in Rust source: `crates/context/src/implementer.rs` (Implementer system + user assembly), `crates/context/src/reviewer.rs` (`REVIEWER_SYSTEM_PROMPT` const + user assembly), `crates/decomposer/src/prompt.rs` (`SYSTEM_TEMPLATE` const + `assemble_user`).

The v5 vision (`docs/vision.md`, `crates/context/CLAUDE.md`) committed in writing to a three-layer override chain and `include_dir!()`-baked `.pmt` tree, but the implementation was deferred. The TODOs at `crates/context/src/implementer.rs:5-12` and `crates/context/src/reviewer.rs:14-22` acknowledge this is a deliberate placeholder.

### Problem

Two issues, one structural and one quality:

1. **Structural.** Prompts cannot be edited without rebuilding the binary. Per-target prompt customization is impossible. User-global prompt edits are impossible. This contradicts the documented architecture and removes a working pattern from v3/v4.

2. **Quality regression.** The inline placeholder for the Implementer system prompt is roughly a 5-line action-list with a one-liner JSON example. v3's `prompts/implementer.pmt` was a 79-line workflow contract: a five-step numbered sequence (Read → Write → Verify → Fix → Ship), explicit anti-loop language ("Do NOT loop back to earlier steps once completed"), iteration-budget discipline ("Wasting iterations re-reading files after successful tool runs is a failure mode"), three concrete iteration examples, and strict format rules ("ONLY a JSON array. No prose, no markdown outside the JSON"). The v5 placeholder dropped all of it. The observed failure mode in recent e2e runs is the model emitting prose preambles ("Let me start by examining the current state..."), re-reading files across iterations after they've already been read, and never converging on a `commit_changes`+`propose_bundle`+`done` sequence — eventually the lifeguard escalates. These are exactly the failure modes v3's prompt explicitly forbade.

### Goals

- Every prompt sent to an LLM (system *and* user) is loaded from a `.pmt` file on disk, never assembled from inline string literals.
- The themed tree is organized like v4 (`agents/<role>/`, `chat/`, `decompose/<tier>/`, `partials/`), even where v5 has no caller for a given subtree yet, so future agents drop into known slots.
- The baked tree is shipped via `include_dir!()` and seeded to `<target>/.loopr/prompts/` by `loopr init` (idempotent merge; `--force` to overwrite).
- Three-layer runtime override: `<target>/.loopr/prompts/` → `~/.config/loopr/prompts/` → baked.
- Handlebars templating with partials in `partials/` so shared content (tool-list rendering, format rules) is authored once.
- v3's Implementer system-prompt **discipline language** is ported verbatim — the five-step Read/Write/Verify/Fix/Ship workflow, the explicit "Do NOT loop" rules, the iteration-budget warnings, the "ONLY a JSON array" format strictness, and the three iteration examples. The **action shape** is adapted from v3's 8-action vocabulary (`read_file`, `write_file`, `run_tool`, `commit`, `propose_bundle`, `create_learning`, `done`, `need_help`) to v5's 5-action vocabulary (`run_tool`, `commit_changes`, `propose_bundle`, `done`, `need_help`): `read_file` and `write_file` are replaced with calls into the dynamic tool list, `commit` becomes `commit_changes`, `create_learning` references collapse into the `need_help` reason field. Example JSON arrays are rewritten to v5's `{"type": "...", ...}` shape rather than v3's `{"action": "...", ...}` shape.
- Same shape-adaptation for v3's Reviewer prompt body, merged with the v5-specific Reviewer guidance the current `REVIEWER_SYSTEM_PROMPT` adds. That v5 guidance covers four cases unknown to v3 and must be preserved verbatim from the current inline string at `crates/context/src/reviewer.rs:33-105`:
  - `force_proposed: true` — Implementer hit its iteration cap without an explicit `propose_bundle`. Reviewer should treat with heightened skepticism.
  - Empty-patch-body on a commit Bundle — diff is empty despite a `head_commit`, indicating structural corruption.
  - Truncation marker (`[... diff truncated; ...]`) — Reviewer is seeing only part of the change.
  - Binary-only diffs — `Binary files <a> and <b> differ` entries with no text content; flag as `warning` requiring manual review.

### Non-Goals

- Multi-tier decomposition (`decompose/{plan,spec,phase}/`). v5 has only single-tier (Plan → Work). Those subtrees ship as empty directory skeletons; their `.pmt` files are written when those decomposer tiers gain callers.
- Speculative `.pmt` files for unbuilt agents (`researcher`, `director`, `tier-gate`, `interview`). The directories exist; the files don't.
- Branch-name fix from the prior debugging session. Orthogonal scope; lands as its own commit.
- Token-budgeting changes. The existing `chars / 4` heuristic transfers unchanged.
- Hot-reload of edited `.pmt` files within a running daemon. Edits take effect on the next CLI invocation; daemon restart on prompt edits is acceptable.
- Cross-platform path normalization. v5 targets Linux; Unix path separators only.

## Proposed Solution

### Overview

Add `crates/context/prompts/` to the source tree. Embed it via `include_dir!()` in the `context` crate. Build a `PromptLoader` in `context` that resolves a relative path through the three layers, compiles via handlebars (caching compiled templates), and renders. Replace every inline-string prompt assembly with calls into the loader. Extend `loopr init` to walk the baked tree and merge-write to `<target>/.loopr/prompts/`. Add a CI script that cross-checks `{{var}}` references in `.pmt` files against the Rust call sites that supply them.

**Naming convention.** v4 used flat-with-suffix (`agents/implementer.pmt` + `agents/implementer-user.pmt`). v5 uses per-role subdirectories (`agents/implementer/system.pmt` + `agents/implementer/user.pmt`). The subdir form scales better when a role gains more than two prompts (e.g. retry variants, mode-specific prompts), and the file names match the Anthropic API field names (`system`, `user`) directly.

### Architecture

```
crates/context/prompts/                        # source-of-truth (in-repo)
├── agents/
│   ├── implementer/
│   │   ├── system.pmt                         # v3 body, verbatim, action-verb adapted
│   │   └── user.pmt                           # ports current render_user_message
│   └── reviewer/
│       ├── system.pmt                         # v3 body merged with current v5 guidance
│       └── user.pmt                           # ports current render_reviewer_user_message
├── chat/                                      # empty skeleton (no v5 callers)
├── decompose/
│   ├── plan/                                  # empty skeleton
│   ├── spec/                                  # empty skeleton
│   ├── phase/                                 # empty skeleton
│   └── work/
│       ├── system.pmt                         # ports current decomposer SYSTEM_TEMPLATE
│       └── user.pmt                           # ports current assemble_user (incl. retry path)
└── partials/                                  # SSOT chunks for cross-prompt reuse
    └── tools-list.pmt                         # rendered tool-list block (extracted from system prompts)

# Embedded:
crates/context/src/loader.rs:
    static BAKED: include_dir::Dir = include_dir!("$CARGO_MANIFEST_DIR/prompts");

# Runtime resolution:
context::PromptLoader::load("agents/implementer/system.pmt")
    1. <target>/.loopr/prompts/agents/implementer/system.pmt   (project layer)
    2. ~/.config/loopr/prompts/agents/implementer/system.pmt   (user layer)
    3. BAKED tree                                              (baked fallback)

# Init seeding:
loopr init                                                     (merge: only writes missing files)
loopr init --force                                             (overwrite all)
```

The single source-of-truth dir lives in the `context` crate, which already owns prompt assembly (per `crates/context/CLAUDE.md`). `decomposer` and `agents` already depend on `context`; they pull prompts through the same loader API. `loopr` (the binary crate) depends on `context` and exposes the baked tree to its `init` command via a thin `context::baked_prompts() -> &'static include_dir::Dir` accessor.

### Data Model

No new persistent records. The loader is a thin wrapper around handlebars's built-in registry:

```rust
/// Wraps a handlebars registry pre-populated with every `.pmt`
/// template, registered with the relative path as the template name.
/// Construction walks the layers in priority order — baked first, then
/// user_root, then target_root — re-registering the same names so each
/// later layer overwrites earlier ones. No external cache: handlebars's
/// registry IS the cache.
pub struct PromptLoader {
    registry: Handlebars<'static>,
}

#[derive(Debug, thiserror::Error)]
pub enum PromptError {
    #[error("prompt not registered (no layer provides it): {name}")]
    NotFound { name: String },
    #[error("handlebars parse error in {name}: {source}")]
    Parse { name: String, source: handlebars::TemplateError },
    #[error("handlebars render error in {name}: {source}")]
    Render { name: String, source: handlebars::RenderError },
    #[error("io error reading {path}: {source}")]
    Io { path: PathBuf, source: std::io::Error },
}
```

Template names are relative paths (e.g. `"agents/implementer/system.pmt"`). Partials use the same naming under `partials/` (e.g. `"partials/tools-list.pmt"` registered as a partial named `tools-list`). The handlebars context per call site is a typed struct — `ImplementerSystemCtx`, `ReviewerSystemCtx`, `DecomposeWorkSystemCtx` — implementing `Serialize`. Call sites stay type-checked; the loader's `render` is generic over `C: Serialize`.

A consequence of all-at-construction registration: prompt edits made to `.loopr/prompts/` after the `PromptLoader` is built do not take effect until the loader is reconstructed (typically: next CLI invocation, or daemon restart). This matches the documented "Hot-reload of edited `.pmt` files within a running daemon" Non-Goal.

### API Design

```rust
// In context::loader
impl PromptLoader {
    /// Construct with target root (typically <cwd>/.loopr/prompts/) and
    /// user root (typically ~/.config/loopr/prompts/). Either may be
    /// None to skip that layer; the baked layer is always available.
    /// Construction fails (`PromptError`) only on partial-registration
    /// errors — i.e. a `partials/*.pmt` exists but is malformed
    /// handlebars. Missing optional layers are fine; missing baked
    /// partials means no partials to register, also fine.
    pub fn new(target_root: Option<PathBuf>, user_root: Option<PathBuf>) -> Result<Self, PromptError>;

    /// Render a `.pmt` template with the given context. The path is
    /// relative (e.g. "agents/implementer/system.pmt"). Lookup order:
    /// target_root, user_root, baked.
    pub fn render<C: Serialize>(&self, path: &str, ctx: &C) -> Result<String, PromptError>;
}

// In context::lib (replaces existing InlineContextBuilder by the
// same name; the file `crates/context/src/implementer.rs` keeps the
// trait-impl shell and the typed render-context structs, but the
// inline rendering helpers (render_system_prompt, render_user_message,
// REVIEWER_SYSTEM_PROMPT, render_reviewer_user_message) are deleted).
pub struct InlineContextBuilder {
    loader: Arc<PromptLoader>,
}

impl ContextBuilder for InlineContextBuilder {
    fn build_for_implementer(...) -> Result<AssembledContext, ContextError> {
        let system = self.loader.render("agents/implementer/system.pmt", &ImplementerSystemCtx { tools: tool_schemas })?;
        let user = self.loader.render("agents/implementer/user.pmt", &ImplementerUserCtx { work, history, state, iteration, worktree_path })?;
        // ... token estimate, span recording, return AssembledContext
    }
    // build_for_reviewer mirrors
}
```

The struct keeps its name `InlineContextBuilder` (callers don't have to change). What "inline" means shifts from "inline string literals" to "stateless inline rendering on top of the loader" — both are stateless, deterministic, thread-safe, but the prompt source moves from Rust strings to `.pmt` files. Per the v5 working rule "no coexistence migrations" (`crates/loopr/CLAUDE.md`), the old inline-string body is deleted in the same commit the loader-backed body lands; no compile-time flag toggles between them.

For `loopr init`:

```rust
// In loopr::commands::init
pub fn run_init(target: &Path, force: bool) -> Result<InitOutcome> {
    let prompts_dir = target.join(".loopr").join("prompts");
    let baked = context::baked_prompts();           // &'static include_dir::Dir
    seed_prompts(&prompts_dir, baked, force)?;
    Ok(InitOutcome { ... })
}

fn seed_prompts(dest: &Path, baked: &include_dir::Dir, force: bool) -> Result<SeedReport> {
    // Walk baked tree; for each file:
    //   - compute dest_path = dest.join(relative_path)
    //   - if dest_path exists and !force: skip, count as preserved
    //   - else: mkdir -p parent, write contents, count as written
    // Empty directories (like decompose/plan/) get created so the layout
    // is visible to the user.
}
```

### Implementation Plan

#### Phase 1: Source-tree skeleton + ported prompt files
**Model:** sonnet
- Create `crates/context/prompts/` with the full v4-style themed tree, including empty directories for unbuilt slots (each empty leaf gets a `.gitkeep` so git tracks the path; the loader and `loopr init` skip files named `.gitkeep` while still creating their parent directories).
- Author `agents/implementer/system.pmt` from v3's `prompts/implementer.pmt`:
  - Keep two distinct sections: a **"Capabilities"** section listing v5's 5 action types (`run_tool`, `commit_changes`, `propose_bundle`, `done`, `need_help`) — this is the JSON-array vocabulary the model emits — and a **"Tools available"** section rendered from `{{> tools-list}}` — these are the concrete tools `run_tool` invokes (e.g. `bash`, `write_file`, `read_file` if registered). v3 conflated these because v3's actions and tools were 1:1; v5 separates them.
  - Port the workflow, anti-loop, iteration-budget, and "ONLY a JSON array" prose verbatim from v3.
  - Rewrite the three iteration examples to v5's `{"type": "run_tool", "tool": "...", "input": {...}}` shape; replace `commit` with `commit_changes`; drop `create_learning` references (collapse them into "include the ambiguity in `need_help`'s reason").
- Port the existing v5 inline `render_user_message` into `agents/implementer/user.pmt` as a handlebars template. Variables: `work_id`, `work_title`, `worktree_path`, `iteration`, `acceptance_criteria` (array), `rejected_bundle_reason` (optional), `prior_iterations` (array of `{iteration, summary}`).
- Author `agents/reviewer/system.pmt` from v3's `prompts/reviewer.pmt` discipline merged with v5's `REVIEWER_SYSTEM_PROMPT` content (verdict schema, `force_proposed`, empty-patch-body, truncation, binary files). v3 contributes the review-criteria prose and verdict-threshold language; v5 contributes the typed-Verdict schema and the four v5-specific guidance sections.
- Port `render_reviewer_user_message` into `agents/reviewer/user.pmt` as a handlebars template (variables match the existing struct's inputs: `bundle`, `work`, `diff` or `noop_files`).
- Dump v5's `decomposer::prompt::SYSTEM_TEMPLATE` (already-adapted-from-v3) verbatim into `decompose/work/system.pmt`. Convert the `{{ TREE }}` substitution from manual `.replace()` to handlebars `{{tree}}` syntax.
- Dump `assemble_user` (including the retry-path branch with its `RETRY_ERROR_MAX_BYTES` truncation) into `decompose/work/user.pmt`.
- Extract the dynamic tool-list block into `partials/tools-list.pmt`. Referenced from `agents/implementer/system.pmt` via `{{> tools-list}}`. Reviewer doesn't need it (Reviewer doesn't call tools).

#### Phase 2: PromptLoader + handlebars + three-layer chain
**Model:** opus
- Add `handlebars` and `include_dir` to `crates/context/Cargo.toml` via `cargo add`. Use whatever `cargo add` resolves at the moment — do not pin to remembered version numbers.
- Create `crates/context/src/loader.rs`. Implement `PromptLoader::new`:
  1. Construct a fresh `Handlebars` registry; call `registry.set_strict_mode(true)`.
  2. Walk the baked `include_dir!()` tree. For each `*.pmt` file under `partials/`, register it as a partial (name = filename stem). For each `*.pmt` file elsewhere, register it as a template (name = relative path).
  3. Walk `user_root` if `Some` and the directory exists. Same registration rules; later registrations overwrite earlier ones with the same name.
  4. Walk `target_root` if `Some` and the directory exists. Same rules; this layer wins.
  5. Skip `.gitkeep` files (or any non-`.pmt` files) at every layer.
- Implement `render<C: Serialize>(&self, name: &str, ctx: &C) -> Result<String, PromptError>`. Delegates to `registry.render(name, ctx)`; maps handlebars errors to `PromptError` variants with the name attached.
- **Strict mode** (`registry.set_strict_mode(true)`) is non-negotiable. Default behavior renders missing variables as empty strings — a typo `{{works_id}}` instead of `{{work_id}}` would silently produce a corrupted prompt. Strict mode turns these into render errors that surface immediately.
- Errors are typed (`PromptError`); no eyre `Result` here — this is a library crate. The `context::ContextError` enum gains a `Prompt(PromptError)` variant via `#[from]` so the existing `ContextBuilder` API doesn't grow new error surface for callers.
- Telemetry: `PromptLoader::render` carries `#[tracing::instrument(level = "debug", skip_all, fields(template = name, rendered_chars = tracing::field::Empty), err)]`. `rendered_chars` is post-recorded after render. `err` ensures `Err` returns log at error level with the name attached.
- Layer-path resolution is the *caller's* responsibility — the loader takes already-resolved `Option<PathBuf>` arguments. The `loopr` binary uses the `dirs` crate to find `~/.config/loopr/prompts/`; integration tests pass `None` or a `tempdir()` path. The loader does not embed any `dirs` lookup logic.
- Tests: load-from-baked, user-overrides-baked, target-overrides-user, target-overrides-everything, missing-template returns `NotFound`, partial-resolution-from-correct-layer, handlebars-parse-error surfaces with correct name, strict-mode-rejects-undefined-variable.

#### Phase 3: Wire context crate to use the loader
**Model:** sonnet
- Modify `InlineContextBuilder` in `crates/context/src/implementer.rs` to hold an `Arc<PromptLoader>` and delegate `build_for_implementer` and `build_for_reviewer` to `loader.render(...)`. The struct name and trait API stay; only the body changes.
- Delete `render_system_prompt`, `render_user_message`, `REVIEWER_SYSTEM_PROMPT`, `render_reviewer_user_message`. Their content lives in `.pmt` files now. After deletion, `crates/context/src/implementer.rs` retains the `InlineContextBuilder` struct, the `ContextBuilder` trait impl, and the typed render-context structs (`ImplementerSystemCtx`, `ImplementerUserCtx`); `crates/context/src/reviewer.rs` retains only the typed render-context structs (`ReviewerSystemCtx`, `ReviewerUserCtx`). Both files stay; they're now thin and role-scoped, matching the existing per-role file layout.
- Update `decomposer::prompt::assemble_system` and `assemble_user` to delegate into the loader. `SYSTEM_TEMPLATE` const and the `.replace("{{ TREE }}", ...)` call both go away. The functions become thin wrappers around `loader.render("decompose/work/system.pmt", ...)` and `loader.render("decompose/work/user.pmt", ...)`. The retry-path branching logic stays in Rust (it picks which template variant to use based on `prev_error.is_some()`); the prompt content moves to `.pmt`.
- Update existing snapshot tests in `crates/context/src/implementer/tests.rs`, `crates/context/src/reviewer/tests.rs`, and `crates/decomposer/src/prompt/tests.rs` to assert against the loaded-and-rendered output. Tests construct a `PromptLoader` with `target_root: None, user_root: None` (baked-only) for determinism.

#### Phase 4: `loopr init` writes the tree
**Model:** sonnet
- Promote `loopr init` from the Stage-5 stub. Walk `context::baked_prompts()` recursively. For each file: compute destination, skip-if-exists (default) or overwrite (`--force`). Skip files named `.gitkeep` (placeholder for git only) but still create their parent directories so the user sees the empty-slot layout.
- Defense-in-depth: assert each computed destination path is a descendant of `<target>/.loopr/prompts/` before writing (no `..` in baked relative paths by construction, but the assertion documents the invariant).
- Output a summary: "Wrote N files, preserved M existing files."
- Tests: init-into-empty-target writes everything; init-into-target-with-edits preserves edits; init --force overwrites; init creates empty-slot directories without writing `.gitkeep`.
- Add `loopr init` to the e2e gate so tests start from a known seeded state.

#### Phase 5: CI placeholder cross-check
**Model:** sonnet
- Port `bin/check-pmt-placeholders` from v4, adapted for handlebars `{{var}}` (and `{{> partial}}`) syntax. Walks `crates/context/prompts/` and extracts:
  - **Variable references** (`{{var}}`, `{{var.field}}`, `{{#each var}}`) — must have a matching field path in the typed render-context struct (e.g. `ImplementerSystemCtx`) defined in `crates/{context,decomposer}/src/`.
  - **Partial references** (`{{> partial-name}}`) — must have a matching `partials/<partial-name>.pmt` file in the source tree.
- Asserts every `{{var}}` has a matching field; every `{{> partial}}` has a matching file. Warns on Rust struct fields that are never referenced (likely-dead context fields).
- Strict-mode rendering at runtime is the primary safety net (catches drift in deployed `.pmt` files via the user-edit and target-edit layers); this CI check is a build-time pre-commit safety net for the *baked* tree, where strict-mode failures would only surface during e2e.
- Add as `otto check-pmt` or a step in `otto ci`.

#### Phase 6: End-to-end gate
**Model:** opus
- Run `bin/e2e` (the v5 end-to-end test, per the `e2e` skill). The success criterion is the inverse of the failure pattern that motivated this design:
  - **No prose preambles in iteration 1+** — the model emits only a JSON array, never narration like "Let me start by examining the current state...".
  - **No file is read twice across iterations** — once a `read_file`-equivalent tool is used on a path, that path doesn't appear in a later read.
  - **Ship sequence in one response** — once tools pass, the next iteration emits `commit_changes` + `propose_bundle` + `done` in a single JSON array.
  - **No lifeguard escalation** — the implementer terminates within its iteration budget.
- If any of these fails, the porting introduced a regression. Most likely cause: the action-shape rewrite dropped or corrupted one of v3's three iteration examples; the discipline language depends on those examples for the model to pattern-match.
- Update `docs/roadmap.md` with the new exit criterion: "Implementer first-gate target completes without lifeguard escalation, prompts loaded from `.loopr/prompts/`."

## Alternatives Considered

### Alternative 1: Keep prompts inline, port only v3's discipline language

- **Description:** Update the inline `render_system_prompt` in `crates/context/src/implementer.rs` to the v3 body verbatim, leave everything else alone. No `.pmt` files, no loader, no `init` extension.
- **Pros:** Trivial diff. Fixes the immediate quality regression. No new dependencies.
- **Cons:** Doesn't satisfy the "every prompt sent to LLM lives on disk" rule. No per-target customization. We'd be back here within one or two stages when the next agent or decomposer tier needs prompts. The vision and `crates/context/CLAUDE.md` already commit to the on-disk model; deferring it again is more drift, not less.
- **Why not chosen:** The structural problem is real and explicitly called out by the user. Punting it leaves the override path absent and the `crates/context/CLAUDE.md` documentation lying.

### Alternative 2: Plain `{placeholder}` + `.replace()` instead of handlebars

- **Description:** Match v4's templating exactly. No engine, no partials. Each `.pmt` file contains its own copy of shared blocks (tool list, JSON-format rules, etc.).
- **Pros:** Zero engine surface area. No `handlebars` dep. Easier to debug (it's just `.replace()`). Matches v4's working pattern verbatim.
- **Cons:** Tool-list rendering, output-format rules, and other cross-prompt shared blocks have to be either duplicated across `agents/implementer/system.pmt` and `agents/reviewer/system.pmt`, or reassembled by Rust callers (which leaks prompt structure back into Rust — the regression we're fixing). v4 paid this cost as duplication. The `handlebars` dep is already declared in `crates/context/CLAUDE.md`.
- **Why not chosen:** Partials are the specific affordance that lets prompts on disk stay on disk. Without them, "shared block" becomes "Rust string concatenation" again.

### Alternative 3: Two-layer override (`<target>/.loopr/prompts/` → baked); skip `~/.config/`

- **Description:** Drop the user-global layer. Only the project-local layer plus baked.
- **Pros:** Simpler: one fewer path to check, one fewer envvar / dirs lookup, one fewer test path.
- **Cons:** Cross-repo prompt edits are impossible without copying into every project. The vision and CLAUDE.md commit to three layers.
- **Why not chosen:** The user-layer lookup is roughly 20 LOC and one extra test. Skipping it would be a documented architectural deviation for tiny savings.

### Alternative 4: Per-consumer-crate `prompts/` directories

- **Description:** `crates/context/prompts/agents/` and `crates/decomposer/prompts/decompose/`, each crate owns its slice. Two `include_dir!()` invocations, two baked trees, the loader walks both.
- **Pros:** Crate-local ownership. No cross-crate coupling for prompt content.
- **Cons:** `loopr init` has to walk two trees and merge them. Loader has to know about both. Partials in `partials/` would have to live in one crate (which one?) or be duplicated. v4 chose a single top-level `resources/` for the same reason.
- **Why not chosen:** Single source of truth wins; `context` is already the natural home (it owns prompt assembly).

## Technical Considerations

### Dependencies

New crate-level: `handlebars` and `include_dir` added to `crates/context/Cargo.toml` via `cargo add`. Both are widely used and small. The `dirs` crate (used by `loopr` to find `~/.config/`) is added to `crates/loopr/Cargo.toml` if not already present.

Removed: `crates/context/src/{implementer,reviewer}.rs` lose their inline-string-rendering helpers; `crates/decomposer/src/prompt.rs` loses `SYSTEM_TEMPLATE`. No external deps removed.

### Performance

Compiled-template caching means each `.pmt` is parsed once per loader instance. `loopr` constructs one loader per CLI invocation (or one per daemon lifetime), so render cost is dominated by handlebars's substitution pass — negligible compared to the LLM call that follows.

`include_dir!()` adds the `.pmt` content to the binary at compile time. Baked tree size is roughly the sum of `.pmt` file sizes (10-20 KB at first-pass scope); negligible binary bloat.

`loopr init` writes ~6 small files on a fresh target. Sub-millisecond.

### Security

Handlebars rendering is not a code-execution vector; it's pure string substitution with builtin helpers (no `eval`, no shell). The user-edited layer (`~/.config/loopr/prompts/`) and project-edited layer (`<target>/.loopr/prompts/`) are read-only inputs to the loader; the loader never executes their contents.

`loopr init` writes only inside `<target>/.loopr/prompts/`. No path traversal: the destination paths are constructed from the baked tree's relative paths, which contain no `..` components by construction. Add an explicit assertion that derived paths stay under the seed root, defense-in-depth.

### Testing Strategy

- **Unit (loader):** layer-resolution table tests (six cases: present-in-each-layer × not-present); partial registration; cache-correctness; error variants per `PromptError`.
- **Unit (init):** seed-into-empty, seed-into-populated-default-merge, seed-into-populated-with-force, seed-creates-empty-directories.
- **Integration (context):** `InlineContextBuilder` constructed against a real loader with `target_root: None, user_root: None` (baked-only) produces output matching golden snapshots; same builder constructed with a `tempdir()`-backed `target_root` containing an override produces output containing the override content.
- **Integration (decomposer):** existing `tests/instrumentation.rs::decomposer_smoke_spans_decompose` continues to pass; `assemble_system` / `assemble_user` work through the loader.
- **CI cross-check:** `bin/check-pmt-placeholders` enforced as a CI step.
- **E2E:** Phase 6 runs the full gate; success criteria are the four listed in Phase 6 (no prose preambles, no double-reads, single-iteration ship sequence, no lifeguard escalation).

### Rollout Plan

- **Commit 1 (Phase 1):** `.pmt` files added to `crates/context/prompts/`. No Rust references them yet; build is unaffected. Reviewable as a standalone "prompt content quality" diff — the most important review in the whole sequence.
- **Commit 2 (Phase 2):** `PromptLoader` + handlebars + strict mode + tests. Loader compiles and tests pass, but no production caller invokes it yet. Reviewable as a standalone "library plumbing" diff.
- **Commit 3 (Phase 3):** Call sites flip to the loader; inline strings deleted. This is the "prompts now come from disk" moment, a single atomic commit per the v5 "no coexistence migrations" rule. Tests and snapshots updated here.
- **Commit 4 (Phase 4):** `loopr init` writes the tree. Separate seam (binary CLI surface), independently reviewable.
- **Commit 5 (Phase 5):** CI cross-check script. Guard rail, not load-bearing.
- **Phase 6 (e2e gate):** verification checkpoint, not a commit. If the gate fails, the implementer fixes whichever phase regressed and re-runs.
- Single workspace-level `bump -m` (minor) after Commit 5 lands. This is `scottidler/loopr-v5`, a personal repo without branch protection; commit on `v5` (current working branch) and tag after.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| v3's 8-action vocabulary doesn't map 1:1 to v5's 5-action vocabulary | Medium | Medium | The mapping is documented above (Goals section): `read_file`/`write_file` → `run_tool` against the dynamic tool list; `commit` → `commit_changes`; `create_learning` → `need_help` reason field. Phase 6 e2e gate verifies the action-shape rewrite preserved v3's discipline. |
| Action-shape JSON-example rewrite changes more than verbs (envelope `{action: ...}` → `{type: ...}`) | High | Low | Acknowledged. The three iteration examples are rewritten from scratch in v5's shape; the *prose* between them is kept verbatim. Snapshot tests in Phase 3 verify the prose blocks are byte-identical to v3 source after the action-section substitutions. |
| Snapshot tests in `crates/context/src/{implementer,reviewer}/tests.rs` break on Phase 3 | High | Low | Update goldens once during Phase 3. Add round-trip tests (load → render → compare against golden) to detect future drift. |
| Loader cache poisoning if cache key is the relative path | Low (avoidable) | High | Cache key is the *resolved* absolute path. Documented in the loader source. Tested by the layer-resolution table. |
| Handlebars partial registration order matters | Low | Medium | Register all partials at `PromptLoader::new` time, walking the resolved partials/ dir. Document that adding a partial requires reconstructing the loader (one-time cost per CLI invocation; trivial). |
| `include_dir!()` rebuild churn — every `.pmt` edit triggers a recompile | Certain | Low | Accepted. v5 builds are fast for the `context` crate. The override layers exist precisely so users can iterate on prompts without rebuilding. |
| `~/.config/loopr/prompts/` doesn't exist on first run | Certain | None | Layer is optional. `PromptLoader::new` checks existence; absent layer is silently skipped (with a debug log). |
| User edits a `.pmt` to use `{{undefined_var}}` | Medium | Low | Strict mode (`registry.set_strict_mode(true)`) makes missing variables a render error rather than an empty-string substitution. The error surfaces immediately at render time with the offending path; the user sees a clear message instead of a silently-corrupted prompt. Phase 5's CI cross-check catches this for the baked tree at build time. |
| Daemon caches old prompts after user edits a file | Low | Medium | Out of scope for this design (see Non-Goals). User restarts the daemon after editing. Documented in `loopr init` output. |

## Open Questions

- [ ] Should `loopr init` also seed empty config files (`config.yml`) alongside prompts, or strictly the prompts tree? Out of scope here; tracked separately with the broader `loopr init` Stage-5 work.
- [ ] Phase 6 success criteria are observable in transcripts but verified manually. Should the e2e harness gain a "no-prose-preamble" assertion (regex against the first iteration's response) as a regression guard? Out of scope; tracked as a follow-up to the `e2e` skill.
- [ ] Cache invalidation on edits to `.loopr/prompts/` files in a long-running daemon — currently requires daemon restart (per Non-Goals). An mtime-on-render check would invalidate the per-resolved-path cache entry when the file is newer than its compile time. Cheap to add; deferred until the daemon's lifetime makes this matter (today the daemon restarts often enough that this is academic).

## References

- `docs/vision.md` "Prompts" section: three-layer override + themed directory structure
- `crates/context/CLAUDE.md`: the in-scope language for this design
- `crates/loopr/CLAUDE.md`: working rules, especially "no coexistence migrations"
- v3 prompts: `~/repos/scottidler/loopr/prompts/{implementer,reviewer}.pmt`
- v4 prompts and structure: `~/repos/scottidler/loopr-v4/resources/`
- v4 placeholder cross-check: `~/repos/scottidler/loopr-v4/bin/check-pmt-placeholders`
- Inline placeholders being replaced: `crates/context/src/implementer.rs:122-191`, `crates/context/src/reviewer.rs:33-105`, `crates/decomposer/src/prompt.rs:24-58`
