# llm

The network boundary for LLM API calls. Agnostic of prompt content, tool schemas, and context assembly — this crate takes fully-formed `Message` structs and talks to Anthropic (or other providers). Prompt assembly lives in `agents`; this crate is the API-bounds layer only.

## In scope

- `LlmClient` trait: swappable LLM backends, typed request/response
- `AnthropicClient` default impl: Messages API with SSE streaming, retry on transient errors
- Cost accounting: every call emits `tracing` spans with `input_tokens`, `output_tokens`, `cost_usd`, and the concrete model ID returned by the API
- Model tier resolution: given a tier name (`primary`, `lightweight`, `advisor`) or a literal model ID, return the concrete model to call
- Config: `LlmConfig` composed into the top-level `Config` by `loopr`

## Out of scope

- **Prompt assembly / `ContextBuilder`.** Lives in `agents`. `llm` receives ready-to-send `Message` vectors, not raw templates or tool schemas. Rationale: `llm` cannot depend on `tools` (would re-couple network and subprocess concerns), so it cannot render tool schemas into prompts. `agents` is the first crate that sees `llm` + `tools` + `domain` + `store` simultaneously; that is where assembly belongs.
- Tool execution itself — that's `tools`
- Retry / escalation / advisor strategy selection — that's `agents`
- Record persistence — that's `store`

## Rule

This crate owns *how we talk to an LLM*, not *what we say*. If you find yourself writing prompt templates, rendering record data, or assembling tool schemas here, move it to `agents` and call it from there.

The Architect's Round 2 finding motivates this boundary: `ContextBuilder` placed in `llm` cannot see the `tools` crate and cannot render tool schemas — which is the whole point of context assembly. Moving assembly up to `agents` resolves the visibility problem.

## Dependencies

`domain` (for record types that travel in prompts as structured context), `telemetry` (for span emission), workspace-shared (`tokio`, `reqwest` or `ureq`, `serde`, `serde_json`, `eyre`). Added via `cargo add` at the time code needs them.

## Instrumentation

`AnthropicClient::complete_with_tool` and `complete_free` keep their existing manual `info_span!("llm.anthropic", ...)` shape: model, system_len, user_len, system_preview / user_preview (truncated to PROMPT_PREVIEW_MAX_BYTES), duration_ms, outcome (`ok` / `retryable` / `fatal`). Phase 4 of the Tier-1 cleanup added prompt-cache fields: `cache_creation_input_tokens`, `cache_read_input_tokens`, and `cache_hit_ratio` are recorded on every `Ok` outcome. A debug event under target `llm.anthropic.cache` mirrors the same triple so log readers can grep for cache activity without parsing span fields. Never emit tool schemas, ToolCall.input, headers, or the API key.

`AnthropicClient::new` now opens `llm.anthropic.new` (`info`, `err`) carrying model + api_base_url + max_tokens. Helper functions `validate_api_base_url`, `classify_response`, `classify_free_response` open their own `debug` spans with `err` so an HTTP-level failure carries status + body length on the span without inlining the response body. The existing `tests/span.rs` snapshot covers the manual outer spans; the new `#[instrument]` attributes are exercised via the test suite's mock-server flows.

## See also

- [../../CLAUDE.md](../../CLAUDE.md): project-wide rules and crate map
- [../../docs/vision.md](../../docs/vision.md): architectural shape, "Models and Budgets" section
- [.otto.yml](.otto.yml): scoped CI for this crate (`otto ci` inside this dir)
