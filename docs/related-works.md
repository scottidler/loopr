# Related Works

References and prior art worth studying for ideas.

## Coding Agents

- **Aider** (github.com/paul-gauthier/aider) - Python-based coding assistant with chat + edit loop. Their git integration (auto-commit each change, easy revert) is worth studying. Solved many of the same UX problems.

- **SWE-agent** (github.com/princeton-nlp/SWE-agent) - Princeton's approach to coding agents. Their "Agent-Computer Interface" (ACI) design - giving the agent a curated set of tools rather than raw shell access - is aligned with loopr's builtin tool approach.

- **OpenHands** (github.com/All-Hands-AI/OpenHands) - Multi-agent coding framework (formerly OpenDevin). Event-stream architecture (events as the unit of work, not messages) is an interesting contrast to loopr's FSM approach.

- **Claude Code** (docs.anthropic.com) - The tool we use to build loopr. Loopr's delegate pattern is very similar to Claude Code's Agent tool. Worth studying for UX patterns.

- **Taskmaster AI** (github.com/eyaltoledano/claude-task-master) - Task decomposition and management for AI agents. Similar Plan-to-Task hierarchy but much simpler. Worth studying for UX simplicity.

## Orchestration / Architecture

- **Stripe Minions** (referenced in docs/minions-stripes-one-shot-end-to-end-coding-agents.md) - Flat model: task, implementation, review. Production-proven at scale.

- **Steve Yegge's Gas Town** (referenced in docs/yegge/) - The orchestrator should be opinionated about sequencing but unopinionated about implementation.

## Key Takeaways

- Aider: auto-commit per change for easy rollback
- SWE-agent: curated tool interface beats raw shell
- OpenHands: event streams as alternative to FSMs for agent coordination
- Taskmaster: UX simplicity for task decomposition
- Stripe: humans write specs, agents implement - validates loopr's interview-then-automate approach
- Yegge: orchestrator sequences, doesn't micromanage implementation
