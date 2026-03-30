# Loopr Architectural Analysis - 2026-03-29

Based on a comprehensive investigation of the loopr project's documentation and codebase, alongside research into the Obsidian knowledge base using the oracle tool, here is an architectural evaluation and analysis of the requested concepts.

---

## 1. Loopr Architecture and Codebase Evaluation

### Architecture Overview

Loopr (specifically its v3 iteration) is a sophisticated, single-authority daemon-based orchestrator designed to act as a "dev team in a box." It orchestrates AI agents using a rigorous Finite State Machine (FSM) approach combined with a git-backed data plane called TaskStore.

- **The Hierarchy:** The domain model is rigorously structured into a hierarchy: `Plan -> Spec -> Phase -> Work -> Bundle -> Tick`.
- **The Agent Roster:** It features specialized roles including a Coordinator (Project Manager/Orchestrator), Researcher (read-only investigator), Implementer (code writer), Reviewer (code reviewer), and a critical, non-LLM, fully deterministic Integrator (state machine merging).
- **Core Philosophy:** A central tenet of the system is that "Your hardest problems are not LLM problems. Prove the system works first." Loopr rejects the chaos of multi-writer agent swarms, instead forcing all mutations through the daemon's FSM via Inter-Process Communication (IPC using Unix sockets).

### Strengths and Design Validation

- **Deterministic Bounds:** By utilizing an Integrator task that operates entirely without LLMs, loopr ensures that its merge-validate-publish pipeline remains immune to LLM hallucinations.
- **Single-Writer Authority:** Centralizing state changes within the daemon sidesteps the file-corruption and race-condition issues that plague decentralized agent swarms.
- **Context Builder:** The use of token budgeting and targeted slicing (e.g., scoping context down to the immediate Work and relevant Learnings) effectively mitigates context-window overflow, a major bottleneck in long-running agentic loops.

### Perceived Next Steps for Loopr

1. **Semantic Decomposition (Upward Feedback):** According to `docs/design/mvps.md`, the "bubble-up" mechanism of the coverage evaluator is not yet fully wired. Completing this pipeline will allow lower-level agents to influence and adjust higher-level plans dynamically.
2. **File-Touch Broadcasting:** Implementing the file-touch advisory lock auto-acquisition (as detailed in `2026-03-01-file-touch-broadcasting.md`) is necessary to prevent Implementer agents from encountering race conditions or conflicting in the same worktree.
3. **Learning Garbage Collection:** MVP4 introduced a confidence scoring system for Learnings, but the decay formula and garbage collection (GC) were deferred. As the system scales, implementing this GC will be crucial to prevent the context window from filling up with obsolete or degraded insights.

---

## 2. Conceptual Investigation (Obsidian Knowledge Base)

Using the oracle tool, the following concepts were investigated to understand how they map to the state of the industry and how loopr implements them.

### A. "Gastown" and "Beads"

**Definition:** "Gastown" is a multi-agent orchestration framework conceptualized by Steve Yegge. It acts as a "factory" for agents (in contrast to a single Claude Code session, which acts as a solitary "worker"). It relies on the MEOW stack (Molecular Expression of Work). "Beads" is the fundamental data and control plane for Gastown - a git-backed issue tracking system where tasks are represented as "yellow sticky notes" (JSON lines) stored directly in the repository.

The oracle vault's note titled "Gas Town vs KAI/PAI: Multi-Agent Orchestrator Comparison" describes Gas Town as having massive parallelism designed for industrial-scale refactoring with git-backed recovery so no work is lost. However, it notes significant weaknesses: it "can wreck your shit in an instant," requires high cognitive load ("chimp wrangling"), and suffers from merge conflicts at scale. Beads is specifically called out in this context as the data layer that suffers from these merge conflicts at scale.

**Relevance to Loopr:** Loopr explicitly borrows the git-backed persistence mechanism from Beads (via its TaskStore) to ensure that agents can survive crashes and easily recover state. However, Loopr's documentation states that it *rejects* Gastown's multi-writer, ad-hoc file-based coordination in favor of a centralized daemon to maintain strict FSM correctness. Loopr takes the git-backed persistence of Gas Town (via TaskStore and worktrees) but completely rejects Gas Town's chaotic "wreck your shit" parallelism by putting a single, non-LLM Integrator daemon in charge of state transitions to prevent the merge conflicts that plague Beads.

### B. Jeffrey Emanuel's "Rule of Five"

**Definition:** Discovered by Jeffrey Emanuel, the "Rule of Five" is a manual agentic coding principle stating: *"When in doubt, have the agent review its own work 5 times."* It involves an agent reviewing its own output iteratively, focusing on different areas until the code converges to production quality.

The oracle vault explicitly defines this as an agentic coding principle: "When in doubt, have the agent review its own work 5 times." The core insight from the vault is that AI coding agents produce dramatically better output when forced to iteratively review their own work 4-5 times, at which point the output "converges." The vault includes the specific 5 prompts Emanuel uses, moving from a "fresh eyes review" of the system, to self-reviewing changes, identifying the "weakest/worst parts of the system", conducting "Peer Agent Review (Deep Analysis)", and finally a mandate to "fix ALL of them completely."

**Relevance to Loopr:** Loopr formalizes this human insight into its automated validation pipelines. Instead of relying on a single LLM to get it right on the first try, Loopr forces agents through a rigorous `generate -> validate -> iterate` loop. It utilizes a dedicated Reviewer agent and a Doc Validator to mathematically mimic the Rule of Five's convergence, guaranteeing higher code quality before integration. It then uses the Rule of Five philosophy via its Reviewer and validation pipelines to ensure the deterministic loops converge to passing tests before anything is published.

### C. "Ralph Wiggum Loops"

**Definition:** Coined by Geoffrey Huntley (named after the *Simpsons* character), a "Ralph Wiggum Loop" is an iteration pattern where LLM agents are run continuously with a fresh, blank context window on each run. The agent has no memory of prior iterations; all progress is carried forward entirely via external state (files, git commits, or a database). It embraces being "deterministically bad in an indeterministic world" by continuously hammering away at a problem until tests pass.

The oracle vault's "Ralph Wiggum Loop: Research Summary" note explicitly compares the Ralph Loop to the ReAct pattern:
- **Context management:** It relies on a "fresh context per iteration" (unlike ReAct's single persistent window).
- **Control locus:** It relies on "external verification" rather than internal LLM judgment.
- **Completion detection:** It uses "stop hooks + file system state".
- **Best for:** "Verifiable, mechanical tasks" rather than dynamic, adaptive reasoning.

**Relevance to Loopr:** Loopr is architecturally built upon the "Multi-Level Ralph Wiggum Loop" concept (as seen in `2026-02-26-multi-level-rwl.md`). The Coordinator acts as the "meta-Ralph," reading the TaskStore fresh on every single iteration. This completely eliminates the unbounded context-scaling problem, as the agent only sees the current state of the world rather than dragging along a massive 50-turn conversational history.

---

## Summary

The loopr project represents a highly disciplined, state-machine-driven evolution of the concepts found in the research. It takes the persistence of Gastown/Beads, the iterative quality-control of the Rule of Five, and the stateless execution model of Ralph Wiggum Loops, synthesizing them into a robust, centralized daemon architecture.

Loopr is fundamentally an implementation of a **Ralph Wiggum Loop** mapped over a highly constrained version of **Gas Town**. It takes the git-backed persistence of Gas Town (via TaskStore and worktrees) but completely rejects Gas Town's chaotic "wreck your shit" parallelism by putting a single, non-LLM Integrator daemon in charge of state transitions to prevent the merge conflicts that plague Beads. It then uses the **Rule of Five** philosophy via its Reviewer and validation pipelines to ensure the deterministic loops converge to passing tests before anything is published.
