# Design Document: Resource Embedding and Concern-Based Reorganization

**Author:** Scott A. Idler
**Date:** 2026-04-14
**Status:** Draft
**Review Passes Completed:** 5/5

## Summary

Loopr ships ~52 text files (.pmt prompts, .yml strategies, .md templates) that drive its behavior. Currently, only prompts and FSM definitions are embedded in the binary - the rest require a filesystem directory tree, which means `cargo install loopr` produces a binary that immediately crashes when pointed at a repo without a pre-existing `strategies/` tree. This design doc proposes (1) embedding all runtime text files using `rust-embed` with uniform filesystem-override semantics, and (2) reorganizing from type-based directories (prompts/, strategies/) to concern-based directories so that all files related to a single pipeline stage live together.

## Problem Statement

### Background

Loopr's behavior is driven by two categories of text files:

- **Prompts (.pmt):** LLM instructions for each agent role and pipeline stage. 25 files.
- **Strategies (.yml):** FSM definitions, trigger conditions, engine strategies, decomposition roles. 24 files.
- **Templates (.md):** Document templates for Spec/Phase/Work. 3 files embedded via `include_str!`. (plan.md exists as reference documentation but is not loaded by code.)

These files are currently organized by artifact type:

```
prompts/
  coordinator.pmt
  decompose/spec.pmt
  validator-spec.pmt
  coverage-plan-specs.pmt
  ...
strategies/
  fsm/work.yml
  triggers/work-safety-nets.yml
  decomposition/default.yml
  roles/decomposer.yml
  ...
docs/templates/
  spec.md
  ...
```

### Problem

**1. `cargo install` is broken.** Only prompts (25 files) and FSM definitions (5 files) are embedded in the binary. Triggers (7 files), engine strategies (10 files across 5 subdirectories), and decomposer roles (2 files) are loaded from the filesystem with `fatal!` on missing directories. A user who installs via `cargo install` and points at a repo without a `strategies/` tree gets an immediate crash.

**2. Adding new prompts requires boilerplate.** Each new .pmt file requires: a `const` with `include_str!`, a field on `PromptStore`, a line in `init()`, and a line in `init_defaults()`. Four touch points for one file addition. This creates drift (files that exist but aren't wired, or constants referencing deleted files).

**3. Organization by artifact type hinders comprehension.** Understanding "how does Spec decomposition work?" requires reading 11 files across 4 directories:

```
prompts/decompose/spec.pmt            - LLM instruction
prompts/decompose/validate.pmt        - decompose-time validation
prompts/decompose/ratify.pmt          - parent check
prompts/validator-spec.pmt            - draft-to-active gate
prompts/coverage-plan-specs.pmt       - coverage evaluation
strategies/decomposition/default.yml  - engine strategy
strategies/decomposition/classify.yml - tier classification
strategies/decomposition/coverage.yml - coverage strategy
strategies/decomposition/validate.yml - validation strategy
strategies/roles/decomposer.yml       - role config (full)
strategies/roles/decomposer-brief.yml - role config (brief)
```

The format (.pmt vs .yml) is an implementation detail. Organizing by format puts the implementation detail above the conceptual concern.

### Goals

- A `cargo install loopr` binary works out of the box - all runtime files embedded as defaults
- Uniform "try filesystem first, fall back to embedded" semantics for every file category
- Adding a new prompt or strategy file requires zero Rust code changes
- Files related to the same pipeline stage live in the same directory
- Existing filesystem override semantics (XDG prompts, repo-local strategies) are preserved

### Non-Goals

- Changing the file formats themselves (.pmt content, .yml schema)
- Template engine or variable interpolation beyond what already exists (the coordinator prompt interpolation stays as-is)
- Compression of embedded resources (at ~200KB total, not worth the complexity)
- A `loopr init` scaffolding command (useful but separate scope)
- Changing how `loopr.yml` config or `LOOPR.md` guidance files are loaded (these already work correctly)

## Proposed Solution

### Overview

Two changes, executed in sequence:

1. **Phase A: Embed everything with `rust-embed`.** Replace manual `include_str!` constants with `#[derive(RustEmbed)]` structs that auto-discover files at compile time. Add filesystem-override fallback (the FSM pattern) to triggers, strategies, and roles. This fixes `cargo install` immediately.

2. **Phase B: Reorganize directory tree by concern.** Move from type-based layout to concern-based layout. Update `rust-embed` folder annotations and filesystem override paths. This is a mechanical rename that touches no runtime logic.

### Architecture

#### Current loading patterns

| Category | Files | Embedded | Filesystem fallback | Fatal on missing |
|----------|-------|----------|-------------------|-----------------|
| Prompts (.pmt) | 25 | Yes (manual include_str!) | XDG override dir | No |
| FSM (.yml) | 5 | Yes (manual include_str!) | Repo strategies/fsm/ | No |
| Triggers (.yml) | 7 | No | Repo strategies/triggers/ | **Yes** |
| Engine strategies (.yml) | 10 | No | Repo strategies/{decomposition,...}/ | **Yes** |
| Roles (.yml) | 2 | No | Repo strategies/roles/ | **Yes** |
| Templates (.md) | 3 | Yes (manual include_str!) | None | N/A (embedded) |

#### Target loading pattern (uniform)

Every file category uses this resolution order:

1. **Repo-local override** - `$REPO_PATH/resources/{path}` (strategies, roles)
2. **XDG override** - `~/.config/loopr/resources/{path}` (prompts, user customization)
3. **Embedded default** - compiled into the binary via `rust-embed`

The coordinator prompt interpolation (status values, abandon ratio) remains a post-load processing step applied after resolution.

**Directory override semantics (load_dir):** When loading a directory of resources (e.g., all trigger YAMLs), the filesystem override *merges* with embedded defaults on a per-file basis. If `$REPO_PATH/resources/engine/triggers/custom.yml` exists, it's added to the set. If `$REPO_PATH/resources/engine/triggers/work-sla.yml` exists, it replaces the embedded version of that specific file. Files only present in the embedded set are still included. This gives users the ability to override individual files or add new ones without maintaining a complete copy of every default.

**Absolute-path escape hatch:** The current prompt system supports absolute paths in config (e.g., `/tmp/experiment/coordinator.pmt`). When a config field contains an absolute path, the resource loader skips ALL fallback and loads directly from that path - fatal on failure. This is deliberate: it prevents A/B testing experiments from silently falling back to the baseline prompt, which would corrupt experiment data. The `Resources::load()` method must preserve this behavior.

#### rust-embed integration

```rust
use rust_embed::Embed;

#[derive(Embed)]
#[folder = "resources/"]
struct Resources;

impl Resources {
    /// Load a resource by path, checking filesystem overrides first.
    /// If `path` is absolute, load directly with no fallback (A/B test mode).
    fn load(path: &str, repo_path: Option<&Path>) -> eyre::Result<String> {
        // 0. Absolute path: direct load, fatal on failure (A/B experiment mode)
        if path.starts_with('/') {
            let content = fs::read_to_string(path)
                .with_context(|| format!("absolute resource path not found: {}", path))?;
            eyre::ensure!(!content.trim().is_empty(), "absolute resource path is empty: {}", path);
            return Ok(content);
        }

        // 1. Repo-local override
        if let Some(repo) = repo_path {
            let local = repo.join("resources").join(path);
            if let Ok(content) = fs::read_to_string(&local) {
                if !content.trim().is_empty() {
                    info!("resource override loaded: {}", local.display());
                    return Ok(content);
                }
            }
        }

        // 2. XDG override
        if let Some(config_dir) = dirs::config_dir() {
            let xdg = config_dir.join("loopr/resources").join(path);
            if let Ok(content) = fs::read_to_string(&xdg) {
                if !content.trim().is_empty() {
                    info!("resource XDG override loaded: {}", xdg.display());
                    return Ok(content);
                }
            }
        }

        // 3. Embedded default
        let file = Self::get(path)
            .ok_or_else(|| eyre::eyre!("resource not found: {}", path))?;
        let content = std::str::from_utf8(file.data.as_ref())
            .map_err(|e| eyre::eyre!("resource is not valid UTF-8: {}: {}", path, e))?;
        Ok(content.to_string())
    }
}
```

In debug builds, `rust-embed` can be configured to read from the filesystem instead of the binary, which means prompt/strategy iteration during development does not require recompilation.

### Proposed Directory Layout

Reorganize from type-based to concern-based:

```
resources/
  decompose/
    validate.pmt             - was prompts/decompose/validate.pmt (cross-level)
    ratify.pmt               - was prompts/decompose/ratify.pmt (cross-level)
    plan/
      validator.pmt          - was prompts/validator-plan.pmt
    spec/
      prompt.pmt             - was prompts/decompose/spec.pmt
      validator.pmt          - was prompts/validator-spec.pmt
      coverage.pmt           - was prompts/coverage-plan-specs.pmt
      template.md            - was docs/templates/spec.md
    phase/
      prompt.pmt             - was prompts/decompose/phase.pmt
      validator.pmt          - was prompts/validator-phase.pmt
      coverage.pmt           - was prompts/coverage-spec-phases.pmt
      template.md            - was docs/templates/phase.md
    work/
      prompt.pmt             - was prompts/decompose/work.pmt
      generation.pmt         - was prompts/generation-work.pmt
      coverage.pmt           - was prompts/coverage-phase-works.pmt
      template.md            - was docs/templates/work.md
    strategies/
      default.yml            - was strategies/decomposition/default.yml
      classify.yml           - was strategies/decomposition/classify.yml
      coverage.yml           - was strategies/decomposition/coverage.yml
      validate.yml           - was strategies/decomposition/validate.yml
      failure.yml            - was strategies/decomposition/failure.yml
    roles/
      full.yml               - was strategies/roles/decomposer.yml
      brief.yml              - was strategies/roles/decomposer-brief.yml
    schema.pmt               - was prompts/validator-schema.pmt
    coverage-schema.pmt      - was prompts/coverage-schema.pmt
    sections.yml             - was docs/templates/sections.yml
  agents/
    coordinator.pmt          - was prompts/coordinator.pmt
    implementer.pmt          - was prompts/implementer.pmt
    reviewer.pmt             - was prompts/reviewer.pmt
    researcher.pmt           - was prompts/researcher.pmt
    interview.pmt            - was prompts/interview.pmt
    tier-gate.pmt            - was prompts/tier-gate.pmt
  chat/
    default.pmt              - was prompts/chat.pmt
    interview.pmt            - was prompts/chat-interview.pmt
    draft.pmt                - was prompts/chat-draft.pmt
    refine.pmt               - was prompts/chat-refine.pmt
    executing.pmt            - was prompts/chat-executing.pmt
  engine/
    fsm/
      work.yml               - was strategies/fsm/work.yml
      bundle.yml             - was strategies/fsm/bundle.yml
      hierarchy.yml          - was strategies/fsm/hierarchy.yml
      tick.yml               - was strategies/fsm/tick.yml
      agent.yml              - was strategies/fsm/agent.yml
    triggers/
      agent-events.yml       - unchanged
      composites.yml         - unchanged
      engine.yml             - unchanged
      plan-quality-gates.yml - unchanged
      reconciliation.yml     - unchanged
      work-safety-nets.yml   - unchanged
      work-sla.yml           - unchanged
    strategies/
      integration.yml        - was strategies/integration/default.yml
      reconciliation.yml     - was strategies/reconciliation/default.yml
      recovery.yml           - was strategies/recovery/default.yml
      supervision.yml        - was strategies/supervision/default.yml
      sweeps.yml             - was strategies/sweeps/default.yml
```

Key principles:
- **Vertical slices:** everything about "spec decomposition" lives in `resources/decompose/spec/`
- **Format is an implementation detail:** .pmt and .yml coexist in the same directory
- **Engine internals grouped together:** FSM, triggers, and strategies are all engine concerns
- **Single `resources/` root:** one `#[derive(Embed)]` annotation, one filesystem override tree

**Before vs. after - "where is Spec decomposition?"**

Before: 11 files across 4 directories:
```
prompts/decompose/spec.pmt, prompts/decompose/validate.pmt, prompts/decompose/ratify.pmt,
prompts/validator-spec.pmt, prompts/coverage-plan-specs.pmt,
strategies/decomposition/{default,classify,coverage,validate,failure}.yml,
strategies/roles/decomposer.yml
```

After: `ls resources/decompose/spec/` plus `ls resources/decompose/strategies/` and `ls resources/decompose/roles/` - three directories under one parent, all visible in a single `tree resources/decompose/`.

### Data Model

No domain model changes. The `PromptStore` struct keeps its pre-loaded `String` fields and `OnceLock` singleton pattern - resources are loaded eagerly at startup so that missing files fail fast rather than surfacing at first use during a decomposition run. The `init()` function switches from `include_str!` constants to `Resources::load()` calls, but the call-site semantics (load once, store in struct, access via `get()`) stay the same.

The `FsmInterpreter::embedded()` and `FsmInterpreter::load()` merge into a single constructor that calls `Resources::load_dir()` with the uniform override chain. The `if fsm_dir.exists()` branching in `daemon/context.rs` disappears.

### API Design

#### Public interface

```rust
/// Unified resource loader. Compiled-in defaults + filesystem overrides.
pub struct Resources;

impl Resources {
    /// Load a text resource by path within the resources/ tree.
    /// Resolution: repo-local override > XDG override > embedded default.
    pub fn load(path: &str, repo_path: Option<&Path>) -> eyre::Result<String>;

    /// Load all files matching a directory prefix.
    /// Returns Vec<(relative_path, content)>.
    pub fn load_dir(prefix: &str, repo_path: Option<&Path>) -> eyre::Result<Vec<(String, String)>>;

    /// Check if a resource exists (in any layer).
    pub fn exists(path: &str, repo_path: Option<&Path>) -> bool;
}
```

#### Migration of existing call sites

| Current | New |
|---------|-----|
| `include_str!("../prompts/coordinator.pmt")` | `Resources::load("agents/coordinator.pmt", repo)` |
| `FsmInterpreter::embedded()` | `Resources::load_dir("engine/fsm/", repo)` |
| `trigger_schema::load_dir(&triggers_dir)` | `Resources::load_dir("engine/triggers/", repo)` |
| `engine::schema::load_dir(&strategies_dir)` | `Resources::load_dir("engine/strategies/", repo)` |

### Implementation Plan

Phases 1-4 correspond to **Phase A** (embed everything - fixes `cargo install`).
Phases 5-7 correspond to **Phase B** (reorganize by concern). These are independently shippable.

#### Phase 1: Add rust-embed and unified loader
**Model:** sonnet

- `cargo add rust-embed`
- Create `src/resources.rs` with the `Resources` wrapper providing `load()`/`load_dir()` methods
- Use two `#[derive(Embed)]` structs during Phase A (rust-embed's `#[folder]` takes one path):
  ```rust
  #[derive(Embed)]
  #[folder = "prompts/"]
  struct EmbeddedPrompts;

  #[derive(Embed)]
  #[folder = "strategies/"]
  struct EmbeddedStrategies;
  ```
  Optionally a third struct for `docs/templates/`. The `Resources` wrapper unifies these behind a single API. Phase B consolidates them into one struct when files move to `resources/`.
- Add unit tests: embedded lookup, missing file error, empty file handling

#### Phase 2: Migrate prompts to rust-embed
**Model:** sonnet

- Replace 25 `include_str!` constants in `src/prompts.rs` with `Resources::load()` calls
- Remove the 25 `const DEFAULT_*` lines
- Simplify `PromptStore` fields to load on-demand or keep as pre-loaded Strings via init()
- Preserve the coordinator interpolation as a post-load step
- Preserve the XDG override and absolute-path semantics
- **Required:** `prompts::init(config, repo_path: Option<&Path>)` must accept a repo path and
  thread it through to `Resources::load()` so that teams can commit custom `.pmt` files to
  `{repo}/resources/` and have them picked up at daemon startup. Pass
  `Some(&config.project.repo_path)` from `main.rs`.
- Run `otto ci`

#### Phase 3: Migrate FSM to rust-embed
**Model:** sonnet

- Replace `EMBEDDED_YAMLS` in `src/fsm/runtime.rs` with `Resources::load_dir("engine/fsm/", ...)`
- Merge `FsmInterpreter::embedded()` and `FsmInterpreter::load()` into one constructor
- Remove the `if fsm_dir.exists()` branching in `daemon/context.rs`
- Run `otto ci`

#### Phase 4: Embed triggers and strategies (fixes cargo install)
**Model:** opus

- Wire `trigger_schema::load_dir` through `Resources::load_dir("engine/triggers/", ...)`
- Wire `engine::schema::load_dir` through `Resources::load_dir("engine/strategies/", ...)`
- Wire decomposer role loading through `Resources::load("decompose/roles/full.yml", ...)` and `Resources::load("decompose/roles/brief.yml", ...)`
- Remove all `fatal!` on missing filesystem directories
- Test: verify daemon starts with no strategies/ directory present
- Run `otto ci`

#### Phase 5: Reorganize directory tree
**Model:** sonnet

- Create `resources/` directory with the proposed concern-based layout
- Move all .pmt, .yml, and template .md files to their new locations
- Update `#[folder = "resources/"]` annotation - collapse `EmbeddedPrompts` and
  `EmbeddedStrategies` into a single `EmbeddedResources` struct; remove the
  extension-based routing in `get_embedded()` (no longer needed once all files
  live under one embedded root)
- Update all `Resources::load()` path strings in Rust source
- Update config path defaults in `loopr.yml`
- **Required:** Implement additive override semantics in `Resources::load_dir()`. The
  current implementation only iterates over embedded file paths, so novel files added
  by users in `{repo}/resources/` or `~/.config/loopr/resources/` are silently ignored.
  Phase 5 must update `load_dir` to union the embedded file list with any files found
  in the repo-local and XDG directories under the same prefix, so users can add custom
  files (e.g., `resources/engine/triggers/custom.yml`) without them being dropped.
- Run `otto ci`

#### Phase 6: Migrate templates to rust-embed
**Model:** sonnet

- Replace `include_str!("../../../docs/templates/spec.md")` etc. in decomposer handler with `Resources::load()`
- Move template .md files into the `resources/decompose/` tree
- Run `otto ci`

#### Phase 7: Cleanup
**Model:** sonnet

- Remove empty `prompts/`, `strategies/`, `docs/templates/` directories
- Update CLAUDE.md codebase map
- Update any design docs referencing old paths
- Run `otto ci`

## Alternatives Considered

### Alternative 1: Keep include_str! - just add it for the missing files
- **Description:** Add `include_str!` and filesystem fallback to triggers, strategies, and roles the same way FSM already does it.
- **Pros:** Minimal change, no new dependency, fixes the cargo install problem.
- **Cons:** Perpetuates the boilerplate problem (4 touch points per file). Does not address the organizational problem. Every new strategy file requires Rust code changes.
- **Why not chosen:** Fixes the symptom but not the structural issues. The boilerplate tax grows with every file addition.

### Alternative 2: Embed + materialize on init (scaffolding model)
- **Description:** Ship all defaults embedded. A `loopr init` command writes them into the target repo's `strategies/` tree. The repo owns customized files from that point.
- **Pros:** User can see and edit all files. Familiar pattern (eslint --init, rails new).
- **Cons:** Requires a separate `loopr init` step before first use. Users who don't customize still have 50+ files committed to their repo. Requires a "did the embedded defaults change" upgrade story.
- **Why not chosen:** This is a good complementary feature but not a replacement. The binary should work without a scaffolding step. Can be added later as a convenience command.

### Alternative 3: NetworkManager model (own and regenerate)
- **Description:** Daemon writes strategy files to a managed location on startup. User edits don't survive restarts.
- **Pros:** Always consistent. No version drift.
- **Cons:** Wrong for loopr because users will want to commit customized strategies to their repos.
- **Why not chosen:** Strategies are user-configurable, not derived state.

### Alternative 4: Reorganize only (no embedding change)
- **Description:** Move files to concern-based layout but keep the same loading patterns.
- **Pros:** Addresses the comprehension problem.
- **Cons:** Still broken for cargo install. Still requires boilerplate for new files.
- **Why not chosen:** Reorganization without embedding fixes only half the problem.

## Technical Considerations

### Dependencies

- **rust-embed** (new) - proc macro that embeds directory contents at compile time. Mature, widely used (ripgrep-all, zola, mdBook). Zero runtime dependencies.

### Performance

- No runtime difference - `include_str!` and `rust-embed` both produce `&'static [u8]` in `.rodata`. The filesystem override check adds one `stat()` per file at startup, negligible.
- In debug builds, `rust-embed` reads from disk instead of binary, which means prompt changes don't require recompilation during development.

### Security

- No change in trust model. Embedded resources are compiled into the binary (same as today). Filesystem overrides come from the user's repo or XDG config (same as today).

### Testing Strategy

- Unit tests for `Resources::load()`: embedded lookup, XDG override, repo-local override, missing file error, empty file treated as absent
- Existing tests that call `load_dir` for triggers and strategies continue to work (they read from the source tree, which `rust-embed` uses in debug mode)
- Integration: verify daemon starts and runs basic decomposition with no filesystem overrides (pure embedded)

### Rollout Plan

- Phase 1-4 can be shipped incrementally, each phase is independently useful
- Phase 5 (directory reorganization) is a breaking change for users who have customized strategies in their repos - document the migration path
- Phase 5 should be coordinated with a version bump

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| rust-embed proc macro increases compile time | Medium | Low | Measure before/after; ~50 small text files should add negligible time |
| Users with customized strategy directories break on reorg | Medium | Medium | Ship embedding fix (phases 1-4) first; reorg (phase 5) in a separate release with migration docs |
| Debug-mode disk reads surprise developers | Low | Low | Document in CLAUDE.md; this is actually a feature (hot-reload prompts) |
| Tests that hardcode paths to strategies/ break | High | Low | Mechanical update in phase 5; tests already use helper functions for paths |
| Config files reference old prompt paths | Medium | Medium | Config defaults change with the code; document migration for users with explicit overrides in loopr.yml |
| Users want to disable an embedded trigger, not just override | Low | Low | Accept as a limitation for now; if needed later, add `enabled: false` to trigger schema |
| rust-embed debug mode vs release behavior differs | Low | Medium | Document: in debug, files read from source tree (hot reload); in release, from binary. Override chain works the same in both modes |

## Open Questions

- [ ] Should the `resources/` directory name be something else? (`assets/`, `data/`, `config/`?) - `resources` is the most neutral term
- [ ] Should the XDG override path change from `~/.config/loopr/prompts/` to `~/.config/loopr/resources/`? Probably yes for consistency, but it's a breaking change for users with XDG overrides
- [ ] Do we want to consolidate `decomposer.yml` and `decomposer-brief.yml` into a single `roles.yml` with keyed entries, or keep them as separate files?
- [ ] Should `docs/templates/sections.yml` and `docs/design/decomposition.yml` move into `resources/` even though they're not loaded by code? They're reference documentation, not runtime resources

## References

- [rust-embed crate](https://crates.io/crates/rust-embed)
- Current prompt loading: `src/prompts.rs`
- Current FSM loading: `src/fsm/runtime.rs`
- Current trigger loading: `src/trigger/schema.rs`, `src/daemon.rs:280-292`
- Current strategy loading: `src/engine/schema.rs`, `src/daemon.rs:298-324`
- Current role loading: `src/agents/decomposer.rs`
