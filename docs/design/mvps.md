# Loopr v3 — MVP Phases

| | **MVP1** | **MVP2** | **MVP3** | **MVP4** |
|---|---|---|---|---|
| **Goal** | Prove the orchestration spine | Add read-only intelligence | LLM agents do the work (code level) | Full multi-level RWL — LLM at every level |
| **LLM** | None. Zero. Human drives everything. | Doc Validator only (read-only, structured reports) | Implementer + Reviewer personas | + Coordinator + Researcher. Integrator is deterministic (no LLM). |
| **Human Role** | Coordinator, Integrator, and Implementer — all hats worn by the human via TUI | Coordinator + Integrator; LLM validates docs | Coordinator oversees; LLM agents implement + review | Human sets goals and overrides; LLM Coordinator runs the pipeline |
| **Pipeline** | Work → Bundle → Tick, fully manual | Same pipeline, LLM gates Spec/Phase/Plan quality | Same pipeline, LLM produces Bundles in worktrees | Full pipeline automated: Goal → Plan → Spec → Phase → Work → Bundle → Tick |
| **What It Proves** | FSMs work. TaskStore works. Daemon-mediated correctness works. Worktree isolation works. TUI is usable. | LLM can be safely inserted without breaking the spine | Code-level RWL works. Agents produce Bundles in worktrees. | Full "dev team in a box" vision. Multi-level RWL with generation + validation. |
| **FSMs** | 3 hand-rolled: Work, Bundle, Tick + HierarchyStatus for Plan/Spec/Phase | Same | Same + staleness cascade automation | Same + Role::Researcher |
| **Parallelism** | Serial. One actor at a time. | Serial. Validator is synchronous gate. | Bounded: 2 Implementers, 2 Reviewers | + 1 Coordinator, 4 Researchers, 1 Integrator (deterministic) |
| **Worktrees** | Create per-Work, human works in them manually | Same | LLM agents work in worktrees via tool execution | Same (Coordinator/Researcher/Integrator don't use worktrees) |
| **Tool Execution** | None — human runs commands in a separate terminal | None — LLM validator is API-call only | OS subprocesses in worktrees (Light Loops, Heavy Tools) | Same + Researcher codebase search (read-only) |
| **Persistence** | TaskStore (JSONL truth, SQLite cache, Git merge driver) | Same | Same | Same + CoordinatorGoal record, enriched Learnings |
| **IPC** | NDJSON over Unix socket, daemon as single authority | Same | Same + streaming LLM output to TUI | Same + coordinator.set_goal/clear_goal |
| **Key Principle** | "Your hardest problems are not LLM problems. Prove the system works first." | "Safest entry point for intelligence: read-only, can't break Tick semantics." | "Everything plugs into the backbone MVP1 built." | "RWL at every level. The Coordinator is the meta-Ralph." |
| **Design Doc** | `design/2026-02-25-loopr-v3-mvp1.md` | `design/2026-02-26-loopr-v3-mvp2.md` | `design/2026-02-26-loopr-v3-mvp3.md` | `design/2026-02-26-loopr-v3-mvp4.md` |

---

| | **MVP5** | **MVP6** | **MVP7** | **MVP8** | **MVP9** |
|---|---|---|---|---|---|
| **Goal** | Coordinator control loop & sequential phase execution | 12 structural fixes (bundle lifecycle, merge, convergence) | IPC type safety & lifecycle audit | Worktree isolation & implementer reliability | Semantic decomposition evaluation & collaborative Plan creation |
| **What It Proves** | Coordinator sequences phases, enforces dependencies, converges | Multi-Tick builds work; merge conflicts handled | Typed IPC prevents silent data loss; Work reaches Done | Implementers produce valid Bundles reliably | Decomposition quality matches implementation quality — tight feedback loops at every level |
| **Key Principle** | "Sequence work like a real dev team" | "Fix the pipeline — every edge case" | "Compile-time contracts across IPC boundaries" | "Exactly one valid worktree per Work" | "Pit of success: high-quality Plans recursively beget high-quality code" |
| **Design Doc** | `design/2026-02-28-loopr-v3-mvp5-coordinator-sequencing.md` | `design/2026-02-28-loopr-v3-mvp6-structural-fixes.md` | `design/2026-02-28-loopr-v3-mvp7-type-safety-audit.md` | `design/2026-02-28-loopr-v3-mvp8-worktree-implementer-reliability.md` | `design/2026-03-03-loopr-v3-mvp9-semantic-decomposition.md` |
