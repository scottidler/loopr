# Loopr v3 - Build Progression

How Loopr was built, layer by layer. All layers below are implemented unless noted.

## Layer 1: Orchestration Spine
Daemon, FSMs (Work/Bundle/Tick), TaskStore, IPC, worktrees, TUI.
- Design: `2026-02-25-orchestration-spine.md`
- Principle: "Your hardest problems are not LLM problems. Prove the system works first."

## Layer 2: Read-Only Intelligence
Doc Validator (sync LLM, structured reports, gates transitions).
- Design: `2026-02-26-taskstore-doc-validator.md`
- Principle: "Safest entry point for intelligence: read-only, can't break Tick semantics."

## Layer 3: Code-Level Agents
Implementer + Reviewer agents. Tool execution in worktrees. Streaming.
- Design: `2026-02-26-implementer-reviewer-agents.md`
- Principle: "Everything plugs into the backbone Layer 1 built."

## Layer 4: Full Agent Roster
Coordinator, Researcher, deterministic Integrator. Multi-level RWL. Context builder. Strategy knobs.
- Design: `2026-02-26-multi-level-rwl.md`
- Principle: "RWL at every level. The Coordinator is the meta-Ralph."

## Layer 5: Pipeline Hardening
Coordinator sequencing, structural fixes, type safety audit, worktree reliability. Agent self-correction. Pull-based work queue. SLA recovery.
- Designs: `2026-02-28-coordinator-sequencing.md`, `2026-02-28-structural-fixes.md`, `2026-02-28-type-safety-audit.md`, `2026-02-28-worktree-implementer-reliability.md`, plus ~15 targeted fix docs.
- Principle: "Fix every edge case. Compile-time contracts across IPC boundaries."

## Layer 6: Chat + Agentic Tool Loop
TUI Chat view. Unified Tool trait with 14+ builtins. Agentic loop with streaming, context compaction, delegation. Chat-to-Plan funnel.
- Designs: `2026-03-03-tui-chat-view.md`, `2026-03-04-native-tool-use.md`, `2026-03-04-unified-tool-system.md`, `2026-03-05-chat-agentic-tool-loop.md`, `2026-03-06-chat-performance.md`
- Principle: "The chat IS the interface. Orchestration grows from conversation."

## Layer 7: Semantic Decomposition (Draft - Partial)
Coverage Evaluator (done). Upward feedback / bubble-up (not wired). Collaborative Plan creation interview flow (FSM state exists, IPC flow incomplete).
- Design: `2026-03-03-semantic-decomposition.md`
- Principle: "Pit of success: high-quality Plans recursively beget high-quality code."

## Remaining Work
- `2026-03-01-file-touch-broadcasting.md` - file-touch advisory lock auto-acquisition (Draft - not started)
- See `docs/design/remaining-gaps.md` for small gaps from audit/completion docs
