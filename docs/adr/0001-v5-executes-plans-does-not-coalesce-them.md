# v5 executes plans, does not coalesce them

v3 and v4 both attempted to bundle a conversational coalescing layer (chat → interview → coordinator-approved hierarchy → execution) into the same product as the execution engine. The TUI half of that surface was the hardest part of v3/v4 and never converged. v5 deliberately scopes the coalescing layer out: Loopr accepts a fully-specified Plan as input and executes it. Producing a Plan (whether by hand-written manifest, a Claude Code skill, or a future TUI) is upstream of v5 and not its responsibility.

## Consequences

- The CLI entry point is `loopr plan <manifest>` (and a degenerate `loopr plan "<one-liner>"` for trivial E2E cases that do not need a manifest). No interactive `/plan` or `/accept` commands.
- E2E tests pass pre-baked manifests; this is not a test-only escape hatch — it is the canonical entry shape.
- There is no pre-Plan record class (no Brief, no Intake, no Charter). The Plan record arrives at status `Draft` only as a transient state during ingestion, transitioning immediately to `Active` once parsed and validated.
- A future Loopr TUI, or an upstream Claude Code skill, may layer a conversational front end on top — but its output must be a valid Plan manifest, not a side channel into the daemon's state.
- The vision document's "First Gate" example (`loopr -C ... plan "Add a --version flag..."`) is the degenerate string-form, not the canonical manifest-form. Both are valid; manifest is the default for non-trivial work.
