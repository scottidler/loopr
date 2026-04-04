# How a Plan Gets Into Loopr

**Date:** 2026-04-04
**Status:** Authoritative

This is the definitive reference for how plans enter the system. If any code, design doc, or agent contradicts this document, this document wins.

---

## The Pipeline

```
Plan enters the system
        |
        v
  Plan .md written to disk, Document record created (Draft)
        |
        v
  User activates the plan
        |
        v
  Decomposer breaks the plan into specs, phases, and works
  (writes .md files, creates Document records for each)
        |
        v
  Coordinator receives the fully decomposed hierarchy
  and executes it (assigns implementers, monitors progress, gates phases)
```

The Coordinator does NOT interview. The Coordinator does NOT decompose. The Coordinator executes.

---

## Primary Path: Chat Funnel

The user is chatting with the Chat agent in the TUI. They go back and forth, exploring ideas. At some point they start to coalesce on an idea.

1. User types `/plan`. Chat agent switches to interview mode. Same LLM, augmented prompt. The agent asks clarifying questions. The user answers. They refine together.
2. User types `/draft`. The Chat agent produces a structured plan document.
3. User reviews. They can suggest edits. The Chat agent revises. They go back and forth until the plan is right.
4. User types `/accept`. The system writes the plan as a .md file on disk and creates a Document record in Draft status.
5. User activates the plan.
6. The Decomposer takes the plan .md and produces all downstream artifacts: spec .md files, phase .md files, work .md files. Each gets a Document record.
7. The Coordinator receives the fully decomposed hierarchy and begins execution.

The Chat agent does the interviewing. The Decomposer does the decomposition. The Coordinator does the execution. Three separate jobs, three separate things.

---

## Secondary Path: E2E Tests

E2E tests skip the chat conversation. They already have a plan.

1. Test provides a plan .md file directly.
2. System writes it to disk and creates a Document record, same as if the Chat funnel produced it.
3. Test activates the plan.
4. Everything from step 6 onward in the primary path is identical.

That's it. The E2E test is the primary path minus the chat conversation. Same pipeline. Same Decomposer. Same Coordinator. The only difference is where the plan came from.

---

## What Each Thing Does

| Component | Job | Does NOT do |
|-----------|-----|-------------|
| Chat agent | Interviews the user, produces a plan .md | Decompose. Execute. |
| Decomposer | Takes a plan .md, produces specs, phases, works | Interview. Execute. |
| Coordinator | Executes a fully decomposed hierarchy | Interview. Decompose. |

No component does another component's job. Ever.

---

## What About the Manifest?

The YAML manifest (`--plan file.yml`) is a third entry point. It provides a pre-decomposed hierarchy (plan + specs + phases + works already defined). It skips the Decomposer because the human already did the decomposition.

This is legitimate for:
- Human-authored decompositions where you already know the structure
- Replaying a known hierarchy
- Testing the Coordinator and execution pipeline in isolation

It is NOT a substitute for the Decomposer working correctly.
