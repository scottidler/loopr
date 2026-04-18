# crates/integrator/docs/

Design documentation scoped to the `integrator` crate.

## What lives here

- Design docs touching only `integrator` (merge, validate, publish, conflict classification). Non-LLM logic only.
- Convention: `design/YYYY-MM-DD-<name>.md`.

## What does NOT live here

- Designs touching a second crate. Those go in [../../../docs/](../../../docs/).
- Scope / in-vs-out rules. Those are in [../CLAUDE.md](../CLAUDE.md).

## Rule

One design doc at a time, motivated by a failing test or real code pain. No design ahead of need. If a proposed design requires an LLM call inside `integrator`, it belongs in a different crate.

## See also

- [../CLAUDE.md](../CLAUDE.md): scope rules for `integrator`
- [../../../docs/v5-shape.md](../../../docs/v5-shape.md): architectural shape
- [../../../CLAUDE.md](../../../CLAUDE.md): project-wide rules and discipline
