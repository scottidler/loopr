# derive

Procedural macros for the workspace. Pure compile-time codegen, no runtime.

## In scope

- `#[derive(Fsm)]` — const transition tables with role guards; generates `validate_transition`, `validate_override`, `is_terminal`
- `#[derive(Record)]` — TaskStore `Record` impl for domain record types (key extraction, serde wiring)
- Future derives that generate code you'd be willing to hand-write

## Out of scope

- **Function-like macros** (`my_macro!(...)`). They hide expansion, encourage DSL-style "magic", and re-introduce the string-keyed-dispatch failure class v5 is built to avoid. If you need one, stop and talk.
- **Attribute macros** (`#[attr] fn ...`). Same reasoning; they rewrite bodies in ways that are hard to read and debug.
- Runtime helpers, traits, or types. Those belong in `domain` or `runtime`.
- Anything requiring network, filesystem, or env at macro-expansion time.

## Rule

One rule that makes this crate safe: **every derive must generate code you'd be willing to hand-write.** If the expansion is "clever" — if it pulls in I/O, if it walks attribute metadata in complicated ways, if `cargo expand` produces something surprising — it doesn't belong here.

The v4 lesson that motivates this crate's narrow scope: composition engines and attribute-driven registries produced a class of seam-drift bugs that compiled clean and died at runtime. Derives that stamp out pure data tables (FSM transitions) or pure trait impls (Record) are not in that failure class; function-like and attribute macros can easily become part of it.

## Dependencies

`proc-macro2`, `syn`, `quote` when needed. Added via `cargo add` at the time a macro actually needs them, not speculatively.

## See also

- [../../CLAUDE.md](../../CLAUDE.md): project-wide rules and crate map
- [../../docs/v5-shape.md](../../docs/v5-shape.md): architectural shape
- [docs/CLAUDE.md](docs/CLAUDE.md): where this crate's design docs go
- [.otto.yml](.otto.yml): scoped CI for this crate (`otto ci` inside this dir)
