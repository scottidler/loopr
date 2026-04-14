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

Following Karpathy's autoresearch pattern exactly, the AR system has three layers - mirroring his prepare.py / train.py / program.md architecture:

1. **Fixed infrastructure** (`bin/e2e`, `loopr score`, `bin/ar-reset`) - the evaluation harness the agent cannot modify. This is our `prepare.py` equivalent.
2. **Strategies** (`strategies/*.yml`, prompt `.pmt` files) - the files the agent modifies. This is our `train.py` equivalent.
3. **program.md** - the human-authored instructions that define how the AR agent operates. The human iterates on this between experiment series to improve research velocity.

The LLM agent (Claude Code) IS the trial runner. There is no separate Python orchestration layer for v1 - the agent reads program.md, decides what to modify, runs `bin/e2e`, reads the score, and loops. This is faithful to Karpathy's pattern where the agent IS the researcher.

A v2 automated runner (overnight cron without an interactive session) is a future concern documented in the Rollout Plan.

**Scorer** (Rust, existing in `src/scorer.rs`) - computes structured metrics from TaskStore after each E2E run. Already wired as `loopr score` CLI subcommand.

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

### Infrastructure Scripts

The AR infrastructure is bash scripts in `bin/`, matching the project's existing `bin/e2e` pattern:

```
bin/
  e2e                  # existing E2E runner (the core trial executor)
  ar-reset             # reset target repo to pristine state between trials
  ar-score             # wrapper: run loopr score, format output, append to results.tsv
```

`bin/ar-reset` resets state between trials:
```bash
#!/usr/bin/env bash
# bin/ar-reset - reset target repo to pristine state for next trial
set -euo pipefail
REPO="${1:?usage: ar-reset <repo-path> [git-ref]}"
REF="${2:-main}"
git -C "$REPO" checkout "$REF"
git -C "$REPO" reset --hard "$REF"
git -C "$REPO" clean -fdx
```

`bin/ar-score` reads and logs the structured score:
```bash
#!/usr/bin/env bash
# bin/ar-score - score a completed trial and append to results.tsv
set -euo pipefail
TARGET="${1:?usage: ar-score <target-dir>}"
RESULTS="${2:-results.tsv}"
DESCRIPTION="${3:-no description}"
STATUS="${4:-keep}"

# Run the scorer
SCORE_JSON=$(loopr --config "${TARGET}/loopr.yml" score --json 2>/dev/null)
COMPOSITE=$(echo "$SCORE_JSON" | jq -r '.composite_score')
COMPLETION=$(echo "$SCORE_JSON" | jq -r '.completion.completion_rate')
QUALITY=$(echo "$SCORE_JSON" | jq -r '.quality.first_try_acceptance_rate')

# Log to TSV
COMMIT=$(git -C "$TARGET" rev-parse --short HEAD 2>/dev/null || echo "none")
printf '%s\t%s\t%s\t%s\t%s\t%s\n' \
    "$COMMIT" "$COMPOSITE" "$COMPLETION" "$QUALITY" "$STATUS" "$DESCRIPTION" \
    >> "$RESULTS"
```

No Python. No Docker. No venv. The LLM agent IS the trial runner.

### The AR Loop (Agent-Driven)

Following Karpathy's pattern exactly, the LLM agent (Claude Code) is the researcher. There is no separate orchestration script. The agent reads `program.md` and drives the loop using shell commands:

1. Read `results.tsv` and `learnings.md` for context
2. Decide what strategy change to try (one change per trial)
3. Edit the relevant strategy YAML or prompt file
4. `git commit` the change
5. `bin/ar-reset <target-repo>` to ensure pristine state
6. `bin/e2e <target> --timeout <budget>` to run the trial
7. `bin/ar-score <target-dir> results.tsv "<description>" <keep|discard|crash>`
8. If improved: keep the commit, append insight to `learnings.md`
9. If worse: `git reset --hard HEAD~1`, log as discard, append insight to `learnings.md`
10. If crashed: log as crash, fix or skip, append insight to `learnings.md`
11. GOTO 1

### program.md (AR Agent Instructions)

Following Karpathy's pattern, the AR agent's behavior is controlled by a `program.md` file. **This is THE human-iterable artifact** - the human improves research velocity by refining program.md between experiment series, not by writing trial configs.

```markdown
# Loopr AutoResearch Program

## Setup

To set up a new experiment series:

1. Agree on a series tag (e.g. `apr13-python-api`). The branch `ar/<tag>` must not exist.
2. Create the branch: `git checkout -b ar/<tag>` from current v4.
3. Read the in-scope files for full context:
   - This `program.md` - your operating instructions
   - `strategies/` directory - the files you modify
   - `prompts/` directory - prompt .pmt files you can modify
   - `bin/e2e-targets/<target>.sh` - the fixed evaluation target
4. Initialize `results.tsv` with the header row.
5. Initialize `learnings.md` with a header.
6. Run the baseline: `bin/e2e <target>` with default strategies. Log to results.tsv.

## What You CAN Modify
- Strategy YAML files in `strategies/` (decomposition pipelines, recovery, triggers, roles)
- Prompt .pmt files in `prompts/` (agent instructions for each role)
- New strategy YAML files (novel recovery strategies, collaboration patterns)

## What You CANNOT Modify
- Loopr source code (Rust) - this is the fixed infrastructure
- The scorer (`src/scorer.rs`) - the evaluation metric is immutable
- The target repo - reset to pristine state before each trial
- Scoring weights (fixed for this experiment series - reward hacking prevention)
- `bin/e2e`, `bin/ar-reset`, `bin/ar-score` - fixed evaluation harness

## The Experiment Loop

LOOP FOREVER:

1. Read `results.tsv` and `learnings.md` for context on what has been tried
2. Form a hypothesis: what ONE change do you expect to improve the score, and why?
3. Make the change - modify exactly ONE strategy file or ONE prompt file per trial
4. `git commit` the change with a descriptive message
5. `bin/ar-reset <target-repo>` to reset target state
6. `bin/e2e <target> --timeout <budget> > run.log 2>&1`
7. Score the run: `bin/ar-score <target-dir> results.tsv "<description>"`
8. If composite improved: KEEP (advance the branch)
9. If composite equal or worse: DISCARD (`git reset --hard HEAD~1`)
10. If the run crashed (daemon hung, invalid YAML, OOM): mark CRASH, fix or skip
11. Append what you learned to `learnings.md` - what worked, what didn't, why
12. GOTO 1

**One change per trial.** Multi-variable changes make it impossible to attribute score differences. If you want to try A+B together, first try A alone, then B alone, then A+B. Attribution matters.

**NEVER STOP.** Once the loop begins, do NOT pause to ask the human if you should continue. The human might be asleep and expects you to work indefinitely until manually stopped. If you run out of ideas, re-read learnings.md, try combining previous near-misses, try more radical changes.

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

## Crash Handling

If a run crashes (daemon hung, invalid strategy YAML, timeout exceeded):
- Quick fix (typo, missing field): fix and re-run the same trial
- Fundamentally broken idea: log as `crash`, revert, move on
- Persistent infrastructure failure: stop and alert the human
```

### learnings.md (Compound Learning)

Each trial is expensive (10-20 minutes). With 30-50 trials per overnight run, every trial matters. The agent maintains a `learnings.md` file that accumulates insights:

```markdown
# AR Learnings - apr13-python-api series

## trial-001 (baseline)
composite: 0.620. Default strategies. This is our floor.

## trial-002 (opus decomposer) - KEEP
composite: 0.645 (+0.025). Switching decomposer from sonnet to opus produced better
work item granularity - fewer oversized tasks, more first-try acceptances.

## trial-003 (3-level pipeline) - DISCARD
composite: 0.610 (-0.010). Three-level decomposition (Plan->Spec->Phase->Work) added
overhead without improving coverage. The extra Spec layer produced redundant contracts
that the implementer ignored anyway. Stick with 2-level for this target size.

## trial-004 (opus decomposer + strict validation) - KEEP
composite: 0.680 (+0.035). Building on trial-002's decomposer win. Adding blocking
validation caught real bugs early. The combination is synergistic.
```

This file serves two purposes:
1. The agent reads it before each trial to avoid repeating failed experiments
2. The human reads it after the series to understand what the agent discovered

### Results Tracking

Following Karpathy's pattern, results are logged to a TSV. The TSV is **untracked by git** - it is a running log, not a committed artifact. The git branch history IS the experiment history (keep = commit stays, discard = commit reverted).

Three statuses:
- **keep** - score improved, commit stays on branch
- **discard** - score equal or worse, commit reverted (`git reset --hard HEAD~1`)
- **crash** - run failed (invalid YAML, daemon hung, OOM). Different from discard: crash means "fix the config," discard means "idea didn't work"

```
commit	composite	completion	quality	status	description
a1b2c3d	0.620	0.700	0.500	keep	baseline - default strategies
b2c3d4e	0.645	0.750	0.520	keep	opus decomposer
c3d4e5f	0.610	0.680	0.500	discard	3-level pipeline (regression)
d4e5f6g	0.000	0.000	0.000	crash	malformed recovery strategy YAML (parse error)
e5f6g7h	0.680	0.800	0.550	keep	opus decomposer + strict validation
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

Each experiment series runs on a dedicated branch (e.g. `ar/apr13-python-api`). Reproducibility comes from git:

1. **Loopr version** - the branch point from `v4` pins the Loopr code version
2. **Strategy state per trial** - each kept trial is a commit on the AR branch. The commit diff IS the trial config.
3. **Target repo ref** - pinned in the series setup (program.md documents which target and ref)
4. **results.tsv** - untracked running log with commit hashes linking to the branch history

To reproduce a specific trial: `git checkout <commit>` on the AR branch, run `bin/e2e <target>`.

To compare the cumulative strategy changes: `git diff v4..ar/<tag> -- strategies/ prompts/`

### Trial Comparison

Since each trial is a git commit (kept or reverted), comparing trials is just `git diff`:

```bash
# Compare the strategy changes between two kept trials
git diff a1b2c3d..e5f6g7h -- strategies/ prompts/

# See the full history of kept experiments
git log --oneline ar/apr13-python-api
```

The agent reads `results.tsv` and `learnings.md` for structured comparison. No separate analyzer tool needed for v1.

### Time Budget

Karpathy's autoresearch uses a fixed 5-minute time budget per experiment because GPU training is deterministic and compute-bound. Loopr E2E is LLM-API-bound with high variance - the same strategy on the same target can take 8 minutes or 18 minutes depending on LLM responses.

**A global fixed time budget does not work here.** Instead, the time budget is fixed **per target, per series**:

| Target | Timeout | Trials/night |
|--------|---------|-------------|
| rust-version | 600s | ~48 |
| python-todo, lua-todo | 900s | ~32 |
| python-api, node-api, rust-cli, react-todo | 1200s | ~24 |

Within a series, all trials run against the **same target** with the **same timeout** (inherited from `bin/e2e-targets/<target>.sh`). Comparability comes from same-target-same-timeout, not from a global fixed budget.

The exit code from `bin/e2e` is itself a signal: 0 = GoalComplete (finished before timeout), 1 = Timeout (ran out of time). A strategy that completes in 8 minutes vs one that times out at 20 minutes is a clear signal captured in the efficiency dimension.

### Implementation Plan

#### Phase 1: Infrastructure Scripts and program.md

1. Create `bin/ar-reset` - target repo reset (bash, ~10 lines)
2. Create `bin/ar-score` - score extraction and TSV logging (bash, ~20 lines)
3. Write `program.md` with AR agent instructions (the human-iterable artifact)
4. Create `learnings.md` template
5. Create `results.tsv` header template
6. Test: manually run one baseline trial using the scripts

#### Phase 2: Trial Config Schema

1. Define trial config YAML schema with base, overrides, target-repo, git-ref
2. Implement config merging: base strategies + trial overrides -> merged strategy set
3. Wire merged config into Loopr's startup (engine loads from merged directory)
4. Unit tests: config parsing, override merging, invalid config rejection

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

#### Future: v2 Automated Runner

When overnight runs without an interactive Claude Code session are needed, a thin API wrapper will be required. This is the only point where the Python packaging question arises. Deferred until v1 proves the pattern works interactively.

## Alternatives Considered

### Alternative 1: Optuna-based trial runner

- **Description:** Use Optuna (Python hyperparameter optimization framework) to manage the trial loop with Bayesian optimization for parameter selection.
- **Pros:** Principled parameter search (TPE sampler). Built-in pruning for bad trials. Dashboard for visualization. Well-tested on millions of HPO runs.
- **Cons:** Optuna optimizes numeric parameters well but struggles with structural changes (which decomposition pipeline? which recovery strategy?). The "suggest_categorical" API can handle pipeline selection but can't generate novel strategy YAML. The LLM-as-optimizer pattern (Karpathy's approach) is better for structural exploration.
- **Why not chosen:** AR's highest-leverage moves are structural (prompt rewrites, pipeline changes), not numeric. Optuna is great for sweeping temperature/max-tokens but the LLM agent is better at reasoning about "what if we tried ask-a-friend?" The Karpathy pattern is simpler and more aligned with structural exploration. Could revisit Optuna for the numeric-sweep layer inside a Karpathy-style outer loop.

### Alternative 2: Python trial runner (Doc 7 v1 proposal)

- **Description:** A Python `ar/` directory with runner.py, config.py, analyzer.py, pyproject.toml, Dockerfile.
- **Pros:** Structured code. Could run as a cron job without an interactive LLM session.
- **Cons:** Introduces Python packaging (uv/venv/Docker) into a Rust project. PyYAML is the only real dependency. The Karpathy pattern doesn't use a separate runner - the LLM agent IS the runner. A Python orchestration layer is premature for v1.
- **Why not chosen:** The LLM agent running in Claude Code is the trial runner for v1. Infrastructure is bash scripts in `bin/`. A Python runner is a v2 concern for automated overnight runs without an interactive session.

### Alternative 3: Rust-native trial runner

- **Description:** Implement the AR loop in Rust as part of the Loopr binary.
- **Pros:** Single language. No Python dependency. Tighter integration with Loopr internals.
- **Cons:** The AR loop is I/O-bound (wait for E2E run, read score, write config). The LLM agent that drives AR doesn't need Rust. Rust adds compile overhead for what is essentially a scripting task.
- **Why not chosen:** Bash scripts + the LLM agent are sufficient. The scorer stays in Rust (it reads TaskStore). The boundary is clean: Rust produces the score, the agent consumes it.

### Alternative 3: Multi-objective optimization (Pareto front)

- **Description:** Instead of a single composite score, optimize multiple objectives independently (completion, quality, validation, efficiency) and track the Pareto front.
- **Pros:** Avoids the arbitrary weight problem. Reveals tradeoffs between objectives.
- **Cons:** More complex to implement and reason about. AR needs a single "is this better?" signal to decide keep/discard. Pareto dominance is less intuitive for an LLM agent than a scalar score. The weights themselves can be tuned as a knob.
- **Why not chosen:** Single composite score with tunable weights is simpler. If AR finds that the weights are wrong, it can change them. Pareto optimization can be added later if needed.

## Technical Considerations

### Dependencies

- **Bash:** `bin/ar-reset`, `bin/ar-score` (jq for JSON parsing)
- **Rust:** Existing scorer (`src/scorer.rs`), existing CLI (`loopr run`, `loopr score`)
- **External:** An LLM API key (for the AR agent session in Claude Code)
- **No Python, no Docker, no venv for v1**

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

- **Infrastructure script tests:** Run `bin/ar-reset` and `bin/ar-score` against a test target, verify TSV output
- **Trial config tests:** Parse valid/invalid trial configs, verify override merging
- **Scorer tests:** Verify configurable weights produce correct composite scores
- **End-to-end:** Run one manual AR cycle (baseline + 2 trials) against `rust-version` target, verify results.tsv and learnings.md
- **Strategy generation tests:** Agent generates a novel strategy YAML, Loopr loads it, engine validates it

### Rollout Plan

- Phase 1 (scripts + program.md) is independent - can start immediately, no Rust changes
- Phase 2 (config schema) depends on the composition engine (Docs 2-6, implemented)
- Phase 3 (configurable scorer) is a small Rust change to the existing scorer
- Phase 4 (strategy generation) requires all of Docs 2-6 (done) and a working E2E pipeline
- **The AR loop is usable as soon as Phase 1 is done** - the agent can manually edit strategy files and run `bin/e2e` without the config merging layer. Phase 2 adds structured override management but isn't required for first experiments.
- Future v2 (automated overnight runner) deferred until v1 proves the interactive pattern works

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| E2E runs are too slow for meaningful AR exploration | Medium | High | 10-20 min/trial with 30-50 trials/night is workable. Optimize target repo selection (small repos for fast iteration). Use brief decomposition pipeline for faster trials. |
| AR generates invalid strategy YAML | Medium | Medium | Startup validation catches this before any work starts. AR treats validation failures as "crash" - discard and try again. Same pattern as Karpathy's error-retry loop. |
| Composite score doesn't capture what matters | Medium | Medium | Weights are tunable. AR can discover better weights. Add new metric dimensions if needed (e.g., time-to-first-done, code quality score). |
| Trial reproducibility breaks due to non-deterministic LLM outputs | High | Low | LLM non-determinism means exact reproduction is impossible. Pin what you can (config, git SHAs). Compare trials by relative improvement, not absolute scores. |
| LLM agent context limits during long AR runs | Medium | Medium | Claude Code sessions have context limits. Long runs (50+ trials) may hit them. Mitigation: learnings.md and results.tsv are external memory the agent re-reads each iteration, so context compression doesn't lose experiment history. |

## Resolved Questions

- [x] **Optuna or Karpathy pattern?** Karpathy pattern. The LLM-as-optimizer is better for structural exploration. Optuna could be a future layer for numeric sweeps within the Karpathy outer loop.
- [x] **Python or Rust for the trial runner?** Neither. The LLM agent IS the trial runner (Karpathy pattern). Infrastructure is bash scripts in `bin/`. Python runner is a v2 concern for unattended overnight runs.
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
