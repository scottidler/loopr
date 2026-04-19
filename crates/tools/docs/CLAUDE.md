# crates/tools/docs/

Design documentation scoped to the `tools` crate.

## What lives here

- Design docs touching only `tools` (Tool trait shape, built-in tool implementations, lane classification, bwrap sandboxing, denylist policy).
- Convention: `design/YYYY-MM-DD-<name>.md`.

## What does NOT live here

- Designs touching a second crate. Those go in [../../../docs/](../../../docs/).
- Scope / in-vs-out rules. Those are in [../CLAUDE.md](../CLAUDE.md).

## Rule

One design doc at a time, motivated by a failing test or real code pain.

## See also

- [../CLAUDE.md](../CLAUDE.md): scope rules for `tools`
- [../../../docs/vision.md](../../../docs/vision.md): "Security" section (lane model, sandbox)
- [../../../CLAUDE.md](../../../CLAUDE.md): project-wide rules
