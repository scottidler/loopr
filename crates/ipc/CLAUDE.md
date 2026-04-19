# ipc

Typed wire protocol between the daemon and its clients (short-lived CLI dispatch now, long-lived TUI later). Message definitions and framing live here; the actual socket transport is consumed by whoever needs to speak the protocol.

## In scope

- Message enums: `Request`, `Response`, `Event` with `#[serde(tag = "kind")]` or equivalent tagging
- `deny_unknown_fields` on every type that crosses the wire
- Framing: the bytes-on-the-wire format (length-prefixed or newline-delimited; chosen once, documented)
- Serde derives, versioning, and forward/backward compat rules
- Round-trip tests: message to bytes to message, byte stability

## Out of scope

- Async I/O, socket acceptance, connection lifecycle. Those live in whoever consumes `ipc` (today that's `loopr`; later, possibly a `daemon` crate if the reactive loop grows enough to deserve one).
- LLM calls (`llm`), tool execution (`tools`), worktree lifecycle (`worktree`), persistence (`store`).
- TUI rendering. That lands in its own `tui` crate when we start building it.
- Orchestration policy (who handles which request). That's `loopr`.

## Rule

This crate must compile without `tokio`, `reqwest`, or any network/LLM dependency. Protocol definitions are pure data. If a message needs a record type, that type belongs in `domain` (the shared vocabulary) and gets referenced here.

The same principle that makes stage-to-stage seams typed Rust calls makes the daemon-client seam a typed protocol crate: seam-drift (kebab/snake mismatch, forgotten fields, renamed variants) becomes a compile error, not a runtime failure when the TUI connects.

## Dependencies

`serde` + `serde_json` (or equivalent) and `domain`. Added via `cargo add` at the time the first message needs them, not speculatively.

## See also

- [../../CLAUDE.md](../../CLAUDE.md): project-wide rules and crate map
- [../../docs/vision.md](../../docs/vision.md): architectural shape
- [docs/CLAUDE.md](docs/CLAUDE.md): where this crate's design docs go
- [.otto.yml](.otto.yml): scoped CI for this crate (`otto ci` inside this dir)
