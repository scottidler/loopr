# Supplemental: description Field Callsites

**Author:** Scott A. Idler
**Date:** 2026-04-07
**Type:** Supplemental reference (callsite map)
**Design Doc:** docs/design/2026-04-07-rip-out-staging-and-description.md

## Summary

Every callsite that reads `.description` from Plan, Spec, Phase, or Work, alongside the other struct fields it pulls. The `description` field is now `#[serde(skip_serializing)]` - it survives in-session but is empty after daemon restart. Every callsite listed here should be replaced with `read_doc_content(repo_path, &id)` to read from `docs/loopr/<id>.md` instead.

## context.rs - building LLM prompt context

| Line | Struct | Fields pulled alongside `.description` |
|------|--------|---------------------------------------|
| 337-342 | Work | `.title`, `.description`, `.parent_id`, `.acceptance_criteria`, `.resource_tags`, `.dependencies` |
| 359 | Plan | `.title`, `.description`, `.id` |
| 375-379 | Phase | `.title`, `.description`, `.parent_id`, `.id` |
| 388-392 | Spec | `.title`, `.description`, `.parent_id`, `.id` |
| 401 | Plan | `.title`, `.description`, `.id` |

## generation.rs - building work-generation prompt

| Line | Struct | Fields pulled alongside `.description` |
|------|--------|---------------------------------------|
| 76-80 | Phase | `.id`, `.parent_id`, `.title`, `.order`, `.description` |
| 96-102 | Work | `.id`, `.title`, `.status()`, `.dependencies`, `.description` |

## integrator.rs - validation calls

| Line | Struct | Fields pulled alongside `.description` |
|------|--------|---------------------------------------|
| 308 | Plan | `.title`, `.description`, `.acceptance_criteria` |
| 328 | Spec | `.title`, `.description` + parent plan `.title` |
| 348 | Phase | `.title`, `.description`, `.order` + parent spec `.title` |

## integrator.rs - coverage evaluator calls

| Line | Struct | Fields pulled alongside `.description` |
|------|--------|---------------------------------------|
| 428 | Spec | `.id`, `.title`, `.description` (format string) |
| 438 | Plan | `.title`, `.description`, `.acceptance_criteria` |
| 464 | Phase | `.id`, `.title`, `.order`, `.description` (format string) |
| 473 | Spec | `.title`, `.description` + parent plan `.title` |
| 499 | Work | `.id`, `.title`, `.description` (format string) |
| 505 | Phase | `.title`, `.description`, `.order` + spec `.title` |

## evaluator.rs

| Line | Struct | Fields pulled alongside `.description` |
|------|--------|---------------------------------------|
| 110 | PhaseWorksParams | `.title`, `.description`, `.order`, `.spec_title` |

## doc_body() - markdown file rendering

| Line | Struct | Uses |
|------|--------|------|
| plan.rs:133 | Plan | `self.description` + `self.acceptance_criteria` |
| spec.rs:77 | Spec | `self.description` + `self.acceptance_criteria` |
| phase.rs:83 | Phase | `self.description` + `self.acceptance_criteria` |
| work.rs:137 | Work | `self.description` + `self.acceptance_criteria` + `self.checklist` |

## handle_*_update - IPC writes to .description

| Line | Struct |
|------|--------|
| plan.rs:300 | Plan |
| spec.rs:338 | Spec |
| phase.rs:339 | Phase |
| work.rs:526 | Work |

## Pattern

Every callsite pulls `.title` and `.id` from the struct, then also pulls `.description`. `.title`, `.id`, `.parent_id`, `.order`, `.acceptance_criteria` are small structured fields that belong on the struct. `.description` is the multi-KB LLM prose blob that is redundant with `docs/loopr/<id>.md`.

Every callsite that reads `.description` already has the record's `.id` and could call `read_doc_content(repo_path, &id)` instead.
