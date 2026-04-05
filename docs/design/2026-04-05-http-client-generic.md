# Design Document: Convert HttpClient to Generic

**Author:** Scott A. Idler
**Date:** 2026-04-05
**Status:** Implemented
**Review Passes Completed:** 5/5

## Summary

`ValidatorClient` (named `LlmClient` in `src/validator/client.rs`) holds
`Box<dyn HttpClient>`, making `HttpClient` an async trait that cannot drop
`#[async_trait]`. Adding a generic type parameter `<H: HttpClient = ReqwestClient>`
removes the `dyn`, enables async_trait removal from this trait, and requires zero
changes to production callers thanks to the default type parameter.

## Problem Statement

### Background

`docs/design/2026-04-05-post-async-migration-cleanup.md` Phase 2 blocked on four async
traits used as `dyn` objects. `HttpClient` is the smallest blast radius of the four:
one struct field in one file, with a default-type-parameter escape hatch available.

### Problem

`HttpClient` has an async method (`post`). With `Box<dyn HttpClient>` in the struct,
native `async fn` in traits is not object-safe and `#[async_trait]` cannot be removed.
Two annotations remain: the trait definition and the `ReqwestClient` impl.

### Goals

- Remove `Box<dyn HttpClient>` from `ValidatorClient`
- Replace with `<H: HttpClient = ReqwestClient>` generic parameter
- Remove `#[async_trait]` from `HttpClient` trait and `ReqwestClient` impl
- Zero changes to production callers of `LlmClient::with_reqwest()`
- `otto ci` passes

### Non-Goals

- Converting any other `dyn` trait
- Removing `async_trait` crate (other traits still use it)
- Changing `HttpClient::post` signature

## Proposed Solution

### Implementation Plan

**`src/validator/client.rs`:**

```rust
// Before
pub struct LlmClient {
    config: ValidatorConfig,
    http_client: Box<dyn HttpClient>,
}
impl LlmClient {
    pub fn new(config: ValidatorConfig, http_client: Box<dyn HttpClient>) -> Self { ... }
    pub fn with_reqwest(config: ValidatorConfig) -> Self {
        Self::new(config, Box::new(ReqwestClient::new()))
    }
}

// After
pub struct LlmClient<H: HttpClient = ReqwestClient> {
    config: ValidatorConfig,
    http_client: H,
}
impl<H: HttpClient> LlmClient<H> {
    pub fn new(config: ValidatorConfig, http_client: H) -> Self { ... }
}
impl LlmClient<ReqwestClient> {
    pub fn with_reqwest(config: ValidatorConfig) -> Self {
        Self::new(config, ReqwestClient::new())
    }
}
```

Remove `#[async_trait]` from the `HttpClient` trait definition and the
`impl HttpClient for ReqwestClient` block. Remove `use async_trait::async_trait` from
the top of the file.

**Test code:** Callers using `LlmClient::new(config, Box::new(MockHttpClient))` change to
`LlmClient::new(config, MockHttpClient)`. All test mock `impl HttpClient` blocks lose
their `#[async_trait]`.

**Commit:**
```
refactor(validator): convert Box<dyn HttpClient> to generic H: HttpClient
```

## Alternatives Considered

### Alternative: Keep Box<dyn HttpClient>

- **Pros:** No change
- **Cons:** async_trait on HttpClient layer stays forever; violates rust.md DI rule
- **Why not chosen:** Default type param makes migration cost near zero

## Technical Considerations

### Testing Strategy

- `otto ci` after the change
- Test files: `src/validator/client.rs` tests, `src/validator.rs` tests, `src/evaluator.rs`
  tests — all use mock `HttpClient` impls; those mocks lose `#[async_trait]` and pass
  concrete types instead of `Box<dyn>`

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|------|------------|--------|------------|
| Callers in daemon/handlers use `dyn HttpClient` explicitly | Low | Low | Grep confirms only 2 production call sites, both in client.rs itself |
| Generic param breaks `LlmClient` usage in `src/validator.rs` | Low | Low | Compiler catches it; default param handles the common case |

## Open Questions

None — blast radius fully enumerated from grep.

## References

- `src/validator/client.rs` - `HttpClient` trait + `LlmClient` struct
- `docs/design/2026-04-05-post-async-migration-cleanup.md` - parent doc (Phase 2)
