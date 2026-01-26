# Design Document: Loop Configuration

**Author:** Scott Idler, Claude
**Date:** 2026-01-25
**Status:** Ready
**Parent Doc:** [loop-architecture.md](loop-architecture.md)

## Summary

Loop configuration spans three levels: global defaults, loop type definitions, and per-execution overrides. This layered approach lets operators set sensible defaults while allowing specific loops to customize behavior for their workload. All configuration follows the "defaults + override" pattern—set it once globally, override where needed.

## Problem Statement

### Background

Loopr's Ralph Wiggum loops need configurable:
- **Iteration limits** - How many retries before giving up
- **Timeouts** - Per-iteration and per-LLM-call limits
- **Concurrency** - How many loops run simultaneously
- **Validation** - What command determines success
- **Tools** - Which tools the LLM can use
- **LLM settings** - Model, tokens, provider

Without clear configuration hierarchy, operators face:
- No way to set organization-wide defaults
- Per-loop config duplication
- Unclear precedence when values conflict

### Problem

1. **Multiple configuration levels**: Global config, loop type definitions, and runtime overrides must compose predictably.
2. **Sensible defaults**: Most users shouldn't need to configure anything—defaults should work.
3. **Override granularity**: Need to override specific fields without respecifying everything.
4. **Discoverability**: Users must be able to see what config applies to a running loop.

### Goals

1. **Three-level hierarchy**: Global → Loop Type → Execution, with later levels overriding earlier.
2. **Complete defaults**: Every field has a sensible default; zero config works.
3. **Partial overrides**: Override only what you need at each level.
4. **Runtime introspection**: Query effective config for any loop.

### Non-Goals

1. **Dynamic reconfiguration**: Config is fixed at loop start; no hot-reload.
2. **Per-iteration config**: All iterations use the same config.
3. **Environment-based config**: Use config files, not env vars (except API keys).

## Proposed Solution

### Overview

Configuration resolves through three layers:

```
┌─────────────────────────────────────────────────────────┐
│ Execution Override (runtime)                            │
│   max_iterations: 25                                    │
├─────────────────────────────────────────────────────────┤
│ Loop Type Definition (YAML file)                        │
│   max_iterations: 50                                    │
│   validation_command: "cargo test"                      │
│   tools: [read, write, bash]                            │
├─────────────────────────────────────────────────────────┤
│ Global Config (~/.config/loopr/loopr.yml)               │
│   validation.max_iterations: 100                        │
│   validation.command: "otto ci"                         │
│   concurrency.max_loops: 50                             │
└─────────────────────────────────────────────────────────┘
```

Resolution: Start with global defaults, overlay loop type definition, then apply execution overrides.

### Configuration Layers

#### Layer 1: Global Configuration

File: `~/.config/loopr/loopr.yml` (or `.loopr.yml` in project root)

```yaml
# LLM provider settings
llm:
  default: anthropic/claude-sonnet-4-20250514
  timeout-ms: 300000  # 5 minutes per LLM call
  providers:
    anthropic:
      api-key-env: ANTHROPIC_API_KEY
      base-url: https://api.anthropic.com
      models:
        claude-sonnet-4-20250514:
          max-tokens: 8192
        claude-opus-4-20250514:
          max-tokens: 4096

# Concurrency limits
concurrency:
  max-loops: 50       # Total concurrent loops
  max-api-calls: 10   # Concurrent LLM API calls
  max-worktrees: 50   # Git worktrees

# Validation defaults (applies to all loop types unless overridden)
validation:
  command: "otto ci"
  iteration-timeout-ms: 300000  # 5 minutes per iteration
  max-iterations: 100

# Progress tracking
progress:
  strategy: system-captured
  max-entries: 5
  max-output-chars: 500

# Git settings
git:
  worktree-dir: /tmp/loopr/worktrees
  disk-quota-gb: 100

# Storage
storage:
  taskstore-dir: ~/.local/share/loopr
  jsonl-warn-mb: 100
  jsonl-error-mb: 500

# Loop type search paths
loops:
  paths:
    - builtin
    - ~/.config/loopr/loops
    - .loopr/loops
```

#### Layer 2: Loop Type Definition

Files in `~/.config/loopr/loops/` or `.loopr/loops/`

Example: `~/.config/loopr/loops/phase.yml`

```yaml
name: phase
description: "Implements a single phase from a spec"

# Prompt template (Handlebars)
prompt: |
  You are implementing Phase {{phase_number}} of {{spec_name}}.

  ## Requirements
  {{phase_requirements}}

  ## Previous Iteration Feedback
  {{#if feedback}}
  The previous attempt failed:
  {{feedback}}
  {{/if}}

  Implement the requirements. Run validation when done.

# Validation (overrides global)
validation-command: "otto ci"
success-exit-code: 0

# Iteration limits (overrides global)
max-iterations: 50

# Per-iteration limits
max-turns: 50              # Tool calls per iteration
iteration-timeout-ms: 300000

# LLM settings
max-tokens: 8192

# Available tools
tools:
  - read
  - write
  - edit
  - list
  - glob
  - bash

# Optional: Inherit from another type
extends: ralph
```

**Loop type inheritance**: Use `extends: parent-type` to inherit settings. Child values override parent values.

#### Layer 3: Execution Override

When spawning a loop programmatically or via CLI:

```bash
# CLI override
loopr run phase --task "Implement auth" --max-iterations 25

# Programmatic (LoopExecution record)
{
  "id": "1737802800",
  "loop_type": "phase",
  "config_overrides": {
    "max_iterations": 25,
    "validation_command": "make test"
  }
}
```

### Configuration Schema

#### Effective Loop Config

The resolved configuration for a running loop:

```rust
pub struct LoopConfig {
    // Identity
    pub loop_type: String,

    // Prompt
    pub prompt_template: String,

    // Validation
    pub validation_command: String,
    pub success_exit_code: i32,

    // Iteration limits
    pub max_iterations: u32,           // Default: 100
    pub max_turns_per_iteration: u32,  // Default: 50
    pub iteration_timeout_ms: u64,     // Default: 300_000 (5 min)

    // LLM settings
    pub max_tokens: u32,               // Default: 16384

    // Tools
    pub tools: Vec<String>,

    // Progress tracking
    pub progress_max_entries: usize,   // Default: 5
    pub progress_max_chars: usize,     // Default: 500
}
```

#### TaskManager Config

Global orchestration settings:

```rust
pub struct TaskManagerConfig {
    pub max_concurrent_tasks: usize,   // Default: 50
    pub poll_interval_secs: u64,       // Default: 60
    pub shutdown_timeout_secs: u64,    // Default: 60
    pub repo_root: PathBuf,
    pub worktree_dir: PathBuf,
}
```

### Resolution Algorithm

```rust
fn resolve_config(
    global: &Config,
    loop_type: &LoopTypeDefinition,
    overrides: &ConfigOverrides,
) -> LoopConfig {
    // Start with compiled-in defaults
    let mut config = LoopConfig::default();

    // Apply global validation settings
    config.max_iterations = global.validation.max_iterations;
    config.iteration_timeout_ms = global.validation.iteration_timeout_ms;
    config.validation_command = global.validation.command.clone();

    // Apply loop type definition (overrides global)
    if let Some(max) = loop_type.max_iterations {
        config.max_iterations = max;
    }
    if let Some(cmd) = &loop_type.validation_command {
        config.validation_command = cmd.clone();
    }
    // ... etc for all fields

    // Apply execution overrides (overrides loop type)
    if let Some(max) = overrides.max_iterations {
        config.max_iterations = max;
    }
    // ... etc

    config
}
```

### Default Values

| Field | Default | Rationale |
|-------|---------|-----------|
| `max_iterations` | 100 | Generous for complex tasks; safe due to validation |
| `max_turns_per_iteration` | 50 | Enough for multi-step tool use |
| `iteration_timeout_ms` | 300,000 (5 min) | Long enough for builds + tests |
| `max_tokens` | 16,384 | Modern model context limits |
| `validation_command` | "otto ci" | Loopr's standard CI |
| `success_exit_code` | 0 | Unix convention |
| `tools` | [read, write, edit, list, glob, bash] | Standard file operations |
| `max_concurrent_tasks` | 50 | Balance parallelism with resources |
| `poll_interval_secs` | 60 | Fallback; event-driven pickup is primary |

### Per-Loop-Type Defaults

Different loop types have different characteristics:

| Loop Type | max_iterations | max_turns | Rationale |
|-----------|----------------|-----------|-----------|
| plan | 10 | 30 | Plans should converge quickly |
| spec | 25 | 40 | Specs need more refinement |
| phase | 50 | 50 | Phases are the core work |
| ralph | 100 | 50 | General-purpose, generous limits |
| explore | 3-10 | 20 | Quick discovery, not implementation |

### Configuration Files Location

Search order (first found wins):
1. `--config` CLI flag (explicit path)
2. `.loopr.yml` (project root)
3. `~/.config/loopr/loopr.yml` (user config)

Loop type search order:
1. `builtin` (compiled into binary)
2. `~/.config/loopr/loops/` (user types)
3. `.loopr/loops/` (project types)

Later paths override earlier ones if same type name.

### Runtime Introspection

Query effective config for a running loop:

```bash
# Show resolved config for a loop
loopr loop config <exec-id>

# Output:
# Loop: 1737802800 (type: phase)
# Effective Configuration:
#   max_iterations: 25 (override)
#   max_turns_per_iteration: 50 (default)
#   validation_command: "cargo test" (loop type)
#   iteration_timeout_ms: 300000 (global)
#   ...
```

The `(source)` annotation shows where each value came from.

### Validation

Config validation runs at:
1. **Daemon startup**: Global config validated
2. **Loop type load**: Type definitions validated
3. **Loop spawn**: Effective config validated before execution

Validation checks:
- Required fields present (prompt_template for loop types)
- Numeric ranges (max_iterations > 0)
- Command executability (validation_command exists)
- Tool availability (tools list matches known tools)

## Alternatives Considered

### Alternative 1: Single Flat Config File

**Description:** One large config file with all settings, including per-loop-type sections.

**Pros:**
- Single file to manage
- No resolution complexity

**Cons:**
- Doesn't scale with many loop types
- No project-local overrides
- Hard to share loop types across projects

**Why not chosen:** Layered config provides flexibility for organization defaults + project customization.

### Alternative 2: Environment Variables for Everything

**Description:** All config via `TD_MAX_ITERATIONS`, `TD_VALIDATION_CMD`, etc.

**Pros:**
- 12-factor app compliance
- Easy container deployment

**Cons:**
- Hard to manage many settings
- No hierarchical override
- Poor discoverability

**Why not chosen:** YAML files are more readable and maintainable. Env vars used only for secrets (API keys).

### Alternative 3: Database-Stored Config

**Description:** Store config in TaskStore alongside loop records.

**Pros:**
- Config versioning for free
- Query config history

**Cons:**
- Chicken-egg problem (need config to connect to store)
- Harder to edit
- Overkill for mostly-static config

**Why not chosen:** Files are simpler and sufficient. Config rarely changes.

## Technical Considerations

### Performance

- Config loaded once at daemon startup
- Loop type definitions cached in memory
- Resolution is O(fields) per loop spawn—negligible

### File Watching

Not implemented. Config changes require daemon restart. This is intentional:
- Avoids mid-flight config changes causing inconsistent behavior
- Simpler mental model: "restart to apply changes"

### Secrets Handling

API keys handled specially:
1. Check environment variable first (`ANTHROPIC_API_KEY`)
2. Fall back to file path if configured (`api-key-file: ~/.secrets/anthropic`)
3. Never log or store keys in TaskStore

### Testing

- Unit tests for config parsing
- Unit tests for resolution (global + type + override)
- Integration tests loading from actual files
- Validation tests for error messages

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Config precedence confusion | Medium | Medium | Clear documentation; `loopr loop config` shows source |
| Missing config file fails silently | Low | High | Explicit startup logging; fail if required fields missing |
| Loop type name collision | Low | Low | Last-loaded wins; warn on collision |
| Invalid config crashes daemon | Medium | High | Validate at load time; refuse to start with bad config |

## Future Work

1. **Config validation CLI**: `loopr config validate` to check files before deployment
2. **Config diff**: Show differences between effective and default config
3. **Config export**: Dump effective config to file for debugging
4. **Hot-reload**: Reload loop types without daemon restart (global config stays static)
5. **Config inheritance visualization**: Show type inheritance chain

## References

- [loop-architecture.md](loop-architecture.md) - Parent design document
- [Ralph Wiggum technique](https://ghuntley.com/ralph/) - Iteration pattern source
- [12-factor app config](https://12factor.net/config) - Env var pattern (partially adopted)
