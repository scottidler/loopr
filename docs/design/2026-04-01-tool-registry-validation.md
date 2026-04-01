# Design Document: Tool Registry Pre-flight Validation

**Author:** Scott Idler + Claude
**Date:** 2026-04-01
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

The `tools.register` IPC handler accepts any command string without verifying the executable exists in PATH. A single bad registration (e.g., `busted --verbose` on a system without `busted`) poisons the tool registry, causing cascading exit-127 failures that kill coordinators, waste researcher iterations, and block all implementer progress. This document adds a synchronous `command -v` gate at registration time, instructive rejection errors for agent self-correction, and a coordinator escalation path when tool bootstrapping fails completely.

## Problem Statement

### Background

The dynamic tool resolution system (see `2026-04-01-dynamic-tool-resolution.md`, status: Implemented) introduced a `tools.register` IPC endpoint and `RegisterTool` agent action. Agents - primarily Researchers and Coordinators - can now discover a project's toolchain at runtime and register tools without modifying `loopr.yml` or the target repo. The 3-layer priority stack (config > runtime > detection) works correctly for resolution ordering.

However, the registration handler (`src/daemon/handlers/tools.rs:handle_tools_register`) performs only structural validation (non-empty name, non-empty command). It does not verify that the executable in the command string actually exists in the environment.

### Problem

In the lua-todo E2E run (2026-04-01), a coordinator registered the `test` tool as `busted --verbose`. The `busted` Lua test framework was not installed in the environment. Because registration succeeded, the poisoned tool propagated to every subsequently spawned agent. The result:

- **9 bad registrations** of `busted` variants vs **1 correct** registration (`lua test_todo.lua`)
- **23 exit-127 failures** across 10 researchers
- **Both coordinators lifeguarded** (escalated to NeedHelp)
- **4 consecutive integrator validation failures**
- **100+ wasted LLM iterations** attempting to use a tool that could never work

The correct registration (`lua test_todo.lua`) was eventually discovered by one researcher, but by that point the poisoned `busted` entry had already been accepted and was being used by all active agents. The system had no mechanism to reject the bad registration or recover from it.

### Goals

- Reject tool registrations where the base executable does not exist in PATH
- Return instructive errors that guide agents toward discovering alternatives
- Escalate to NeedHelp when a phase requires validation tools but none can be registered
- Preserve the agent-driven fallback model (no hardcoded language-specific heuristics)

### Non-Goals

- Adding a language-to-tool mapping matrix (the LLM handles this via reasoning)
- Persisting tool validation state across daemon restarts (session-scoped by design)
- Changing how `validation_commands` in IntegratorConfig work (orthogonal system)
- Modifying the 3-layer resolution priority stack (config > runtime > detection)
- Hard-halting the entire daemon on any tool registration failure

## Proposed Solution

### Overview

Three changes, in priority order:

1. **Gatekeeper** - Add pre-flight validation in `handle_tools_register` before inserting into `runtime_tools`. Three branches: bare commands validated via `command -v`, absolute paths via `Path::exists()`, relative paths resolved against an optional `context_dir` (the caller's worktree).

2. **Instructive rejection** - Return an RpcError that tells the agent what failed, what was tried, and suggests it discover alternatives using its file search and analysis tools.

3. **Coordinator escalation** - When all tool registration attempts for a phase fail, the coordinator stops dispatching implementers and reports NeedHelp (exit code 2) rather than hard-halting. Hard halt only in the catastrophic case: validation-commands are defined for a phase AND no valid test tool exists after all researchers have exhausted their attempts.

### Architecture

```
Registration Flow (current):
  Agent -> RegisterTool action -> tools.register IPC -> insert into runtime_tools -> OK

Registration Flow (proposed):
  Agent -> RegisterTool action (now includes context_dir) -> tools.register IPC
    -> extract executable from command string
    -> executable starts with "/"?  -> Path::exists() check
    -> executable contains "/"?     -> resolve against context_dir, then Path::exists()
    -> bare command?                -> sh -c "command -v <executable>"
    -> valid? -> insert into runtime_tools -> OK
    -> invalid? -> RpcError with instructive message -> agent self-corrects

Escalation Flow:
  Researcher attempts RegisterTool -> rejected (exe not found)
  Researcher retries with different command -> rejected again
  Researcher exhausts attempts -> Lifeguard escalates
  Coordinator sees all researchers failed for phase -> NeedHelp (exit 2)
```

### Implementation Plan

#### Change 1: Pre-flight Validation Gate

**File: `src/daemon/handlers/tools.rs` - `handle_tools_register`**

After validating that `name` and `command` are non-empty, extract the base executable and validate it exists:

```rust
// Extract the base executable from the command string.
// "busted --verbose" -> "busted"
// "/usr/bin/lua test.lua" -> "/usr/bin/lua"
// "./scripts/test.sh -v" -> "./scripts/test.sh"
// "sh -c 'run tests'" -> "sh"
let executable = command
    .split_whitespace()
    .next()
    .unwrap_or(&command);

// Optional worktree context for resolving relative paths.
// Passed by the executor via the "context_dir" IPC param.
let context_dir = req
    .params
    .get("context_dir")
    .and_then(|v| v.as_str())
    .map(std::path::PathBuf::from);

// Three-branch validation based on executable form.
let exe_valid = if executable.starts_with('/') {
    // Absolute path: check filesystem directly.
    // e.g., "/usr/bin/lua", "/opt/tools/jest"
    std::path::Path::new(executable).exists()
} else if executable.contains('/') {
    // Relative path: resolve against worktree if provided.
    // e.g., "./scripts/test.sh", "node_modules/.bin/jest", "venv/bin/pytest"
    match &context_dir {
        Some(dir) => dir.join(executable).exists(),
        None => {
            // No worktree context - can't validate relative paths.
            // Accept with a logged warning rather than rejecting valid tools.
            log::warn!(
                "Cannot validate relative path '{}' without context_dir; accepting on faith",
                executable,
            );
            true
        }
    }
} else {
    // Bare command: check PATH via command -v.
    // e.g., "busted", "cargo", "lua", "pytest"
    let check = std::process::Command::new("sh")
        .arg("-c")
        .arg(format!("command -v '{}'", executable.replace('\'', "'\\''")))
        .output();
    matches!(check, Ok(output) if output.status.success())
};

if !exe_valid {
    let hint = if executable.contains('/') {
        format!(
            "File '{}' does not exist{}.",
            executable,
            context_dir.as_ref().map_or(String::new(), |d| format!(" (resolved from {:?})", d)),
        )
    } else {
        format!("Executable '{}' not found in PATH.", executable)
    };
    return Ok(DaemonResponse::err(
        req.id,
        RpcError::invalid_params(&format!(
            "{} The command '{}' cannot be registered because the base \
             executable does not exist in this environment. \
             Use your file search tools to discover what testing \
             frameworks or tools are actually installed, then \
             register the correct command.",
            hint, command
        )),
    ));
}
```

**Executor-side change** (`src/agents/executor.rs` - RegisterTool handler):

Pass `worktree_path` as `context_dir` so relative paths can be resolved:

```rust
let params = serde_json::json!({
    "name": name,
    "command": command,
    "timeout_secs": timeout_secs,
    "worktree": worktree,
    "context_dir": worktree_path.to_string_lossy(),
});
```

**Key design decisions:**

- **Three-branch validation** covers all real-world executable forms: bare commands (`pytest`), absolute paths (`/usr/bin/lua`), and relative paths (`./node_modules/.bin/jest`, `venv/bin/pytest`).
- **Bare commands use `command -v`** - POSIX-specified, works in all shells, no external binary dependency.
- **Absolute paths use `Path::exists()`** - direct filesystem check, no shell needed.
- **Relative paths resolve against `context_dir`** (the caller's worktree). If no context is provided, the tool is accepted with a warning rather than rejected - this avoids false negatives for daemon-level registrations that don't have a worktree context yet.
- **Synchronous `std::process::Command`**, not async. The IPC handler is already on a blocking thread. The `command -v` call completes in <1ms.
- **Extract first token** as the executable. This handles `busted --verbose`, `lua test.lua`, `./scripts/test.sh -v`. It does NOT handle complex shell expressions like `cd foo && run_tests` - those should be wrapped in a script file.
- **POSIX single-quote escaping** prevents injection for the `command -v` branch. Path-based branches don't invoke a shell.

#### Change 2: Instructive Error Messages

The rejection error (shown above) is already instructive. The key elements:

1. **What failed**: "Executable 'busted' not found in PATH"
2. **What was tried**: "The command 'busted --verbose' cannot be registered"
3. **What to do next**: "Use your file search tools to discover what testing frameworks or tools are actually installed"

This feeds back to the agent via `ActionResult::ActionError`, which the LLM receives as a correction prompt. The agent can then:
- Use `SearchFiles` to look for test runners (`find . -name "*.test.*"`)
- Check `package.json` scripts, `Makefile` targets, or project READMEs
- Try a different command (e.g., `lua test_todo.lua` instead of `busted --verbose`)

No hardcoded fallback matrix needed - the LLM reasons about alternatives from the error context.

#### Change 3: Coordinator Escalation

This change is more nuanced than a hard halt. Three severity levels:

**Level 1 - Registration rejection (immediate, per-attempt)**
Already handled by Change 1. The agent receives ActionError and self-corrects within the same iteration. The Lifeguard monitors for repeated identical failures.

**Level 2 - Agent exhaustion (per-agent, via Lifeguard) - NO CODE CHANGE NEEDED**
If an agent repeatedly fails to register any valid tool (same error 3+ times), the Lifeguard escalates with `Verdict::Escalate`. This transitions the agent's work to NeedHelp. The coordinator can then retry with a different researcher or mark the work as failed.

This already works today via the existing Lifeguard in `src/agents/lifeguard.rs`. The improved error message from Change 1 ensures the Lifeguard sees consistent error strings for dedup.

**Level 3 - Phase-level tool failure (coordinator decision) - NEW CODE**
If the coordinator observes that:
- A phase has `validation_commands` defined (meaning testing is required)
- No valid `test` tool exists in the registry after all researchers for that phase have failed
- All researcher work items for tool discovery are terminal (Done with no tool, Abandoned, or NeedHelp)

Then the coordinator should transition the phase to NeedHelp rather than dispatching implementers who will inevitably fail validation.

**File: `src/agents/coordinator.rs` - Executing state handler**

Add a guard before dispatching implementers:

```rust
// Before assigning an implementer to work in this phase:
// Check if the phase requires validation tools and none are registered.
fn phase_has_required_tools(
    phase: &Phase,
    tool_runner: &ToolRunner,
) -> bool {
    // If no validation_commands on this phase, tools aren't strictly required.
    // Note: Phase.validation_commands are phase-scoped (set in the manifest YAML).
    // IntegratorConfig.validation_commands are global (set in loopr.yml).
    // The Integrator merges both via effective_validation_commands().
    // Here we only check the phase-level signal - if the phase declares
    // validation commands, it expects tools to exist.
    if phase.validation_commands.is_empty() {
        return true;
    }
    // Check if at least one tool exists that could satisfy validation.
    // ToolRunner exposes get_tool(name) -> Option<&ToolEntry>.
    tool_runner.get_tool("test").is_some()
}
```

**Why not hard halt?** Some work genuinely doesn't need tools. A pure file-write task (generate a README, scaffold a config) should not be blocked because `busted` isn't installed. The coordinator already knows which phases need validation via `validation_commands` - that's the right signal for when missing tools are catastrophic.

### Data Model

No new data structures. The existing types are sufficient:

- `ToolEntry` - unchanged
- `RpcError` - used for rejection (existing)
- `ActionResult::ActionError` - used for agent feedback (existing)
- `Verdict::Escalate` - used for Lifeguard escalation (existing)
- `Phase.validation_commands` - used to determine if tools are required (existing)

### API Design

No new IPC methods. The `tools.register` endpoint gains a validation step and one new optional parameter:

- **`context_dir`** (optional, string) - The caller's worktree path. Used to resolve relative-path executables (e.g., `./scripts/test.sh`). The executor passes this automatically from `worktree_path`. If omitted, relative paths are accepted without validation.

On rejection, it returns an `RpcError` with code `-32602` (invalid_params) instead of a success response.

## Alternatives Considered

### Alternative 1: Async Validation with Timeout

- **Description:** Run `command -v` asynchronously with a timeout, allowing the handler to remain non-blocking.
- **Pros:** Consistent with Loopr's async-everywhere pattern.
- **Cons:** `command -v` completes in <1ms. Spawning a tokio task, awaiting it, and handling timeout adds complexity for no measurable benefit. The IPC handler is already on a blocking-compatible thread.
- **Why not chosen:** Over-engineering. Synchronous `std::process::Command` is the right tool for a sub-millisecond check.

### Alternative 2: Hardcoded Language Fallback Matrix

- **Description:** When `busted` isn't found, automatically try known Lua test alternatives (`luaunit`, `lua test.lua`, etc.) based on a language-to-tool mapping.
- **Pros:** Faster recovery - no LLM round-trip needed.
- **Cons:** Every new language/framework requires a code change. The matrix will always be incomplete. Loopr is an agentic system - the LLM's reasoning is the fallback mechanism.
- **Why not chosen:** Agent-driven fallback (Option A from the original analysis) is architecturally correct. The error message IS the fallback mechanism - the agent reads it and reasons about alternatives.

### Alternative 3: Global Hard Halt on Any Tool Failure

- **Description:** If any tool registration fails, immediately halt the entire run.
- **Pros:** Prevents any wasted work downstream.
- **Cons:** Too aggressive. Some tasks don't need tools (file writes, scaffolding). Some phases have optional validation. A global halt kills unrelated parallel work.
- **Why not chosen:** The nuanced 3-level escalation (reject -> agent exhaust -> phase NeedHelp) provides the same safety without the blast radius.

### Alternative 4: Deferred Validation (Validate on First Use)

- **Description:** Accept any registration, but validate the executable when `run_tool` is first called.
- **Pros:** Simpler registration path. Handles cases where tools are installed between registration and use.
- **Cons:** The poisoned tool sits in the registry, contaminating all agents that resolve it. By the time `run_tool` fails, the damage is done - multiple agents have already planned their work around a tool that doesn't work. This is exactly what happened in the lua-todo failure.
- **Why not chosen:** Fail-fast at registration is fundamentally safer. The window between registration and use is when the poison spreads.

## Technical Considerations

### Dependencies

None. Uses `std::process::Command` (stdlib) for the `command -v` check and POSIX single-quote escaping (no external crate needed).

### Performance

Negligible. `command -v` is a shell builtin that completes in <1ms. Tool registration happens at most a handful of times per session (typically 1-3 tools discovered per project). The synchronous check does not block the async runtime.

### Security

Only the bare-command branch invokes a shell (`sh -c "command -v ..."`). POSIX single-quote escaping is required to prevent injection. The code wraps the executable in single quotes with internal `'` escaped as `'\''`.

Example attack vector without escaping:
```
command: "test; rm -rf / #"
executable extracted: "test;"
sh -c "command -v test; rm -rf / #" -> dangerous
```

With proper escaping:
```
sh -c "command -v 'test; rm -rf / #'" -> safe (treated as literal filename)
```

The absolute-path and relative-path branches use `Path::exists()` - no shell invocation, no injection risk.

### Testing Strategy

**Bare command branch:**
1. **Unit test - valid bare command accepted:** Register `echo` (universally available), verify success
2. **Unit test - missing bare command rejected:** Register `definitely_not_a_real_command_xyz`, verify RpcError
3. **Unit test - error message is instructive:** Verify rejection includes executable name and guidance

**Absolute path branch:**
4. **Unit test - valid absolute path accepted:** Register `/bin/sh test.sh`, verify success
5. **Unit test - missing absolute path rejected:** Register `/nonexistent/path/tool`, verify RpcError

**Relative path branch:**
6. **Unit test - relative path with context_dir:** Create a temp dir with `./scripts/test.sh`, pass as `context_dir`, verify success
7. **Unit test - relative path missing in context_dir:** Pass `context_dir` but file doesn't exist, verify RpcError
8. **Unit test - relative path without context_dir:** Register `./scripts/test.sh` with no `context_dir`, verify accepted with warning (not rejected)

**Cross-cutting:**
9. **Unit test - first token extraction:** Register `lua test_todo.lua --verbose`, verify `lua` is the executable checked
10. **Unit test - executor passes context_dir:** Verify the RegisterTool executor includes `worktree_path` as `context_dir` in IPC params
11. **Integration test - agent self-correction flow:** Register bad tool -> receive ActionError -> register corrected tool -> verify success
12. **E2E test - re-run lua-todo:** After implementation, re-run the lua-todo E2E target to verify the fix prevents the cascade

### Rollout Plan

Single deployment. All three changes land in one commit (or small PR) since they form a cohesive fix:
1. Validation gate in handler (the critical fix)
2. Instructive error message (part of the gate implementation)
3. Phase-level tool guard in coordinator (defense-in-depth)

No feature flag needed - this is a strict improvement with no backward-compatibility risk. Valid tool registrations continue to succeed. Only invalid ones are now rejected instead of silently poisoning the registry.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| `command -v` false negative (tool exists but not in daemon's PATH) | Low | Medium | Document that the daemon inherits the shell's PATH. If a tool is installed in a non-standard location, use the full path in the command (e.g., `/opt/lua/bin/busted --verbose`). |
| Shell escaping edge case allows injection | Very Low | High | Use a well-tested escaping function. Add a unit test with adversarial inputs. |
| Coordinator guard prevents valid work dispatch | Low | Medium | The guard only blocks implementers when validation_commands are defined AND no test tool exists. Work without validation requirements proceeds normally. |
| Agent enters infinite retry loop trying different invalid tools | Low | Low | Lifeguard already detects repeated failures (3+ same error) and escalates. The instructive error message helps the agent converge faster. |
| `command -v` adds latency to registration | Very Low | Very Low | Sub-millisecond operation. Unmeasurable in practice. |
| Relative path resolved against wrong directory (no context_dir) | Low | Low | If `context_dir` is missing, relative paths are accepted with a warning rather than rejected. The executor always passes `worktree_path` as `context_dir`, so this only affects direct IPC callers (e.g., manual testing). |
| Config tools (`loopr.yml`) bypass validation entirely | N/A | Low | By design - user-configured tools are the user's responsibility. Config is Layer 1 (highest priority) and should not be second-guessed by the daemon. |

## Open Questions

- [ ] Should we also validate that the command actually runs successfully (e.g., `busted --version` exit 0), or is executable-exists sufficient?
- [ ] Should `tools.register` support a `--force` flag to bypass validation for edge cases (e.g., tool will be installed later by a setup script)?
- [ ] Should failed registration attempts be logged to the Learning system so future researchers avoid the same bad commands?
- [x] ~~Should validation also check for the executable as a relative path in the worktree?~~ Yes - resolved via three-branch validation with `context_dir` param.
- [ ] Should config tools (`loopr.yml` Layer 1) also be validated at daemon startup, or is that the user's responsibility?

## References

- `docs/design/2026-04-01-dynamic-tool-resolution.md` - The parent design doc (Implemented) that introduced tools.register
- `src/daemon/handlers/tools.rs` - Registration handler (primary change target)
- `src/agents/coordinator.rs` - Coordinator executing state (escalation guard)
- `src/agents/lifeguard.rs` - Lifeguard escalation logic (existing, no changes needed)
- `src/agents/executor.rs` - RegisterTool action handler (error propagation path)
- `src/tools/mod.rs` - ToolRunner.run() error messages
- `bin/e2e-targets/lua-todo.sh` + `lua-todo.yml` - E2E target that exposed the bug
