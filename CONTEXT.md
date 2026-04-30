# Loopr v5

A reactive daemon that takes a fully-specified Plan as input and runs a typed pipeline (decompose, implement, review, integrate) against a target git repository, ultimately publishing a Tick. Stage 9 is the current target; v5 is currently single-tier (Plan to Work, flat) and earns Spec/Phase, Coordinator-as-LLM, and Director on the Tier-1/2/3 deferred roadmap.

## Language

### Hierarchy levels

**Plan**:
A PRD-shaped specification of intent passed to Loopr as a markdown string; the top of the decomposition tree. The full markdown is stored verbatim in `Plan.goal`; there is no parsed manifest schema.
_Avoid_: Goal, Brief, Charter, Intake.

**Spec**:
A design-document-level decomposition of a Plan into a technical approach. _Not yet built in v5_; Tier 3.4 deferred. v3 and v4 had it.
_Avoid_: Design, Module, Feature.

**Phase**:
An ordered milestone within a Spec. _Not yet built in v5_; Tier 3.4 deferred. v3 and v4 had it.
_Avoid_: Stage, Milestone, Step.

**Work**:
The atomic unit of assignable coding work; gets its own Implementer; carries an `AcceptanceCriteria` (must be non-empty), declared `dependencies: Vec<WorkId>` on sibling Works, and a `files: Vec<PathBuf>` allow-list emitted by the decomposer.
_Avoid_: Task, Issue, Ticket, Job.

**Bundle**:
The Implementer's output artifact for one Work. Carries the branch name (`loopr/wk-<work-id>-<seq>`), claims, `paths` (populated by the integrator from the branch-vs-base diff), and verification notes.
_Avoid_: PR, Patch, Submission, Commit.

**Tick**:
A published codebase snapshot produced by the Integrator merging accepted Bundles into the per-Plan integration branch. Aggregates 1+ Bundles.
_Avoid_: Release, Build, Snapshot, Checkpoint.

### Roles

There are seven Role variants in `domain::Role`. They differ by *Kind* (LLM Ralph loop, deterministic code, daemon-internal routing, or pure function), and only some are LLM agents. Lumping them as "agents" was a v4 habit; v5 untangles it via [docs/roles-and-states.md](docs/roles-and-states.md).

**Reactor**:
The daemon itself, not an LLM. Performs deterministic FSM routing: dependency gating, post-Verdict transitions, parent-child cascades. _Not_ an agent; never holds an LLM opinion. Renamed from `Coordinator` per ADR-0002.
_Avoid_: Coordinator, Manager, Foreman, Mayor, Daemon, Router.

**Decomposer**:
A pure function (`fn decompose<L: LlmClient>(plan, target, llm) -> Result<Vec<Work>>`) that calls an LLM. Owns the `Plan: Active to Complete` edge as well as the daemon's. Not a Ralph loop, not an agent.
_Avoid_: Planner, Decomposition agent.

**Implementer**:
An LLM Ralph loop in `crates/agents` that takes one Work to a proposed Bundle inside its dedicated worktree. The only Implementer-authored FSM edge is `Work: InProgress to InReview`.
_Avoid_: Coder, Worker, Polecat, Builder.

**Reviewer**:
An LLM Ralph loop in `crates/agents` that returns a typed `Verdict` (approve/reject with rationale) on a Bundle. **Authorizes no FSM edges directly** — its Verdict is a persisted artifact; the Coordinator reads the Verdict and routes the next transition. There is no `Approved` Work state.
_Avoid_: Auditor, QA, Gatekeeper.

**Integrator**:
A deterministic non-LLM crate (`crates/integrator`) whose Cargo manifest mechanically forbids an `llm` dependency. Merges accepted Bundles to the per-Plan integration branch and publishes a Tick. Authorizes only `Work: InReview to Integrated`.
_Avoid_: Merger, Refinery, Releaser.

**Researcher**:
An LLM Ralph loop that produces a typed `Finding` artifact about the target. Authorizes no FSM edges. _Not yet built_; deferred to Tier 2.1.
_Avoid_: Investigator, Scout, Indexer.

**Director**:
An LLM Ralph loop for escalation and judgment: kicking a Plan back to Draft for re-decomposition, abandoning a stuck Plan, superseding overtaken Works. Authorizes the override edges that Coordinator cannot. _Not yet built_; deferred to Tier 3.1. Until it lands, escalations exit with an error.
_Avoid_: Supervisor, Lifeguard, Witness, Watchdog.

### Operating concepts

**Target**:
A git repository Loopr operates on; not Loopr's own source tree. The `.loopr-source-guard` sentinel at the repo root prevents Loopr from being its own target.
_Avoid_: Project, Repo, Workspace.

**Ralph-Wiggum loop**:
An iteration pattern where each LLM turn runs with a fresh context window; progress is carried by external state (TaskStore, git, files), not in-context memory. Implementer and Reviewer are Ralph loops; Coordinator, Decomposer, and Integrator are not.
_Avoid_: Agent loop, Inner loop, ReAct loop.

**Verdict**:
The Reviewer's typed output for a Bundle: `Approve | Reject` with structured rationale. Persisted as a record; not an FSM state.
_Avoid_: Review, Approval, Decision.

**Acceptance criteria**:
Boolean-assertable conditions on a Work that all-must-be-true for the Bundle to be accepted. Decomposer must produce at least one per Work; empty AC is a hard error.
_Avoid_: Tests, Requirements, Specs, Definition of Done.

**Tier**:
The decomposition shape selector — `Brief` (Plan to Work flat) or `Full` (Plan to Spec to Phase to Work). v5 is currently Brief-only; `Full` lands with Tier 3.4 spec-phase-hierarchy. v4 had a Haiku-driven `classify_tier` call; v5 has not yet ported it.

## Relationships

- A **Plan** decomposes into 1+ **Works** (currently flat; will pass through 1+ **Specs** and 1+ **Phases** when 3.4 lands).
- A **Work** declares `dependencies: Vec<WorkId>` on sibling Works and a `files: Vec<PathBuf>` allow-list emitted by the **Decomposer**.
- One **Work** produces 0+ **Bundles** (one per attempt; only the accepted Bundle survives review).
- One **Tick** aggregates 1+ accepted **Bundles** on a per-**Plan** integration branch.
- **Implementer** produces a Bundle and fires `Work: InProgress to InReview`.
- **Reviewer** inspects, returns a **Verdict** record (no FSM edge).
- **Reactor** reads the Verdict and either fires `Work: InReview to InProgress` (Reject) or hands the Bundle to the **Integrator** (Approve).
- **Integrator** fires `Work: InReview to Integrated` after a clean merge and publishes a **Tick**.
- **Reactor** fires `Work: Integrated to Done` once no active sessions remain.

## Example dialogue

> **Dev:** "Why doesn't the **Reviewer** just fire the FSM transition itself?"
> **Domain expert:** "Because the **Reviewer** is an LLM doing semantic analysis and the **Integrator** is deterministic Rust doing a mechanical merge. Their failure modes and recovery paths are different. Keeping the **Reactor** as the sole router between LLM opinion and mechanical action means state advances in one place, not two."

> **Dev:** "When the **Reviewer** rejects a **Bundle**, does the **Work** restart in the same worktree?"
> **Domain expert:** "No. Each attempt gets its own worktree at `<work-id>-<seq>`. The rejected Bundle is persisted as history; a fresh Implementer runs in a fresh worktree on a fresh branch. There is no rebase-on-retry."

> **Dev:** "What does it mean for `Plan.goal` to 'be a PRD'?"
> **Domain expert:** "Today it's the entire markdown file as one string field. The **Decomposer** LLM reads it and pulls out structure semantically. There is no validator, no parser, no required schema — convention only. Frontmatter, typed fields, and structured PRD parsing are deferred."

## Lineage of the Reactor name

- **v3** had `Coordinator` as the top LLM agent (1199 lines in `~/repos/scottidler/loopr/src/agents/coordinator.rs`). Five-Role enum: `Coordinator | Integrator | Implementer | Reviewer | Researcher`.
- **v4** renamed that agent to `Director` (1421-line `agents/director.rs`; `coordinator.rs` was deleted). `Coordinator` survived as a Role enum variant for the daemon's mechanical FSM edges. Seven-Role enum (added `Decomposer`, `Director`).
- **v5** inherits v4's seven Roles. ADR-0002 renames `Coordinator` to **`Reactor`** — the v5 LLM-orchestrator role is `Director` (deferred), and the daemon's deterministic plane wants a name that reads as mechanical, not as an agent. The `Reactor` name also encodes vision.md's "Loopr is reactive" thesis directly into the type system.

## Flagged ambiguities

- **"Goal"** is `Plan.goal: String` — the entire PRD-style markdown blob passed to `loopr plan`. There is no separate Goal record. The v3/v4 `cg-*` Goal records do not exist in v5.

- **"Plan FSM"** has a `Draft` status, but Plans created by `loopr plan "<text>"` enter at `Draft` only as a transient on the way to `Active`. There is no coalescing-state-as-`Draft` semantics in v5.

- **`deferred-roadmap.md` Tier 1.2 "Coordinator agent"** described what is really `Director` Phase 1 (routine orchestration before judgment/escalation lands). Per ADR-0002 the entry is reframed as `Director — Phase 1`, with Tier 3.1 becoming `Director — Phase 2`. There is no "Coordinator agent" in v5.
