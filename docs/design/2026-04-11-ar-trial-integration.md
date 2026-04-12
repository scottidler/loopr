# Design Document: v4 AR Trial Integration

**Author:** Scott A. Idler
**Date:** 2026-04-11
**Status:** Draft
**Review Passes Completed:** 5/5

## Summary

This document defines how AutoResearch (AR) generates, loads, and scores novel strategy compositions in v4. AR is a Python-based autonomous experiment loop - inspired by Karpathy's autoresearch framework - that writes trial configs, runs Loopr E2E against a target repo, reads the structured score, and iterates. v4 unlocks AR's ability to explore not just numeric parameters and prompt content (v3 capability) but entirely new orchestration strategies: alternative decomposition pipelines, recovery policies, agent collaboration patterns, and scoring weights.

## Problem Statement

### Background

v3's AR config surface (docs/design/2026-04-10-ar-config-surface.md, implemented in v0.1.119-121) exposed LLM parameters, prompt paths, and behavioral knobs as YAML config fields. The scorer (`src/scorer.rs`) produces a composite score (40% completion, 30% first-try acceptance, 20% validation, 10% efficiency). E2E runs against target repos take 10-20 minutes, yielding 30-50 trials per night.

But v3's AR can only tune *values* - it can't change *structure*. It can set `max_work_retries: 5` but can't add "ask a friend after 2 failures." It can switch the decomposer model from Sonnet to Opus but can't try a 3-level decomposition pipeline. The orchestration structure is hardcoded Rust.

v4's YAML strategy layer (Docs 2-6) makes structure tunable. AR can now generate novel strategy YAML files, not just config values.

### Problem

No trial runner exists that:
1. Generates trial configs that override strategy defaults (not just loopr.yml values)
2. Loads named strategy sets at startup (the base strategies + trial-specific overrides)
3. Runs E2E orchestration with the trial's strategy set
4. Produces comparable scores across trials with different strategies
5. Attributes score differences to specific strategy changes

### Goals

- Trial config format: YAML that specifies a base strategy set + per-trial overrides
- Python trial runner (AR loop) that generates configs, runs Loopr, reads scores, iterates
- The scorer (existing Rust) produces structured JSON usable by the Python AR loop
- AR can generate strategy YAML files (new decomposition pipelines, new recovery strategies)
- Trial reproducibility: pin strategy versions via git SHA
- Trial comparison: structured diff between trial configs and scores
- The AR loop follows Karpathy's autoresearch pattern: autonomous, overnight, no human in the loop

### Non-Goals

- Distributed trial execution (one trial at a time, one target repo)
- AR generating Rust code (AR generates YAML only; new primitives require human Rust changes)
- Real-time AR dashboarding (results are TSV/JSON files, analyzed after the run)
- Multi-objective optimization (single composite score, weights are themselves a tunable knob)

## What AR Can Now Explore (v4 vs v3)

| Dimension | v3 (values only) | v4 (values + structure) |
|-----------|------------------|------------------------|
| LLM parameters | temperature, max-tokens, model per role | Same |
| Prompt content | .pmt file paths | Same |
| Retry limits | max_work_retries, max_session_failures | Same, plus new trigger thresholds |
| Decomposition | brief vs full (binary) | Any pipeline: 2-level, 3-level, 4-level, iterative, strict |
| Recovery | fixed retry-then-abandon | Composable: ask-a-friend, escalate, re-decompose, custom chains |
| Agent collaboration | fixed roles | New roles, new tool sets, new iteration limits |
| Scoring weights | fixed 40/30/20/10 | Tunable weights per trial |
| Quality gates | fixed abandon ratio | Tunable thresholds, new gate types |
| Integration policy | fixed stale-reject | Configurable: reject, replan, auto-replay |
| Work queue priority | fixed formula | Configurable priority formula via YAML |

## Proposed Solution

### Overview

The AR system has three layers:

1. **Trial config** (YAML) - declares which strategies to use and what overrides to apply
2. **Trial runner** (Python) - the autonomous loop that generates configs, runs Loopr, reads scores
3. **Scorer** (Rust, existing) - computes structured metrics from TaskStore after each E2E run

### Trial Config Format

A trial config is a YAML file that specifies a base strategy set and per-trial overrides:

```yaml
# trials/trial-042.yml
trial:
  id: trial-042
  description: 3-level decomposition with ask-a-friend recovery
  base: default                            # base strategy set (strategies/ directory)
  target-repo: ~/repos/scottidler/python-api
  git-ref: main                            # target repo branch/tag
  seed: 42                                 # for reproducibility (future use)

overrides:
  # Override which decomposition pipeline to use
  decomposition:
    pipeline: three-level                  # instead of "full"

  # Override a trigger threshold
  triggers:
    work-retry-exhaustion:
      value: 5                             # instead of default 3

  # Add a new strategy (file path relative to trial directory)
  strategies:
    - strategies/recovery/ask-a-friend.yml

  # Override role config
  roles:
    implementer:
      model: claude-opus-4-6               # instead of sonnet
      max-iterations: 30                   # instead of 20

  # Override scoring weights
  scoring:
    completion-weight: 0.50                # instead of 0.40
    quality-weight: 0.20                   # instead of 0.30
    validation-weight: 0.20                # same
    efficiency-weight: 0.10                # same
```

**How overrides work:**

1. Load the base strategy set from `strategies/` (the default strategies from Docs 2-6)
2. Apply overrides: replace decomposition pipeline, modify trigger values, add/replace strategy files, update role configs
3. The merged strategy set is what the engine loads at startup for this trial

### Trial Runner (Python)

The trial runner is a Python script following Karpathy's autoresearch pattern: autonomous, overnight, no human input required after launch.

```
ar/
  runner.py            # main AR loop
  config.py            # trial config generation
  analyzer.py          # score analysis and trial comparison
  program.md           # AR agent instructions (Karpathy-style)
  pyproject.toml       # Python dependencies (uv-managed)
  Dockerfile           # reproducible environment
```

**Dependencies:** Python 3.11+, PyYAML, standard library. No Optuna or heavy ML frameworks - the loop is simple enough that we don't need a hyperparameter framework. The agent (Claude/Codex) IS the optimizer, following the Karpathy autoresearch pattern where the LLM decides what to try next based on prior results.

#### The AR Loop

```python
# ar/runner.py (simplified)

def run_trial(trial_config: TrialConfig) -> Score:
    """Run a single trial: reset state, write config, run loopr e2e, read score."""
    # 0. Reset target repo to pristine state (prevent state leakage between trials)
    reset_target_repo(trial_config.target_repo, trial_config.git_ref)

    # 1. Kill any orphaned daemons from prior trial
    kill_daemon(trial_config.target_repo)

    # 2. Write trial config to loopr's config directory
    write_trial_config(trial_config)

    # 3. Run loopr e2e against the target repo
    result = subprocess.run(
        ["loopr", "run", "--config", trial_config.config_path,
         "--target", trial_config.target_repo],
        capture_output=True, timeout=trial_config.timeout_secs,
    )

    # 4. Explicitly stop daemon after trial completes
    kill_daemon(trial_config.target_repo)

    # 5. Read the structured score
    score_path = trial_config.output_dir / "score.json"
    return Score.from_json(score_path)


def reset_target_repo(repo_path: str, git_ref: str):
    """Reset target repo to pristine state. Prevents state leakage."""
    subprocess.run(["git", "-C", repo_path, "checkout", git_ref], check=True)
    subprocess.run(["git", "-C", repo_path, "reset", "--hard", git_ref], check=True)
    subprocess.run(["git", "-C", repo_path, "clean", "-fdx"], check=True)


def kill_daemon(repo_path: str):
    """Kill any running loopr daemon for this repo."""
    subprocess.run(["loopr", "shutdown"], capture_output=True, timeout=10)


def ar_loop(program: Program, target_repo: str):
    """Autonomous research loop. Runs until interrupted."""
    results = ResultsLog("results.tsv")

    # Establish baseline with default strategies
    baseline = run_trial(TrialConfig.baseline(target_repo))
    results.log(baseline)

    while True:
        # Generate next trial config (the LLM agent does this)
        trial_config = generate_next_trial(results, program)

        # Run the trial
        score = run_trial(trial_config)

        # Log results
        results.log(trial_config, score)

        # Keep or discard (score compared to current best)
        if score.composite > results.best_score:
            results.mark_keep(trial_config)
        else:
            results.mark_discard(trial_config)
```

#### program.md (AR Agent Instructions)

Following Karpathy's pattern, the AR agent's behavior is controlled by a `program.md` file:

```markdown
# Loopr AutoResearch Program

## Goal
Find the strategy configuration that maximizes the composite score on the target repo.

## What You Can Modify
- Trial config YAML files (strategies, triggers, roles, scoring weights)
- Strategy YAML files (new recovery strategies, decomposition pipelines)
- Prompt .pmt files (agent instructions)

## What You Cannot Modify
- Loopr source code (Rust)
- The scorer (fixed evaluation metric)
- The target repo
- prepare.py equivalent (fixed test harness)

## The Loop
1. Read results.tsv to understand what's been tried
2. Analyze patterns: which changes improved scores? which degraded?
3. Generate a trial config with a clear hypothesis
4. Run the trial: `python ar/runner.py --trial trials/trial-NNN.yml`
5. Record the result
6. If improved: keep. If not: discard
7. NEVER STOP. Continue until interrupted.

## Scoring
The composite score is [0.0, 1.0]:
- 40% completion rate (works done / total)
- 30% first-try acceptance rate (bundles accepted without revision)
- 20% validation pass rate (test/lint commands passing)
- 10% efficiency (1 - failed sessions / total sessions)

## Strategy Dimensions to Explore
(ordered by expected impact, highest first)

1. **Prompt content** - rewrite .pmt files for each agent role
2. **Decomposition pipeline** - try 3-level, brief, strict validation
3. **Recovery strategies** - add ask-a-friend, change retry limits
4. **Model selection** - swap roles between opus/sonnet/haiku
5. **Agent collaboration** - new tool sets, iteration limits
6. **Trigger thresholds** - adjust safety net values

## What You CANNOT Modify
- Scoring weights (fixed for this experiment series - reward hacking prevention)
- Loopr source code (Rust)
- The scorer (fixed evaluation metric)
- The target repo (reset to pristine state before each trial)
```

### Results Tracking

Following Karpathy's pattern, results are logged to a TSV:

```
trial_id	composite	completion	quality	validation	efficiency	status	description
baseline	0.620	0.700	0.500	0.800	1.000	keep	default strategies
trial-001	0.645	0.750	0.520	0.800	0.950	keep	opus decomposer
trial-002	0.610	0.680	0.500	0.800	0.900	discard	3-level pipeline (regression)
trial-003	0.680	0.800	0.550	0.850	0.950	keep	opus decomposer + strict validation
```

### Scorer Integration

The existing Rust scorer (`src/scorer.rs`) already produces the right output. The AR loop reads `score.json`:

```json
{
  "version": 1,
  "duration_secs": 720,
  "completion": {
    "works_total": 10,
    "works_done": 7,
    "works_abandoned": 2,
    "works_blocked": 1,
    "completion_rate": 0.70
  },
  "quality": {
    "first_try_acceptance_rate": 0.50
  },
  "efficiency": {
    "sessions_failed": 2,
    "total_sessions": 20
  },
  "validation": {
    "validation_commands_passed": 4,
    "validation_commands_total": 5
  },
  "composite_score": 0.62
}
```

**v4 enhancement:** Scoring weights are configurable per experiment SERIES, not per trial:

```yaml
# strategies/scoring/default.yml
default:
  completion-weight: 0.40
  quality-weight: 0.30
  validation-weight: 0.20
  efficiency-weight: 0.10
```

**Critical constraint (reward hacking prevention):** Scoring weights are fixed for an entire AR experiment series. The AR agent CANNOT modify scoring weights within a series - it would trivially "reward hack" by setting `completion-weight: 1.0` and ignoring quality. A human chooses the weights when launching a series. The weights file is outside the AR agent's writable scope. Different series can use different weights to explore tradeoffs (e.g., "optimize for quality" vs "optimize for speed"), but within a series, the fitness function is immutable.

### Trial Reproducibility

Reproducibility comes from pinning:

1. **Loopr version** - git SHA of the loopr-v4 repo (strategies are committed)
2. **Trial config** - the YAML file is saved per trial
3. **Target repo ref** - git SHA or tag of the target repo
4. **Custom strategy files** - any trial-specific YAML saved alongside the trial config

The trial runner records all of these in the results log:

```
trial_id	loopr_sha	target_sha	composite	status	description
trial-042	a1b2c3d	d4e5f6g	0.680	keep	3-level + ask-a-friend
```

### Trial Comparison

The analyzer script compares trials:

```python
# ar/analyzer.py
def compare_trials(trial_a: str, trial_b: str) -> Comparison:
    """Structured diff between two trials."""
    config_diff = yaml_diff(trial_a.config, trial_b.config)
    score_diff = score_delta(trial_a.score, trial_b.score)
    return Comparison(config_diff, score_diff)
```

Output:
```
trial-001 vs trial-003:
  config changes:
    + strategies/recovery/ask-a-friend.yml
    ~ validation.blocking: false -> true
  score delta:
    composite: +0.035 (0.645 -> 0.680)
    completion: +0.050
    quality: +0.030
    validation: +0.050
    efficiency: +0.000
```

### Docker Environment

For reproducible AR runs, a Dockerfile packages the Python AR loop:

```dockerfile
FROM python:3.11-slim

# Install uv for Python package management
RUN curl -LsSf https://astral.sh/uv/install.sh | sh

# Install loopr binary (pre-built or cargo install)
COPY --from=loopr-builder /usr/local/bin/loopr /usr/local/bin/loopr

# Install AR dependencies
WORKDIR /app
COPY ar/ /app/ar/
COPY pyproject.toml /app/
RUN uv sync

# Mount points for strategy files and target repos
VOLUME /strategies
VOLUME /target

ENTRYPOINT ["uv", "run", "python", "ar/runner.py"]
```

Alternatively, for local development using uv:

```bash
cd ar/
uv sync
uv run python ar/runner.py
```

### Implementation Plan

#### Phase 1: Trial Config Schema

1. Define `TrialConfig` YAML schema with base, overrides, target-repo, git-ref
2. Implement config merging: base strategies + trial overrides -> merged strategy set
3. Wire merged config into Loopr's startup (engine loads from merged directory)
4. Unit tests: config parsing, override merging, invalid config rejection

#### Phase 2: Python Trial Runner

1. Create `ar/` directory with runner.py, config.py, analyzer.py
2. Implement the AR loop: write config, run subprocess, read score, log results
3. Implement results.tsv logging and best-score tracking
4. Create program.md with AR agent instructions
5. Create pyproject.toml (uv-managed, PyYAML only) and Dockerfile
6. Integration test: run baseline trial against a test target repo

#### Phase 3: Configurable Scorer (per-series, not per-trial)

1. Move scoring weights from hardcoded constants to `strategies/scoring/default.yml`
2. Weights are fixed per experiment series (set by human at series launch)
3. AR agent cannot modify weights within a series (reward hacking prevention)
4. Unit tests: verify different weights produce different composite scores

#### Phase 4: Strategy Generation

1. Document how AR generates strategy YAML (examples in program.md)
2. Validate generated strategies at startup (existing startup validation from Docs 3-5)
3. AR can generate: new decomposition pipelines, new recovery strategies, modified trigger thresholds, role configs
4. Integration test: AR generates a novel strategy, Loopr loads and runs it

## Alternatives Considered

### Alternative 1: Optuna-based trial runner

- **Description:** Use Optuna (Python hyperparameter optimization framework) to manage the trial loop with Bayesian optimization for parameter selection.
- **Pros:** Principled parameter search (TPE sampler). Built-in pruning for bad trials. Dashboard for visualization. Well-tested on millions of HPO runs.
- **Cons:** Optuna optimizes numeric parameters well but struggles with structural changes (which decomposition pipeline? which recovery strategy?). The "suggest_categorical" API can handle pipeline selection but can't generate novel strategy YAML. The LLM-as-optimizer pattern (Karpathy's approach) is better for structural exploration.
- **Why not chosen:** AR's highest-leverage moves are structural (prompt rewrites, pipeline changes), not numeric. Optuna is great for sweeping temperature/max-tokens but the LLM agent is better at reasoning about "what if we tried ask-a-friend?" The Karpathy pattern is simpler and more aligned with structural exploration. Could revisit Optuna for the numeric-sweep layer inside a Karpathy-style outer loop.

### Alternative 2: Rust-native trial runner

- **Description:** Implement the AR loop in Rust as part of the Loopr binary.
- **Pros:** Single language. No Python dependency. Tighter integration with Loopr internals.
- **Cons:** The AR loop is I/O-bound (wait for E2E run, read score, write config). Python is perfectly adequate. The LLM agent that drives AR works with Python (Karpathy's autoresearch is Python). Rust adds compile overhead for what is essentially a scripting task.
- **Why not chosen:** Python is the right tool for the trial runner. The scorer stays in Rust (it reads TaskStore). The boundary is clean: Rust produces the score, Python consumes it.

### Alternative 3: Multi-objective optimization (Pareto front)

- **Description:** Instead of a single composite score, optimize multiple objectives independently (completion, quality, validation, efficiency) and track the Pareto front.
- **Pros:** Avoids the arbitrary weight problem. Reveals tradeoffs between objectives.
- **Cons:** More complex to implement and reason about. AR needs a single "is this better?" signal to decide keep/discard. Pareto dominance is less intuitive for an LLM agent than a scalar score. The weights themselves can be tuned as a knob.
- **Why not chosen:** Single composite score with tunable weights is simpler. If AR finds that the weights are wrong, it can change them. Pareto optimization can be added later if needed.

## Technical Considerations

### Dependencies

- **Python:** PyYAML, subprocess, json (all stdlib except PyYAML)
- **Rust:** Existing scorer, existing CLI (`loopr run`, `loopr score`)
- **External:** An LLM API key (for the AR agent that generates trial configs)
- **Docker (optional):** For reproducible AR environments

### Performance

- Each E2E trial takes 10-20 minutes depending on target repo complexity
- At 12 experiments/hour (Karpathy's rate with 5-min runs), a night yields 30-50 trials with Loopr's longer runs
- The AR loop overhead (config generation, score reading, analysis) is negligible compared to the E2E run time
- No parallelism needed - trials run sequentially on one target repo

### Security

- AR generates YAML config files, not executable code
- Generated strategies are validated at Loopr startup (Docs 3-5 validation)
- The AR agent needs an LLM API key but does not access production systems
- Target repos are local clones, not remote

### Testing Strategy

- **Trial config tests:** Parse valid/invalid trial configs, verify override merging
- **Runner tests:** Mock subprocess to simulate Loopr E2E, verify score reading and result logging
- **Scorer tests:** Verify configurable weights produce correct composite scores
- **End-to-end:** Run AR loop for 3-5 trials against a test target, verify results.tsv is populated correctly
- **Strategy generation tests:** AR generates a novel strategy, Loopr loads it, engine validates it

### Rollout Plan

- Phase 1 (config schema) depends on the composition engine (Doc 5) being implemented
- Phase 2 (Python runner) is independent - can start now as a standalone script
- Phase 3 (configurable scorer) is a small Rust change to the existing scorer
- Phase 4 (strategy generation) requires all of Docs 2-6 to be implemented
- The AR loop is usable as soon as Phase 2 is done, even with v3-style config-only trials

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| E2E runs are too slow for meaningful AR exploration | Medium | High | 10-20 min/trial with 30-50 trials/night is workable. Optimize target repo selection (small repos for fast iteration). Use brief decomposition pipeline for faster trials. |
| AR generates invalid strategy YAML | Medium | Medium | Startup validation catches this before any work starts. AR treats validation failures as "crash" - discard and try again. Same pattern as Karpathy's error-retry loop. |
| Composite score doesn't capture what matters | Medium | Medium | Weights are tunable. AR can discover better weights. Add new metric dimensions if needed (e.g., time-to-first-done, code quality score). |
| Trial reproducibility breaks due to non-deterministic LLM outputs | High | Low | LLM non-determinism means exact reproduction is impossible. Pin what you can (config, git SHAs). Compare trials by relative improvement, not absolute scores. |
| Python/Rust boundary is fragile | Low | Medium | The boundary is clean: Rust writes score.json, Python reads it. Subprocess invocation of `loopr run` is the only integration point. |

## Resolved Questions

- [x] **Optuna or Karpathy pattern?** Karpathy pattern. The LLM-as-optimizer is better for structural exploration. Optuna could be a future layer for numeric sweeps within the Karpathy outer loop.
- [x] **Python or Rust for the trial runner?** Python. The loop is I/O-bound scripting. Rust adds compile overhead for no benefit.
- [x] **Single score or Pareto front?** Single composite score with tunable weights. Simpler for the LLM agent to reason about.
- [x] **Reward hacking via mutable weights?** Scoring weights are fixed per experiment series (set by human). AR agent cannot modify weights. Prevents trivial reward hacking.
- [x] **State leakage between trials?** Target repo reset to pristine (`git reset --hard` + `git clean -fdx`) before each trial. Daemon explicitly killed between trials.
- [x] **Flakiness handling?** Accept noise for v1. LLM non-determinism means exact reproduction is impossible. The Karpathy pattern relies on large trial counts (30-50/night) to surface real patterns above the noise floor. Future enhancement: run N=3 per config and average for high-stakes comparisons.

## Open Questions

- [x] **A/B comparison or sequential?** Sequential trial-and-compare. A/B requires concurrent runs which cause resource contention (CPU, memory, network) that pollutes efficiency and timing metrics. Sequential execution guarantees isolated, pristine environments. The Karpathy pattern is inherently sequential.
- [x] **Strategy diffs or full file overrides?** Full file overrides. Diffs (JSON Patch, YAML merge keys) are brittle - if the base strategy changes, diffs silently mis-apply. Full overrides are structurally complete and independently validatable by the startup schema parser. The AR agent generates complete strategy files, not patches.
- [x] **Per-run API cost budget?** Yes. The engine must enforce a hard API call / token limit per AR run. A malformed strategy loop (e.g., fail -> re-decompose -> fail with a bad guard) will spin rapidly, burning hundreds of dollars overnight before the subprocess timeout catches it. The budget is a config field in the trial config, defaulting to a sane ceiling (e.g., 10,000 API calls or $50 per trial).

## References

- `docs/design/2026-04-10-ar-config-surface.md` - v3 AR config surface (implemented)
- `docs/v4-vision.md` - v4 architecture vision (Doc 7 section)
- `docs/design/2026-04-11-primitive-vocabulary.md` - primitive catalog (Doc 2)
- `docs/design/2026-04-11-strategy-composition.md` - composition engine (Doc 5)
- `docs/design/2026-04-11-decomposer-as-strategy.md` - decomposition pipelines (Doc 6)
- `src/scorer.rs` - existing Rust scorer
- [karpathy/autoresearch](https://github.com/karpathy/autoresearch) - Karpathy's autoresearch framework (630 lines, program.md pattern)
- [Auto Researching, not hyperparameter tuning (arXiv:2603.15916)](https://arxiv.org/abs/2603.15916) - 10,000 LLM-guided experiments, architectural choices explain 94% of variance
- [AgentHPO (arXiv:2402.01881)](https://arxiv.org/abs/2402.01881) - LLM agent for hyperparameter optimization
- [A Vision for Auto Research with LLM Agents (arXiv:2504.18765)](https://arxiv.org/abs/2504.18765) - multi-agent research framework
