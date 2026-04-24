# tools

The subprocess-execution and capability-exposure layer. Owns the `Tool` trait, built-in tool impls, the lane classification table (`Local` / `Net` / `Heavy`), and the bwrap sandbox integration. Tools are agent-callable capabilities with typed input/output.

## In scope

- `Tool` trait with typed `Input` / `Output` / `Error` associated types
- Built-in tool impls — first-gate set: `Read`, `Write`, `Edit`, `Bash`, `Grep`, `Glob`. Later additions per vision.md Deferred Enhancements.
- Lane classification: `fn classify(tool_name: &str) -> Lane` (v4-verbatim pattern)
- Lane policy: `LanePolicy` with slot limits, timeouts, sandbox-net flag
- `LaneRouter`: enforces per-lane concurrency caps via tokio semaphores
- Sandbox integration: `bwrap --unshare-net` wrapping for `Local` lane; slot limit for `Heavy` lane
- Sandbox detection + the `required|preferred|off` posture logic (per vision.md "Security" section)
- Bash denylist (rm -rf /, sudo *, curl | sh, git push, gh repo delete) + target-level extensions
- Tool schema export: tools can produce their schema for prompt rendering (consumed by `agents::ContextBuilder`)
- Config: `ToolsConfig` composed into the top-level `Config` by `loopr`

## Out of scope

- **LLM prompt assembly.** `agents::ContextBuilder` calls into `tools` to get schemas but the rendering itself lives in `agents`. `tools` exposes *what* a tool is; it does not shape *how* that shows up in a prompt.
- LLM API calls — that's `llm`
- Git worktree lifecycle — that's `worktree`. Tools may execute *inside* a worktree but the worktree itself is created and torn down by `worktree`.
- Permission tier UI / approval flows — TUI-era concern, deferred
- Per-target security overrides beyond the denylist (those come from `loopr init` reading the target's config)

## Rule

Tools are the hands of the agent. They should be small, testable, and side-effect-contained. Each tool gets its own file under `src/tools/` named after the tool (`read.rs`, `bash.rs`, etc.). Unknown tools default to `Heavy` lane (conservative: slot-limit + long timeout).

Sandbox discipline: the `required|preferred|off` knob is enforced here. `loopr init` verifies and refuses appropriately; this crate provides the detection + wrapping primitives.

## Dependencies

`telemetry` (for span emission per tool call) and workspace-shared (`tokio`, `serde`, `eyre`). Added via `cargo add`.

**Notably does NOT depend on `domain`.** IDs like `SessionId` / `WorkId` that show up in tool-invocation spans are attached by callers (typically `agents`) via `tracing::Span::current().record(...)`, not by `tools` itself. This keeps `tools` agnostic of loopr's pipeline types — a subprocess executor shouldn't know or care about `Plan`/`Work`/`Bundle`. Round 3 Architect finding.

## Long subprocess output over IPC (TODO for Stage 7)

Subprocess output (e.g., `cargo test`, `npm install`) can exceed the `ipc` crate's 1 MiB `LinesCodec` max line length. Hitting the cap severs the client-daemon connection. When Stage 7 lands and `tools` starts capturing real subprocess output for streaming back to clients:

- **Preferred approach:** chunked multi-message IPC. `tools` captures output, emits it as a sequence of `DaemonEvent::ToolOutputChunk { tool_invocation_id, seq, text }` events where each chunk stays well under the 1 MiB cap. Clients reassemble on the other side.
- **Fallback if chunking is too complex:** truncate at N KB with head+tail summary sent over IPC; full output dumped to `.loopr/runs/<session-id>/work/<work-id>/<tool-invocation-id>.log` for reference.

Decide in the Stage 7 design doc (`docs/design/2026-04-21-tool-registry.md`). User preference is chunked multi-message; raise on the design doc review if that becomes impractical.

## See also

- [../../CLAUDE.md](../../CLAUDE.md): project-wide rules and crate map
- [../../docs/vision.md](../../docs/vision.md): "Security" section covers the lane model and sandbox policy
- [.otto.yml](.otto.yml): scoped CI for this crate (`otto ci` inside this dir)
