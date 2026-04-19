# crates/context/docs/

Design documentation scoped to the `context` crate.

## What lives here

- Design docs touching only `context` (ContextBuilder API shape, template resolution, handlebars partials registry, token-budgeting strategy, per-role entry points).
- Convention: `design/YYYY-MM-DD-<name>.md`.

## What does NOT live here

- Designs touching a second crate. Those go in [../../../docs/](../../../docs/).
- Prompt content authoring. Prompts live as `.pmt` files in `.loopr/prompts/` (per-target) or baked into the binary (defaults).
- Scope / in-vs-out rules. Those are in [../CLAUDE.md](../CLAUDE.md).

## Rule

One design doc at a time, motivated by a failing test or real code pain.

## See also

- [../CLAUDE.md](../CLAUDE.md): scope rules for `context`
- [../../../docs/vision.md](../../../docs/vision.md): "Prompts" section
- [../../../CLAUDE.md](../../../CLAUDE.md): project-wide rules
