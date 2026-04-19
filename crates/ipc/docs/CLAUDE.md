# crates/ipc/docs/

Design documentation scoped to the `ipc` crate.

## What lives here

- Design docs touching only `ipc` (message schemas, framing choice, versioning policy).
- Convention: `design/YYYY-MM-DD-<name>.md`.

## What does NOT live here

- Designs touching a second crate. Those go in [../../../docs/](../../../docs/).
- Scope / in-vs-out rules. Those are in [../CLAUDE.md](../CLAUDE.md).

## Rule

One design doc at a time, motivated by a failing test or real code pain. No design ahead of need. If the doc could be deleted without losing anything anyone is actually using, don't write it.

## See also

- [../CLAUDE.md](../CLAUDE.md): scope rules for `ipc`
- [../../../docs/vision.md](../../../docs/vision.md): architectural shape
- [../../../CLAUDE.md](../../../CLAUDE.md): project-wide rules and discipline
