# Add --version Flag to Rust CLI

## Problem Statement

The scaffolded Rust binary built by `cargo init` has no version reporting. Users
have no way to confirm which version of the tool they are running. Adding
`--version` is the baseline expectation for any CLI tool and is required before
the binary is useful in scripted environments.

## Goals

- The binary prints the crate version when invoked with `--version`
- The binary retains its existing behavior when invoked without `--version`
- A cargo test verifies the version output matches `Cargo.toml`

## Requirements

| As a... | I want to... | So that... |
|---------|-------------|-----------|
| user | run `./e2e-target --version` | I can confirm which version I have installed |
| CI pipeline | parse the version output | I can assert the deployed binary matches the expected release |

## Scope

- Modify `src/main.rs` to handle `--version`
- Add a test that asserts version output

## Constraints

- No external dependencies. Use `std::env::args()` for argument parsing.
- Version string must come from `env!("CARGO_PKG_VERSION")`, not a hardcoded literal.
- Keep the existing Hello World output when `--version` is not passed.

## Contracts

No contract changes. This plan adds a CLI flag to an existing binary without
defining new data models or public interfaces.

## Acceptance Criteria

| Given... | When... | Then... |
|----------|---------|---------|
| a compiled binary | the user runs `./e2e-target --version` | stdout contains the version string from Cargo.toml |
| a compiled binary | the user runs `./e2e-target` with no flags | stdout prints the existing Hello World message |
| the test suite | `cargo test` runs | the version test passes |

### Final Validation

```
cargo test
```

## Work Items

- **Add --version to main.rs**: Parse `std::env::args()`. If the first argument is
  `--version`, print `env!("CARGO_PKG_VERSION")` and exit with code 0. Otherwise
  execute the existing body. All changes in `src/main.rs`.

- **Add version test**: In `src/main.rs` under `#[cfg(test)]`, add a test that
  builds the binary with `std::process::Command` and asserts the output of
  `--version` contains the string from `env!("CARGO_PKG_VERSION")`.
