# crates/loopr/docs/

Design documentation scoped to the `loopr` binary crate.

## What lives here

- Design docs touching only `loopr` (daemon, IPC protocol, TUI, CLI dispatch, experiment harness).
- Convention: `design/YYYY-MM-DD-<name>.md`.

## What does NOT live here

- Designs touching a second crate. Those go in [../../../docs/](../../../docs/).
- Scope / in-vs-out rules. Those are in [../CLAUDE.md](../CLAUDE.md).
- Stage logic. If a doc is describing decomposer, agent, or integrator behavior, it belongs in that stage's crate.

## Rule

One design doc at a time, motivated by a failing test or real code pain. No design ahead of need. This crate orchestrates; a doc here describes how the driver threads state, not what the stages do.

## See also

- [../CLAUDE.md](../CLAUDE.md): scope rules for `loopr`
- [../../../docs/v5-shape.md](../../../docs/v5-shape.md): architectural shape
- [../../../CLAUDE.md](../../../CLAUDE.md): project-wide rules and discipline
