# Plan Template

The Plan is the PRD - single source of truth for the entire implementation.
Every Spec, Phase, and Work item must be derivable from this document.
Scale depth to the project.

**Cardinality:** A Plan produces one or more Specs. Each Spec produces one
or more Phases. Each Phase produces one or more Work items.

**Tier gate:** Does this work introduce or modify a contract (data model, API,
public interface)? If yes, use Full mode (Plan -> Spec -> Phase -> Work). If no,
use Brief mode (Plan -> Work). In Brief mode, Spec and Phase are skipped entirely.

---

## Problem Statement

What problem are we solving and why? Ground in evidence when available.
For a small project, 2-3 sentences. For a large project, paragraphs with
ticket counts, manual workflows, and user pain.

Brief mode: 1 sentence with evidence.

## Goals

Concrete, measurable outcomes that define success. What does "done" look
like from the user's perspective?

## Requirements

User stories and use cases. Every capability the system must have maps to
a requirement here. If a capability doesn't have a requirement, question
whether it belongs.

| As a... | I want to... | So that... |
|---------|-------------|-----------|

Minimum 2 rows. Scale with project scope.

## Scope

Bounded capabilities this plan covers, tied to requirements above.

## Constraints

Hard limits the implementation must operate within:
- Technology choices (language, framework, stdlib-only, etc.)
- Compatibility requirements (Docker, specific Python version, etc.)
- Budget, timeline, or resource limits when applicable

Full mode only. Omit in Brief mode when standard constraints apply.

## Contracts

The data model and API contract. These are IMMUTABLE - they flow down
unchanged through Spec, Phase, and Work. If these need to change, the
Plan changes first.

Brief mode: "No contract changes" or a reference to a shared spec
(e.g., "Follows docs/specs/e2e-target.md"). The section is still
required - it must explicitly state what the contract situation is.

### Data Model

Every entity with exact field names, types, defaults, constraints.

### API Contract

Every endpoint, function signature, or CLI command. Names, parameters,
return types.

## Acceptance Criteria

Executable scenarios that prove the plan is complete. These are test
cases, not vague assertions.

| Given... | When... | Then... |
|----------|---------|---------|

Minimum 3 rows.

### Final Validation
The command that proves everything works end to end.

## Specs

Full mode. The architectural domains or subsystems this plan decomposes
into. Each entry names the domain, which requirements it covers, and
what contracts it inherits. The Spec template owns the full design -
this section is the outline only.

A small Full-mode project has 1 Spec. A large project has 2-3, each
covering a coherent subsystem (e.g., "frontend," "API," "data pipeline").

## Work Items

Brief mode. When Spec and Phase are skipped, the Plan produces Work
items directly. Each entry names what it delivers, what it depends on,
and what validation command proves it's done. A single work item is
typical. Scale with scope.

## Dependencies

What must exist before this plan can execute - upstream systems, data,
APIs, libraries, infrastructure.

Full mode only. Omit in Brief mode when there are no external dependencies.

---

## Conditional Sections

Include when relevant in either mode. Omit when not applicable.
These are never required - the Full/Brief tier controls required sections only.

### Non-Goals

What we are explicitly NOT doing. Each item explains WHY it's excluded -
not just that it is.

### Assumptions

What is believed true but not verified. Each is a statement that, if
wrong, would change the plan.

### Open Questions

| # | Question | Owner | Status |
|---|----------|-------|--------|

Resolved questions stay with their resolution.

### Risks

| Risk | Impact | Mitigation |
|------|--------|-----------|

### Success Metrics

Measurable targets with thresholds. When acceptance criteria alone don't
capture the full definition of success.
