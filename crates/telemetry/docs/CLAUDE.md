# crates/telemetry/docs/

Design documentation scoped to the `telemetry` crate.

## What lives here

- Design docs touching only `telemetry` (subscriber composition, run-id semantics, span conventions, log-query helpers).
- Convention: `design/YYYY-MM-DD-<name>.md`.

## What does NOT live here

- Designs touching a second crate. Those go in [../../../docs/](../../../docs/).
- Scope / in-vs-out rules. Those are in [../CLAUDE.md](../CLAUDE.md).

## Rule

One design doc at a time, motivated by a failing test or real code pain. No design ahead of need. If the doc could be deleted without losing anything anyone is actually using, don't write it.

## See also

- [../CLAUDE.md](../CLAUDE.md): scope rules for `telemetry`
- [../../../docs/vision.md](../../../docs/vision.md): architectural shape, Observability section
- [../../../docs/roadmap.md](../../../docs/roadmap.md): Stage 2 lists the first docs this crate needs
- [../../../CLAUDE.md](../../../CLAUDE.md): project-wide rules and discipline
