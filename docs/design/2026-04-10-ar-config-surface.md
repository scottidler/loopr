# Design Document: AutoResearch Config Surface

**Author:** Scott A. Idler
**Date:** 2026-04-10
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

Expose a uniform, AR-friendly configuration surface across all LLM-calling subsystems in Loopr. Every section that makes an LLM call shares the same base schema. Prompt file paths become first-class config fields alongside model selection. A post-run scorer emits structured metrics that AR uses to evaluate trials. The goal: turn Loopr into a system that AutoResearch can optimize overnight.

## What is AutoResearch?

AutoResearch (AR) is Andrej Karpathy's open-source framework for automated experimentation loops. The core idea: give an AI agent a system with tunable inputs and a measurable output, then let it run hundreds of experiments autonomously - modifying inputs, measuring results, keeping improvements, discarding failures.

AR requires three things:
1. **An objective metric** - a score function that evaluates each trial
2. **Tunable inputs** - knobs the script can modify between trials via config files
3. **Fast feedback loops** - each trial completes quickly enough to run many overnight

The pattern: AR writes a config, runs the system, reads the score, analyzes what worked and what didn't, writes a new config, repeats. Knowledge compounds across experiments via a learnings file. The loop is the hero, not any single trial.

### How We Intend to Leverage AR

Loopr's E2E pipeline is a natural AR target. The inputs are prompt files and config values. The output is measurable: how many Work items reach Done, how many bundles get accepted on first review, how many merge conflicts occur, how long it takes. A human has been running this loop manually for 30 releases - observing failures, writing design docs, adjusting prompts and config, running again. AR automates the loop.

The AR script:
1. Generates a `loopr.yml` config file (model assignments, behavioral knobs, prompt file paths)
2. Optionally generates modified `.pmt` prompt files for this trial
3. Launches an E2E run against a target project (e.g., python-api)
4. Reads the structured score output when the run completes
5. Analyzes results, updates its learnings, generates the next trial's config
6. Repeats until morning

At 10-20 minutes per E2E run, AR gets 30-50 trials per night. Each trial explores a different combination of prompts, model assignments, and behavioral knobs. The trials that score highest reveal which configuration dimensions matter most.

### What AR Optimizes

The highest-leverage knobs, in order:

1. **Prompt content** - the .pmt files that define what each agent sees. The decomposer prompt determines whether work is split sanely. The implementer prompt determines whether it handles edge cases. The reviewer prompt determines whether it catches real bugs or rejects good code. These are the primary optimization target.

2. **Model selection** - which model runs each role. Does the decomposer need Opus to produce good hierarchies, or is Sonnet sufficient? Would Haiku work for the reviewer? Each model swap changes both quality and cost per trial.

3. **Behavioral knobs** - retry limits, quality gates, stale policies, pool sizes. These determine how the system recovers from failures and how aggressively it parallelizes.

4. **Generation parameters** - temperature, max-tokens. These exist as knobs but are the weakest lever. A prompt rewrite or model swap dwarfs the effect of a temperature change.

## Problem Statement

### Background

Loopr has 12 distinct subsystems that make LLM calls. Each has its own config struct with a different field set. Some share `AgentRoleConfig`, others have bespoke structs (`ValidatorConfig`, `TierGateConfig`, `ClarityGateConfig`, `DecomposerConfig`). Prompt file paths are hardcoded in `prompts.rs` - the XDG override directory works but only for the same filenames.

### Problem

**P1 - Config schema inconsistency.** An AR script that wants to set the model for the decomposer writes `decomposer.model`. For the reviewer, it writes `agents.reviewer.model`. For the clarity gate, it writes `strategy.clarity-gate.model`. For the tier gate, it writes `tier-gate.model`. Four different nesting paths for the same concept. The schema is not intuitable from one section to the next.

| Subsystem | Config Path | Has model | Has temperature | Has max-tokens | Has prompt |
|-----------|-------------|-----------|-----------------|----------------|------------|
| Coordinator | `agents.coordinator.*` | yes | yes | yes | no |
| Implementer | `agents.implementer.*` | yes | yes | yes | no |
| Reviewer | `agents.reviewer.*` | yes | yes | yes | no |
| Researcher | `agents.researcher.*` | yes | yes | yes | no |
| Decomposer | `decomposer.*` | yes | yes | yes | no |
| Validator | `validator.*` | yes | yes | yes | no |
| Tier Gate | `tier-gate.*` | yes | yes | yes | no |
| Evaluator | `evaluator.*` | yes | yes | yes | no |
| Clarity Gate | `strategy.clarity-gate.*` | yes | no | no | no |
| Chat | `chat.*` | yes | yes | yes | no |

`ClarityGateConfig` is the worst offender: buried inside `StrategyConfig`, missing `temperature` and `max-tokens`. Every other LLM-calling config has those fields.

**P2 - Prompt paths are not configurable.** `prompts.rs` loads prompts from hardcoded filenames. To use a different decomposer work prompt, you must place a file named exactly `decompose/work.pmt` in `~/.config/loopr/prompts/`. There is no config field to point at an arbitrary path. AR cannot run two concurrent trials with different prompts without filesystem gymnastics.

The existing prompt override mechanism is fine for human use (drop a file with the right name). For AR, it needs to be a config field: "for this trial, use this prompt file."

**P3 - No structured score output.** E2E runs produce logs and JSONL files, but no structured summary. AR needs a single JSON file with graduated metrics to evaluate each trial. Currently, scoring requires parsing logs and counting JSONL records by hand.

### Goals

- Uniform base schema for every LLM-calling config section: `model`, `max-tokens`, `temperature`, `api-key-env`
- Prompt file path(s) as a config field on every section that uses prompts
- ClarityGate promoted to a top-level config peer, matching the shared schema
- Structured score output from E2E runs
- All existing defaults preserved - zero behavioral change with default config

### Non-Goals

- Building the AR script itself (that is a separate project)
- Changing prompt content (this doc is about the surface, not the content)
- Changing the E2E test harness infrastructure
- Adding new LLM-calling subsystems
- Template engine for .pmt files (the existing `{placeholder}` interpolation is sufficient)

## Proposed Solution

### Overview

Three changes:

1. **Uniform LLM config base** - define a `LlmConfig` struct that every LLM-calling section embeds. Promote ClarityGate to a top-level config section.
2. **Prompt config fields** - add prompt path fields to every section that uses .pmt files. `PromptStore::init` reads paths from Config instead of hardcoded strings.
3. **Score output** - add a scorer that reads JSONL after an E2E run and writes `score.json`.

### Part 1: Uniform LLM Config Base

Define a shared base that every LLM-calling section flattens into itself:

```rust
/// Base configuration for any subsystem that calls an LLM.
/// Every section that makes an LLM call embeds this via #[serde(flatten)].
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct LlmConfig {
    pub model: String,
    pub api_key_env: String,
    pub max_tokens: u32,
    pub temperature: f32,
}
```

Each section flattens `LlmConfig` and adds its own fields:

```rust
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct DecomposerConfig {
    #[serde(flatten)]
    pub llm: LlmConfig,
    pub validation_model: String,
    pub prompts: DecomposerPrompts,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct ValidatorConfig {
    #[serde(flatten)]
    pub llm: LlmConfig,
    pub enabled: bool,
    pub prompt: String,  // path relative to prompts dir
}
```

`AgentRoleConfig` becomes a superset of `LlmConfig` with agent-specific fields:

```rust
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct AgentRoleConfig {
    #[serde(flatten)]
    pub llm: LlmConfig,
    pub max_iterations: u32,
    pub max_pool: u32,
    pub session_timeout_secs: Option<u64>,
    pub max_requeries: u32,
    pub prompt: String,  // path relative to prompts dir
}
```

**The pattern:** If you know the schema for one section, you know the base schema for all sections. `model`, `max-tokens`, `temperature`, `api-key-env` appear at the same nesting depth in every section. Section-specific fields layer on top.

**ClarityGate promotion:** Move from `strategy.clarity-gate` to top-level `clarity-gate`:

```yaml
# Before
strategy:
  clarity-gate:
    model: claude-sonnet-4-6
    min-score: 3

# After
clarity-gate:
  model: claude-sonnet-4-6
  max-tokens: 1024
  temperature: 0.0
  min-score: 3
```

### Part 2: Prompt Config Fields

Every section that uses a .pmt file gets a `prompt:` field (or `prompts:` for sections with multiple prompts). The value is a path relative to the prompts directory. Default values match the current hardcoded filenames.

**Single-prompt sections:**

```yaml
agents:
  coordinator:
    model: claude-opus-4-6
    max-tokens: 8192
    max-iterations: 4294967295
    prompt: coordinator              # resolves to prompts/coordinator.pmt
  implementer:
    model: claude-sonnet-4-6
    max-tokens: 8192
    max-iterations: 20
    prompt: implementer
  reviewer:
    model: claude-sonnet-4-6
    max-tokens: 4096
    max-iterations: 5
    prompt: reviewer
  researcher:
    model: claude-sonnet-4-6
    max-tokens: 4096
    max-iterations: 10
    prompt: researcher

tier-gate:
  model: claude-haiku-4-5-20251001
  max-tokens: 16
  prompt: tier-gate

clarity-gate:
  model: claude-sonnet-4-6
  max-tokens: 1024
  min-score: 3
  # No prompt field - clarity gate currently uses a hardcoded prompt.
  # Adding a .pmt file for it is a future enhancement.
```

**Multi-prompt sections:**

The decomposer uses 5 prompt files. The validator uses 4. The evaluator uses 4. Chat uses 5. These get a `prompts:` map:

```yaml
decomposer:
  model: claude-sonnet-4-6
  max-tokens: 4096
  validation-model: claude-haiku-4-5-20251001
  prompts:
    spec: decompose/spec
    phase: decompose/phase
    work: decompose/work
    validate: decompose/validate
    ratify: decompose/ratify

validator:
  model: claude-sonnet-4-6
  max-tokens: 4096
  enabled: false
  prompts:
    schema: validator-schema
    plan: validator-plan
    spec: validator-spec
    phase: validator-phase

evaluator:
  model: claude-sonnet-4-6
  max-tokens: 4096
  enabled: false
  prompts:
    schema: coverage-schema
    plan-specs: coverage-plan-specs
    spec-phases: coverage-spec-phases
    phase-works: coverage-phase-works

chat:
  model: claude-sonnet-4-6
  max-tokens: 8192
  delegate-model: claude-haiku-4-5-20251001
  prompts:
    default: chat
    interview: chat-interview
    draft: chat-draft
    refine: chat-refine
    executing: chat-executing
```

**Resolution rules:**
1. If the value is a relative path (no leading `/`), append `.pmt` and resolve: first check `~/.config/loopr/prompts/<path>.pmt`, then fall back to the compiled-in binary default for that filename.
2. If the value is an absolute path, load from that path directly (no `.pmt` suffix appended). This lets AR place trial-specific prompts anywhere on the filesystem with any extension.
3. If the file does not exist at the resolved path and no compiled-in default matches, the daemon fails to start with a clear error naming the missing path.

**Changes to `prompts.rs`:**

`PromptStore::init` currently takes only `max_abandon_ratio`. It will take `&Config` and read prompt paths from the config structs instead of hardcoded filenames. The `include_str!` defaults remain as fallbacks when config values match the default filenames and no XDG override exists.

```rust
pub fn init(config: &Config) -> eyre::Result<()> {
    let overrides_dir = dirs::config_dir().map(|d| d.join("loopr/prompts"));

    let load = |configured_path: &str, default: &str| -> eyre::Result<String> {
        // 1. Absolute path - load directly, FATAL on failure.
        //    If AR provides an absolute path and it doesn't exist, that is a
        //    configuration error, not an occasion to silently fall back.
        //    Silent fallback would corrupt AR experimental data by scoring
        //    the baseline default as if it were the trial prompt.
        if configured_path.starts_with('/') {
            let content = fs::read_to_string(configured_path)
                .context(format!("absolute prompt path not found: {}", configured_path))?;
            eyre::ensure!(
                !content.trim().is_empty(),
                "absolute prompt path is empty: {}", configured_path
            );
            info!("prompt loaded from absolute path: {}", configured_path);
            return Ok(content);
        }
        // 2. XDG override - check ~/.config/loopr/prompts/<path>.pmt
        //    Relative paths get the fallback chain: XDG -> compiled-in default.
        if let Some(ref dir) = overrides_dir {
            let path = dir.join(format!("{}.pmt", configured_path));
            match fs::read_to_string(&path) {
                Ok(content) if !content.trim().is_empty() => {
                    info!("prompt override loaded: {}", path.display());
                    return Ok(content);
                }
                _ => {}
            }
        }
        // 3. Compiled-in default
        Ok(default.to_string())
    };

    let max_abandon_ratio = config.agents.coordinator.max_abandon_ratio;
    let _ = STORE.set(PromptStore {
        coordinator: interpolate_status_values(
            load(&config.agents.coordinator.role.prompt, DEFAULT_COORDINATOR)?,
            max_abandon_ratio,
        ),
        implementer: load(&config.agents.implementer.prompt, DEFAULT_IMPLEMENTER)?,
        reviewer: load(&config.agents.reviewer.prompt, DEFAULT_REVIEWER)?,
        // ... etc for all 25 prompts
    });
    Ok(())
}
```

**Invariant:** Absolute paths are fatal on failure. Relative paths get the fallback chain (XDG -> compiled-in default). This distinction is critical for AR: a typo in an absolute trial path must halt the run, not silently score the baseline.

### Part 3: Score Output

After each E2E run, write a structured score file that AR reads to evaluate the trial.

**Location:** `<e2e-output-dir>/score.json`

**Schema:**

```json
{
  "version": 1,
  "target": "python-api",
  "timestamp": "2026-04-10T22:15:30Z",
  "duration_secs": 847,
  "completion": {
    "works_total": 11,
    "works_done": 7,
    "works_abandoned": 3,
    "works_blocked": 1,
    "completion_rate": 0.636
  },
  "quality": {
    "bundles_total": 14,
    "bundles_accepted_first_try": 5,
    "bundles_accepted_after_revision": 3,
    "bundles_rejected_terminal": 6,
    "first_try_acceptance_rate": 0.357,
    "noop_bundles": 2,
    "merge_conflicts": 1
  },
  "efficiency": {
    "total_sessions": 22,
    "sessions_completed": 15,
    "sessions_failed": 7,
    "avg_attempts_per_work": 2.0,
    "avg_rejections_per_bundle": 1.1
  },
  "validation": {
    "tests_passed": true,
    "docker_exit_code": 0,
    "validation_commands_passed": 3,
    "validation_commands_total": 3
  },
  "composite_score": 0.58
}
```

The `composite_score` is a weighted blend:
- 40% completion_rate (works reaching Done)
- 30% first_try_acceptance_rate (bundles accepted without revision)
- 20% validation pass (validation_commands_passed / validation_commands_total, 1.0 if none)
- 10% efficiency (1 - sessions_failed / total_sessions)

AR optimizes for `composite_score`. The validation component is graduated (partial credit for passing some commands) rather than binary, so a run that passes 2/3 validation commands scores 0.133 instead of 0.0. The weights are a fixed starting point; AR can also read raw sub-metrics directly from the JSON if it needs finer-grained signal.

**Implementation:** A new `loopr score` CLI subcommand that reads JSONL files from the E2E output directory and writes `score.json`. AR calls this after each E2E run completes.

### Full Config Reference

The complete default `loopr.yml` with all AR-tunable fields, organized by the uniform schema pattern:

```yaml
# === Agent Roles ===
# Base: model, max-tokens, temperature, api-key-env
# Agent: + max-iterations, max-pool, session-timeout-secs, max-requeries, prompt

agents:
  enabled: false
  pull-based-workers: false
  worker-pool-size: auto
  coordinator:
    model: claude-opus-4-6
    max-tokens: 8192
    temperature: 0.2
    max-iterations: 4294967295
    max-pool: 1
    max-requeries: 3
    prompt: coordinator
    active-interval-secs: 5
    idle-interval-secs: 30
    interview-mode: interactive
    max-work-retries: 3
    max-validation-attempts: 3
    max-researcher-spawns: 3
    max-abandon-ratio: 0.4
    phase-timeout-secs: 3600
    goal-timeout-secs: 14400
  implementer:
    model: claude-sonnet-4-6
    max-tokens: 8192
    temperature: 0.3
    max-iterations: 20
    max-pool: 4294967295
    session-timeout-secs: 1800
    max-requeries: 3
    prompt: implementer
  reviewer:
    model: claude-sonnet-4-6
    max-tokens: 4096
    temperature: 0.1
    max-iterations: 5
    max-pool: 4294967295
    session-timeout-secs: 600
    max-requeries: 3
    prompt: reviewer
  researcher:
    model: claude-sonnet-4-6
    max-tokens: 4096
    temperature: 0.1
    max-iterations: 10
    max-pool: 4
    session-timeout-secs: 600
    max-requeries: 3
    prompt: researcher

# === LLM System Calls ===
# Base: model, max-tokens, temperature, api-key-env
# + section-specific fields

decomposer:
  model: claude-sonnet-4-6
  max-tokens: 4096
  temperature: 0.3
  validation-model: claude-haiku-4-5-20251001
  prompts:
    spec: decompose/spec
    phase: decompose/phase
    work: decompose/work
    validate: decompose/validate
    ratify: decompose/ratify
    generation-work: generation-work   # Work creation (called from agents/generation.rs)

validator:
  model: claude-sonnet-4-6
  max-tokens: 4096
  temperature: 0.0
  enabled: false
  prompts:
    schema: validator-schema
    plan: validator-plan
    spec: validator-spec
    phase: validator-phase

evaluator:
  model: claude-sonnet-4-6
  max-tokens: 4096
  temperature: 0.0
  enabled: false
  prompts:
    schema: coverage-schema
    plan-specs: coverage-plan-specs
    spec-phases: coverage-spec-phases
    phase-works: coverage-phase-works

tier-gate:
  model: claude-haiku-4-5-20251001
  max-tokens: 16
  temperature: 0.0
  enabled: true
  prompt: tier-gate

clarity-gate:
  model: claude-sonnet-4-6
  max-tokens: 1024
  temperature: 0.0
  enabled: true
  min-score: 3

chat:
  model: claude-sonnet-4-6
  max-tokens: 8192
  temperature: 0.3
  delegate-model: claude-haiku-4-5-20251001
  prompts:
    default: chat
    interview: chat-interview
    draft: chat-draft
    refine: chat-refine
    executing: chat-executing

# === Behavioral Knobs ===

strategy:
  stale-policy: replan-at-safe-point
  conflict-policy: retry-once
  max-session-failures: 3
  max-decomposition-attempts: 3
  max-bubble-up-depth: 2
  coverage-enabled: true

reconciler:
  interval-secs: 60
  enabled: true
```

### Knob Inventory for AR

Organized by expected impact on composite score:

| Knob | Config Path | Type | Default | AR Leverage |
|------|-------------|------|---------|-------------|
| Decomposer work prompt | `decomposer.prompts.work` | path | `decompose/work` | **Critical** - determines work split quality, dependency declarations, parallelism |
| Decomposer spec prompt | `decomposer.prompts.spec` | path | `decompose/spec` | **Critical** - determines architectural decomposition |
| Implementer prompt | `agents.implementer.prompt` | path | `implementer` | **High** - determines code quality, noop handling, scope adherence |
| Reviewer prompt | `agents.reviewer.prompt` | path | `reviewer` | **High** - determines accept/reject calibration |
| Coordinator prompt | `agents.coordinator.prompt` | path | `coordinator` | **High** - determines orchestration decisions, retry vs abandon |
| Decomposer model | `decomposer.model` | string | `claude-sonnet-4-6` | **High** - Opus may produce better hierarchies |
| Coordinator model | `agents.coordinator.model` | string | `claude-opus-4-6` | **Medium** - already Opus; could test Sonnet to reduce cost |
| Reviewer model | `agents.reviewer.model` | string | `claude-sonnet-4-6` | **Medium** - Haiku might suffice for simple verdicts |
| Max work retries | `agents.coordinator.max-work-retries` | u32 | 3 | **Medium** - too few = premature abandon; too many = wasted sessions |
| Max abandon ratio | `agents.coordinator.max-abandon-ratio` | f64 | 0.4 | **Medium** - quality gate threshold |
| Max session failures | `strategy.max-session-failures` | u32 | 3 | **Medium** - circuit breaker sensitivity |
| Interview mode | `agents.coordinator.interview-mode` | enum | `interactive` | **Medium** - AR should always use `skip` or `auto` |
| Stale policy | `strategy.stale-policy` | enum | `replan-at-safe-point` | **Medium** - determines recovery from stale base ticks |
| Max iterations (impl) | `agents.implementer.max-iterations` | u32 | 20 | **Low** - tool-use budget; 20 is usually sufficient |
| Max decomposition attempts | `strategy.max-decomposition-attempts` | u32 | 3 | **Low** - retry budget for decomposition |
| Validation model | `decomposer.validation-model` | string | `claude-haiku-4-5-20251001` | **Low** - Haiku is sufficient for template checks |
| Implementer model | `agents.implementer.model` | string | `claude-sonnet-4-6` | **Low** - already well-calibrated |
| Reconciler interval | `reconciler.interval-secs` | u64 | 60 | **Low** - affects recovery speed, not quality |
| Temperature (any) | `*.temperature` | f32 | varies | **Low** - weakest lever; dwarfed by prompt/model changes |
| Max tokens (any) | `*.max-tokens` | u32 | varies | **Low** - only matters when hitting the ceiling |

### Architecture

```
AR Script                          Loopr
=========                          =====

Generate loopr.yml  ─────────────> Config::load()
Generate trial .pmt files ───────> PromptStore::init(&config)
                                      │
Launch E2E run     ─────────────>  daemon + agents + target project
                                      │
Wait for completion                   │
                                      v
Read score.json  <───────────────  `loopr score` reads JSONL, writes score.json

Analyze results
Update learnings
Generate next trial config
Repeat
```

**Concrete trial walkthrough:**

Trial 1 (baseline): AR uses the default `decompose/work.pmt` and Sonnet for the decomposer. The E2E run scores 0.42 - 5/11 works done, 3 merge conflicts from parallel same-file writes, 2 noop death loops.

Trial 2: AR analyzes the failure log, identifies "parallel same-file writes" as the dominant failure mode. It generates a modified `work-trial2.pmt` that adds explicit dependency rules for same-file works, and points the config at it:

```yaml
decomposer:
  model: claude-sonnet-4-6
  prompts:
    work: /tmp/ar/trials/002/work-trial2   # absolute path to AR-generated prompt
```

Score: 0.61. Merge conflicts eliminated. But 2 works now unnecessarily serialized.

Trial 3: AR tries promoting the decomposer to Opus, hypothesizing that a stronger model makes better dependency decisions with the same prompt:

```yaml
decomposer:
  model: claude-opus-4-6
  prompts:
    work: /tmp/ar/trials/002/work-trial2   # keep the winning prompt from trial 2
```

Score: 0.71. Opus produces tighter work splits with correct dependencies. 8/11 works done.

Trial 4: AR generates a new reviewer prompt that adds cross-module signature checking (the `update_bookmark` bug from the v0.1.107 post-mortem), keeping the trial 3 decomposer config:

```yaml
decomposer:
  model: claude-opus-4-6
  prompts:
    work: /tmp/ar/trials/002/work-trial2
agents:
  reviewer:
    prompt: /tmp/ar/trials/004/reviewer-trial4
```

Score: 0.78. Cross-module bugs caught. 9/11 works done, validation passes.

Each trial builds on previous winners. Knowledge compounds.

No new runtime components. The AR script is external. Loopr's changes are:
1. Config structs gain uniform shape + prompt fields
2. PromptStore reads paths from config
3. New `loopr score` subcommand

### Data Model

**New struct:**

```rust
pub struct LlmConfig {
    pub model: String,
    pub api_key_env: String,
    pub max_tokens: u32,
    pub temperature: f32,
}
```

**Modified structs (field additions only):**

| Struct | New Field | Type | Default |
|--------|-----------|------|---------|
| `AgentRoleConfig` | `prompt` | `String` | role name (e.g., `"implementer"`) |
| `DecomposerConfig` | `prompts` | `DecomposerPrompts` | current filenames |
| `ValidatorConfig` | `prompts` | `ValidatorPrompts` | current filenames |
| `EvaluatorConfig` | `prompts` | `EvaluatorPrompts` | current filenames |
| `TierGateConfig` | `prompt` | `String` | `"tier-gate"` |
| `ChatConfig` | `prompts` | `ChatPrompts` | current filenames |
| `ClarityGateConfig` | `max_tokens`, `temperature` | `u32`, `f32` | `1024`, `0.0` |

**Promoted struct:**

`ClarityGateConfig` moves from `StrategyConfig.clarity_gate` to `Config.clarity_gate` (top-level).

**New CLI subcommand:**

`loopr score --dir <e2e-output-dir>` - reads JSONL, writes `score.json`.

### Implementation Plan

**Phase 1: LlmConfig base + ClarityGate promotion**
- Define `LlmConfig` struct
- Refactor `AgentRoleConfig` to flatten `LlmConfig`
- Refactor `DecomposerConfig`, `ValidatorConfig`, `TierGateConfig`, `EvaluatorConfig` to flatten `LlmConfig`
- Add missing fields to `ClarityGateConfig` (`max_tokens`, `temperature`)
- Move `ClarityGateConfig` from `strategy` to top-level `Config`
- Add serde backward compat: if `strategy.clarity-gate` exists in old configs, deserialize from there
- `otto ci` to verify

**Phase 2: Prompt config fields**
- Add `prompt: String` field to `AgentRoleConfig`, `TierGateConfig`
- Add `prompts: XxxPrompts` structs to `DecomposerConfig`, `ValidatorConfig`, `EvaluatorConfig`, `ChatConfig`
- Refactor `PromptStore::init` to take `&Config` and read paths from config
- Preserve `include_str!` fallback for default paths
- Support absolute paths for AR trial prompts
- `otto ci` to verify
- Verify XDG override still works (existing test covers this)

**Phase 3: Score output**
- Define `Score` struct matching the JSON schema above
- Implement scorer: read JSONL files, count statuses, compute rates
- Add `loopr score` CLI subcommand
- Write tests for scorer logic

## Alternatives Considered

### Alternative 1: Flat override section in config
- **Description:** A top-level `ar-overrides:` section with flat key-value pairs that get merged over the nested config.
- **Pros:** Simple interface for AR scripts.
- **Cons:** AR writes a script. It can handle nested YAML. Adding a parallel config path creates ambiguity (which wins when both are set?), doubles the config surface, and is unnecessary complexity.
- **Why not chosen:** Solving a problem that does not exist. AR generates YAML programmatically.

### Alternative 2: Environment variable overrides for every knob
- **Description:** `LOOPR_DECOMPOSER_MODEL=claude-opus-4-6` overrides `decomposer.model`.
- **Pros:** No config file generation needed. Simple per-trial overrides.
- **Cons:** 50+ environment variables. Hard to audit what a trial used. No structured record of the config.
- **Why not chosen:** The config file IS the record of what a trial used. AR should produce it as an artifact.

### Alternative 3: Keep prompt paths hardcoded, vary only via XDG directory
- **Description:** AR creates a unique XDG prompts directory per trial, symlinks it before each run.
- **Pros:** No code changes to prompt loading.
- **Cons:** Filesystem gymnastics. Cannot run concurrent trials. Trial config doesn't capture which prompts were used (you'd need to inspect the symlink target). Fragile.
- **Why not chosen:** A `prompt:` config field is cleaner, explicit, and the config file captures the full trial state.

### Alternative 4: Separate AR-specific config format
- **Description:** AR uses its own config format (JSON, TOML, etc.) that gets translated to `loopr.yml`.
- **Pros:** AR format could be flatter or more convenient.
- **Cons:** Translation layer is unnecessary complexity. Two schemas to maintain. AR already writes YAML naturally.
- **Why not chosen:** One config format, one schema, one source of truth.

## Technical Considerations

### Dependencies

- No new crate dependencies
- `LlmConfig` is internal to the config module
- Score output uses `serde_json` (already a dependency)

### Performance

- Prompt loading adds one filesystem stat per configured path (same as current XDG override check)
- `LlmConfig` flatten has zero runtime cost (compile-time serde transformation)
- Scorer runs once at end of E2E, reads JSONL files sequentially

### Security

No security implications. Config files are local. Prompt files are local. Score output is local.

### Testing Strategy

**Unit tests:**
- `LlmConfig` serde round-trip (YAML -> struct -> YAML)
- Flatten collision check: round-trip every config struct that embeds `LlmConfig` through YAML serialize/deserialize and verify all fields survive
- Backward compat: old config without `prompt:` fields deserializes correctly
- Backward compat: `strategy.clarity-gate` still deserializes to top-level `clarity_gate`
- `StrategyConfig` rejects `clarity-gate` key after migration (ghost prevention)
- Prompt path resolution: relative, absolute, XDG override, compiled-in fallback
- Absolute prompt path that does not exist returns `Err`, not silent fallback
- Scorer: known JSONL input -> expected score.json output
- Scorer: empty/missing JSONL -> `score.json` with `composite_score: 0.0`, no panic
- Scorer: zero validation commands -> validation component scores 0.0, no div-by-zero

**Integration tests:**
- Full config load with prompt overrides -> verify PromptStore contains correct content
- E2E run + `loopr score` -> verify score.json is written and parseable

### Rollout Plan

All changes are additive. Default values reproduce current behavior exactly. Existing configs without the new fields work via serde defaults. ClarityGate backward compat handles old configs with `strategy.clarity-gate`.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Serde flatten conflicts between LlmConfig and section-specific fields | Low | Medium | Unit test every struct's round-trip; no field name collisions in LlmConfig |
| ClarityGate promotion breaks existing configs | Medium | Low | Backward compat: accept both `strategy.clarity-gate` and top-level `clarity-gate` |
| Absolute prompt paths enable loading arbitrary files | Low | Low | Local-only; same trust model as XDG overrides |
| Composite score weights don't reflect actual quality | Medium | Medium | Weights are a starting point; AR can be configured to use raw metrics instead |
| PromptStore::init signature change breaks callers | Low | Low | Only called from daemon startup; update one call site |

## Edge Cases

### OnceLock and PromptStore reinitialization

`PromptStore` uses a `OnceLock<PromptStore>` global - once initialized, it cannot be changed. This is correct for AR: each trial is a fresh daemon process with its own `init()` call. There is no need for runtime prompt swapping. If we ever need hot-reloading (e.g., a long-running daemon that reloads config on SIGHUP), OnceLock would need to be replaced with `RwLock<PromptStore>`. That is out of scope for this design.

### Concurrent AR trials

Each AR trial runs its own daemon process against its own working directory. Concurrency between trials is filesystem-level isolation, not in-process. Two trials can run simultaneously with different `loopr.yml` configs and different prompt files as long as they target different working directories (different repo clones or worktrees). The config and prompt files are read at daemon startup and do not need locks.

### Backward compatibility for `strategy.clarity-gate`

Existing configs that set `strategy.clarity-gate` must continue to work. The migration strategy: add `#[serde(alias = "clarity-gate")]` on the top-level `Config.clarity_gate` field, and implement a custom `Deserialize` on `Config` that checks for the old nested location. The `clarity_gate` field must be **fully removed** from `StrategyConfig` - not left as a dead field. Leaving it would violate the No Ghosts rule: dead struct fields accumulate over time, confuse future readers, and create ambiguity about which path is authoritative. A unit test must assert that `StrategyConfig` does not accept a `clarity-gate` key after migration.

### serde(flatten) field collision

When `LlmConfig` is flattened into a parent struct, its fields (`model`, `api_key_env`, `max_tokens`, `temperature`) occupy the same serde namespace as the parent's own fields. The refactor must **remove** these fields from each parent struct as they move into `LlmConfig` - they cannot coexist. For example, `DecomposerConfig` currently has `pub model: String` as a direct field; after the refactor, it has `#[serde(flatten)] pub llm: LlmConfig` and `model` is accessed as `self.llm.model`. A compile-time collision (duplicate field name in the same serde namespace) would cause a runtime deserialization error, not a compile error, so this must be verified with a unit test that round-trips every config struct through YAML serialization/deserialization.

### Score output when E2E crashes

If the E2E run crashes or times out before validation commands execute, the JSONL may contain no validation events. The scorer must handle this: `validation_commands_total = 0` means the validation component scores 0.0 (not a div-by-zero panic, not 1.0). A crashed run with zero validation is worse than a run that ran validation and passed - the score should reflect that. Similarly, if no Work or Bundle records exist (daemon crashed during decomposition), the scorer should produce a valid `score.json` with `composite_score: 0.0` and all zeroed sub-metrics, not crash.

### `provider` field on system-call configs

`DecomposerConfig`, `ValidatorConfig`, `TierGateConfig`, and `EvaluatorConfig` each have a `provider: String` field (always `"anthropic"` today). Agent roles do not. This field is NOT part of `LlmConfig` because agents use the Anthropic client directly while system calls go through a provider abstraction. System-call configs keep `provider` as a section-specific field on top of the flattened `LlmConfig`. If multi-provider support becomes important, `provider` can migrate into `LlmConfig` later.

## Open Questions

- [x] ~~Should `generation-work.pmt` live under `decomposer.prompts`?~~ Resolved: yes, under `decomposer.prompts.generation-work`. It is part of the decomposition flow even though the code path goes through `agents/generation.rs`.
- [ ] Should `interview.pmt` live under `agents.coordinator.prompts` or `chat.prompts`? It is used in the chat interview flow but is driven by the coordinator's interview mode. Leaning toward `chat.prompts.interview` since that's where the other chat-state prompts live.
- [ ] Should the composite score weights be configurable? Leaning toward fixed weights initially, with AR reading raw sub-metrics when it needs finer control. Meta-optimization of the scoring function adds complexity without clear benefit at this stage.

## References

- Andrej Karpathy's AutoResearch: https://github.com/karpathy/autoresearch
- AR overview (Obsidian): `notes/claude-code-karpathys-autoresearch-the-new-meta.md`
- AR agent loops (Obsidian): `notes/autoresearch-agent-loops-and-the-future-of-work.md`
- Current config: `src/config.rs`
- Current prompt loading: `src/prompts.rs`
- E2E infrastructure: `src/tests/integration/`
- Build progression: `docs/design/mvps.md`
