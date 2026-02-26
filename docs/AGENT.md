# docs/ — Agent Guide

This directory contains the design history and reference material for Loopr v3. Read this first to orient yourself.

## Quick Map

| File | What It Is | Read When |
|------|-----------|-----------|
| `mvps.md` | Matrix comparing MVP1, MVP2, MVP3+ — the phased implementation plan for v3 | You need to understand what each MVP phase delivers and why |
| `design/2026-02-25-loopr-v3-mvp1.md` | MVP1 design doc. Full v3 MVP1 specification: architecture, data model, FSMs, IPC, TUI, worktrees. 5 review passes completed. **MVP1 is complete.** | You need to understand the existing spine architecture |
| `design/2026-02-26-loopr-v3-mvp2.md` | **The design doc.** Full v3 MVP2 specification: TaskStore persistence, Doc Validator LLM, TUI event processing. 5 review passes completed. | You need to build, review, or understand the current implementation target |
| `v2-proven-patterns.md` | 13 infrastructure patterns that worked in v2 (fork-to-daemon, IPC protocol, TUI architecture, crash recovery, etc.) with code examples | You need to understand *how* to implement v3's infrastructure — these patterns are the blueprint |
| `v2-implementation-status.md` | What v2 actually built vs. what was just designed. Includes full module map of v2's 65+ files. | You need to know what the prior build attempt accomplished |
| `v2-light-loops-heavy-tools.md` | The "Light Loops, Heavy Tools" design principle: Tokio tasks for LLM loops, OS subprocesses for tool execution | You need to understand the concurrency model (relevant for MVP3+) |
| `v3-preplan-conversation.md` | Conversation that settled key v3 decisions: keep daemon+IPC (not Gas Town's file-based coordination), carry patterns not code, TaskStore is storage not communication bus | You need to understand *why* v3's architecture was chosen |
| `v3-chatgpt-loopr-architecture-conversation.md` | Original architecture conversation establishing the domain model: Plan/Spec/Phase/WorkItem hierarchy, Ralph Wiggum Loop, persona model, Tick semantics, strategy knobs | You need deep context on the domain model and design rationale |
| `v3-claude-loopr-mvp-and-fsm-conversation.md` | Conversation that scoped MVP phasing (MVP1=no LLM, MVP2=validator, MVP3+=agents) and designed the 3 FSMs (WorkItem, Bundle, Tick) | You need to understand FSM design decisions and MVP phasing rationale |
| `yegge/welcome-to-gas-town.md` | Steve Yegge's Gas Town launch post — the multi-agent orchestration concept that inspired Loopr | You want to understand what Loopr is inspired by (and what it rejects) |
| `yegge/the-future-of-coding-agents.md` | Yegge's predictions for coding agents in 2026 | Background reading |
| `yegge/gas-town-emergency-user-manual.md` | Gas Town user guide — illustrates the chaos (Murder Mystery, heresies, stale workers) that Loopr's correctness-first approach is designed to prevent | You want to understand *why* Loopr rejects Gas Town's multi-writer coordination |

## Reading Order

1. **`mvps.md`** — 2 minutes. Get the MVP1 → MVP2 → MVP3+ arc.
2. **`design/2026-02-26-loopr-v3-mvp2.md`** — 20 minutes. The current build target. Start here.
3. **`design/2026-02-25-loopr-v3-mvp1.md`** — Reference for the existing spine architecture (MVP1 is complete).
4. Everything else is reference material. Read as needed.

## History

### Build Attempts (v1, v2, v3)

Loopr has been through three major build attempts:

- **v1**: Proof of concept. Completed a 10-phase build but the architecture was unsatisfactory. Established that worktrees + TaskStore could work together.
- **v2**: Infrastructure push. Built a real daemon, IPC protocol, TUI, LLM client, tool system, and more (65+ source files). Proved that the client-fork-to-daemon pattern, NDJSON IPC, and thin-TUI architecture all work. Hit a wall because the domain model was too generic (single `Loop` type trying to do everything) and complexity grew faster than end-to-end functionality.
- **v3** (current): Clean-slate rebuild. Carries v2's 13 proven patterns but none of its code. Uses the domain model from the ChatGPT architecture conversation (Plan → Spec → Phase → WorkItem hierarchy, 3 FSMs, role-based guards).

### MVP Phases (within v3)

v3 is implemented in three MVP phases — see `mvps.md` for the full matrix:

- **MVP1**: No LLM. Human acts as all personas. Proves the orchestration spine.
- **MVP2**: LLM as read-only doc validator. Safest entry point for intelligence.
- **MVP3+**: LLM implementers + reviewers. The full "dev team in a box" vision.

### Inspiration

The Yegge docs (`yegge/`) are inspiration, not specification. Loopr learns from Gas Town's multi-agent concept but explicitly rejects its multi-writer file-based coordination in favor of daemon-mediated single-writer correctness.
