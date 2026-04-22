# docs/ (repo root)

All v5 documentation lives here: vision, roadmap, design docs, reference notes.

## What lives here

- **Vision:** [vision.md](vision.md) is the canonical architectural shape. Read-heavy, edit-light.
- **Roadmap:** [roadmap.md](roadmap.md) - stages, what's built, what's next.
- **Design docs:** `design/YYYY-MM-DD-<name>.md`. **Every** design doc lives here - there are no per-crate design-doc directories. Cross-cutting docs (the common case) and single-crate docs coexist in one flat location.

## What does NOT live here

- Per-crate scope rules. Those are `../crates/<name>/CLAUDE.md`.
- Build or CI config. Those are `.otto.yml` at the relevant root.
- Prompt content (`.pmt` files). Those live under `.loopr/prompts/` per vision's "Prompts" section.

## Rule

One location. The previous tiered rule (cross-cutting -> root, single-crate -> per-crate `docs/design/`) forced a subjective "primary crate" call on every doc and left eight of thirteen per-crate dirs empty; rule folded into one place as of 2026-04-22.

Every design doc carries:

- A dated filename: `YYYY-MM-DD-<slug>.md`.
- A frontmatter-ish `Crates touched:` line naming every crate the doc affects, even when it's only one. (Example: `Crates touched: domain, context, agents`.)
- A `Status:` field: `Draft | In Review | Implemented | Superseded`.

Finding docs by crate is a grep: `grep -l 'Crates touched:.*agents' docs/design/`. The directory is flat; the crate-scope signal lives on the doc.

## See also

- [../CLAUDE.md](../CLAUDE.md): project-wide rules and crate map
- [vision.md](vision.md): architectural shape
- [roadmap.md](roadmap.md): stage tracking
- `../crates/<name>/CLAUDE.md`: scope rules for a specific crate
