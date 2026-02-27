# Loopr v3 — MVP Phases

| | **MVP1** | **MVP2** | **MVP3** | **MVP4** |
|---|---|---|---|---|
| **Goal** | Prove the orchestration spine | Add read-only intelligence | LLM agents do the work (code level) | Full multi-level RWL — LLM at every level |
| **LLM** | None. Zero. Human drives everything. | Doc Validator only (read-only, structured reports) | Implementer + Reviewer personas | + Coordinator + Researcher. Integrator is deterministic (no LLM). |
| **Human Role** | Coordinator, Integrator, and Implementer — all hats worn by the human via TUI | Coordinator + Integrator; LLM validates docs | Coordinator oversees; LLM agents implement + review | Human sets goals and overrides; LLM Coordinator runs the pipeline |
| **Pipeline** | WorkItem → Bundle → Tick, fully manual | Same pipeline, LLM gates Spec/Phase/Plan quality | Same pipeline, LLM produces Bundles in worktrees | Full pipeline automated: Goal → Plan → Spec → Phase → WorkItem → Bundle → Tick |
| **What It Proves** | FSMs work. TaskStore works. Daemon-mediated correctness works. Worktree isolation works. TUI is usable. | LLM can be safely inserted without breaking the spine | Code-level RWL works. Agents produce Bundles in worktrees. | Full "dev team in a box" vision. Multi-level RWL with generation + validation. |
| **FSMs** | 3 hand-rolled: WorkItem, Bundle, Tick + HierarchyStatus for Plan/Spec/Phase | Same | Same + staleness cascade automation | Same + Role::Researcher |
| **Parallelism** | Serial. One actor at a time. | Serial. Validator is synchronous gate. | Bounded: 2 Implementers, 2 Reviewers | + 1 Coordinator, 4 Researchers, 1 Integrator (deterministic) |
| **Worktrees** | Create per-WorkItem, human works in them manually | Same | LLM agents work in worktrees via tool execution | Same (Coordinator/Researcher/Integrator don't use worktrees) |
| **Tool Execution** | None — human runs commands in a separate terminal | None — LLM validator is API-call only | OS subprocesses in worktrees (Light Loops, Heavy Tools) | Same + Researcher codebase search (read-only) |
| **Persistence** | TaskStore (JSONL truth, SQLite cache, Git merge driver) | Same | Same | Same + CoordinatorGoal record, enriched Learnings |
| **IPC** | NDJSON over Unix socket, daemon as single authority | Same | Same + streaming LLM output to TUI | Same + coordinator.set_goal/clear_goal |
| **Key Principle** | "Your hardest problems are not LLM problems. Prove the system works first." | "Safest entry point for intelligence: read-only, can't break Tick semantics." | "Everything plugs into the backbone MVP1 built." | "RWL at every level. The Coordinator is the meta-Ralph." |
| **Design Doc** | `design/2026-02-25-loopr-v3-mvp1.md` | `design/2026-02-26-loopr-v3-mvp2.md` | `design/2026-02-26-loopr-v3-mvp3.md` | `design/2026-02-26-loopr-v3-mvp4.md` |
