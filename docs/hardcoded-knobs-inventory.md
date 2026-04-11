# Loopr Hardcoded Knobs Inventory

Reference document cataloging every hardcoded behavioral parameter in Loopr v3 (as of v0.1.121).
This inventory motivates the v4 YAML-composable orchestration architecture.

## Safety Nets (circuit breakers)

| Knob | Value | File | Granularity | What It Guards |
|------|-------|------|-------------|----------------|
| MAX_WORK_ATTEMPTS | 3 | handlers/work.rs | meso | Work reset cycles before abandon |
| max_session_failures | 3 | config.rs | meso | Consecutive agent crashes before blocking |
| max_decomposition_attempts | 3 | config.rs | macro | Decomposer retries per parent doc |
| MAX_CONSECUTIVE_FAILURES | 3 | agentic_loop.rs | micro | Tool call failures before loop exits |
| MAX_OUTPUT_RECOVERY_ATTEMPTS | 3 | agentic_loop.rs | micro | Output parse recovery retries |
| max_requeries | 3 | config.rs (all roles) | micro | Self-correction retries on parse/tool errors |
| max_validation_attempts | 3 | config.rs | macro | Coordinator validation retries |
| max_work_retries | 3 | config.rs | macro | Coordinator work retry limit |
| max_restarts | 5 | supervisor.rs | macro | Coordinator process restarts |
| max_researcher_spawns | 3 | config.rs | meso | Concurrent researcher agents per scope |
| max_abandon_ratio | 0.4 | config.rs | macro | 40% abandoned works triggers need_help |
| max_rebase_lag | 5 | config.rs | meso | Consecutive rebase failures before abandon |
| action_threshold | 5 | lifeguard | micro | Identical actions before intervention |
| error_threshold | 3/10 | lifeguard | micro | Errors in sliding window |

## Timeouts (wall clock limits)

| Knob | Value | What It Guards |
|------|-------|----------------|
| Implementer session | 1800s (30m) | Single implementation session |
| Reviewer session | 600s (10m) | Single review session |
| Researcher session | 600s (10m) | Single research session |
| Integrator session | 1200s (20m) | Single integration session |
| Work SLA wall clock | 1800s (30m) | Total time per work item |
| Phase timeout | 3600s (1h) | Total time per phase |
| Goal timeout | 14400s (4h) | Total time per plan |
| LLM call timeout | 180s (3m) | Single decomposer LLM call |
| HTTP timeout (clarity) | 10s | Clarity gate API call |
| Validator HTTP | 120s | Doc validator call |
| Tool test timeout | 300s | Tool test execution |
| Tool lint timeout | 120s | Tool lint execution |
| Tool format timeout | 30s | Tool format execution |
| Search subprocess | 30s | Researcher search |

## Capacity Limits (concurrency and size)

| Knob | Value | What It Controls |
|------|-------|-----------------|
| Implementer max_iterations | 20 | Tool calls per session |
| Reviewer max_iterations | 5 | Tool calls per session |
| Researcher max_iterations | 10 | Tool calls per session |
| Implementer max_pool | unlimited | Concurrent implementers |
| Researcher max_pool | 4 | Concurrent researchers |
| Coordinator max_pool | 1 | Always singleton |
| Total active agents cap | 20 | Hard ceiling on parallelism |
| MAX_TOOL_RESULT_CHARS | 32,768 | Truncation per tool output |
| COMPACTION_THRESHOLD | 150,000 | Token budget before compaction |
| MAX_INPUT_TOKENS | 190,000 | Hard context overflow |
| Max research results | 200 | Researcher result cap |
| PROTECTED_TAIL_MESSAGES | 6 | Messages shielded from compaction |
| Bundle max_files_touched | 8 | Bundle size limit |
| Bundle max_loc_changed | 300 | Bundle size limit |

## LLM Parameters (model selection and tuning)

| Knob | Value | Role |
|------|-------|------|
| Coordinator | opus, 8192 tokens, temp 0.2 | Strategic, deterministic |
| Implementer | sonnet, 8192 tokens, temp 0.3 | Creative but stable |
| Reviewer | sonnet, 4096 tokens, temp 0.1 | Analytical |
| Researcher | sonnet, 4096 tokens, temp 0.1 | Analytical |
| Decomposer | sonnet, 4096 tokens, temp 0.3 | Creative structure |
| Tier gate | haiku, 16 tokens, temp 0.0 | Binary classifier |
| Clarity gate | configurable, 1024 tokens, temp 0.0 | Scoring |
| Validation model | haiku | Lightweight checks |

## Decomposer Structure (hierarchy and splitting)

| Knob | Current State | What It Controls |
|------|---------------|-----------------|
| Hierarchy depth | Fixed 4-level | Plan/Spec/Phase/Work (never changes) |
| Brief vs Full | Tier-gate binary | Skips Spec+Phase or not |
| Spec count | "1-3" in prompt | Soft LLM guidance |
| Phase count per spec | "1-5" in prompt | Soft LLM guidance |
| Work count per phase | "1-5" in prompt | Soft LLM guidance |
| Work size guidance | "5-10 iterations" in prompt | Soft LLM guidance |
| Same-file split threshold | ">500 lines" in prompt | Soft LLM guidance |
| Dependency scope | Same-level, same-parent | Hard code constraint |
| Parallelism preference | "fan-out, not chains" in prompt | Soft LLM guidance |
| Validation on failure | Warning only (non-blocking) | Hard code decision |
| Ratification | Always runs, non-blocking | Hard code decision |

## Scoring & Quality Gates

| Knob | Value | What It Controls |
|------|-------|-----------------|
| Composite weights | 40/30/20/10 | completion/quality/validation/efficiency |
| Clarity min_score | 3 (of 5) | Minimum passing clarity score |
| Learning promotion | 3 reinforcements, 30 days | When learnings graduate |

## Polling & Coordination (tick rates)

| Knob | Value | What It Controls |
|------|-------|-----------------|
| Coordinator active interval | 5s | FSM tick rate when busy |
| Coordinator idle interval | 30s | FSM tick rate when idle |
| Integrator interval | 15s | Merge check frequency |
| Reconciliation interval | 60s | Store consistency sweep |
| Supervisor base_delay | 10s | Restart backoff base |
| Supervisor max_delay | 300s | Restart backoff ceiling |

## Work Queue Priority Scoring

| Factor | Formula | Effect |
|--------|---------|--------|
| Dependency-free bonus | (10 - deps.len().min(10)) * 10 | Up to +100 points |
| Attempt penalty | attempt_count.min(5) * 50 | Up to -250 points |

## Observations

### What the safety nets have in common
All are circuit breakers: "if X happens N times, stop trying." They protect against infinite loops
and resource waste at different layers of the stack.

### What makes them different
They operate at different granularities:
- **Micro** (agentic loop): tool failures, parse retries — seconds-scale
- **Meso** (session/work): session failures, work attempts — minutes-scale
- **Macro** (plan/coordinator): decomposition retries, abandon ratio — hours-scale

### The v4 opportunity
Every value in this document is a candidate for YAML configuration. But more importantly,
the *relationships* between these values — what triggers what, what recovery strategy follows
what failure — are hardcoded in procedural Rust. The v4 architecture aims to make those
compositions declarative and YAML-driven, enabling AR to explore not just parameter values
but entirely new behavioral strategies without writing Rust.
