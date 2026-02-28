# Design Document: Extract LLM Prompts to `.pmt` Files

**Author:** Scott Idler + Claude
**Date:** 2026-02-28
**Status:** Draft
**Review Passes Completed:** 5/5

## Summary

Every LLM prompt in loopr is hardcoded as a Rust string constant or inline `format!()` template across 6 source files. This makes prompts impossible to read, edit, or iterate on without touching Rust code. We extract all 12 prompt items to standalone `.pmt` files in `prompts/`, loaded via `include_str!` at compile time, with runtime override from `~/.config/loopr/prompts/`.

## Problem Statement

### Background

Loopr has 5 LLM-backed roles (Coordinator, Implementer, Reviewer, Researcher, Validator) plus a generation pipeline that produces documents at 4 hierarchy levels. Each role's "personality" — its system prompt defining identity, capabilities, rules, and output format — is embedded as a `const &str` in its Rust module. The generation and validator prompts are likewise inline.

### Problem

The current prompts are thin (a single identity sentence followed by mechanical action listings). Improving them requires editing Rust source, recompiling, and navigating around `r#"..."#` raw string escapes. There's no way to:
- Read a prompt as a standalone document
- Override prompts for experimentation without recompilation
- Diff prompt changes independently of code changes

### Goals

- All prompt text lives in `.pmt` files, readable and editable as plain text
- Compiled into the binary by default (no external file dependency for deployment)
- Runtime override from `~/.config/loopr/prompts/` for prompt tuning without recompile
- Zero behavioral change — extracted prompts are byte-for-byte identical to current inline text

### Non-Goals

- Template engine or mustache-style processing (dynamic assembly stays in Rust)
- Prompt versioning or A/B testing infrastructure
- Enriching the prompt content (separate effort after extraction)

## Proposed Solution

### Overview

1. Create `prompts/` directory with 12 `.pmt` files (verbatim extracts from current source)
2. Create `src/prompts.rs` module with `include_str!` defaults + runtime override via `OnceLock`
3. Wire `prompts::init()` in `main.rs` after config load
4. Replace all inline prompt constants/strings with `prompts::store()` references

### Architecture

```
prompts/                          ← tracked in git, baked via include_str!
  coordinator.pmt
  implementer.pmt
  reviewer.pmt
  researcher.pmt
  validator-schema.pmt
  validator-plan.pmt
  validator-spec.pmt
  validator-phase.pmt
  generation-plan.pmt
  generation-spec.pmt
  generation-phase.pmt
  generation-work.pmt

~/.config/loopr/prompts/          ← optional runtime overrides (same filenames)

src/prompts.rs                    ← PromptStore + init() + store()
```

**Why `.pmt`?** A dedicated extension avoids confusion with `.txt` (generic) or `.md` (rendered as markdown). Editors treat `.pmt` as plain text. The extension signals "this is a prompt template" to anyone browsing the repo.

### The PromptStore

```rust
use std::fs;
use std::path::Path;
use std::sync::OnceLock;
use log::{info, warn};

// Compile-time defaults — baked into the binary data segment, zero runtime cost
const DEFAULT_COORDINATOR: &str = include_str!("../prompts/coordinator.pmt");
const DEFAULT_IMPLEMENTER: &str = include_str!("../prompts/implementer.pmt");
const DEFAULT_REVIEWER: &str = include_str!("../prompts/reviewer.pmt");
const DEFAULT_RESEARCHER: &str = include_str!("../prompts/researcher.pmt");
const DEFAULT_VALIDATOR_SCHEMA: &str = include_str!("../prompts/validator-schema.pmt");
const DEFAULT_VALIDATOR_PLAN: &str = include_str!("../prompts/validator-plan.pmt");
const DEFAULT_VALIDATOR_SPEC: &str = include_str!("../prompts/validator-spec.pmt");
const DEFAULT_VALIDATOR_PHASE: &str = include_str!("../prompts/validator-phase.pmt");
const DEFAULT_GENERATION_PLAN: &str = include_str!("../prompts/generation-plan.pmt");
const DEFAULT_GENERATION_SPEC: &str = include_str!("../prompts/generation-spec.pmt");
const DEFAULT_GENERATION_PHASE: &str = include_str!("../prompts/generation-phase.pmt");
const DEFAULT_GENERATION_WORKITEM: &str = include_str!("../prompts/generation-work.pmt");

pub struct PromptStore {
    pub coordinator: String,
    pub implementer: String,
    pub reviewer: String,
    pub researcher: String,
    pub validator_schema: String,
    pub validator_plan: String,
    pub validator_spec: String,
    pub validator_phase: String,
    pub generation_plan: String,
    pub generation_spec: String,
    pub generation_phase: String,
    pub generation_work: String,
}

static STORE: OnceLock<PromptStore> = OnceLock::new();

/// Initialize the global prompt store. Call once at startup after config load.
/// Checks ~/.config/loopr/prompts/ for overrides.
pub fn init() {
    let overrides_dir = dirs::config_dir().map(|d| d.join("loopr/prompts"));

    let load = |filename: &str, default: &str| -> String {
        if let Some(ref dir) = overrides_dir {
            let path = dir.join(filename);
            match fs::read_to_string(&path) {
                Ok(content) if !content.trim().is_empty() => {
                    info!("prompt override loaded: {}", path.display());
                    return content;
                }
                Ok(_) => {
                    // Empty file — fall back to default
                }
                Err(_) => {
                    // File not found — expected, use default
                }
            }
        }
        default.to_string()
    };

    // OnceLock::set returns Err if already initialized — harmless no-op
    let _ = STORE.set(PromptStore {
        coordinator: load("coordinator.pmt", DEFAULT_COORDINATOR),
        implementer: load("implementer.pmt", DEFAULT_IMPLEMENTER),
        reviewer: load("reviewer.pmt", DEFAULT_REVIEWER),
        researcher: load("researcher.pmt", DEFAULT_RESEARCHER),
        validator_schema: load("validator-schema.pmt", DEFAULT_VALIDATOR_SCHEMA),
        validator_plan: load("validator-plan.pmt", DEFAULT_VALIDATOR_PLAN),
        validator_spec: load("validator-spec.pmt", DEFAULT_VALIDATOR_SPEC),
        validator_phase: load("validator-phase.pmt", DEFAULT_VALIDATOR_PHASE),
        generation_plan: load("generation-plan.pmt", DEFAULT_GENERATION_PLAN),
        generation_spec: load("generation-spec.pmt", DEFAULT_GENERATION_SPEC),
        generation_phase: load("generation-phase.pmt", DEFAULT_GENERATION_PHASE),
        generation_work: load("generation-work.pmt", DEFAULT_GENERATION_WORKITEM),
    });
}

/// Initialize with compiled-in defaults only (no filesystem). For tests.
pub fn init_defaults() {
    let _ = STORE.set(PromptStore {
        coordinator: DEFAULT_COORDINATOR.to_string(),
        implementer: DEFAULT_IMPLEMENTER.to_string(),
        reviewer: DEFAULT_REVIEWER.to_string(),
        researcher: DEFAULT_RESEARCHER.to_string(),
        validator_schema: DEFAULT_VALIDATOR_SCHEMA.to_string(),
        validator_plan: DEFAULT_VALIDATOR_PLAN.to_string(),
        validator_spec: DEFAULT_VALIDATOR_SPEC.to_string(),
        validator_phase: DEFAULT_VALIDATOR_PHASE.to_string(),
        generation_plan: DEFAULT_GENERATION_PLAN.to_string(),
        generation_spec: DEFAULT_GENERATION_SPEC.to_string(),
        generation_phase: DEFAULT_GENERATION_PHASE.to_string(),
        generation_work: DEFAULT_GENERATION_WORKITEM.to_string(),
    });
}

/// Get the global prompt store. Panics if init() was not called.
pub fn store() -> &'static PromptStore {
    STORE.get().expect("prompts::init() must be called before prompts::store()")
}
```

**Override resolution:** `dirs::config_dir()` returns `~/.config` on Linux. If it returns `None` (unlikely but possible — e.g. no `HOME` env var), all overrides are skipped and defaults are used. For each `.pmt` filename, if a matching file exists and is non-empty, use it. Otherwise use the compiled-in default. Overrides log at `info!`, failures at `warn!`.

**Why OnceLock:** Available in std since Rust 1.70 (codebase uses edition 2024, requiring Rust 1.85+). No external dependency. `init()` + `store()` pattern makes initialization explicit and testable. Second call to `init()` is a harmless no-op (OnceLock returns Err, which we discard).

### Data Model

No data model changes. The `PromptStore` is a flat struct of 12 `String` fields.

### Consumer Changes

| File | Current | After |
|------|---------|-------|
| `src/agents/coordinator.rs:38` | `const SYSTEM_PROMPT: &str = r#"..."#;` (51 lines) | Remove const. Use `prompts::store().coordinator` |
| `src/agents/implementer.rs:28` | `const SYSTEM_PROMPT: &str = r#"..."#;` (36 lines) | Remove const. Use `prompts::store().implementer` |
| `src/agents/reviewer.rs:66` | `const SYSTEM_PROMPT: &str = r#"..."#;` (27 lines) | Remove const. Use `prompts::store().reviewer` |
| `src/agents/researcher.rs:18` | `const SYSTEM_PROMPT: &str = r#"..."#;` + `build_system_prompt(query)` | Remove const. `build_system_prompt` does `.replace("{query}", query)` on `prompts::store().researcher` |
| `src/validator/prompts.rs:7` | `const RESPONSE_SCHEMA` + 3 `format!()` functions | Remove const. Functions use `.replace()` chains on loaded templates (see example below) |
| `src/agents/generation.rs` | 4 inline instruction strings in `build_*_prompt()` | Replace instruction `push_str()` args with `prompts::store().generation_*` references |

**Validator migration example** — `plan_prompt()` before and after:

```rust
// BEFORE (format! with named args)
pub fn plan_prompt(title: &str, description: &str, acceptance_criteria: &str) -> String {
    format!(r#"...{title}...{description}...{acceptance_criteria}...{schema}"#,
        title = title,
        description = description,
        acceptance_criteria = acceptance_criteria,
        schema = RESPONSE_SCHEMA,
    )
}

// AFTER (.replace() chains on loaded template)
pub fn plan_prompt(title: &str, description: &str, acceptance_criteria: &str) -> String {
    crate::prompts::store().validator_plan
        .replace("{title}", title)
        .replace("{description}", description)
        .replace("{acceptance_criteria}", acceptance_criteria)
        .replace("{schema}", &crate::prompts::store().validator_schema)
}
```

Note: `phase_prompt` has an `order: u32` parameter. The `.replace()` call requires `&str`, so use `&order.to_string()`.

**Generation migration example** — `build_plan_prompt()` before and after:

```rust
// BEFORE (inline push_str)
msg.push_str("### Instructions\n");
msg.push_str(
    "Create a Plan with:\n\
     - A clear, bounded title\n\
     ...",
);

// AFTER (loaded from .pmt)
msg.push_str("### Instructions\n");
msg.push_str(&crate::prompts::store().generation_plan);
```

The dynamic assembly (context sections, learnings, validation failures) stays in Rust. Only the static instruction text is extracted to `.pmt` files.

### Implementation Plan

Each step keeps the build green:

**Step 1: Create `prompts/` directory and 12 `.pmt` files**
- Verbatim extract of prompt text from each source location
- No Rust changes yet — just new files

**Step 2: Create `src/prompts.rs`**
- `PromptStore` struct, `init()`, `init_defaults()`, `store()`
- `include_str!` for all 12 defaults
- Override logic using `dirs::config_dir().map(|d| d.join("loopr/prompts"))`
- Tests for: defaults non-empty, override from temp dir, empty override falls back, placeholder assertions, byte-for-byte match against `include_str!` defaults

**Step 3: Wire into build and startup**
- `src/lib.rs`: add `pub mod prompts;`
- `build.rs`: add `println!("cargo:rerun-if-changed=prompts/");`
- `src/main.rs`: call `loopr::prompts::init()` after config load (after line 17)

**Step 4: Migrate consumers (one file at a time, `cargo check` after each)**
1. `coordinator.rs` — remove const, replace refs with `prompts::store().coordinator`
2. `implementer.rs` — remove const, replace refs
3. `reviewer.rs` — remove const, replace refs
4. `researcher.rs` — remove const, keep `build_system_prompt()` with `.replace("{query}", query)` on loaded template
5. `validator/prompts.rs` — remove `RESPONSE_SCHEMA` const, convert `format!()` to `.replace()` chains; use `&order.to_string()` for the `u32` param in `phase_prompt()`
6. `generation.rs` — replace 4 inline instruction `push_str()` args with `prompts::store().generation_*`

**Step 5: Update tests**
- Add `crate::prompts::init_defaults()` to tests that reference prompts (idempotent via OnceLock)
- Existing test assertions unchanged — content is identical
- Add byte-for-byte comparison tests in `src/prompts.rs` confirming `.pmt` content matches expected

## Alternatives Considered

### Alternative 1: Runtime-only loading (no `include_str!`)
- **Description:** Load all prompts from disk at startup
- **Pros:** Simpler code (no compile-time embedding)
- **Cons:** Binary requires external `prompts/` directory to function. Missing files = crash at startup.
- **Why not chosen:** Deployment fragility. The binary should work standalone.

### Alternative 2: Full template engine (Tera, Handlebars)
- **Description:** Use `{{variable}}` syntax in `.pmt` files with a template engine
- **Pros:** All prompt text in `.pmt` files, including dynamic parts
- **Cons:** Adds a dependency. Template syntax is another thing to get wrong. Overkill for simple string replacement.
- **Why not chosen:** The dynamic assembly logic (context sections, learnings, validation failures) is better expressed in Rust. Only static instruction text needs extraction.

### Alternative 3: Embed as `&str` references (no runtime override)
- **Description:** Use `include_str!` only, no override mechanism
- **Pros:** Simpler — no `OnceLock`, no filesystem scanning
- **Cons:** Every prompt change requires recompilation. Loses the iteration speed benefit.
- **Why not chosen:** Runtime override is the primary UX goal for prompt tuning.

## Technical Considerations

### Dependencies
- `dirs` crate (already in `Cargo.toml`) — for `config_dir()`
- `std::sync::OnceLock` (std) — no new external dependencies

### Performance
- **Compile-time:** `include_str!` is zero-cost (baked into binary data segment)
- **Startup:** One directory scan + up to 12 file reads. Negligible.
- **Runtime:** `store()` is a single `OnceLock::get()` — effectively free

### Testing Strategy
- `src/prompts.rs` unit tests:
  - All 12 defaults are non-empty after `init_defaults()`
  - Override from temp dir replaces default
  - Empty override file falls back to default
  - Placeholder assertions (e.g. `researcher` contains `{query}`, validator templates contain `{title}`)
  - Byte-for-byte: `store().coordinator == DEFAULT_COORDINATOR` (catches extraction drift)
- Consumer tests: add `init_defaults()` call, existing assertions unchanged
- `otto ci` validates everything end-to-end

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| `.pmt` file content doesn't match original const exactly | Med | Low | Byte-for-byte comparison tests; verbatim copy-paste extraction |
| Tests fail because `init()` not called | Med | Low | Clear panic message; `init_defaults()` helper for tests |
| `format!()` → `.replace()` changes behavior for validator prompts | Low | Med | Existing tests catch any semantic drift |
| Editor adds trailing newline to `.pmt` files | Med | Low | Prompts are LLM input — trailing whitespace is harmless |
| `dirs::config_dir()` returns `None` | Low | Low | Gracefully skip override scan, use defaults |

## Open Questions

- (none — all design decisions resolved)

## References

- `src/agents/coordinator.rs:38-88` — Coordinator system prompt
- `src/agents/implementer.rs:28-63` — Implementer system prompt
- `src/agents/reviewer.rs:66-92` — Reviewer system prompt
- `src/agents/researcher.rs:18-48` — Researcher system prompt + `build_system_prompt()` at line 113
- `src/validator/prompts.rs:7-18` — `RESPONSE_SCHEMA` const
- `src/validator/prompts.rs:21-116` — 3 validator prompt functions (`plan_prompt`, `spec_prompt`, `phase_prompt`)
- `src/agents/generation.rs:88-94` — Plan generation instructions
- `src/agents/generation.rs:153-160` — Spec generation instructions
- `src/agents/generation.rs:203-212` — Phase generation instructions
- `src/agents/generation.rs:277-285` — Work generation instructions
