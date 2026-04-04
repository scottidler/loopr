# Spec Template

The Spec translates the Plan's "what" into "how." It owns the architecture
and technical design. It references the Plan for data model and API
contract - does not repeat them. Scale depth to the project.

**Cardinality:** A Plan produces one or more Specs. Each Spec produces
one or more Phases.

**Brief mode:** Skip this document entirely. Brief mode is for
contract-neutral work where the "how" is obvious from the "what."
If you need a Spec, you're in Full mode.

---

## Overview

What this spec delivers and which Plan it implements (by ID or name).
Architectural approach in one paragraph.

## Data Flow

How data moves through the system end to end. Happy path AND error path.
Actual data shapes at each boundary - what enters, what exits.

## Module Structure

Every file, its responsibility, what it depends on, what it exports.
Table or bullet list depending on project size.

| File | Responsibility | Depends On | Exports |
|------|---------------|------------|---------|

## Dependencies

External packages with versions, runtime/test classification, and purpose.

| Package | Version | Runtime/Test | Purpose |
|---------|---------|-------------|---------|

## Interfaces

Public interfaces between modules. Function signatures, API endpoints,
message formats. What crosses each boundary. These are inherited from
the Plan's contracts and elaborated with module-level detail.

## Failure Modes

What can go wrong at each layer and how it is handled. Include edge
cases - boundary conditions, empty states, concurrent access, malformed
input. Concrete decisions per layer, not generic advice.

| Layer | Failure | Handling | Rationale |
|-------|---------|----------|-----------|

## Testing

Isolation method, test client/runner, named test inventory (every test
function listed), what is NOT tested and why.

### Test Inventory

Number each test. The reviewer checks them off.

### Not Tested

What is out of scope for the test suite and why.

## Phases

The execution decomposition. Each entry names the phase, its ordering,
what it proves when complete, and dependencies on other phases. The
Phase template owns the full detail - this section is the outline only.

A simple Spec has 1 Phase. A complex Spec has 3-5, each with a distinct
validation gate (e.g., "data layer," "API layer," "integration tests").

---

## Conditional Sections

Include when relevant. Omit when not applicable.

### Performance

Expected load, latency budget, resource constraints. "Single-user, no
performance concerns" is valid - state it explicitly.

### Security

Authentication, authorization, input validation, data sensitivity.
"Internal tool, no external access" is valid - state it explicitly.

### Open Questions

| # | Question | Status |
|---|----------|--------|

Resolved questions stay with their resolution.

### Alternatives

Viable approaches that were considered. Each gets Pros, Cons, Verdict
with "Rejected because:" language.

### Key Decisions

Non-obvious architectural choices that future engineers might question.
Name the decision, explain the rationale, prevent re-litigation.

### Glossary

Domain terms an implementer might not know.

| Term | Definition |
|------|-----------|
