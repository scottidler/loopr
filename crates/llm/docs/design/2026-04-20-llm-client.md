# Design Document: Stage 6 LlmClient Trait and Anthropic Backend

**Author:** Claude (with Scott)
**Date:** 2026-04-20
**Status:** Implemented
**Review Passes Completed:** 5/5 + Architect round 1
**Scope gate:** [`docs/design/2026-04-20-stage-6-scope.md`](../../../../docs/design/2026-04-20-stage-6-scope.md) — decisions D6, D7, A+3, U+1, U+2, U+3, U+4 are locked there and referenced by row rather than re-litigated.

## Summary

Introduce a `LlmClient` trait and one concrete `AnthropicClient` implementation in `crates/llm/`. The trait exposes a single buffered, non-streaming `complete_with_tool` method; the Anthropic backend talks to the Messages API with `tool_choice` locked to a caller-supplied tool schema. Errors surface as a typed `LlmError` enum that distinguishes `Retryable` from `Fatal` so callers implement retry policy without string-matching error messages. Streaming, multi-turn, and model-tier resolution are Stage 7+ concerns and are explicitly out of scope.

This is the second of three Stage 6 design docs; [`hierarchy.md`](../../../domain/docs/design/2026-04-20-hierarchy.md) shipped the `Work` record, and `plan-then-decompose.md` will follow to wire the decomposer's call to this trait into `loopr plan`.

## Problem Statement

### Background

Stage 5 ships a daemon that persists `Plan` records but does nothing with them. Stage 6's exit criterion is that `loopr plan "..."` produces at least one `Work` record persisted to `.loopr/taskstore/works.jsonl`. The decomposer is the code that turns a Plan into Works, and the decomposer needs to call an LLM. `hierarchy.md` shipped the `Work` record itself; this doc ships the network boundary the decomposer calls through.

The v3 implementation (`src/agents/llm_client.rs`, ~600 lines) has SSE streaming, multi-turn history, and a TUI broadcast channel. v4 had ~1000 lines of the same. Stage 6 needs ~10% of that surface: one buffered call, one tool schema, typed errors, no streaming, no history.

### Problem

`crates/llm/src/lib.rs` is empty. The `decomposer` crate depends on `llm` (see roadmap.md crate map) and cannot be written until a minimal trait exists. The trait has to be small enough to land in Stage 6 but shaped so Stage 7's streaming + multi-turn + tier-resolution additions are additive, not a rewrite.

### Goals

- A `LlmClient` trait with a single async method that takes a system prompt, a user prompt, and a tool schema, and returns the model's tool-call response.
- A concrete `AnthropicClient` that implements the trait against Anthropic's Messages API, using a single buffered HTTP call (no SSE).
- Typed `LlmError` enum with `Retryable { reason: String }` and `Fatal { reason: FatalReason }` variants. `FatalReason` is itself typed (auth failure, context-limit exceeded, schema validation failure, invalid config, …).
- `LlmConfig` struct loadable from `.loopr/config.yml`, composed into the top-level `Config` by `loopr`. Fields: `model`, `max-tokens`, `temperature`, `api-key-env` (the NAME of the env var holding the key, never the literal key), `api-base-url` (overridable for Bedrock / proxies / local mocks; defaults to the hosted Anthropic endpoint).
- Generics-based DI (no `dyn`); the trait is not object-safe and doesn't need to be.
- Seam tests exercising request-body serialization, response-body deserialization (success + error paths), and the HTTP status → `LlmError` mapping.

### Non-Goals

- **Streaming / SSE.** Stage 7 earns `complete_streaming` and `complete_agentic` when the agents crate needs them. The trait does not expose those methods yet.
- **Multi-turn history.** Stage 6's decomposer is one-shot. Message history is Stage 7.
- **Model-tier resolution** (`primary` / `lightweight` / `advisor` tier names in config). Stage 6 accepts a literal model ID string only. Tier resolution lands when AutoResearch or the agents crate needs to sweep configurations.
- **Cost accounting spans with `input_tokens` / `output_tokens` / `cost_usd`.** The vision.md target remains; Stage 6 emits a span carrying the model ID and duration but not token counts. Token extraction from the Anthropic response is a one-line follow-up that need not ride with the trait's first cut.
- **Prompt assembly.** The decomposer builds its own prompt strings for Stage 6 (scope memo D11 defers `context-builder.md` to Stage 7). `llm` sees only ready-to-send `&str`s.
- **`Message` / multi-turn types.** vision.md's "ready-to-send `Vec<Message>`" surface is Stage 7+. Stage 6's trait takes bare `&str` for system and user because there is exactly one user turn.
- **LLM response cache, retries, circuit breakers.** Retry policy lives in the caller (decomposer picks a `RetryStrategy`); `llm` reports `Retryable` and stops.

## Proposed Solution

### Overview

Five files under `crates/llm/src/`:

1. `error.rs` — `LlmError` + `FatalReason` typed enums.
2. `tool.rs` — `ToolSchema` and `ToolCall` structs that travel across the trait boundary.
3. `config.rs` — `LlmConfig` struct with serde + `#[serde(rename_all = "kebab-case")]`.
4. `client.rs` — the `LlmClient` trait.
5. `anthropic.rs` — the `AnthropicClient` concrete impl.

`lib.rs` wires them together and re-exports the public surface.

### Architecture

```
crates/llm/
├── Cargo.toml                      (deps: domain, telemetry, reqwest, serde, serde_json, thiserror, tokio)
├── .otto.yml                       (scoped CI, mirrors domain's)
├── CLAUDE.md                       (existing)
├── docs/
│   └── design/2026-04-20-llm-client.md   (this doc)
└── src/
    ├── lib.rs                      (wire + re-exports)
    ├── error.rs                    (LlmError + FatalReason)
    ├── tool.rs                     (ToolSchema, ToolCall)
    ├── config.rs                   (LlmConfig)
    ├── client.rs                   (LlmClient trait)
    └── anthropic.rs                (AnthropicClient impl)
```

Seam boundary: the `AnthropicClient` holds a `reqwest::Client` (one per daemon per U+3) and an `LlmConfig`. The trait's `complete_with_tool` method takes `&self`, so the client is shareable across spawned tasks by `Arc<AnthropicClient>` if the decomposer ever spawns in parallel.

### Data Model

#### `LlmError` (`src/error.rs`)

```rust
#[derive(Debug, thiserror::Error)]
pub enum LlmError {
    #[error("retryable LLM call failure: {reason}")]
    Retryable { reason: String },

    #[error("fatal LLM call failure: {reason:?}")]
    Fatal { reason: FatalReason },
}

#[derive(Debug)]
pub enum FatalReason {
    /// 401/403 from the provider. API key missing, invalid, or revoked.
    Auth(String),

    /// 400 with an `invalid_request_error` or model-side refusal.
    BadRequest(String),

    /// The response's content array had no `tool_use` block, or the
    /// block's `input` field wasn't parseable JSON. This captures
    /// STRUCTURAL problems only; semantic validation (required fields,
    /// field types matching the caller's JSON Schema) is the caller's
    /// job, not `llm`'s. The decomposer uses this variant as the
    /// signal to try its fallback text-parse path.
    ///
    /// Classified `Fatal` on purpose (Architect round 1 flagged the
    /// alternative): when `tool_choice: {type: "tool", name: "..."}`
    /// is set but the response has no `tool_use` block, the model
    /// has explicitly declined the tool contract — re-running the
    /// identical prompt is rerunning-for-its-own-sake, not recovery.
    /// "input not valid JSON" from Anthropic is extraordinarily rare
    /// and more likely a broken proxy than transient model noise.
    /// If real-world data shows transient JSON malformations at
    /// temperature 0.3 warrant a blind retry, split this into
    /// `SchemaValidation` (Fatal, no tool_use block) and
    /// `SchemaMalformed` (Retryable, unparseable JSON) then.
    SchemaValidation(String),

    /// The model's response was truncated at `max_tokens` before
    /// producing a complete tool-input payload. `used` is
    /// `usage.output_tokens` from the Anthropic response; `limit` is
    /// the `max_tokens` value the caller configured. Marked Fatal (not
    /// Retryable) because retrying with the SAME config will fail
    /// identically. A caller that chooses to retry with a LARGER
    /// `max_tokens` can do so explicitly.
    ContextExhausted { used: u32, limit: u32 },

    /// Config was missing the API key, or the key env var was unset.
    ConfigInvalid(String),
}
```

The `Retryable` / `Fatal` split encodes *invariants*, not *policy*. "Retrying this same call with this same config will almost certainly fail" is an invariant the caller cannot learn by inspecting stop_reason directly, and string-dispatching on the underlying condition is exactly the v4-era failure mode vision.md and scope memo A+3 exist to prevent. Retry *counts* and *backoff shape* remain caller-owned (e.g. the decomposer picks a `RetryStrategy`); the taxonomy here just surfaces the what-retrying-achieves signal in a form callers can `match` on.

Classification decisions:

| HTTP status / condition | Variant |
|---|---|
| 401, 403 | `Fatal(Auth(body))` |
| 400 without a specific marker | `Fatal(BadRequest(body))` |
| 408, 429, 5xx | `Retryable { reason }` |
| `reqwest::Error` with `is_timeout()` or `is_connect()` (including the 120s client ceiling) | `Retryable { reason }` |
| Response status 2xx but body isn't valid JSON (e.g. HTML injected by a corporate proxy) | `Retryable { reason: "non-JSON response body" }` |
| Response parsed, no `tool_use` block present | `Fatal(SchemaValidation("no tool-use content block"))` |
| Response has `tool_use` block but `input` is not valid JSON | `Fatal(SchemaValidation("tool-use input unparseable: …"))` |
| `stop_reason == "max_tokens"` with incomplete tool input | `Fatal(ContextExhausted { … })` |
| API key env var unset or empty | `Fatal(ConfigInvalid("…"))` |

#### `ToolSchema` and `ToolCall` (`src/tool.rs`)

```rust
/// A tool definition the caller wants the model to invoke. The shape
/// matches Anthropic's `tools` array element.
#[derive(Debug, Clone, Serialize)]
pub struct ToolSchema {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

/// The model's tool-use response, extracted from the first `tool_use`
/// content block. Opaque `input` — the decomposer's schema-specific
/// code parses it.
#[derive(Debug, Clone)]
pub struct ToolCall {
    pub tool_name: String,
    pub input: serde_json::Value,
}
```

`input_schema` is `serde_json::Value` rather than a typed struct so the decomposer can ship any JSON Schema the Anthropic API accepts without cluttering `llm` with caller-specific shape. Validation that the returned `input` matches the caller's schema is the caller's job; `llm` reports only "tool-use block present and well-formed JSON" vs. "absent or unparseable" as `SchemaValidation`.

#### `LlmConfig` (`src/config.rs`)

```rust
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct LlmConfig {
    /// Literal Anthropic model ID. Stage 7+ will promote this to a
    /// tier name or literal via a small resolver.
    pub model: String,

    /// Upper bound on generation tokens. Anthropic default is model-
    /// specific; we pin to match the scope memo (8192) so the
    /// decomposer's tool-input JSON does not truncate.
    pub max_tokens: u32,

    /// Sampling temperature in [0, 1].
    pub temperature: f32,

    /// Name of the env var holding the API key. Never the literal key.
    pub api_key_env: String,

    /// Base URL of the Anthropic-compatible Messages API. Defaults
    /// to the hosted endpoint. Overridable to support local mocks
    /// (testing), corporate proxies, or Anthropic-compatible
    /// gateways (e.g. AWS Bedrock fronted by a shim). The value is
    /// concatenated with `/v1/messages` at call time; must not end
    /// with a slash.
    pub api_base_url: String,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            model: "claude-sonnet-4-6".into(),
            max_tokens: 8192,
            temperature: 0.3,
            api_key_env: "ANTHROPIC_API_KEY".into(),
            api_base_url: "https://api.anthropic.com".into(),
        }
    }
}
```

API-key precedence (U+2) is enforced outside `LlmConfig`:

- `AnthropicClient::new(config, api_key_override: Option<String>)` takes the precedence-resolved key.
- The daemon resolves: `--api-key` CLI flag (when wired) > `env::var(&config.api_key_env)` > `env::var("ANTHROPIC_API_KEY")` as a last fallback.
- `AnthropicClient::new` returns `Fatal(ConfigInvalid("no API key"))` if the resolved key is empty.

### API Design

```rust
pub trait LlmClient {
    fn complete_with_tool<'a>(
        &'a self,
        system: &'a str,
        user: &'a str,
        tool: ToolSchema,
    ) -> impl Future<Output = Result<ToolCall, LlmError>> + Send + 'a;
}
```

Stage 6 shape locked by scope memo D6. Generics rather than `dyn` (U+4). `impl Future<...> + Send` rather than `async fn` in the trait because we want the Send bound explicit; tokio-spawned tasks need it.

A single named lifetime `'a` binds `&self`, `system`, and `user` so the returned future captures all three under one constraint. The anonymous `+ '_` form (an earlier draft of this doc) does not unify the three elided lifetimes and the compiler rejects it: `&self` and each `&str` get distinct anonymous lifetimes and the return type must outlive all of them together. The Architect's round-1 review flagged the underlying concern — we want the prompts borrowed, not `String`-copied — and the named-lifetime form is the idiomatic way to say that.

Callers use the trait via generic bounds:

```rust
pub async fn decompose<L: LlmClient>(plan: &Plan, llm: &L) -> Result<Vec<Work>, DecomposerError> {
    let response = llm.complete_with_tool(system_prompt, user_prompt, submit_schema).await?;
    // …
}
```

`AnthropicClient` implements the trait:

```rust
pub struct AnthropicClient {
    http: reqwest::Client,
    config: LlmConfig,
    // NOTE: the API key is NOT a field. It lives only inside
    // `http`'s default_headers as a sensitive HeaderValue.
}

/// Wall-clock ceiling for a single Anthropic call. Sonnet with 8192
/// max_tokens usually completes in 1-10s; 120s is 12x headroom for
/// slow paths without letting a hung call stall the decomposer
/// indefinitely. The daemon is async, so other IPC traffic is
/// unaffected while this call is outstanding — the ceiling exists
/// only to guarantee the decomposer's request handler eventually
/// returns an error instead of hanging forever.
const REQUEST_TIMEOUT_SECS: u64 = 120;
const ANTHROPIC_VERSION: &str = "2023-06-01";

impl AnthropicClient {
    pub fn new(config: LlmConfig, api_key: String) -> Result<Self, LlmError> {
        if api_key.is_empty() {
            return Err(LlmError::Fatal {
                reason: FatalReason::ConfigInvalid("empty API key".into()),
            });
        }

        // Build default headers once and install them on the client.
        // `set_sensitive(true)` marks the value so `reqwest`'s own
        // Debug impls redact it; the raw `api_key` String is dropped
        // when this function returns, so no struct field holds the
        // secret.
        use reqwest::header::{HeaderMap, HeaderValue};
        let mut headers = HeaderMap::new();
        let mut key_val = HeaderValue::from_str(&api_key).map_err(|e| LlmError::Fatal {
            reason: FatalReason::ConfigInvalid(format!("invalid API key header: {e}")),
        })?;
        key_val.set_sensitive(true);
        headers.insert("x-api-key", key_val);
        headers.insert("anthropic-version", HeaderValue::from_static(ANTHROPIC_VERSION));

        let http = reqwest::Client::builder()
            .default_headers(headers)
            .timeout(std::time::Duration::from_secs(REQUEST_TIMEOUT_SECS))
            .build()
            .map_err(|e| LlmError::Fatal {
                reason: FatalReason::ConfigInvalid(format!("reqwest client build failed: {e}")),
            })?;
        Ok(Self { http, config })
    }
}

impl LlmClient for AnthropicClient {
    fn complete_with_tool(
        &self,
        system: &str,
        user: &str,
        tool: ToolSchema,
    ) -> impl Future<Output = Result<ToolCall, LlmError>> + Send + '_ {
        async move {
            // Build the Messages API request body, POST to
            // `{config.api_base_url}/v1/messages` (headers already
            // baked into `self.http` via `default_headers`), classify
            // HTTP status and reqwest errors into `LlmError` per the
            // table in Data Model, and extract the first tool_use
            // content block as a `ToolCall`.
        }
    }
}
```

Request-body shape (Anthropic Messages API):

```json
{
  "model": "claude-sonnet-4-6",
  "max_tokens": 8192,
  "temperature": 0.3,
  "system": "<system prompt>",
  "messages": [{"role": "user", "content": "<user prompt>"}],
  "tools": [{
    "name": "submit_decomposition",
    "description": "…",
    "input_schema": { … }
  }],
  "tool_choice": {"type": "tool", "name": "submit_decomposition"}
}
```

Response-body extraction:

```json
{
  "content": [{"type": "tool_use", "id": "…", "name": "submit_decomposition", "input": {…}}],
  "stop_reason": "tool_use",
  "model": "claude-sonnet-4-6-20260115",
  "usage": {"input_tokens": …, "output_tokens": …}
}
```

Extract the first `content[i]` with `type == "tool_use"` and whose `name` matches the caller's tool. `input` is returned as `ToolCall.input: serde_json::Value`.

### Implementation Plan

#### Phase 1: Scaffold types and config
**Model:** sonnet
- Create `src/error.rs` (`LlmError`, `FatalReason`).
- Create `src/tool.rs` (`ToolSchema`, `ToolCall`).
- Create `src/config.rs` (`LlmConfig`).
- Wire into `src/lib.rs` with re-exports.
- `cargo add thiserror` (via `cargo add`, no version pinning).
- Compile check: `cargo check -p llm` passes.

#### Phase 2: `LlmClient` trait
**Model:** sonnet
- Create `src/client.rs` with the trait definition.
- Re-export from `lib.rs`.
- Compile check passes.

#### Phase 3: `AnthropicClient` impl
**Model:** sonnet
- `cargo add reqwest --no-default-features --features json,rustls-tls,http2` and `cargo add serde_json` (workspace already has serde). Explicit `rustls-tls` avoids pulling system OpenSSL into the dep graph.
- Create `src/anthropic.rs` with `AnthropicClient` struct, `new` constructor, and trait impl.
- Pin `const ANTHROPIC_VERSION: &str = "2023-06-01";` at the top of `anthropic.rs`; this is the long-stable Messages API version and the same value v3 uses.
- Implement request-body construction, HTTP call with `x-api-key` and `anthropic-version` headers, status → error mapping, tool-use extraction.
- Re-export `AnthropicClient` from `lib.rs`.
- Compile check passes.

#### Phase 4: Tests
**Model:** sonnet
- `cargo add --dev wiremock` (hermetic local HTTP mock).
- `cargo add --dev tokio --features macros,rt` (test-time async runtime; production code only consumes `reqwest`'s async surface transitively).
- `crates/llm/tests/anthropic.rs`:
  - Request-body serialization: given a system/user/tool, assert the JSON body sent.
  - Header assertions: `x-api-key` is the resolved key; `anthropic-version` matches the pinned const.
  - 200 happy path: mock returns a valid `tool_use` response; assert `ToolCall` contents.
  - 401 → `Fatal(Auth)` mapping.
  - 429 → `Retryable` mapping.
  - 500 → `Retryable` mapping.
  - `stop_reason: "max_tokens"` with incomplete tool input → `Fatal(ContextExhausted { used, limit })` with both fields populated from the response.
  - Missing `tool_use` content block → `Fatal(SchemaValidation)`.
  - `tool_use` block with malformed (non-JSON) `input` → `Fatal(SchemaValidation)`.
  - 200 status with HTML response body (simulating a corporate proxy) → `Retryable`.
  - Mock server that sleeps longer than `REQUEST_TIMEOUT_SECS` → `Retryable` via `is_timeout()`. Test uses a reduced timeout constant override to keep the suite fast; don't literally wait 120s in CI.
- `crates/llm/src/config/tests.rs`:
  - Default values match the scope memo (`claude-sonnet-4-6`, 8192, 0.3, `ANTHROPIC_API_KEY`) plus `api-base-url` defaulting to `https://api.anthropic.com`.
  - Kebab-case wire form round-trips (notably `api-base-url` and `api-key-env`).
  - `deny_unknown_fields` rejects an unknown key (e.g. `max_token` singular).
  - Missing required fields (e.g. YAML without `model:`) produces a clean `serde` error rather than silently defaulting. This pins the "strict config, no hidden defaults" posture; a user who edits `.loopr/config.yml` by removing a field gets a loud error instead of an invisible fallback.
  - A YAML with `api-base-url: http://localhost:8080` round-trips cleanly; the value flows through to `AnthropicClient` without mangling. This pins the Bedrock / proxy / local-mock override path.
- `AnthropicClient::new` rejects empty API key with `Fatal(ConfigInvalid)`.
- `AnthropicClient::new` with an invalid `api-base-url` (e.g. embedded control characters) rejects with `Fatal(ConfigInvalid)` rather than silently constructing a broken client.
- The wiremock-based integration tests in `tests/anthropic.rs` point `api-base-url` at the mock server's `uri()`; this exercises the override path on every Anthropic seam test for free, not just the happy one.
- `otto ci` at `crates/llm/` passes.

#### Phase 5: Ship
**Model:** sonnet
- Update design doc status → Implemented.
- Commit, `/bump -a`, push, install.
- `llm-client.md` done; `plan-then-decompose.md` is the next design doc.

## Alternatives Considered

### Alternative 1: Ship v3's full `AgentLlmClient` verbatim

- **Description:** Port the ~600-line v3 implementation (SSE streaming, multi-turn history, retry loop) straight across and stub the unused paths.
- **Pros:** Stage 7 lands without another `llm` change.
- **Cons:** Six times the surface, six times the chance of shipping a bug unrelated to Stage 6's failing run. Violates the working rule "One design doc at a time, motivated by a failing run" — Stage 6's failing run is "no decomposer call," not "no streaming UI."
- **Why not chosen:** Scope memo D6. Stage 7 adds streaming and multi-turn when the agents crate motivates it; the `LlmClient` trait gains new methods additively. A `complete_with_tool`-only trait is forward-compatible.

### Alternative 2: `async fn` in trait (no explicit `Send` bound)

- **Description:** Declare the trait method as `async fn complete_with_tool(...)` and rely on Rust's implicit future type.
- **Pros:** Prettier syntax.
- **Cons:** The returned future is not `Send` by default, which breaks `tokio::spawn` call sites that want to move the future across threads. Downstream code would grow `.boxed()` calls or trait methods would sprout `Send + Sync` requirements on every `&self` method.
- **Why not chosen:** `impl Future<Output = ...> + Send` is ugly but makes the Send bound explicit and compile-time-checked. Scope memo U+4 locks generics over dyn; this flows through to wanting explicit Send on the future.

### Alternative 3: Bare `eyre::Report` error instead of typed `LlmError`

- **Description:** Return `eyre::Result<ToolCall>` and let callers parse error context strings to decide retry vs. abort.
- **Pros:** Fewer types to maintain.
- **Cons:** The decomposer and Stage 7's retry strategies would have to match on `err.to_string().contains("rate_limit")`-style string checks. That directly violates vision.md's typed-seams thesis; the post-mortem rationale for leaving v4 behind flags string-keyed dispatch as a recurring source of drift bugs.
- **Why not chosen:** Scope memo A+3 locks typed Retryable / Fatal directly: "Not a bare `RpcError::Internal(String)` dump." Also `crates/llm/CLAUDE.md` is a library per rules/rust.md, so `thiserror` enums are the correct error pattern anyway; `eyre` is for CLIs.

### Alternative 4: Put `ToolSchema` and `ToolCall` in the `tools` crate

- **Description:** The `tools` crate already owns agent-callable tools; reuse its types for the LLM's tool-use API.
- **Pros:** Single source of truth for "what a tool is."
- **Cons:** The `tools` crate handles local subprocess execution (Read, Write, Bash); `llm`'s tool-use types describe JSON Schema going to a remote API. They look similar but serve different domains; coupling them would make `llm` depend on `tools`, which vision.md forbids (the dep graph must stay acyclic and `llm` must not pull in subprocess concerns).
- **Why not chosen:** Different problems; shared keyword is not shared type. The decomposer, which bridges them, constructs an `llm::ToolSchema` in its own prompt-assembly code; no tools-crate dep needed.

### Alternative 5: Use `ureq` instead of `reqwest`

- **Description:** `ureq` is smaller, blocking, and simpler.
- **Pros:** Fewer transitive deps.
- **Cons:** Would force the trait method to be sync (blocking the async runtime for multi-second LLM calls is a deadlock risk under concurrency) or wrap the blocking call in `tokio::task::spawn_blocking`. Either is worse than `reqwest`'s native async path.
- **Why not chosen:** `crates/llm/CLAUDE.md` pre-approves either; the daemon is async and we want the LLM call on the async runtime's IO reactor, not a blocking-thread pool.

## Technical Considerations

### Dependencies

New (via `cargo add`, no version pinning):
- `reqwest` with `json` feature — HTTP client, async.
- `thiserror` — typed error derives.
- `serde_json` — `serde_json::Value` for `ToolSchema.input_schema` and `ToolCall.input`.
- `wiremock` (dev-dep) — hermetic HTTP mock for tests.

Existing workspace deps already available:
- `domain` — for any record types this crate surfaces (none in Stage 6; the trait is primitive-shaped).
- `telemetry` — for `tracing` span emission.
- `serde` with `derive` feature.
- `tokio` — for the async runtime in tests.

### Performance

- One buffered HTTP call per `complete_with_tool` invocation.
- Decomposer makes one call per `loopr plan`. Stage 9's E2E target runs this once per run.
- Expected latency: 1-10s wall-clock for a sonnet call with 8192 max tokens. This bounds the daemon's `plan.create` handler latency; the daemon remains async so other IPC calls are unblocked.
- `reqwest::Client` pools connections; one client per daemon is enough (U+3).

### Security

- API key never serialized to disk: only the env var NAME (`api-key-env`) lives in `.loopr/config.yml`. The resolved key is installed into `reqwest::Client::default_headers` as a sensitive `HeaderValue` during `AnthropicClient::new` and the original `String` is dropped. `AnthropicClient` holds no `api_key` field, so `#[derive(Debug)]` on it (or a parent struct) cannot leak the secret, panic payloads don't contain it, and `reqwest`'s own `Debug` impl redacts the sensitive header value automatically. Per Architect round 1.
- `AnthropicClient::new` rejects empty keys fast; no accidental calls with an unset env var.
- Token counts, prompts, and responses are logged to the structured events file; this file lives at `.loopr/runs/<run-id>/events.log` which is NOT committed. Users pushing their `.loopr/` directory must understand this, but the taskstore `.gitignore` already excludes `.loopr/runs/`.
- `telemetry` spans emit the prompt string as a field (for reproducibility). Stage 6 accepts this; a future `--redact` flag is a deferred enhancement tracked in vision.md.
- **Telemetry payload rules (locked by Architect round 1):**
  - **System + user prompt:** emitted as span fields, truncated to 4 KiB each with an ellipsis + original-length suffix. Bounds `events.log` growth on long prompts while keeping enough signal for debugging.
  - **Tool schema:** NOT emitted (stable per call-site; adds no per-call signal).
  - **Response `ToolCall.input`:** NOT emitted. Response size is unbounded by the model; dumping it into the tracing subscriber would balloon `.loopr/runs/<run-id>/events.log` on a single decomposition with many AC items. Callers that want the response in logs can emit their own span.
  - **HTTP headers, API key, `x-api-key` value:** NEVER emitted. `reqwest`'s `HeaderValue::set_sensitive(true)` handles this automatically.

### Testing Strategy

Seam tests, hermetic (no network):

1. **Config tests** (unit): defaults match the scope memo, serde kebab-case round-trips, `deny_unknown_fields` rejects typos.
2. **Error classification tests** (unit): each HTTP status / condition maps to the documented `LlmError` variant.
3. **Integration tests with `wiremock`** (`crates/llm/tests/anthropic.rs`):
   - Starts a local mock HTTP server.
   - Mock returns canned Anthropic responses (200 with `tool_use`, 401, 429, 500, truncated).
   - Assert `complete_with_tool` returns the correct `ToolCall` or `LlmError` variant.
   - Assert the request body sent to the mock matches the Messages API shape with the caller's system/user/tool.
4. **No Anthropic API calls in CI.** Even with a dev key, this would pin CI to a live third-party service and burn dollars. `wiremock` is the standard Rust pattern.

### Rollout Plan

- Single-crate change: `crates/llm/` gains code. Downstream crates (`decomposer`, `agents`) don't yet depend on `llm`'s surface because they haven't been written.
- One commit per phase (five total), one version bump (patch).
- `otto ci` at both `crates/llm/` and workspace root must pass.

## Risks and Mitigations

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| Anthropic changes the Messages API shape between Stage 6 and Stage 7's agents landing. | Low | High | Anthropic has a versioning header (`anthropic-version: 2023-06-01` or later); pin it explicitly. Seam tests catch field-rename regressions because they assert exact request-body JSON. |
| `wiremock` dev-dep introduces async runtime conflicts with the crate's tokio bounds. | Low | Low | Phase 4 adds `tokio` with `rt + macros` explicitly as a dev-dep so `#[tokio::test]` drives the mock server cleanly. If a runtime conflict surfaces anyway, swap to `httpmock` (same API shape). |
| `reqwest`'s default `native-tls` feature pulls in system OpenSSL and breaks hermetic CI (or on systems without libssl-dev). | Medium | Medium | Phase 3 explicitly opts out of defaults and enables `rustls-tls` instead: `cargo add reqwest --no-default-features --features json,rustls-tls,http2`. `rustls` is a pure-Rust TLS implementation with no system deps. |
| API key leaks into logs via `telemetry` span fields that capture request bodies. | Low | High | `complete_with_tool` span carries model, prompt lengths, duration; NEVER the Authorization/x-api-key header value or the raw API key. Phase 4 includes a test that snapshots emitted span fields to catch regressions. |
| `impl Future<Output = ...> + Send` syntax breaks with future Rust edition changes. | Very Low | Low | Edition 2024 stabilized the pattern. Worst case: swap to `async_trait` macro temporarily. |
| Decomposer's `submit_decomposition` tool returns a response that passes JSON parsing but has wrong shape. | Medium | Medium | `llm` reports `SchemaValidation` when the `tool_use` block is absent or unparseable-as-JSON; further shape validation is the decomposer's job (documented in plan-then-decompose.md). |
| `ContextExhausted` is classified `Fatal` but the retry policy upstream assumes it's retryable. | Medium | Medium | Doc'd explicitly in `FatalReason::ContextExhausted` doc-comment: "retrying with the same `max_tokens` will fail identically." Decomposer's retry strategy may upgrade to a larger `max_tokens` value and retry, but that's an explicit choice, not an accidental loop. |
| `anthropic-version` header value goes stale. | Medium | Low | Pin to a known-good date in `anthropic.rs` as `const ANTHROPIC_VERSION: &str = "2023-06-01"` (or latest at implementation time); Phase 5 updates it. |

## Open Questions

- [ ] `anthropic-version` header pin: Phase 3 ships with `2023-06-01` (matches v3 / v4 / long-stable). Upgrade deliberately when a specific newer feature is needed; don't track-the-latest.
- [ ] Span naming: vision.md says `ralph.<role>` / `stage.<name>` / `tool.<name>`; a raw LLM call doesn't fit any of those. Ship as `llm.anthropic` for now and revisit in Stage 7 when the agents loop wraps it in `ralph.implementer` / `ralph.reviewer` spans.
- [ ] Does `AnthropicClient` need a builder (`AnthropicClientBuilder::new().with_timeout(…)`)? Stage 6 says no (scope memo says bare `reqwest::Client::new()`). If Stage 7 needs per-call timeout overrides, earn the builder then.
- [ ] Should `LlmError::Retryable` carry a structured `retry_after: Option<Duration>` parsed from the `retry-after` header on 429? Stage 6's decomposer has a simple retry strategy that doesn't use it; leave it as `String` for now and upgrade when the agents crate's retry loop wants it.
- [ ] Token-count extraction from `usage` block: ship or defer? Leaning defer; the scope memo D6 doesn't require it for Stage 6, and `telemetry` span emission without token counts is strictly acceptable for one-shot decomposition. Earn it when cost accounting motivates it.

**Forward-referenced for Stage 7 (not Stage 6 concerns, but flagged by Architect round 1 so Stage 7's design doesn't get surprised):**

- [ ] **Connection pooling for concurrent agents.** Stage 7's agents crate will make 2-N concurrent LLM calls through the same `AnthropicClient`. Default `reqwest::Client` pool sizing may be wrong for that load; revisit `pool_max_idle_per_host` / `pool_idle_timeout` when the agents crate lands.
- [ ] **Timeout split (total vs. connect vs. read).** The 120s `REQUEST_TIMEOUT_SECS` applies to the entire request. Under a degraded-Anthropic trickle-tokens scenario, a 90%-complete response gets sliced at the boundary and reclassified `Retryable` when the model was actually succeeding. Stage 7's retry strategy may want the split to preserve near-completions.
- [ ] **Response payload size ceiling.** `reqwest::Response::json::<Value>()` reads the entire body into memory and lets `serde_json` unbound. A malicious or malfunctioning upstream returning pathologically nested JSON could overflow. Anthropic is trusted; corporate-proxy scenarios with `api-base-url` overrides are less so. Consider a max-bytes guard when such an override is detected.

## References

- [Scope memo](../../../../docs/design/2026-04-20-stage-6-scope.md) — decisions D6, D7, A+3, U+1-U+5 lock this doc's shape.
- [Vision](../../../../docs/vision.md) — architectural shape, "Models and Budgets" section, the llm-crate ABI contract (note: this doc deliberately ships a smaller surface than vision's full `Message`-vector form; vision.md is Stage-7-aspirational for the llm crate).
- [`crates/llm/CLAUDE.md`](../../CLAUDE.md) — scope rules for this crate.
- [Roles and states](../../../../docs/roles-and-states.md) — doesn't apply to `llm` directly (no FSM), but establishes that `llm` is a network-boundary library, not an actor.
- `~/repos/scottidler/loopr/src/agents/llm_client.rs` — v3's full implementation, the reference for the Anthropic request/response shapes.
- [Anthropic Messages API](https://docs.anthropic.com/claude/reference/messages_post) — external; pin `anthropic-version` header in Phase 3.
