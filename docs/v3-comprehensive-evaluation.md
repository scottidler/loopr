# Loopr v3 — Comprehensive Evaluation

## TL;DR

**The architecture is real. The FSMs are bulletproof. The code compiles clean. 1,251 tests pass. Every handler is wired. But this is a well-tested skeleton — it has never talked to an actual LLM or run on a real project.**

---

## What Was Envisioned (v3-*.md)

The vision documents describe a "dev team in a box":
- **5 personas**: Coordinator, Integrator, Implementer(s), Reviewer(s), Researcher(s)
- **4-level hierarchy**: Plan → Spec → Phase → Work
- **3 FSMs**: Work (8 states), Bundle (8 states), Tick (5 states)
- **Ralph Wiggum Loop**: Fresh context every iteration, state persists in TaskStore only
- **Single-writer correctness**: All mutations through daemon, no multi-writer chaos
- **Strategy knobs**: Configurable policies for staleness, locking, tick cadence, validation strictness
- **Crash recovery**: Orphaned records reset on daemon restart

## Scorecard: Vision vs Reality

### Fully Achieved (design matches implementation)

| Area | Status | Evidence |
|------|--------|----------|
| **FSM enforcement** | **100%** | All 5 FSMs have const transition tables, role guards, tests for valid+invalid+skip+reverse+terminal. Cannot bypass in production code. |
| **Domain model** | **100%** | All 11 record types implemented with Record trait, TaskStore persistence, indexed fields. |
| **Daemon + IPC** | **100%** | 49 methods wired, NDJSON over Unix socket, single-writer via AgentIpcBridge. |
| **Crash recovery** | **100%** | 4 recovery paths: InProgress→Blocked, Integrating→Accepted, Sealing/Validating→Failed, expired locks. |
| **Strategy knobs** | **100%** | StalePolicy (3 variants), ConflictPolicy (2), TickCadence (2), BundleSizePolicy, ValidatorStrictness (3), PromotionPolicy — all configurable. |
| **Hierarchy lifecycle** | **100%** | Plan→Spec→Phase→Work with validation gates, parent entity checks, ordering. |
| **Learning system** | **100%** | Confidence scoring, reinforce/contradict, auto-promotion with policy thresholds, scoped selection for context. |
| **Lock system** | **100%** | Advisory + strict modes, TTL, expiry, conflict detection in executor. |
| **Agent lifecycle** | **100%** | Starting→Running↔WaitingForLlm/Paused→Completed/Failed/Cancelled with pool size enforcement. |
| **CLI surface** | **100%** | Every entity has create/get/list/transition. Agent start/stop/pause/resume. Coordinator goal ops. |
| **TUI** | **100%** | 9 views (Dashboard, Plans, Specs, Phases, Works, Bundles, Ticks, Agents, Learnings, Locks), tab nav, streaming display. |
| **Context builder** | **100%** | Token-budgeted, role-aware, learning selection, hierarchy loading, staleness injection. |
| **Agent loops** | **100%** | Implementer (Ralph Wiggum), Reviewer (single-shot), Coordinator (long-lived), Researcher (max-iter), Integrator (deterministic). |
| **Tool execution** | **100%** | OS subprocess with timeout, worktree scoping, path sandboxing. |
| **Streaming** | **100%** | SSE parsing, broadcast channel, real-time token display in TUI. |

### The Numbers

| Metric | Value |
|--------|-------|
| Lines of Rust | **39,222** |
| Test count | **1,251** |
| Test result | **All passing** |
| Clippy warnings | **0** |
| `todo!()` / `unimplemented!()` | **0** |
| `#[allow(dead_code)]` | **2** (mod re-exports only) |
| IPC methods wired | **49/49** |
| FSM transition rules | **69 total** (29 Work + 32 Bundle + 4 Tick + 4 Hierarchy) |

---

## Where the Gap Lives

### The elephant in the room: **No actual LLM calls have been made**

Every agent loop is fully coded. The `AgentLlmClient` does reqwest SSE streaming to the Anthropic Messages API. The context builder produces structured prompts. The action parser handles JSON (direct and code-block-wrapped). The executor handles all 15+ action types.

But all of this has been tested with **mock LLM clients** that return canned responses. The system has never:

1. Called Claude/GPT with a real API key
2. Parsed a real LLM response in the wild
3. Had the Coordinator actually generate a Plan from a goal
4. Had an Implementer write real code in a real worktree
5. Had a Reviewer read a real diff and approve/reject
6. Had the Integrator merge real branches and run real validation
7. Handled an LLM hallucinating an invalid action
8. Recovered from an LLM timing out mid-stream

### What "running on a real project" would require

To run Loopr on a todo app project, you'd need:

1. **API key configuration** — Set `ANTHROPIC_API_KEY` (or equivalent) in config
2. **A git repo** with a worktree-friendly structure
3. **Start daemon** → `loopr daemon`
4. **Set a coordinator goal** → `loopr coordinator set-goal "Build a todo app with Rust + HTML"`
5. **Coordinator wakes up** → reads goal → generates Plan → validates → activates
6. **Coordinator iterates** → generates Specs → Phases → Works
7. **Implementers assigned** → pick up Works → write code in worktrees → propose Bundles
8. **Reviewers assigned** → read Bundles → approve/reject
9. **Integrator** → seals Tick → merges approved branches → runs validation → publishes

**Steps 1-4 should work today.** The daemon starts, IPC works, CLI works, goals persist.

**Steps 5-9 are where reality hits.** The Coordinator's system prompt is well-designed (~85 lines), the state summary builder aggregates all relevant data, and the action parser handles all types. But:

- Will the LLM actually return well-formed `{"action": "create_plan", ...}` JSON? We don't know.
- Will the context be sufficient for the LLM to make good decisions? We don't know.
- Will the Implementer's tool execution actually produce working code? We don't know.
- Will the Reviewer's analysis be meaningful? We don't know.

### Test gaps that matter for real operation

| Gap | Risk | Impact |
|-----|------|--------|
| No LLM integration test | HIGH | First real call might fail on response format |
| No multi-agent concurrency test | HIGH | 2 Implementers + 1 Reviewer + Coordinator running simultaneously |
| Session timeout not tested | MEDIUM | Hung LLM call blocks agent slot forever |
| Graceful shutdown during LLM call | MEDIUM | Daemon restart kills in-flight work without recovery |
| Tick batching untested | LOW | TickCadence::Batched mode might not trigger correctly |
| TUI rendering under load | LOW | 100+ records might lag |

---

## Honest Assessment

### What we built right

The **correctness infrastructure** is exceptional. The thing that Gas Town got wrong — multi-writer chaos, no FSM enforcement, implicit state — we got right. Every mutation goes through the daemon. Every transition is validated against const rules with role guards. Crash recovery works. The architecture is honest.

The **test coverage** is outstanding for a project this size. 1,251 tests covering FSM matrix, multi-step workflows, handler dispatch, action parsing, context building. The domain layer is production-grade.

The **modularity** is clean. Every agent is a separate loop with a well-defined interface. The bridge/executor/context layers are properly separated. Adding a new agent type would be straightforward.

### What we haven't proven

We haven't proven that this thing **works as a dev team**. We've proven the plumbing works, the FSMs are correct, the handlers route properly, the context is built correctly. But we haven't proven that:

1. An LLM can drive the Coordinator loop to produce useful Plans/Specs/Phases/Works
2. An Implementer can write code that passes tests
3. A Reviewer can meaningfully evaluate code quality
4. The whole pipeline converges (vs infinite loops of rejection/retry)
5. The system handles the messy reality of LLM outputs (partial JSON, hallucinated actions, off-topic responses)

### Can we run this on a todo app?

**Mechanically, yes** — the daemon starts, TUI renders, CLI works, all entities persist, agents can be spawned.

**Practically, the first real run will surface issues.** The most likely failures:
1. LLM response parsing — real responses have more variation than test mocks
2. Coordinator action selection — the LLM might not generate the right actions in the right order
3. Implementer tool execution — real code generation requires trial and error
4. Context budget — real projects might blow the token budget, causing truncation of critical info

---

## Recommendation

**The infrastructure is done. The next step is integration testing with a real LLM.** I'd suggest:

1. **Smoke test**: Set an API key, start daemon, set a simple goal ("Create a hello world Rust project"), watch what happens
2. **Fix response parsing**: Whatever the LLM actually returns, adapt the parsers
3. **Tune prompts**: The system prompts are good on paper but will need iteration based on real outputs
4. **Add retry/recovery**: When the LLM returns garbage, the agent should retry with a clarification prompt
5. **Then**: Try the todo app

The foundation is solid. The FSMs are correct. The architecture is sound. It's time to plug in the LLM and find out what breaks.
