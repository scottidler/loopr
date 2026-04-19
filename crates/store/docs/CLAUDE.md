# crates/store/docs/

Design documentation scoped to the `store` crate.

## What lives here

- Design docs touching only `store` (wrapper API, collection accessors, path resolution, error taxonomy).
- Convention: `design/YYYY-MM-DD-<name>.md`.

## What does NOT live here

- Designs touching a second crate. Those go in [../../../docs/](../../../docs/).
- Scope / in-vs-out rules. Those are in [../CLAUDE.md](../CLAUDE.md).

## Rule

One design doc at a time, motivated by a failing test or real code pain. No design ahead of need.

## See also

- [../CLAUDE.md](../CLAUDE.md): scope rules for `store`
- [../../../docs/vision.md](../../../docs/vision.md): architectural shape
- [../../../CLAUDE.md](../../../CLAUDE.md): project-wide rules
