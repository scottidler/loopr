# Design Document: Tool Discovery Loop Fix

**Author:** Scott Idler + Claude + Gemini
**Date:** 2026-04-01
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

After implementing pre-flight validation for tool registration (v0.1.42), the lua-todo E2E run exposed a behavioral loop: agents correctly reject bad tools (`busted --verbose`) but never discover and register the correct one (`lua test_todo.lua`). The coordinator spawns unlimited researchers, researchers search endlessly without registering anything, and the pipeline stalls safely but indefinitely. This document fixes three issues: surfacing `validation-commands` as a hint to the coordinator, removing a prompt/code mismatch in the researcher, and adding a hard FSM spawn limit.

## Problem Statement

### Background

The tool registry validation gate (v0.1.42, `docs/design/2026-04-01-tool-registry-validation.md`) successfully prevents poisoned tool registrations. In the lua-todo E2E run, `busted --verbose` was correctly rejected because the `busted` executable doesn't exist in the environment. Zero exit-127 cascading failures occurred (vs. 23 in v0.1.41). Zero bad bundles were proposed. The git history remained pristine.

However, the system stalled indefinitely because no agent ever registered the correct test command (`lua test_todo.lua`).

### Problem

Three compounding failures create an infinite stall:

1. **The coordinator has the answer but can't see it.** The phase's `validation-commands` field already declares `lua test_todo.lua` - the exact command needed. But `phase_missing_test_tool()` only warns that a tool is missing without surfacing the validation commands. The coordinator has no way to know what command to register.

2. **The researcher prompt lies about its capabilities.** `prompts/researcher.pmt` lists `register_tool` as action #6. But `is_allowed_researcher_action()` in `src/agents/researcher.rs:173` does not include `RegisterTool`. The LLM plans around a capability it doesn't have, wastes iterations searching for tool configs to feed into a `register_tool` call that will be rejected, and never produces a useful `create_learning` instead.

3. **The coordinator spawns unlimited researchers.** When a researcher fails (max iterations), the coordinator spawns another with a slightly tweaked query. `CoordinatorState` has no spawn tracking for researchers, so there's no limit. In the E2E run: 35 researchers spawned, all failed, none produced actionable findings.

### Goals

- Give the coordinator the validation-commands hint so it can register the correct tool without spawning researchers
- Remove the prompt/code mismatch so researchers don't waste iterations on phantom capabilities
- Add a hard FSM guard on researcher spawns per scope to prevent infinite loops

### Non-Goals

- Changing the researcher's code-level action whitelist (already correct - `RegisterTool` is already blocked)
- Adding a language-to-tool mapping matrix (the LLM + validation-commands hint is the mechanism)
- Modifying the tool validation gate itself (working correctly)
- Changing how the Lifeguard or escalation system works

## Proposed Solution

### Overview

Three changes, ordered by impact:

1. **Surface validation-commands in the tool warning** - Modify `phase_missing_test_tool()` to include the phase's `validation_commands` in the warning message. The coordinator reads this, extracts the executable (e.g., `lua` from `lua test_todo.lua`), and calls `register_tool` directly. No researcher needed.

2. **Remove register_tool from researcher prompt** - Delete action #6 from `prompts/researcher.pmt`. The code already blocks it; the prompt should match reality. Researchers should use `create_learning` to report tool findings, not try to register tools directly.

3. **Hard FSM spawn limit per scope** - Add `researcher_spawns: HashMap<String, u32>` to `CoordinatorState`. Increment when `SpawnResearcher` executes. When the count for a scope hits the limit (default: 3), reject the action with an `ActionError` telling the coordinator it must escalate via `need_help`.

### Architecture

```
Current flow (broken):
  Coordinator sees "no test tool" warning
  -> spawns Researcher to discover tool
  -> Researcher searches for .busted, never finds it, max iterations
  -> Coordinator spawns another Researcher (repeat forever)

Proposed flow:
  Coordinator sees warning WITH validation-commands: ["lua test_todo.lua"]
  -> Coordinator calls register_tool("test", "lua test_todo.lua")
  -> Validation gate checks: command -v lua -> exists!
  -> Tool registered, implementers unblocked

  Fallback (no validation-commands declared):
  -> Coordinator spawns Researcher (up to 3 per scope)
  -> Researcher searches, reports findings via create_learning
  -> Coordinator reads learning, registers tool
  -> If 3 researchers fail: hard reject, coordinator must need_help
```

### Implementation Plan

#### Change 1: Surface validation-commands in coordinator warning

**File: `src/agents/coordinator.rs` - `phase_missing_test_tool()`**

The function currently returns a generic warning. Add the phase's validation commands to the message:

```rust
fn phase_missing_test_tool(stores: &Stores, coord_state: &CoordinatorState) -> String {
    let phase_id = match &coord_state.current_phase_id {
        Some(id) => id,
        None => return String::new(),
    };
    let phase = {
        let Ok(phases) = stores.read_phases() else {
            return String::new();
        };
        match phases.get(phase_id) {
            Some(p) => p.clone(),
            None => return String::new(),
        }
    };
    if phase.validation_commands.is_empty() {
        return String::new();
    }
    let has_test_tool = stores
        .read_tool_runner()
        .ok()
        .is_some_and(|runner| runner.get_tool("test").is_some());
    if has_test_tool {
        return String::new();
    }

    // Surface the declared validation commands as a hint
    let cmds_list = phase
        .validation_commands
        .iter()
        .map(|c| format!("  - `{}`", c))
        .collect::<Vec<_>>()
        .join("\n");

    format!(
        "**WARNING: This phase has validation-commands but no 'test' tool \
         is registered.** The declared validation commands for this phase are:\n\
         {}\n\
         You MUST use `register_tool` to register a test command based on \
         these commands BEFORE dispatching implementers. Extract the \
         executable from the commands above and register it. \
         Do NOT spawn researchers for tool discovery when validation \
         commands are already declared.\n\n",
        cmds_list
    )
}
```

**Key design decisions:**

- The validation commands are presented verbatim. The LLM extracts the executable.
- The instruction explicitly says "Do NOT spawn researchers" when commands are declared. This short-circuits the infinite loop for the common case.
- When `validation_commands` is empty, the warning is still empty (no change in behavior). The coordinator can still spawn researchers for projects that don't declare validation commands.

#### Change 2: Remove register_tool from researcher prompt

**File: `prompts/researcher.pmt`**

Remove action #6 and renumber:

```
Before:
6. `register_tool`   {"action": "register_tool", "name": "test", "command": "busted --verbose"}
   Register a project tool (test runner, linter, etc.) for use by other agents...

After:
(deleted - action numbers 7/8 become 6/7)
```

No code change needed - `is_allowed_researcher_action()` already excludes `RegisterTool`. This is purely a prompt correction to eliminate the mismatch.

**File: `prompts/coordinator.pmt` - line 40-42**

Update the `register_tool` example and guidance to remove the `busted` hallucination bait and align with the new validation-commands flow:

```
Before:
14. `register_tool`   {"action": "register_tool", "name": "test", "command": "busted --verbose"}
    Register a project tool for use by agents. Use when an Implementer reports "Tool not found".
    Dispatch a Researcher first to discover the correct command, then register it.

After:
14. `register_tool`   {"action": "register_tool", "name": "test", "command": "cargo test"}
    Register a project tool for use by agents. When a phase has validation-commands,
    extract the test command from those and register it directly. Only spawn a
    Researcher if no validation commands are declared.
```

#### Change 3: Hard FSM spawn limit per scope

**File: `src/domain/coordinator_state.rs` - `CoordinatorState`**

Add a new tracking field:

```rust
/// Number of researchers spawned per scope_id in the current phase.
/// Used to enforce the spawn limit (default: 3 per scope).
/// Reset when the phase changes.
#[serde(default)]
pub researcher_spawns: HashMap<String, u32>,
```

Add methods:

```rust
/// Increment the researcher spawn counter for a scope. Returns the new count.
pub fn increment_researcher_spawns(&mut self, scope_id: &str) -> u32 {
    let count = self.researcher_spawns.entry(scope_id.to_string()).or_insert(0);
    *count += 1;
    self.updated_at = id::now_millis();
    *count
}

/// Get the researcher spawn count for a scope.
pub fn researcher_spawn_count(&self, scope_id: &str) -> u32 {
    self.researcher_spawns.get(scope_id).copied().unwrap_or(0)
}
```

Reset in `activate_phase()`:

```rust
pub fn activate_phase(&mut self, phase_id: String) {
    self.current_phase_id = Some(phase_id);
    self.phase_activated_at = Some(id::now_millis());
    self.researcher_spawns.clear();  // Reset per-phase
    self.fsm_state = CoordinatorFsmState::Executing;
    self.updated_at = id::now_millis();
}
```

**File: `src/agents/coordinator.rs` - action execution loop (~line 1762)**

The coordinator's `run_iteration` iterates over parsed actions and calls `execute_action` generically. The existing pattern for pre-execution guards uses `if let` matching before the `execute_action` call (see the `AssignAgent` / `ContextOverflow` guard at ~line 1638). The spawn limit follows the same pattern:

```rust
// Pre-execution guard: researcher spawn limit per scope
if let AgentAction::SpawnResearcher { scope_id, .. } = action_ref {
    let count = coord_state.researcher_spawn_count(scope_id);
    if count >= self.config.max_researcher_spawns {
        self.ctx.warn(&format!(
            "researcher spawn limit reached ({}/{}) for scope '{}'",
            count, self.config.max_researcher_spawns, scope_id
        ));
        last_summary = format!(
            "Researcher spawn limit reached for scope '{}'. \
             You MUST escalate via need_help.",
            scope_id
        );
        continue;
    }
}

// ... existing execute_action call ...

// Post-execution: track successful researcher spawns
if let AgentAction::SpawnResearcher { scope_id, .. } = action_ref {
    if matches!(result, ActionResult::AgentSpawned { .. }) {
        coord_state.increment_researcher_spawns(scope_id);
    }
}
```

The guard uses `continue` (skip this action) rather than returning an error, matching how the existing `ContextOverflow` guard works. The `last_summary` is set so the coordinator receives feedback about the limit in its next iteration.

**File: `src/config.rs` - CoordinatorConfig**

Add the configurable limit:

```rust
pub struct CoordinatorConfig {
    // ... existing fields ...
    /// Maximum researchers the coordinator can spawn per scope before
    /// being forced to escalate. Default: 3.
    #[serde(default = "default_max_researcher_spawns")]
    pub max_researcher_spawns: u32,
}

fn default_max_researcher_spawns() -> u32 { 3 }
```

### Data Model

One new field on `CoordinatorState`:

- `researcher_spawns: HashMap<String, u32>` - Per-scope spawn counter. Session-scoped (persisted in TaskStore). Reset on phase activation.

One new field on `CoordinatorConfig`:

- `max_researcher_spawns: u32` - Configurable limit (default: 3).

### API Design

No new IPC methods. The spawn limit is enforced in the coordinator's action loop before the `agent.start` IPC call is made.

## Alternatives Considered

### Alternative 1: Prompt-Only Spawn Limit

- **Description:** Tell the coordinator in its prompt "do not spawn more than 3 researchers per scope" without a hard code guard.
- **Pros:** Zero code change. Just update `coordinator.pmt`.
- **Cons:** LLMs cannot reliably count their own past actions. The coordinator in the E2E run already demonstrated this - it kept spawning researchers despite clear failure signals.
- **Why not chosen:** A hard FSM guard in Rust is deterministic. Prompt instructions are probabilistic.

### Alternative 2: Researcher Registers Tools Directly

- **Description:** Add `RegisterTool` to `is_allowed_researcher_action()` so researchers can register tools they discover.
- **Pros:** Shorter feedback loop - researcher finds tool and registers it in one session.
- **Cons:** Breaks the read-only researcher contract. Researchers lack the coordinator's phase context (validation-commands, phase requirements). In the E2E run, the coordinator (not a researcher) tried to register `busted --verbose` - giving researchers the same power would add another hallucination vector without the phase-level context needed to make good decisions.
- **Why not chosen:** Tool registration is a coordinator decision. Researchers discover and report; coordinators decide and act.

### Alternative 3: Auto-Register from validation-commands

- **Description:** When a phase has `validation_commands` and no test tool, automatically extract the executable and register it at phase activation time. No LLM involvement.
- **Pros:** Deterministic. Fastest possible path.
- **Cons:** `validation_commands` aren't always tool commands (could be `cargo fmt --check`). Extracting the "test" tool from a list of commands requires heuristic logic. The coordinator LLM is better positioned to make this judgment.
- **Why not chosen:** Over-automation. The LLM should read the commands and decide which one to register as the `test` tool. We surface the information; the LLM makes the decision.

## Technical Considerations

### Dependencies

None. All changes use existing types and patterns (`HashMap`, `CoordinatorState` fields, prompt files).

### Performance

Negligible. The spawn counter adds one HashMap lookup per `SpawnResearcher` action. The validation-commands are already loaded with the phase.

### Security

No new security considerations. The tool validation gate (v0.1.42) still validates all `register_tool` calls.

### Testing Strategy

**Change 1 - validation-commands in warning:**
1. **Unit test:** Phase with validation_commands and no test tool returns warning containing the commands
2. **Unit test:** Phase with validation_commands AND a registered test tool returns empty warning
3. **Unit test:** Phase with empty validation_commands returns empty warning (existing test, verify unchanged)

**Change 2 - researcher prompt:**
4. **Manual verification:** Read `prompts/researcher.pmt`, confirm `register_tool` is not listed
5. **Existing test:** `is_allowed_researcher_action` already blocks `RegisterTool` (no change needed)

**Change 3 - spawn limit:**
6. **Unit test:** `increment_researcher_spawns` returns correct counts
7. **Unit test:** `researcher_spawn_count` returns 0 for unknown scope
8. **Unit test:** `activate_phase` clears researcher_spawns
9. **Unit test:** Serde roundtrip with `researcher_spawns` field
10. **Unit test:** Backward compat - old JSON without `researcher_spawns` deserializes with empty HashMap
11. **Integration test:** Coordinator action loop rejects `SpawnResearcher` when limit reached
12. **E2E test:** Re-run lua-todo, verify coordinator registers tool from validation-commands hint without spawning researchers

### Rollout Plan

Single deployment. All three changes land in one commit:
1. Prompt fix (zero risk - removes a phantom capability)
2. Warning enhancement (low risk - adds information to existing warning)
3. Spawn limit (low risk - new field with serde default, backward compatible)

No feature flag needed. All changes are strict improvements with no backward-compatibility risk.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Coordinator misreads validation-commands and registers wrong tool | Low | Low | Validation gate still rejects bad executables. If the executable exists but the command is wrong for the `test` role (e.g., registers a lint command as `test`), the integrator's validation step will catch the mismatch. |
| Phase has validation-commands but none are test commands | Low | Low | The coordinator LLM judges which command is most appropriate for `test`. If no command fits, it can still spawn a researcher (within the limit). |
| Spawn limit too low (3) blocks legitimate research | Low | Medium | Configurable via `max_researcher_spawns` in `loopr.yml`. Default of 3 matches the Lifeguard's 3-strike threshold. |
| Researcher prompt removal breaks existing behavior | Very Low | Low | `RegisterTool` was already blocked in code. Removing it from the prompt aligns behavior. |
| `researcher_spawns` HashMap grows unbounded | Very Low | Very Low | Reset on phase activation. Typical phase has 1-3 scopes. |
| Old coordinator states missing `researcher_spawns` field | N/A | N/A | `#[serde(default)]` handles backward compatibility. |

## Open Questions

- [x] ~~Should researchers be able to register tools?~~ No. Read-only contract. Coordinator owns registration.
- [x] ~~Should the spawn limit be per-scope or per-phase?~~ Per-scope. A scope that exhausted 3 researchers doesn't block a different scope's researchers. Reset on phase activation.
- [x] ~~Should the coordinator prompt explicitly mention `register_tool` for validation-commands?~~ Yes. Both the warning message (Change 1) and the `coordinator.pmt` example (Change 2) guide the coordinator toward direct registration.
- [ ] Should we track spawn counts for implementers too, or just researchers? (Implementers already have `work_attempts` tracking - may not need a separate limit.)

## References

- `docs/design/2026-04-01-tool-registry-validation.md` - Pre-flight validation gate (Implemented)
- `src/agents/coordinator.rs` - Coordinator FSM, `phase_missing_test_tool()`, action loop
- `src/agents/researcher.rs` - `is_allowed_researcher_action()` whitelist
- `src/domain/coordinator_state.rs` - `CoordinatorState` struct
- `src/config.rs` - `CoordinatorConfig`
- `prompts/coordinator.pmt` - Coordinator prompt (register_tool guidance)
- `prompts/researcher.pmt` - Researcher prompt (phantom register_tool action)
