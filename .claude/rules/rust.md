---
# Loopr Rust Rules
---

# Rust Conventions (Loopr-specific)

## Test Organization

Test code must **not** live in `#[cfg(test)]` blocks inside implementation files.

- Unit tests belong in a sibling `tests.rs` or `tests/` module directory next to the implementation file.
- Integration and FSM tests belong under `src/tests/`.

### Correct

```
src/agents/coordinator/mod.rs       ← implementation only
src/agents/coordinator/tests.rs     ← unit tests for coordinator
```

or, when the test module itself needs sub-files:

```
src/agents/coordinator/mod.rs
src/agents/coordinator/tests/
src/agents/coordinator/tests/mod.rs
src/agents/coordinator/tests/fsm.rs
```

### Incorrect

```rust
// src/agents/coordinator/mod.rs
impl Coordinator { ... }

#[cfg(test)]
mod tests {          // ← forbidden: inline test block in impl file
    ...
}
```

### Why

Keeping tests out of implementation files reduces noise when reading source, makes the module boundary explicit, and avoids the pattern where tests accumulate at the bottom of already-large files.

`tests.rs` and `tests/mod.rs` are equivalent in Rust's module system - use `tests.rs` for a single file, graduate to `tests/` only when the test module itself needs to be split.

## Banned Crates

| Crate | Reason | Alternative |
|-------|--------|-------------|
| `async-trait` | Unnecessary since Edition 2024 / Rust 1.75+. Generates hidden `Pin<Box<dyn Future>>` that we can write explicitly when needed. | Native `async fn` in traits (non-dyn); manual `Pin<Box<dyn Future + Send + 'a>>` (dyn-required) |
