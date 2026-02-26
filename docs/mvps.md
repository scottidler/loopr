# Loopr v3 — MVP Phases

| | **MVP1** | **MVP2** | **MVP3+** |
|---|---|---|---|
| **Goal** | Prove the orchestration spine | Add read-only intelligence | LLM agents do the work |
| **LLM** | None. Zero. Human drives everything. | Doc Validator only (read-only, structured reports) | Implementer + Reviewer personas |
| **Human Role** | Coordinator, Integrator, and Implementer — all hats worn by the human via TUI | Coordinator + Integrator; LLM validates docs | Coordinator oversees; LLM agents implement + review |
| **Pipeline** | WorkItem → Bundle → Tick, fully manual | Same pipeline, LLM gates Spec/Phase/Plan quality | Same pipeline, LLM produces Bundles in worktrees |
| **What It Proves** | FSMs work. TaskStore works. Daemon-mediated correctness works. Worktree isolation works. TUI is usable. | LLM can be safely inserted without breaking the spine | Full "dev team in a box" vision |
| **FSMs** | 3 hand-rolled: WorkItem, Bundle, Tick + HierarchyStatus for Plan/Spec/Phase | Same | Same + staleness cascade automation |
| **Parallelism** | Serial. One actor at a time. | Serial. Validator is synchronous gate. | Bounded: 2-4 Implementers, swarms for Spec/Review/Research |
| **Worktrees** | Create per-WorkItem, human works in them manually | Same | LLM agents work in worktrees via tool execution |
| **Tool Execution** | None — human runs commands in a separate terminal | None — LLM validator is API-call only | OS subprocesses in worktrees (Light Loops, Heavy Tools) |
| **Persistence** | TaskStore (JSONL truth, SQLite cache, Git merge driver) | Same | Same |
| **IPC** | NDJSON over Unix socket, daemon as single authority | Same | Same + streaming LLM output to TUI |
| **Key Principle** | "Your hardest problems are not LLM problems. Prove the system works first." | "Safest entry point for intelligence: read-only, can't break Tick semantics." | "Everything plugs into the backbone MVP1 built." |
| **Design Doc** | `design/2026-02-25-loopr-v3-mvp1.md` | `design/2026-02-26-loopr-v3-mvp2.md` | TBD |
