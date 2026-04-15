# Phase Template

A Phase is a validation gate - scopes what gets built and how it is
validated. Keep it thin. A phase for a small project is 10 lines.

**Cardinality:** A Spec produces one or more Phases. Each Phase produces
one or more Work items.

**Brief mode:** Skip this document entirely. Brief mode goes directly
from Plan to Work with no phasing. If you need validation gates between
stages of work, you're in Full mode.

---

## Parent

Which Spec produced this phase (by ID or name).

## Deliverables

Files this phase creates or modifies. State explicitly what it does NOT
touch.

## Validation

### This phase validates with:
Command and what it proves.

### This phase does NOT validate with:
Command, which later phase owns it, and why running it here fails.

## Dependencies

What must be complete before this phase starts. "None" is valid for the
first phase.

## Work Items

How this phase decomposes into work assignments. Each entry names the
work item, its scope, resource tags, and dependencies on other items
within this phase. The Work template owns the full brief - this section
is the outline only.
