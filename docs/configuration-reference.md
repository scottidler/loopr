# Configuration Reference

**Author:** Scott A. Idler
**Date:** 2026-01-31
**Status:** Implementation Spec

---

## Summary

This document consolidates all Loopr configuration options. Configuration is loaded from YAML files with a hierarchical precedence.

---

## Configuration Files

### Load Order (Highest to Lowest Priority)

1. **Explicit path** - `-c /path/to/config.yml`
2. **Project local** - `.loopr.yml` in project root
3. **User global** - `~/.config/loopr/loopr.yml`
4. **Built-in defaults** - Embedded in binary

Later files override earlier ones at the key level.

### File Locations

```
~/.config/loopr/
├── loopr.yml           # Global configuration
├── catalog.toml        # Tool catalog (overrides built-in)
├── loops/              # Custom loop type definitions
│   └── custom.yml
└── prompts/            # Custom prompt templates
    └── code.md

.loopr/                 # Project-specific (in repo root)
├── loopr.yml           # Project config
├── loops/              # Project loop types
└── prompts/            # Project prompts
```

---

## Complete Configuration Schema

```yaml
# ~/.config/loopr/loopr.yml

# Logging
log_level: "info"  # trace, debug, info, warn, error

# LLM Configuration
llm:
  model: "claude-sonnet-4-20250514"
  max_tokens: 8192
  timeout_ms: 300000

  # Model selection by purpose
  models:
    default: "claude-sonnet-4-20250514"
    review: "claude-3-haiku-20240307"    # For LLM-as-Judge validation
    complex: "claude-opus-4-5-20250514"  # For difficult tasks

  # Provider configuration
  providers:
    anthropic:
      api_key_env: "ANTHROPIC_API_KEY"
      api_key_file: null  # Optional: path to file containing key
      base_url: "https://api.anthropic.com"

# Concurrency Limits
concurrency:
  max_loops: 50           # Maximum concurrent loops
  max_api_calls: 10       # Maximum concurrent LLM API calls
  max_worktrees: 50       # Maximum git worktrees

  # Per-type limits
  per_type:
    plan: 2               # Max concurrent plan loops
    spec: 5               # Max concurrent spec loops
    phase: 20             # Max concurrent phase loops
    code: 50              # Max concurrent code loops

# Scheduler Configuration
scheduler:
  poll_interval_secs: 1   # How often to check for runnable loops

  # Priority weights (higher = more important)
  priority:
    plan: 40              # Base priority for plan loops
    spec: 60              # Base priority for spec loops
    phase: 80             # Base priority for phase loops
    code: 100             # Base priority for code loops

    # Age boosting (prevents starvation)
    age_boost_per_minute: 1
    age_boost_max: 50

    # Depth boosting (deeper = higher priority)
    depth_boost_per_level: 10

    # Retry penalty (failed iterations reduce priority)
    retry_penalty_per_iteration: 5
    retry_penalty_max: 30

# Validation Configuration
validation:
  command: "otto ci"      # Default validation command
  iteration_timeout_ms: 300000  # 5 minutes
  max_iterations: 100     # Default max iterations

  # Per-type overrides
  per_type:
    plan:
      max_iterations: 50
      command: "loopr validate plan"
    spec:
      max_iterations: 30
      command: "loopr validate spec"
    phase:
      max_iterations: 20
      command: "loopr validate phase"
    code:
      max_iterations: 100
      command: "cargo test"

# Git Configuration
git:
  worktree_base: ".loopr/worktrees"  # Where to create worktrees
  main_branch: "main"                 # Branch to merge to
  auto_merge: false                   # Auto-merge on completion
  preserve_failed_branches: true      # Keep branches from failed loops

  # Divergence handling
  divergence_check_interval_secs: 60
  divergence_strategy: "pause"        # pause, abort, rebase

# Storage Configuration
storage:
  taskstore_dir: "~/.local/share/loopr"
  jsonl_warn_mb: 100      # Warn when JSONL files exceed this
  jsonl_error_mb: 500     # Error when JSONL files exceed this

  # Disk quotas
  disk_quota_min_gb: 5    # Minimum free disk space

# Runner Configuration
runners:
  no_net:
    slots: 10                      # Concurrent tool executions
    timeout_default_ms: 30000      # 30 seconds
    max_output_bytes: 100000       # 100KB output limit
    sandbox_method: "namespace"    # namespace, seccomp, firejail

  net:
    slots: 5
    timeout_default_ms: 60000      # 60 seconds
    max_output_bytes: 100000

  heavy:
    slots: 1                       # Low concurrency for builds
    timeout_default_ms: 600000     # 10 minutes
    max_output_bytes: 1000000      # 1MB for build output

# TUI Configuration
tui:
  tick_rate_ms: 250       # UI refresh rate
  scroll_page_size: 10    # Lines per Page Up/Down

  colors:
    running: "#00FF7F"    # Spring green
    pending: "#FFD700"    # Gold
    complete: "#32CD32"   # Lime green
    failed: "#DC143C"     # Crimson
    paused: "#87CEEB"     # Sky blue
    rebasing: "#DDA0DD"   # Plum
    invalidated: "#808080" # Gray

# Debug Configuration
debug:
  save_prompts: false     # Save rendered prompts to disk
  save_responses: false   # Save LLM responses to disk
  trace_tools: false      # Verbose tool execution logging
```

---

## Environment Variables

| Variable | Description | Default |
|----------|-------------|---------|
| `ANTHROPIC_API_KEY` | Anthropic API key (required) | - |
| `LOOPR_CONFIG` | Path to config file | `~/.config/loopr/loopr.yml` |
| `LOOPR_LOG` | Log level override | `info` |
| `LOOPR_DATA_DIR` | Data directory | `~/.local/share/loopr` |
| `LOOPR_SOCKET` | Daemon socket path | `~/.loopr/daemon.sock` |

---

## Loop Type Configuration

Loop types are defined in YAML files with inheritance support.

### Location Priority

1. `.loopr/loops/*.yml` (project-specific)
2. `~/.config/loopr/loops/*.yml` (user global)
3. Built-in types (embedded in binary)

### Schema

```yaml
# ~/.config/loopr/loops/code.yml
name: "code"
extends: "base"           # Inherit from another type
parent: "phase"           # Cascade parent type

description: "Execute coding tasks from a phase specification"

prompt_template: |
  You are implementing phase {{phase_number}} of {{phases_total}}.

  ## Task
  {{task}}

  ## Files to Modify
  {{#each files}}
  - {{this}}
  {{/each}}

  {{#if progress}}
  ## Previous Attempt Feedback
  {{progress}}
  {{/if}}

validation_command: "cargo test"
success_exit_code: 0
max_iterations: 100
iteration_timeout_ms: 600000

# Template variables (from context)
inputs:
  - task
  - phase_number
  - phases_total
  - files
  - progress

# Artifacts produced
outputs:
  - "code changes in worktree"

# Tools available to this loop type
tools:
  - read_file
  - write_file
  - search_files
  - execute_command
```

### Type Inheritance

Child types inherit from parent, with child values overriding:

```yaml
# base.yml
name: "base"
max_iterations: 50
iteration_timeout_ms: 300000
tools:
  - read_file
  - search_files

# code.yml (inherits from base)
name: "code"
extends: "base"
max_iterations: 100        # Overrides parent
iteration_timeout_ms: 600000  # Overrides parent
tools:                     # Merged with parent
  - write_file
  - execute_command
# Result: tools = [read_file, search_files, write_file, execute_command]
```

---

## Tool Catalog Configuration

Tools are defined in `catalog.toml`:

```toml
# ~/.config/loopr/catalog.toml

[defaults]
lane = "no-net"
timeout_ms = 30000
max_output_bytes = 100000

[tools.read_file]
description = "Read contents of a file"
lane = "no-net"
timeout_ms = 5000
parameters = { path = "string" }

[tools.write_file]
description = "Write content to a file"
lane = "no-net"
timeout_ms = 10000
parameters = { path = "string", content = "string" }

[tools.search_files]
description = "Search for files matching a pattern"
lane = "no-net"
timeout_ms = 30000
parameters = { pattern = "string", path = "string?" }

[tools.execute_command]
description = "Execute a shell command"
lane = "heavy"
timeout_ms = 600000
parameters = { command = "string", cwd = "string?" }

[tools.web_search]
description = "Search the web"
lane = "net"
timeout_ms = 60000
parameters = { query = "string" }
```

---

## Rust Structs

For implementation reference, here are the Rust configuration structs:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub log_level: Option<String>,
    pub llm: LlmConfig,
    pub concurrency: ConcurrencyConfig,
    pub validation: ValidationConfig,
    pub progress: ProgressConfig,
    pub git: GitConfig,
    pub storage: StorageConfig,
    pub runners: RunnersConfig,
    pub tui: TuiConfig,
    pub debug: DebugConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LlmConfig {
    pub model: String,
    pub max_tokens: u32,
    pub timeout_ms: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ConcurrencyConfig {
    pub max_loops: u32,
    pub max_api_calls: u32,
    pub max_worktrees: u32,
    pub per_type: HashMap<String, u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationConfig {
    pub command: String,
    pub iteration_timeout_ms: u64,
    pub max_iterations: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageConfig {
    pub taskstore_dir: PathBuf,
    pub jsonl_warn_mb: u32,
    pub jsonl_error_mb: u32,
    pub disk_quota_min_gb: u32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunnerConfig {
    pub slots: usize,
    pub timeout_default_ms: u64,
    pub max_output_bytes: usize,
    pub sandbox_method: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SchedulerConfig {
    pub poll_interval_secs: u64,
    pub max_concurrent_loops: usize,
    pub per_type_limit: HashMap<LoopType, usize>,
    pub priority_weights: PriorityWeights,
}
```

---

## Configuration Loading

```rust
impl Config {
    pub fn load(config_path: Option<&PathBuf>) -> Result<Self> {
        // 1. Start with defaults
        let mut config = Self::default();

        // 2. Load user global (~/.config/loopr/loopr.yml)
        if let Some(global) = Self::load_file(dirs::config_dir()?.join("loopr/loopr.yml"))? {
            config.merge(global);
        }

        // 3. Load project local (.loopr.yml)
        if let Some(local) = Self::load_file(".loopr.yml")? {
            config.merge(local);
        }

        // 4. Load explicit path (if provided)
        if let Some(path) = config_path {
            config.merge(Self::load_file(path)?);
        }

        // 5. Apply environment overrides
        config.apply_env_overrides();

        Ok(config)
    }
}
```

---

## Validation

Configuration is validated on load:

```rust
impl Config {
    pub fn validate(&self) -> Result<()> {
        // LLM
        ensure!(!self.llm.model.is_empty(), "llm.model is required");
        ensure!(self.llm.max_tokens > 0, "llm.max_tokens must be positive");

        // Concurrency
        ensure!(self.concurrency.max_loops > 0, "max_loops must be positive");
        ensure!(self.concurrency.max_loops <= 1000, "max_loops cannot exceed 1000");

        // Storage
        let storage_path = Path::new(&self.storage.taskstore_dir);
        ensure!(storage_path.exists() || storage_path.parent().map(|p| p.exists()).unwrap_or(false),
            "storage.taskstore_dir parent must exist");

        // Runners
        ensure!(self.runners.no_net.slots > 0, "no_net.slots must be positive");
        ensure!(self.runners.heavy.slots >= 1, "heavy.slots must be at least 1");

        Ok(())
    }
}
```

---

## Common Configuration Patterns

### Minimal Configuration

```yaml
# .loopr.yml - Project using all defaults
llm:
  model: "claude-sonnet-4-20250514"
```

### High-Throughput Configuration

```yaml
# For large projects with many parallel tasks
concurrency:
  max_loops: 100
  max_api_calls: 20
  per_type:
    code: 80

runners:
  no_net:
    slots: 20
  net:
    slots: 10
  heavy:
    slots: 3
```

### Conservative Configuration

```yaml
# For limited resources or cost control
concurrency:
  max_loops: 10
  max_api_calls: 3
  per_type:
    plan: 1
    spec: 2
    phase: 5
    code: 10

validation:
  max_iterations: 20  # Fail faster

llm:
  model: "claude-3-haiku-20240307"  # Cheaper model
```

### CI/CD Configuration

```yaml
# For automated pipelines
log_level: "warn"

git:
  auto_merge: true
  preserve_failed_branches: false

tui:
  tick_rate_ms: 1000  # Lower refresh rate

debug:
  save_prompts: true
  save_responses: true
```

---

## References

- [implementation-patterns.md](implementation-patterns.md) - Config system implementation
- [runners.md](runners.md) - Runner configuration details
- [scheduler.md](scheduler.md) - Scheduler priority configuration
- [tool-catalog.md](tool-catalog.md) - Tool catalog format
- [llm-client.md](llm-client.md) - LLM configuration
- [tui.md](tui.md) - TUI configuration
