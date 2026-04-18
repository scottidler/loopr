# docs/ (repo root)

Top-level documentation: vision, cross-cutting design, reference material.

## What lives here

- **Vision:** [v5-shape.md](v5-shape.md) is the canonical architectural shape. Read-heavy, edit-light.
- **Cross-cutting design docs:** anything touching two or more crates. Convention: `design/YYYY-MM-DD-<name>.md`.
- **Research, reference, post-mortems:** long-form notes that aren't crate-scoped.

## What does NOT live here

- Crate-scoped design docs. Those go in `../crates/<name>/docs/design/`.
- Per-crate scope rules. Those are `../crates/<name>/CLAUDE.md`.
- Build or CI config. Those are `.otto.yml` at the relevant root.

## Rule

Before putting a design doc here, ask: does this genuinely touch two or more crates? If no, it belongs in a single crate's `docs/`. Location is scope; this folder is the most expensive place for a design doc, and that expense is deliberate - it nudges design toward single-crate scope, which is the blast-radius discipline from the root [CLAUDE.md](../CLAUDE.md).

## See also

- [../CLAUDE.md](../CLAUDE.md): project-wide rules and crate map
- [v5-shape.md](v5-shape.md): architectural shape
- `../crates/<name>/docs/CLAUDE.md`: rules for a specific crate's docs
