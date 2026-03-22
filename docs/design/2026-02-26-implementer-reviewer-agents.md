# Design Document: Loopr v3 — MVP3

**Author:** Scott Aidler
**Date:** 2026-02-26
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

MVP3 transforms Loopr from a human-driven orchestrator into the "dev team in a box" it was designed to be. LLM agents — Implementers and Reviewers — run as Tokio tasks inside the daemon, executing the Ralph Wiggum Loop pattern: fresh context each iteration, state persisted in TaskStore. Agents work in Git worktrees, execute tools as OS subprocesses, produce Bundles, and review each other's work. The human Coordinator oversees, intervenes when needed, and retains full authority over the hierarchy and gates. Everything plugs into the backbone MVP1+MVP2 built — same daemon, same FSMs, same IPC, same TaskStore.

## Glossary

| Term | Definition |
|------|-----------|
| **Agent** | A Tokio task inside the daemon that runs an LLM in a Ralph Wiggum Loop. Each agent has a Role (Implementer, Reviewer, Researcher), a prompt template, and access to tools. |
| **Agent Loop** | One iteration of an agent's Ralph Wiggum Loop: load context from TaskStore → construct prompt → call LLM → parse actions → execute actions → persist results. |
| **Tool** | An OS subprocess that an agent can execute in a worktree. Examples: `cargo test`, `cargo clippy`, `git diff`. Tools are configured per-project. |
| **Tool Catalog** | A configured list of tools available to agents, with names, commands, timeouts, and worktree requirements. |
| **Agent Pool** | A bounded set of concurrent agent tasks. The Implementer pool defaults to 2-4 agents. |
| **Streaming** | Real-time delivery of agent output (LLM tokens, tool stdout/stderr) to the TUI via the existing broadcast channel. |
| **Staleness Cascade** | When a Tick publishes, all in-progress Works with an older `base_tick_id` are automatically notified. Their Bundles are marked stale. Agents must refresh their worktree and rebase before resubmitting. |

## Problem Statement

### Background

MVP1 proved the orchestration spine: three FSMs, daemon-mediated correctness, NDJSON IPC, Git worktree management, and a ratatui TUI. MVP2 added durable persistence via TaskStore and a read-only Doc Validator LLM that gates document quality. Both MVPs are human-driven — the human wears every hat (Coordinator, Implementer, Integrator) via the TUI and CLI. The system is correct but inert.

The original ChatGPT architecture conversation established the principle: **"Prove the system works first. Then insert brains."** MVP1+MVP2 proved the system works. Now it's time to insert brains.

### Problem

- **No automated implementation.** Works sit in `Ready` until a human manually creates code in a worktree, commits it, and proposes a Bundle. The whole point of Loopr is to automate this.
- **No automated review.** Bundles move from `Proposed` through `Triaged → Reviewed → Accepted` entirely via human judgment. LLM-powered code review would catch issues the human misses and reduce the review bottleneck.
- **No tool execution.** Agents need to run tests, linters, and other tools to validate their own work before submitting Bundles. Currently, tool execution only happens during Tick validation (Integrator), not during implementation.
- **No streaming visibility.** When agents work, the TUI shows nothing. The human has no visibility into what agents are doing, thinking, or producing until a Bundle appears.
- **Manual staleness handling.** When a Tick publishes, in-progress Works with older `base_tick_id` are stale, but no one is automatically notified. Agents would re-submit stale Bundles without realizing.

### Goals

- **G1:** LLM Implementer agents autonomously take Works from `InProgress`, work in worktrees, produce code, run tools, and propose Bundles
- **G2:** LLM Reviewer agents review proposed Bundles and provide structured feedback (approve/request-changes)
- **G3:** Tool execution system allows agents to run configured commands (tests, linters, formatters) as OS subprocesses in worktrees
- **G4:** Agent output streams to the TUI in real-time via the broadcast channel
- **G5:** Staleness cascade automatically notifies in-progress agents when a new Tick publishes
- **G6:** Bounded parallelism — configurable number of concurrent Implementer and Reviewer agents
- **G7:** Human Coordinator retains full authority — can pause/resume/cancel agents, override decisions, and gate transitions
- **G8:** Agents use the same IPC protocol as the TUI — no special privileged access

### Non-Goals

- **No Coordinator agent.** The human remains the Coordinator. Automating the "PM" role is a different problem (MVP4+).
- **No Integrator agent.** Tick management (seal, validate, publish) remains human-driven. The Integrator role requires too much trust for MVP3.
- **No Spec/Design swarms.** The architecture supports them (Tokio tasks with Researcher role), but MVP3 focuses on the implementation+review loop. Swarms are MVP4+.
- **No multi-repo support.** Agents work in one target repo's worktrees.
- **No remote agents.** All agents run locally inside the daemon process.
- **No model-agnostic agent framework.** MVP3 targets Anthropic's Claude API. Abstracting to support other providers is future work.
- **No mandatory locking.** Locks remain advisory. Mandatory lock enforcement is MVP4+.
- **No Learning → Policy automation.** Learnings are created by agents but promotion to Policy remains manual (Coordinator decision).

## Proposed Solution

### Overview

Three new subsystems, all plugging into the existing daemon architecture:

**1. Agent System** — A new `agents` module that defines the agent loop, prompt construction, action parsing, and lifecycle management. Two agent types: Implementer (produces code) and Reviewer (reviews Bundles). Agents run as Tokio tasks spawned by the daemon, bounded by a configurable pool size. Each agent iteration is a Ralph Wiggum Loop: load context → build prompt → call LLM → parse structured actions → execute actions via IPC + tools → persist results.

**2. Tool Execution System** — A new `tools` module that runs OS subprocesses in worktrees. Tools are configured in `loopr.yml` with names, commands, timeouts, and working directory requirements. The tool system captures stdout, stderr, and exit code, providing structured results back to the agent.

**3. Agent Streaming** — Extensions to the IPC event system to carry agent output (LLM tokens, tool results, status changes) to the TUI in real-time. The TUI gets a new Agent view showing live agent activity.

### Architecture

```
┌──────────────────────────────────────────────────────────────────┐
│                        TUI / CLI                                  │
│  ┌──────────┐  ┌──────────┐  ┌──────────┐  ┌────────────────┐   │
│  │Dashboard │  │Works │  │ Bundles  │  │  Agent View    │   │
│  │          │  │          │  │          │  │ (new in MVP3)  │   │
│  │          │  │          │  │          │  │ • live output  │   │
│  │          │  │          │  │          │  │ • tool results │   │
│  │          │  │          │  │          │  │ • agent status │   │
│  └──────────┘  └──────────┘  └──────────┘  └────────────────┘   │
└──────────────────────┬───────────────────────────────────────────┘
                       │ NDJSON / Unix Socket
┌──────────────────────▼───────────────────────────────────────────┐
│                         Daemon                                    │
│                                                                   │
│  ┌──────────────────────────────────────────────────────────┐    │
│  │                      Handlers                             │    │
│  │  *.create  *.get  *.list  *.transition                    │    │
│  │  agent.start  agent.stop  agent.status  agent.list        │    │
│  └─────────┬──────────────────────┬────────────┬─────────────┘    │
│            │                      │            │                  │
│  ┌─────────▼──────────┐  ┌───────▼────────┐  ┌▼──────────────┐  │
│  │     TaskStore       │  │  Agent System  │  │ Tool System   │  │
│  │  ┌──────────────┐   │  │ ┌────────────┐ │  │ ┌──────────┐ │  │
│  │  │ JSONL (truth) │   │  │ │Implementer│ │  │ │Command   │ │  │
│  │  │ SQLite (cache)│   │  │ │  Pool     │ │  │ │::new()   │ │  │
│  │  └──────────────┘   │  │ ├────────────┤ │  │ │in worktree│ │  │
│  └─────────────────────┘  │ │ Reviewer   │ │  │ └──────────┘ │  │
│                           │ │  Pool      │ │  └──────────────┘  │
│  ┌─────────────────────┐  │ └────────────┘ │                    │
│  │    Doc Validator     │  │ ┌────────────┐ │                    │
│  │  (MVP2, unchanged)   │  │ │ LLM Client │ │                    │
│  └─────────────────────┘  │ │ (streaming) │ │                    │
│                           │ └────────────┘ │                    │
│                           └────────────────┘                    │
└──────────────────────────────────────────────────────────────────┘
```

### Data Model

#### AgentSession

A new record type tracking agent lifecycle and output.

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSession {
    pub id: String,                    // ULID
    pub agent_type: AgentType,         // Implementer | Reviewer
    pub work_id: Option<String>,  // what it's working on (Implementer)
    pub bundle_id: Option<String>,     // what it's reviewing (Reviewer)
    pub status: AgentStatus,
    pub iteration: u32,                // current Ralph Wiggum iteration number
    pub model: String,                 // LLM model used
    pub worktree_path: Option<String>, // where the agent is working
    pub created_at: i64,
    pub updated_at: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentType {
    Implementer,
    Reviewer,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentStatus {
    Starting,      // agent task spawned, loading context
    Running,       // actively calling LLM or executing tools
    WaitingForLlm, // blocked on LLM API response
    Paused,        // human paused the agent
    Completed,     // agent finished its work successfully
    Failed,        // agent encountered an unrecoverable error
    Cancelled,     // human cancelled the agent
}
```

AgentSession implements `Record` for TaskStore persistence.

**Agent status transitions:**
- `Starting → Running` — context loaded, first LLM call begins
- `Running → WaitingForLlm` — blocked on LLM API response
- `WaitingForLlm → Running` — LLM response received, executing actions
- `Running → Paused` — Coordinator requested pause; takes effect at the **next iteration boundary** (not mid-action). The current iteration completes before pausing.
- `Paused → Running` — Coordinator resumes
- `Running → Completed` — agent issued `Done` action
- `Running → Failed` — unrecoverable error (LLM failure, tool crash, etc.)
- `* → Cancelled` — Coordinator cancels; if mid-iteration, current iteration completes then stops

#### AgentAction

Structured actions that an LLM agent can request. The agent's response is parsed into a sequence of these.

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum AgentAction {
    /// Run a tool in the worktree
    RunTool {
        tool_name: String,
        args: Vec<String>,
    },
    /// Write or modify a file in the worktree
    WriteFile {
        path: String,
        content: String,
    },
    /// Read a file from the worktree
    ReadFile {
        path: String,
    },
    /// Create a git commit in the worktree
    Commit {
        message: String,
        paths: Vec<String>,
    },
    /// Propose a Bundle from the worktree
    ProposeBundle {
        description: String,
        claims: Vec<String>,
    },
    /// Transition a record's state (via IPC)
    Transition {
        collection: String,
        id: String,
        target_state: String,
    },
    /// Create a Learning
    CreateLearning {
        content: String,
        scope: String,
        source_id: String,
    },
    /// Signal that the agent is done with this iteration
    Done {
        summary: String,
    },
    /// Signal that the agent needs human intervention
    NeedHelp {
        reason: String,
    },
}
```

#### ToolConfig (in loopr.yml)

```yaml
agents:
  enabled: true
  implementer:
    model: "claude-sonnet-4-6"
    api_key_env: "ANTHROPIC_API_KEY"
    max_tokens: 8192
    max_iterations: 20        # safety cap per work item
    pool_size: 2              # max concurrent implementers
    temperature: 0.3
  reviewer:
    model: "claude-sonnet-4-6"
    api_key_env: "ANTHROPIC_API_KEY"
    max_tokens: 4096
    max_iterations: 5         # reviewers converge faster
    pool_size: 2
    temperature: 0.1          # more deterministic for review
  tools:
    - name: "test"
      command: "cargo test"
      timeout_secs: 300
      worktree: true          # run in worktree
    - name: "clippy"
      command: "cargo clippy -- -D warnings"
      timeout_secs: 120
      worktree: true
    - name: "fmt-check"
      command: "cargo fmt --check"
      timeout_secs: 30
      worktree: true
    - name: "fmt"
      command: "cargo fmt"
      timeout_secs: 30
      worktree: true
    - name: "build"
      command: "cargo build"
      timeout_secs: 300
      worktree: true
```

#### ToolResult

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolResult {
    pub tool_name: String,
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub duration_ms: u64,
    pub truncated: bool,     // true if output was truncated to fit context
}
```

#### AgentConfig (in Config)

```rust
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct AgentConfig {
    pub enabled: bool,
    pub implementer: AgentRoleConfig,
    pub reviewer: AgentRoleConfig,
    pub tools: Vec<ToolEntry>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct AgentRoleConfig {
    pub model: String,
    pub api_key_env: String,
    pub max_tokens: u32,
    pub max_iterations: u32,
    pub pool_size: u32,
    pub temperature: f32,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ToolEntry {
    pub name: String,
    pub command: String,
    pub timeout_secs: u64,
    pub worktree: bool,
}
```

### Agent Loop Design

The core of MVP3: how agents think and act.

#### Relationship to the Ralph Wiggum Loop

The `bin/loop.sh` outer loop from MVP1+MVP2 runs Claude Code as a subprocess with fresh context each iteration. MVP3 agents are the **in-daemon equivalent**: each agent iteration starts with fresh LLM context (no conversation history), with state persisted only in TaskStore. The key differences:

| | `bin/loop.sh` (MVP1-2) | In-Daemon Agent (MVP3) |
|---|---|---|
| **Process** | Separate Claude Code subprocess | Tokio task inside daemon |
| **Context** | `PROMPT.md` + `progress.txt` | System prompt + TaskStore queries |
| **State persistence** | `progress.txt` + git commits | TaskStore + AgentSession record |
| **Tool execution** | Claude Code's built-in tools | Configured tool catalog via `ToolRunner` |
| **Validation** | `otto ci` run externally after each iteration | Agent runs tools itself before proposing Bundle |
| **Communication** | File-based (read PROMPT.md, write progress.txt) | In-process `AgentIpcBridge` → handler dispatch |

The agent loop is **not** a port of `loop.sh` — it's a purpose-built implementation that follows the same Ralph Wiggum principle (fresh context, persistent state) while leveraging the daemon's IPC and FSM infrastructure.

#### Concrete Example: One Implementer Iteration

Here's what happens when an Implementer agent processes iteration #3 for Work "Add error handling to the parser":

1. **Load context**: Query TaskStore for the full hierarchy — Plan "Improve parser robustness", Spec "Error recovery design", Phase 2 "Implement error types", Work "Add error handling". Query Learnings scoped to this Work. Run `git diff --stat` and `git log --oneline -5` in the worktree to see current state.

2. **Construct prompt**: Assemble ~4K tokens of context:
   - System prompt (Implementer role, capabilities, rules)
   - Hierarchy summary (Plan → Spec → Phase → Work descriptions)
   - Previous iteration summary: "Iteration 2: Added ParseError enum and From impls. Tests failing: 2 of 5 error cases not handled."
   - Worktree state: "Modified files: src/error.rs, src/parser.rs. 2 uncommitted changes."
   - Available tools: test, clippy, fmt-check, fmt, build

3. **Call LLM**: Send prompt to Claude API. Stream tokens to TUI.

4. **Parse response**: LLM returns:
   ```json
   [
     {"action": "write_file", "path": "src/parser.rs", "content": "...updated parser with remaining error cases..."},
     {"action": "run_tool", "tool_name": "test", "args": []},
     {"action": "commit", "message": "feat(parser): handle remaining error cases", "paths": ["src/parser.rs"]},
     {"action": "run_tool", "tool_name": "clippy", "args": []},
     {"action": "done", "summary": "All 5 error cases handled. Tests pass. Clippy clean."}
   ]
   ```

5. **Execute actions** sequentially:
   - Write updated `src/parser.rs` to worktree
   - Run `cargo test` → exit code 0, all tests pass
   - Git commit the change
   - Run `cargo clippy` → exit code 0, no warnings
   - Done signal → mark AgentSession as Completed

6. **Persist**: Update AgentSession (iteration=3, status=Completed). The agent does **not** auto-propose a Bundle in this example — it signals Done, and the Coordinator decides when to propose.

#### Implementer Agent Loop

Each Implementer agent is assigned a single Work in `InProgress` state. It works in the Work's worktree.

```
┌─────────────────────────────────────────────────────────┐
│                 Implementer Agent Loop                    │
│                                                          │
│  1. Load Context                                         │
│     ├─ Plan → Spec → Phase → Work (full hierarchy)   │
│     ├─ Relevant Learnings (scoped to this Work)      │
│     ├─ Current worktree state (git diff, file listing)   │
│     ├─ Previous iteration summary (if any)               │
│     └─ Tool catalog (available tools)                    │
│                                                          │
│  2. Construct Prompt                                     │
│     ├─ System prompt: Implementer role, capabilities     │
│     ├─ Context: hierarchy + learnings + worktree state   │
│     ├─ Task: "Implement this Work"                   │
│     └─ Output format: JSON array of AgentActions         │
│                                                          │
│  3. Call LLM (streaming to TUI)                          │
│                                                          │
│  4. Parse Actions                                        │
│     └─ Extract AgentAction[] from LLM response           │
│                                                          │
│  5. Execute Actions (sequentially)                       │
│     ├─ WriteFile → write to worktree                     │
│     ├─ RunTool → subprocess in worktree, capture output  │
│     ├─ Commit → git commit in worktree                   │
│     ├─ ReadFile → read from worktree                     │
│     ├─ ProposeBundle → IPC to daemon                     │
│     ├─ CreateLearning → IPC to daemon                    │
│     ├─ Done → signal completion, break loop              │
│     └─ NeedHelp → pause agent, notify Coordinator        │
│                                                          │
│  6. Persist iteration results                            │
│     ├─ Update AgentSession (iteration count, status)     │
│     └─ Store iteration summary as Learning               │
│                                                          │
│  7. Check loop conditions                                │
│     ├─ Done action received? → exit                      │
│     ├─ NeedHelp action? → pause, wait for Coordinator    │
│     ├─ Max iterations reached? → pause + notify          │
│     ├─ Staleness detected? → refresh worktree, continue  │
│     └─ Otherwise → next iteration (go to 1)             │
│                                                          │
└─────────────────────────────────────────────────────────┘
```

#### Reviewer Agent Loop

Each Reviewer agent is assigned a Bundle in `Triaged` state. It computes the diff from the Bundle's worktree using `git diff <base_tick_sha>..<head>` and reads the related Work/Phase/Spec context.

**FSM update required:** The current transition table requires `Coordinator` role for `Triaged → Reviewed`. MVP3 must add `Reviewer` as an allowed role for this transition so Reviewer agents can record their verdict.

```
┌─────────────────────────────────────────────────────────┐
│                  Reviewer Agent Loop                     │
│                                                          │
│  1. Load Context                                         │
│     ├─ Bundle diff (computed via git diff in worktree)   │
│     ├─ Bundle metadata (claims, touched paths)           │
│     ├─ Work → Phase → Spec → Plan (hierarchy)        │
│     ├─ Relevant Learnings                                │
│     └─ Previous review comments (if re-review)           │
│                                                          │
│  2. Construct Prompt                                     │
│     ├─ System prompt: Reviewer role, review criteria     │
│     ├─ Context: bundle diff + hierarchy                  │
│     ├─ Task: "Review this Bundle"                        │
│     └─ Output format: ReviewResult JSON                  │
│                                                          │
│  3. Call LLM                                             │
│                                                          │
│  4. Parse ReviewResult                                   │
│     ├─ verdict: Approve | RequestChanges | Reject        │
│     ├─ issues: Vec<ReviewIssue>                          │
│     ├─ suggestions: Vec<String>                          │
│     └─ summary: String                                   │
│                                                          │
│  5. Execute Actions                                      │
│     ├─ Store review as Learning (scoped to Bundle)       │
│     ├─ If Approve → transition Bundle to Reviewed        │
│     ├─ If RequestChanges → transition Bundle to          │
│     │   Reviewed with comments, notify Implementer       │
│     └─ If Reject → transition Bundle to Rejected         │
│                                                          │
│  6. Done — Reviewer is typically single-iteration        │
│                                                          │
└─────────────────────────────────────────────────────────┘
```

#### Agent-Daemon Interaction

Agents interact with the daemon **through the same IPC protocol** that the TUI and CLI use. This is a critical design decision:

- Agents construct `DaemonRequest` messages and send them via an in-process channel (not a socket — they're in the same process)
- The daemon dispatches these requests through the same handler pipeline
- FSM validations, role guards, and parent checks all apply equally
- Agent actions are fully auditable — every state change goes through the same code path

```rust
/// In-process channel for agent ↔ daemon communication.
/// Avoids the overhead of Unix socket for same-process agents.
/// Uses the same dispatch() function as socket-based IPC — same FSM
/// validation, role guards, and parent checks apply.
pub struct AgentIpcBridge {
    stores: Arc<Stores>,
    event_tx: broadcast::Sender<DaemonEvent>,
    worktree_mgr: WorktreeManager,
    config: Config,
}

impl AgentIpcBridge {
    /// Send a request through the handler pipeline, same as socket-based IPC.
    /// The dispatch() signature matches the current codebase:
    /// dispatch(stores, event_tx, worktree_mgr, integrator_config, req)
    pub fn request(&self, req: DaemonRequest) -> DaemonResponse {
        handlers::dispatch(
            &self.stores,
            &self.event_tx,
            &self.worktree_mgr,
            &self.config.integrator,
            req,
        )
    }
}
```

### Tool Execution System

Tools are OS subprocesses. No embedded runtimes, no WASI, no containers. Just `Command::new()`.

```rust
pub struct ToolRunner {
    tools: HashMap<String, ToolEntry>,
}

impl ToolRunner {
    /// Execute a tool in the given working directory.
    pub fn run(
        &self,
        tool_name: &str,
        args: &[String],
        working_dir: &Path,
    ) -> Result<ToolResult> {
        let entry = self.tools.get(tool_name)
            .ok_or_else(|| eyre::eyre!("Unknown tool: {}", tool_name))?;

        let start = Instant::now();
        let mut cmd = Command::new("sh");
        cmd.arg("-c");

        // Compose: "command arg1 arg2"
        let full_command = if args.is_empty() {
            entry.command.clone()
        } else {
            format!("{} {}", entry.command, args.join(" "))
        };
        cmd.arg(&full_command);

        if entry.worktree {
            cmd.current_dir(working_dir);
        }

        let output = cmd
            .output()
            .context(format!("Failed to execute tool: {}", tool_name))?;

        let duration = start.elapsed();

        let mut stdout = String::from_utf8_lossy(&output.stdout).to_string();
        let mut stderr = String::from_utf8_lossy(&output.stderr).to_string();
        let mut truncated = false;

        // Truncate output to prevent blowing up context windows
        const MAX_OUTPUT: usize = 32_000; // ~8K tokens
        if stdout.len() > MAX_OUTPUT {
            stdout.truncate(MAX_OUTPUT);
            stdout.push_str("\n... (truncated)");
            truncated = true;
        }
        if stderr.len() > MAX_OUTPUT {
            stderr.truncate(MAX_OUTPUT);
            stderr.push_str("\n... (truncated)");
            truncated = true;
        }

        Ok(ToolResult {
            tool_name: tool_name.to_string(),
            exit_code: output.status.code().unwrap_or(-1),
            stdout,
            stderr,
            duration_ms: duration.as_millis() as u64,
            truncated,
        })
    }
}
```

**Timeout enforcement:** Tools have a configured `timeout_secs`. If exceeded, the subprocess is killed (SIGTERM, wait 5s, SIGKILL). The actual implementation uses `tokio::process::Command` (async subprocess) with `tokio::time::timeout`, not the sync `std::process::Command` shown in the pseudocode above. The sync version is shown for clarity; the real implementation is fully async since agents run in Tokio tasks.

**Why `sh -c` instead of direct execution?** Tool commands in config are shell expressions (`cargo test -- --test-threads=1`). Shell interpretation handles pipes, redirects, and argument splitting correctly.

### Streaming Architecture

Agents produce three types of streamable output:

1. **LLM tokens** — as the LLM generates its response
2. **Tool output** — stdout/stderr from tool execution
3. **Status events** — agent status changes (Starting, Running, WaitingForLlm, etc.)

All three flow through the existing `broadcast::Sender<DaemonEvent>` channel:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum AgentEvent {
    /// Agent status changed
    StatusChange {
        session_id: String,
        status: AgentStatus,
    },
    /// LLM is generating output (streaming tokens)
    LlmOutput {
        session_id: String,
        chunk: String,        // partial token output
        is_final: bool,       // last chunk of this LLM call
    },
    /// Tool execution started
    ToolStarted {
        session_id: String,
        tool_name: String,
    },
    /// Tool execution completed
    ToolCompleted {
        session_id: String,
        result: ToolResult,
    },
    /// Agent completed an action
    ActionCompleted {
        session_id: String,
        action_summary: String,
    },
    /// Agent iteration completed
    IterationCompleted {
        session_id: String,
        iteration: u32,
        summary: String,
    },
}
```

These events are wrapped in `DaemonEvent` and broadcast to all connected clients (TUI, CLI watchers).

**Event buffering:** The broadcast channel is ephemeral — events not consumed are lost. For the `agent.output` IPC method (which allows CLI clients to poll for recent events), each `AgentSession` maintains a bounded ring buffer of recent events (default: last 1000 events). This enables `loopr agent status <id>` to show recent output even if the CLI wasn't connected when the events occurred.

**Streaming LLM tokens:** MVP2's `ureq`-based client is synchronous and non-streaming. MVP3 needs streaming for real-time TUI updates. Options:

1. **`reqwest` with streaming** — panics inside Tokio runtime if using `blocking`, but `reqwest::Client` (async) works fine in a Tokio task. Since agents run as Tokio tasks (not in sync handler context), this is viable.
2. **`ureq` with chunked reading** — `ureq` supports reading response body incrementally. Less ergonomic but avoids a new dependency.
3. **`eventsource-client`** — dedicated SSE client. Anthropic's streaming API uses SSE. This is the cleanest fit.

**Decision:** Use `reqwest` (async) for the agent LLM client. The agent loop is fully async (Tokio task), so `reqwest` is natural. Keep `ureq` for the MVP2 DocValidator (sync, in handler context). Two different HTTP clients for two different execution contexts.

### Safety Guardrails

**Path validation:** `WriteFile` actions are restricted to paths within the agent's worktree. The action executor resolves the path, canonicalizes it, and rejects any path that escapes the worktree root:

```rust
fn validate_path(worktree_root: &Path, relative_path: &str) -> Result<PathBuf> {
    let full = worktree_root.join(relative_path).canonicalize()
        .unwrap_or_else(|_| worktree_root.join(relative_path));
    if !full.starts_with(worktree_root) {
        bail!("Path escapes worktree: {}", relative_path);
    }
    Ok(full)
}
```

**LLM failure handling:** When the LLM API call fails:
- **Transient errors (429, 500, 503):** Exponential backoff with jitter. Max 3 retries per iteration.
- **Persistent errors (401, 403):** Immediately pause agent with `Failed` status and notify Coordinator.
- **Parse failures:** Retry once with error feedback (same pattern as MVP2 DocValidator). If second attempt fails, pause agent with `NeedHelp` and include raw LLM output in the notification.
- **Max consecutive failures:** If 3 consecutive iterations fail (LLM or action execution), auto-pause the agent.

**External state changes:** When a Work's state changes externally (Coordinator transitions it to `Abandoned`, `Blocked`, etc.) while an agent is running:
- The agent checks Work status at the start of each iteration via `AgentIpcBridge`
- If Work is no longer `InProgress`, the agent stops gracefully (Completed or Cancelled depending on the new state)
- The current iteration is allowed to complete — no mid-iteration interruption

**Concurrent file conflicts:** Two Implementer agents working on overlapping files is detected at Bundle merge time (Integrator validates during Tick). MVP3 does not prevent this — advisory locks exist but aren't enforced. The Integrator's validation commands (tests, build) catch conflicts. This is the same correctness guarantee as MVP1, just with LLM agents instead of humans. Mandatory locking is deferred to MVP4.

### Staleness Cascade

When the Integrator publishes a Tick:

1. Daemon handler broadcasts `tick.published` event
2. Agent system listens for this event
3. For each running Implementer agent:
   - Compare the agent's Work `base_tick_id` with the new Tick number
   - If stale: set a "stale" flag on the agent
   - On next iteration, agent sees the stale flag
   - Agent refreshes worktree: `worktree_mgr.refresh(work_id, new_tick_sha)`
   - Agent updates Work's `base_tick_id` to the new Tick
   - Agent re-evaluates its progress against the new codebase

```rust
// In the agent loop, at the start of each iteration:
if agent_state.stale {
    // Refresh worktree to latest Tick
    let latest_tick = ipc_bridge.get_latest_published_tick()?;
    worktree_mgr.refresh(&work_id, &latest_tick.integration_sha)?;

    // Update Work's base_tick_id
    ipc_bridge.request(DaemonRequest {
        method: "work.update_base_tick".into(),
        params: json!({
            "id": work_id,
            "base_tick_id": latest_tick.id,
        }),
    });

    agent_state.stale = false;
    // Add staleness context to next prompt
    agent_state.context.push("Your worktree has been rebased to a new Tick. Review your changes for conflicts.");
}
```

### Context Window Management

Agent context must fit within the LLM's context window. Context is assembled with priority-based budgeting:

| Priority | Section | Typical Size | Max Budget |
|----------|---------|-------------|------------|
| 1 (highest) | System prompt | ~500 tokens | 1K tokens |
| 2 | Work description + acceptance criteria | ~200 tokens | 1K tokens |
| 3 | Previous iteration summary | ~200 tokens | 1K tokens |
| 4 | Worktree state (git diff --stat, modified files) | ~500 tokens | 2K tokens |
| 5 | Tool catalog | ~100 tokens | 500 tokens |
| 6 | Phase/Spec description | ~300 tokens | 2K tokens |
| 7 | Relevant Learnings | ~200 tokens | 2K tokens |
| 8 (lowest) | Plan description | ~100 tokens | 1K tokens |

**Total budget:** ~10K tokens for context, leaving the rest for LLM response. If any section exceeds its budget, it's truncated with a `(truncated — N tokens omitted)` marker.

For Reviewer agents, the Bundle diff replaces the worktree state section and gets a larger budget (up to 8K tokens) since the diff is the primary input.

### Prompt Design

#### Implementer System Prompt

```
You are an Implementer agent in the Loopr development orchestrator. Your role is
to implement a specific Work by writing code in a Git worktree.

## Your Capabilities

You can perform the following actions (respond with a JSON array of actions):

1. `write_file` — Create or overwrite a file in the worktree
2. `read_file` — Read a file from the worktree
3. `run_tool` — Execute a configured tool (test, clippy, fmt, build)
4. `commit` — Create a git commit with specified files
5. `propose_bundle` — Submit your work as a Bundle for review
6. `create_learning` — Record a discovery or insight
7. `done` — Signal that you've completed this iteration
8. `need_help` — Request human Coordinator intervention

## Rules

- Work incrementally. Don't try to implement everything in one iteration.
- Run tests after making changes. Fix failures before proposing a Bundle.
- Create small, focused commits with descriptive messages.
- When you encounter an ambiguity, create a Learning and ask for help.
- Your code must pass `clippy` and `fmt --check` before proposing a Bundle.
- Do not modify files outside the Work's scope.

## Output Format

Respond with ONLY a JSON array of AgentAction objects. Example:

```json
[
  {"action": "write_file", "path": "src/foo.rs", "content": "..."},
  {"action": "run_tool", "tool_name": "test", "args": []},
  {"action": "commit", "message": "feat(foo): add initial implementation", "paths": ["src/foo.rs"]},
  {"action": "done", "summary": "Implemented foo module with tests passing"}
]
```
```

#### Reviewer System Prompt

```
You are a Reviewer agent in the Loopr development orchestrator. Your role is to
review a Bundle (proposed code change) and provide structured feedback.

## Review Criteria

1. **Correctness** — Does the code do what the Work requires?
2. **Quality** — Is the code clean, idiomatic, and maintainable?
3. **Tests** — Are there adequate tests? Do they cover edge cases?
4. **Scope** — Does the change stay within the Work's boundaries?
5. **Safety** — Are there security concerns (OWASP top 10, injection, etc.)?

## Output Format

Respond with ONLY valid JSON matching this schema:

{
  "verdict": "approve" | "request_changes" | "reject",
  "issues": [
    {
      "severity": "error" | "warning" | "info",
      "file": "path/to/file.rs",
      "line": 42,
      "message": "Description of the issue",
      "suggestion": "How to fix it (optional)"
    }
  ],
  "summary": "Overall review summary"
}
```

### API Design

#### New IPC Methods

| Method | Params | Returns | Role | Description |
|--------|--------|---------|------|-------------|
| `agent.start` | `{agent_type, work_id?, bundle_id?}` | `AgentSession` | Coordinator | Start an agent for a Work or Bundle |
| `agent.stop` | `{session_id}` | `AgentSession` | Coordinator | Stop a running agent |
| `agent.pause` | `{session_id}` | `AgentSession` | Coordinator | Pause an agent (can resume later) |
| `agent.resume` | `{session_id}` | `AgentSession` | Coordinator | Resume a paused agent |
| `agent.status` | `{session_id}` | `AgentSession` | Any | Get current agent status |
| `agent.list` | `{status?, agent_type?}` | `[AgentSession]` | Any | List agent sessions with optional filters |
| `agent.output` | `{session_id, since?}` | `[AgentEvent]` | Any | Get recent output events for an agent |

#### New CLI Commands

```
loopr agent start implementer <work_id>     # Start Implementer agent
loopr agent start reviewer <bundle_id>            # Start Reviewer agent
loopr agent stop <session_id>                     # Stop an agent
loopr agent pause <session_id>                    # Pause an agent
loopr agent resume <session_id>                   # Resume a paused agent
loopr agent list                                  # List all agent sessions
loopr agent status <session_id>                   # Show agent status + recent output
```

#### Modified Handlers

The `work.transition` handler gains awareness of agent sessions:

- When a Work transitions to `InProgress` and `agents.enabled = true`, optionally auto-start an Implementer agent
- When a Work transitions to `Abandoned` or `Done`, stop any running agent for that Work
- When a Bundle transitions to `Triaged` and `agents.enabled = true`, optionally auto-start a Reviewer agent

Auto-start behavior is configurable:
```yaml
agents:
  auto_start_implementer: false  # manual by default
  auto_start_reviewer: false     # manual by default
```

### Implementation Plan

**Phase dependencies:**

```
Phase 1 (Agent Foundation)
    │
    ├──→ Phase 3 (Tool System) ──┐
    │                            ├──→ Phase 2 (Implementer) ──→ Phase 4 (Streaming)
    └────────────────────────────┘         │                          │
                                           │                          │
                                           └──→ Phase 5 (Reviewer) ──┘
                                                                      │
                                           Phase 6 (Staleness) ──────┘
                                               │
                                               └──→ Phase 7 (TUI + CLI + Polish)
```

Phase 2 (Implementer) depends on both Phase 1 (Foundation) and Phase 3 (Tool System). Phase 3 can be developed in parallel with Phase 1. Phase 5 (Reviewer) depends on Phase 2. Phase 6 (Staleness) is independent until integration.

#### Phase 1: Agent Foundation

1. Add `AgentConfig`, `AgentRoleConfig`, `ToolEntry` to `Config`
2. Create `src/agents/mod.rs` — `AgentType`, `AgentStatus`, `AgentSession` types
3. Implement `Record` for `AgentSession` (TaskStore persistence)
4. Create `AgentIpcBridge` for in-process daemon communication
5. Add `agent.start`, `agent.stop`, `agent.pause`, `agent.resume`, `agent.status`, `agent.list` handlers
6. Agent session lifecycle management (create, track, cleanup)

#### Phase 2: Implementer Agent

1. Create `src/agents/implementer.rs` — the Implementer agent loop
2. Implement context loading (hierarchy traversal, Learnings, worktree state)
3. Implement prompt construction from system prompt + context
4. Add `AgentAction` enum and JSON parsing
5. Implement action execution (WriteFile, ReadFile, Commit, ProposeBundle, etc.)
6. Wire Implementer into Tokio task spawning from `agent.start` handler
7. Iteration tracking and `max_iterations` safety cap
8. Error handling and graceful failure

#### Phase 3: Tool Execution System

1. Create `src/tools/mod.rs` — `ToolRunner`, `ToolResult`
2. Implement `ToolRunner::run()` with subprocess execution
3. Add timeout enforcement with SIGTERM → SIGKILL escalation
4. Output truncation for context window management
5. Tool catalog loading from config
6. Integration with agent action execution

#### Phase 4: Streaming

1. Add `AgentEvent` variants to the event system
2. Replace `ureq` with `reqwest` (async) for agent LLM client
3. Implement SSE streaming from Anthropic API
4. Forward LLM token chunks through broadcast channel
5. Forward tool output through broadcast channel
6. Agent status change events

#### Phase 5: Reviewer Agent

1. Create `src/agents/reviewer.rs` — the Reviewer agent loop
2. Implement review context loading (Bundle diff, hierarchy, Learnings)
3. Implement review prompt construction
4. Parse `ReviewResult` from LLM response
5. Execute review actions (transition Bundle, create Learning)
6. Wire Reviewer into Tokio task spawning

#### Phase 6: Staleness Cascade

1. Listen for `tick.published` events in agent system
2. Identify stale Implementer agents (compare base_tick_id)
3. Set stale flag and inject staleness context into next iteration
4. Worktree refresh on stale detection
5. Update Work `base_tick_id` after refresh

#### Phase 7: TUI + CLI + Polish

1. Add Agent view to TUI (list agents, show live output)
2. Add `loopr agent *` CLI commands
3. Agent session detail view (iterations, actions, tool results)
4. Dashboard integration (agent count, active/paused status)
5. Comprehensive integration tests
6. Documentation

### LLM Client Architecture

MVP3 introduces a second LLM client alongside the MVP2 DocValidator:

```
┌────────────────────────────────────────────────────┐
│                  LLM Clients                        │
│                                                     │
│  ┌──────────────────┐   ┌────────────────────────┐ │
│  │ DocValidator      │   │ AgentLlmClient          │ │
│  │ (MVP2, unchanged) │   │ (new in MVP3)           │ │
│  │                   │   │                         │ │
│  │ • ureq (sync)     │   │ • reqwest (async)       │ │
│  │ • non-streaming   │   │ • streaming SSE         │ │
│  │ • handler context │   │ • Tokio task context    │ │
│  │ • blocking OK     │   │ • fully async           │ │
│  │ • single call     │   │ • iterative loop        │ │
│  └──────────────────┘   └────────────────────────┘ │
└────────────────────────────────────────────────────┘
```

The `AgentLlmClient` wraps `reqwest::Client` and handles:
- Anthropic Messages API with streaming (`stream: true`)
- SSE event parsing (Anthropic's `message_start`, `content_block_delta`, `message_stop`)
- Token-by-token forwarding to broadcast channel
- Response accumulation for action parsing
- Rate limit handling (429 → exponential backoff)
- System prompt + context assembly

```rust
pub struct AgentLlmClient {
    client: reqwest::Client,
    config: AgentRoleConfig,
}

impl AgentLlmClient {
    /// Send a prompt and stream the response.
    /// Yields chunks to the caller via a channel.
    pub async fn call_streaming(
        &self,
        system_prompt: &str,
        messages: &[Message],
        chunk_tx: mpsc::Sender<String>,
    ) -> Result<String> {
        // ... SSE streaming implementation
    }

    /// Non-streaming call for simpler use cases.
    pub async fn call(&self, system_prompt: &str, messages: &[Message]) -> Result<String> {
        // ... single response
    }
}
```

## Alternatives Considered

### Alternative 1: Claude Code SDK / Agent SDK

- **Description:** Use Anthropic's official Claude Code SDK or Agent SDK to implement agents, rather than building a custom agent loop.
- **Pros:** Mature tool use, computer use, file editing capabilities built-in. Supported and maintained by Anthropic.
- **Cons:** SDK is opinionated about agent architecture. Agents would run as separate processes, not Tokio tasks. Loses the single-daemon-as-authority correctness guarantee. IPC overhead between daemon and agent processes. Less control over prompt construction and action parsing.
- **Why not chosen:** Loopr's core value proposition is daemon-mediated correctness. Agents must go through the FSM validation pipeline. Running agents as external SDK processes would require bridging the correctness gap — essentially re-implementing the daemon's authority checks in the agent process. The custom agent loop (prompt → parse → execute via IPC) is simpler and preserves the architecture.

### Alternative 2: Gas Town Model — Agents as Separate Claude Code Sessions

- **Description:** Each agent is a separate Claude Code session in a tmux pane, communicating through TaskStore (file-based).
- **Pros:** Dead simple to implement. Leverages Claude Code's existing tool use. Each agent gets full Claude Code capabilities.
- **Cons:** Multi-writer chaos (the reason Gas Town has "nondeterministic idempotence"). No FSM enforcement — agents write records directly. Race conditions. No streaming to TUI. Harder to coordinate. This is exactly what Loopr was designed NOT to be.
- **Why not chosen:** Violates the fundamental architectural principle. The daemon is the single authority. Agents must not bypass it.

### Alternative 3: Agent as External Process with IPC

- **Description:** Agents run as separate OS processes, communicating with the daemon over Unix socket IPC (same as TUI).
- **Pros:** Process isolation — agent crash doesn't take down daemon. Can use different languages/runtimes. Straightforward resource limits (cgroups, memory).
- **Cons:** IPC overhead for every action. Subprocess management complexity. Can't share broadcast channels natively. Harder to implement streaming. More moving parts.
- **Why not chosen:** Over-engineering for a single-user local tool. Tokio tasks provide sufficient isolation. If an agent panics, Tokio's panic handling catches it without crashing the daemon. The in-process `AgentIpcBridge` is simpler and faster.

### Alternative 4: Function-Calling API Instead of Action Parsing

- **Description:** Use Anthropic's tool_use/function-calling API feature instead of asking the LLM to output JSON arrays of actions.
- **Pros:** More structured. The API enforces the action schema. Less parsing code.
- **Cons:** While tool_use supports multiple parallel tool calls, the results must be sent back in a follow-up message, creating a multi-turn conversation per iteration. Each tool execution (especially `RunTool`) requires a round-trip: agent calls tool → daemon returns result → agent decides next tool. This multi-turn pattern increases latency and cost compared to the batch-action approach (one LLM call returns a full action plan).
- **Why not chosen:** The batch-action pattern (one LLM call → multiple actions executed sequentially) is more efficient for agents that can plan ahead. However, this is revisitable — if action parsing proves error-prone, a hybrid approach (tool_use for structured actions like ProposeBundle/Transition, batch for file writes) may be worth exploring.

### Alternative 5: Shared Async LLM Client (Replace ureq Everywhere)

- **Description:** Replace `ureq` in the DocValidator with `reqwest` (async) too, so there's one HTTP client.
- **Pros:** One dependency instead of two. Consistent API.
- **Cons:** DocValidator runs in sync handler context. Adding `reqwest` there requires `tokio::task::spawn_blocking` or restructuring handlers to be async. The handler architecture is sync by design (sub-millisecond operations). Making it async for one infrequent operation (validation) adds complexity everywhere.
- **Why not chosen:** Two clients for two contexts is the right trade-off. `ureq` is tiny and works perfectly in sync handler context. `reqwest` works in async Tokio tasks. They don't conflict.

## Technical Considerations

### Dependencies

**New runtime dependencies:**

- `reqwest` — async HTTP client for agent LLM calls. Features: `json`, `stream`. Brings `hyper`, `http`, `tokio-rustls`.
- `tokio-stream` — already in use, needed for SSE parsing.
- `futures` — already in use, needed for stream combinators.

**No new external dependencies beyond reqwest.** The agent system, tool execution, and streaming are built on Tokio + std.

### Performance

- **LLM API latency** is the dominant cost. Each agent iteration takes 5-30 seconds (LLM response time). Tool execution adds 1-300 seconds depending on the tool (test suites vary widely).
- **Agent pool bounds** prevent runaway resource usage. Default 2 Implementers + 2 Reviewers = 4 concurrent LLM calls maximum.
- **Tool subprocesses** run with configured timeouts. Runaway processes are killed.
- **Output truncation** prevents tool output from blowing up context windows. Default cap: 32KB per tool output (~8K tokens).
- **Broadcast channel** handles event fan-out efficiently. Agents produce events at human-readable rates (not millions/sec).
- **In-process IPC bridge** has zero network overhead. Handler dispatch is sub-millisecond.

### Security

- **API keys** remain in environment variables, never stored in config or TaskStore.
- **Tool execution** runs arbitrary shell commands. This is **by design** — the tool catalog is configured by the project owner. But it means:
  - Tools execute with the daemon's user permissions
  - Tools run in worktrees, not in the main repo
  - No sandboxing in MVP3 (future consideration)
- **LLM-generated code** is not automatically trusted. The review pipeline (Reviewer agent → human Coordinator → Integrator validation) provides defense in depth.
- **Agent actions go through FSM validation** — an agent cannot force an invalid state transition. The daemon rejects it the same way it would reject an invalid TUI command.
- **No network exposure** — agents are in-process Tokio tasks. No new sockets or ports.

### Testing Strategy

**Unit tests:**
- Agent loop state machine (Starting → Running → WaitingForLlm → etc.)
- Action parsing (valid JSON, malformed JSON, missing fields)
- Tool execution (mock subprocess, timeout, output truncation)
- Prompt construction (hierarchy context assembly, Learnings injection)
- Staleness detection (base_tick_id comparison)
- Agent session lifecycle (create, pause, resume, stop, cleanup)

**Integration tests:**
- Full Implementer loop with mock LLM: Work → agent writes file → runs test tool → proposes Bundle
- Full Reviewer loop with mock LLM: Bundle → agent reviews → approves/rejects
- Staleness cascade: publish Tick → agent detects staleness → refreshes → continues
- Agent pool bounds: start more agents than pool_size → queued or rejected
- Agent crash recovery: agent session persists in TaskStore → restart recovers

**Mock LLM for tests:**
- The `AgentLlmClient` accepts a trait-based interface for testability
- Mock returns canned action sequences for predictable test scenarios
- Mock can simulate streaming (yield chunks with delays)

### Rollout Plan

1. **Phase 1-3** ship together — Foundation + Implementer + Tools. This gives a working (non-streaming) Implementer that can take a Work and produce a Bundle.
2. **Phase 4** (Streaming) ships next — adds real-time visibility.
3. **Phase 5** (Reviewer) ships after — completes the implement+review loop.
4. **Phase 6** (Staleness) ships after — handles multi-agent coordination.
5. **Phase 7** (TUI + CLI) ships last — polishes the user experience.

Agents are opt-in (`agents.enabled = false` by default). The human-driven workflow from MVP1+MVP2 continues to work unchanged.

## Success Criteria

| # | Criterion | How to Verify |
|---|-----------|---------------|
| 1 | Implementer agent takes a Work from InProgress and produces a Bundle | `loopr agent start implementer <wi_id>` → agent writes code → `loopr bundle list` shows new Bundle |
| 2 | Implementer agent runs tools (test, clippy) before proposing Bundle | Agent output shows tool execution with pass/fail results |
| 3 | Reviewer agent reviews a Bundle and provides structured feedback | `loopr agent start reviewer <bundle_id>` → Bundle transitions to Reviewed with review comments |
| 4 | Agent output streams to TUI in real-time | Open TUI → start agent → Agent view shows live LLM output and tool results |
| 5 | Staleness cascade notifies agents when Tick publishes | Publish Tick → running Implementer agent refreshes worktree on next iteration |
| 6 | Agent pool bounds are enforced | Start 5 Implementers with pool_size=2 → only 2 run, 3 are rejected/queued |
| 7 | Agent actions go through FSM validation | Agent attempts invalid transition → daemon rejects → agent handles gracefully |
| 8 | Agents disabled by default | Default config → no agents, human-driven workflow unchanged |
| 9 | Agent sessions persist in TaskStore | Start agent → restart daemon → `loopr agent list` shows previous sessions |
| 10 | Max iterations cap prevents runaway agents | Set max_iterations=3 → agent pauses after 3 iterations with notification |
| 11 | Coordinator can pause/resume/stop agents | `loopr agent pause <id>` → agent stops at next iteration boundary → `resume` continues |
| 12 | Tool timeout kills runaway subprocesses | Configure tool with timeout_secs=5 → run long tool → killed after 5s |

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| LLM produces unparseable action JSON | High | Medium | Retry with error feedback (same pattern as DocValidator). Fall back to `NeedHelp` action if retry fails. |
| LLM writes incorrect/broken code | High | Low | Defense in depth: agent runs tools (test, clippy) before proposing Bundle. Reviewer agent catches issues. Integrator validates during Tick. Human Coordinator gates. |
| Agent gets stuck in a loop (same error each iteration) | Medium | Medium | Max iteration cap. Detect repeated failures (same error 3x → pause + notify). Staleness detection on self-produced diffs. |
| Runaway tool subprocess consumes resources | Medium | High | Timeout enforcement (SIGTERM → wait → SIGKILL). Memory limits via `ulimit` in tool command. |
| `reqwest` dependency size/compile time increase | Low | Low | `reqwest` is widely used. Only needed for agent module. Feature-gate if compile time becomes an issue. |
| Anthropic API rate limits under concurrent agents | Medium | Medium | Pool bounds (default 2-4 agents). Rate limit detection (429) with exponential backoff. Configurable pool size. |
| Agent crash takes down daemon | Low | High | Tokio task panic handling. `catch_unwind` wrapper around agent loop. Agent crash → Failed status, daemon continues. |
| Streaming SSE parsing edge cases | Medium | Low | Use battle-tested SSE parsing (Anthropic's documented format). Fallback to non-streaming on parse failure. |
| Context window overflow (too much hierarchy + diff + tool output) | Medium | Medium | Output truncation. Priority-based context assembly (Work description > Learnings > full hierarchy). Configurable max_tokens per context section. |
| Two HTTP client dependencies (ureq + reqwest) | Low | Low | They serve different execution contexts. Consider consolidating in MVP4 if sync handler context becomes async. |
| Uncontrolled API costs from long-running agents | Medium | Medium | Max iterations cap. Token usage logged per iteration. Configurable `max_tokens_per_session` budget (future). Dashboard shows cumulative token usage. |
| Agent writes files outside worktree (path traversal) | Low | High | Path validation canonicalizes and rejects paths escaping worktree root. `WriteFile` is sandboxed to the worktree. |

## Open Questions

- [ ] Should agents use Anthropic's `tool_use` API feature for critical actions (ProposeBundle, Transition), even if it means multiple API calls per iteration?
- [ ] Should there be a `Researcher` agent type in MVP3 for exploration tasks, or defer to MVP4?
- [ ] Should agent auto-start be tied to Work/Bundle transitions, or always require explicit `agent.start`?
- [ ] How should agents handle merge conflicts when refreshing stale worktrees? Pause and notify, or attempt automatic resolution?
- [ ] Should `AgentSession` store full iteration transcripts (all actions + tool output), or just summaries?
- [ ] What's the right default `max_iterations` cap? Too low and agents can't complete complex tasks. Too high and costs escalate.
- [ ] Should the Reviewer agent be allowed to run tools in the worktree (e.g., `cargo test` to verify claims), or should it be read-only?
- [ ] Should there be a cost tracking / token usage tracking system for agent sessions?

## References

- `docs/design/2026-02-25-orchestration-spine.md` — MVP1 design doc (orchestration spine)
- `docs/design/2026-02-26-taskstore-doc-validator.md` — MVP2 design doc (TaskStore + Doc Validator)
- `docs/mvps.md` — MVP phase comparison table
- `docs/v3-chatgpt-loopr-architecture-conversation.md` — Original architecture conversation with ChatGPT
- `docs/v3-claude-loopr-mvp-and-fsm-conversation.md` — FSM architecture discussion
- `docs/v3-preplan-conversation.md` — Pre-design synthesis (IPC vs file-based, Gas Town analysis)
- `scottidler/taskstore` — TaskStore crate
- Anthropic Messages API — `/v1/messages` endpoint with streaming
