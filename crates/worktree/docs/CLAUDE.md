# crates/worktree/docs/

Design documentation scoped to the `worktree` crate.

## What lives here

- Design docs touching only `worktree` (lifecycle, registry format, crash recovery, branch naming).
- Convention: `design/YYYY-MM-DD-<name>.md`.

## What does NOT live here

- Designs touching a second crate. Those go in [../../../docs/](../../../docs/).
- Scope / in-vs-out rules. Those are in [../CLAUDE.md](../CLAUDE.md).

## Rule

One design doc at a time, motivated by a failing test or real code pain.

## See also

- [../CLAUDE.md](../CLAUDE.md): scope rules for `worktree`
- [../../../docs/vision.md](../../../docs/vision.md): "Target Repo Layout" + crash-recovery subsection
- [../../../CLAUDE.md](../../../CLAUDE.md): project-wide rules
