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

`domain` (for any shared types like `ToolInvocationId`), `telemetry` (for span emission per tool call), workspace-shared (`tokio`, `serde`, `eyre`). Added via `cargo add`.

## See also

- [../../CLAUDE.md](../../CLAUDE.md): project-wide rules and crate map
- [../../docs/vision.md](../../docs/vision.md): "Security" section covers the lane model and sandbox policy
- [docs/CLAUDE.md](docs/CLAUDE.md): where this crate's design docs go
- [.otto.yml](.otto.yml): scoped CI for this crate (`otto ci` inside this dir)
