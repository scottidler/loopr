# Handoff: evaluate a task-level Claude Code worker

- **Next action:** read [vision](vision.md) and [roadmap](roadmap.md), select one repeated repository task, and define a comparison between the current Claude Code workflow and a single Loopr-owned worker execution.
- **Status:** lower-priority experiment proposed; no worker adapter implemented.
- **Baseline:** local `v5`, `c50682a3`, last commit July 13, 2026. `docs/ringer-positioning.md` was untracked during evaluation; preserve it and do not assume another clone has it.
- **Goal:** determine whether Loopr's DAG/worktree/integration machinery improves a workflow whose daily AI driver is Claude Code.

## Architectural placement

- [LlmClient](../crates/llm/src/client.rs) models completion calls. A Claude Code process owns a tool-using session, so it is not automatically a drop-in implementation of that interface.
- Prototype at the agent/worktree boundary: work item, workspace, constraints, and acceptance command in; session identifier, change identity, and verification evidence out.
- Preserve typed work/bundle transitions and the deterministic integrator. Do not move model judgment into the integrator or create a second competing scheduler.
- Read the optional [positioning note](ringer-positioning.md) when available. Its discussion is context, not a previously approved implementation plan.
- Existing Clyde code has experience invoking Claude CLI processes; inspect it locally if available, but do not introduce a cross-repo dependency without a reason.

## Bounded experiment

- Use one task class and one execution lane. Write down the actual friction this removes before building a generalized engine abstraction.
- Define the worker lifecycle: launch, structured result, cancellation, restart/resume, timeout policy, and handling an incomplete workspace.
- Make configuration loading explicit: which Claude rules, skills, permissions, and project instructions the worker inherits. Confirm behavior on the installed Claude version.
- Give the worker an isolated worktree and a bounded task. Record the session ID and resulting diff/commit, then run the acceptance command independently.
- A process exiting zero or saying “done” does not establish acceptance. Failed checks must stop integration and preserve useful evidence for recovery.
- Compare completion, human interventions, verified checks, configuration reuse, and usage with the existing workflow. Document inconclusive results rather than generalizing from one successful run.

## Acceptance and continuation

- A passing worker result reaches the existing integration path with verifiable change identity; a failed check cannot integrate.
- Cancellation leaves no unintended worker process operating on the workspace. Define descendant ownership and recovery before claiming this property.
- A restart does not duplicate work or integrate a stale result; operator changes remain protected.
- Setup/permission behavior is verified through an installed Claude Code invocation. Reference: [programmatic Claude Code](https://code.claude.com/docs/en/headless).
- Keep the older [July recovery handoff](handoff-2026-07-13.md) separate: it describes different work and contains dated state that must not be revived as current findings.
- If the experiment earns implementation, create a dated design under `docs/design/` with `Crates touched:` and `Status:` per [docs rules](CLAUDE.md). Cross-crate scope needs a top-level design.
- **Suggested skills:** `/handoff`, `/create-design-doc`, `/tdd`; use the current project e2e workflow once a worker path exists, after verifying its instructions are current.
