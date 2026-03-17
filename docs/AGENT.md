# docs/ - Agent Guide

This directory contains the design history and reference material for Loopr v3. Read this first to orient yourself.

## Quick Map

| File | What It Is | Read When |
|------|-----------|-----------|
| `design/mvps.md` | Build progression - how Loopr was built layer by layer | You need the big picture |
| `design/2026-02-25-orchestration-spine.md` | Layer 1: daemon, FSMs, TaskStore, IPC, worktrees, TUI | You need to understand the spine |
| `design/2026-02-26-multi-level-rwl.md` | Layer 4: Coordinator, Researcher, Integrator, multi-level RWL | You need to understand the agent roster |
| `design/2026-03-05-chat-agentic-tool-loop.md` | Layer 6: chat with agentic tool loop, streaming, delegation | You need to understand the chat system |
| `design/remaining-gaps.md` | Consolidated list of unfinished work | You need to know what's left to build |
| `v2-proven-patterns.md` | 13 infrastructure patterns carried from v2 | You need to understand the implementation blueprint |
| `hierarchy.md` | Domain hierarchy: Plan, Spec, Phase, Work, Bundle, Tick | You need to understand the domain model |
| `related-works.md` | Prior art: Aider, SWE-agent, OpenHands, Stripe Minions, Gas Town | You want external references |

## Reading Order

1. **`design/mvps.md`** - 2 minutes. The layer-by-layer build progression.
2. **`design/2026-02-25-orchestration-spine.md`** - 20 minutes. The foundation everything builds on.
3. **`design/2026-02-26-multi-level-rwl.md`** - 20 minutes. The full agent roster and RWL.
4. **`design/2026-03-05-chat-agentic-tool-loop.md`** - 10 minutes. The chat interface.
5. **`design/remaining-gaps.md`** - 5 minutes. What's left to build.
6. Everything else is reference material. Read as needed.

## History

### Build Attempts (v1, v2, v3)

- **v1**: Proof of concept. Established that worktrees + TaskStore could work.
- **v2**: Infrastructure push. 65+ source files. Proved daemon, IPC, TUI, LLM client patterns. Hit a wall on domain model generality.
- **v3** (current): Clean-slate rebuild. Carries v2's proven patterns, none of its code. Domain model from the architecture conversations (Plan/Spec/Phase/Work, 3 FSMs, role guards).

### Inspiration

The Yegge docs (`yegge/`) are inspiration, not specification. Loopr learns from Gas Town's multi-agent concept but rejects its multi-writer file-based coordination in favor of daemon-mediated single-writer correctness.
