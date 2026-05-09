# Design Document: Make `temperature` optional in LLM config and wire

**Author:** Scott Idler
**Date:** 2026-05-09
**Status:** Implemented
**Crates touched:** llm, loopr

## Summary

Make the LLM `temperature` setting an `Option<f32>` end-to-end (config + wire). The wire layer is dumb: it sends what the config holds and omits the key when the config holds `None`. No model-capability heuristic; no string-prefix matching. Triggered by an e2e python-api run on 2026-05-09 where the Director's first call to `claude-opus-4-7` failed with `400 invalid_request_error: temperature is deprecated for this model`.

## Problem Statement

### Background

`crates/llm/src/config.rs` declares `temperature: f32` as a required `LlmConfig` field with default 0.3. `crates/llm/src/anthropic.rs` declares the same field on both wire structs (`AnthropicRequest`, `AnthropicFreeRequest`) and unconditionally populates it from `self.config.temperature` at both call sites (`send_request`, `send_free_request`).

Anthropic deprecated `temperature` for Opus 4.7 (and presumably the newer reasoning-model family that follows). Sending the field on those models is a hard `400 BadRequest` with `"temperature is deprecated for this model"`. There is no retry path: the same payload re-fires identically, fails identically, and after the configured restart budget the Director exits with `director exited with error`.

### Problem

The crate cannot talk to Opus 4.7 at all. Every Director call dies on a wire-level field that the model rejects.

The first draft of this doc proposed a `model_supports_temperature(model: &str) -> bool` predicate gated on a `claude-opus-4-7` prefix match. Architect review rejected that shape for two well-grounded reasons:

1. **`api_base_url` already supports gateways and Bedrock shims** that mangle model identifiers (`us.anthropic.claude-opus-4-7`, `bedrock-opus-4-7`). A literal-prefix predicate fails to match those aliases and silently falls back to sending `temperature`, which re-triggers the exact 400 the design tries to prevent.
2. **A predicate hidden in the wire layer silently overrides user intent.** A user who explicitly sets `temperature: 0.8` for an Opus model would have their value dropped without acknowledgment.

The Architect's hardest question - "should we instead require users to set `temperature: null` explicitly in their config for incompatible models rather than guessing via fragile string heuristics?" - converges with making the field optional end-to-end and removing the predicate entirely.

### Goals

- Allow `temperature` to be omitted from the YAML config without a deserialization error.
- Allow the wire layer to omit `temperature` from the JSON request body.
- Make "user did not configure temperature" and "user explicitly chose `0.3`" distinguishable on the deserialization path.
- Provide observability for which calls included the field and which omitted it.
- Keep existing call shapes (`complete_with_tool`, `complete_free`) unchanged for callers.

### Non-Goals

- Per-model capability matrix or model-prefix matching. Removed entirely.
- Magic injection of a default temperature at the wire layer. The wire layer is dumb.
- Changing the `LlmClient` trait surface.
- Removing `temperature` everywhere. Sonnet 4.6 still uses it; configs that want 0.3 set it explicitly.

## Proposed Solution

### Overview

Three small edits, all in the `llm` crate, plus test updates in two crates.

1. `LlmConfig.temperature` becomes `Option<f32>` with `#[serde(default)]`. Field-level `serde(default)` calls `<Option<f32>>::default()` which returns `None`, so a YAML that omits `temperature:` deserializes to `None`. `LlmConfig::default()` (used when no config file is present at all) also returns `None` - no implicit default.
2. Wire structs' `temperature` field becomes `Option<f32>` with `#[serde(skip_serializing_if = "Option::is_none")]`. Both send sites pass `self.config.temperature` through directly.
3. Both `llm.anthropic` and `llm.anthropic.free` spans gain a `temperature_present: bool` field so operators can grep events.log to confirm whether a given call sent the parameter.

### Behavior

| Config value | Wire body | Sonnet 4.6 result | Opus 4.7 result |
|---|---|---|---|
| `temperature: 0.3` | `"temperature": 0.3` | sampled at 0.3 | 400 BadRequest |
| `temperature: 0.0` | `"temperature": 0.0` | greedy | 400 BadRequest |
| (omitted) or `temperature: null` | (no key) | Anthropic default | accepted |

The user's choice is now load-bearing. Configs that want tight generation on Sonnet keep `temperature: 0.3`. Configs that target Opus 4.7 omit the field. A future model that re-introduces a different sampling parameter is a separate field, not a re-purposed `temperature`.

### Code shape

`crates/llm/src/config.rs`:
```rust
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(rename_all = "kebab-case", deny_unknown_fields)]
pub struct LlmConfig {
    pub model: String,
    pub max_tokens: u32,

    /// Sampling temperature in [0, 1]. `None` means "do not send the
    /// parameter," which is required for models that have deprecated
    /// it (e.g., Opus 4.7) and acceptable for any model where the
    /// caller is fine with the API default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f32>,

    pub api_key_env: String,
    pub api_base_url: String,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            model: "claude-sonnet-4-6".into(),
            max_tokens: 8192,
            temperature: None,
            api_key_env: "ANTHROPIC_API_KEY".into(),
            api_base_url: "https://api.anthropic.com".into(),
        }
    }
}
```

`crates/llm/src/anthropic.rs` wire structs:
```rust
#[derive(Serialize)]
struct AnthropicRequest<'a> {
    model: &'a str,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    system: Value,
    messages: [AnthropicMessage<'a>; 1],
    tools: [ToolSchemaWire<'a>; 1],
    tool_choice: ToolChoiceWire<'a>,
}
```
(Same shape applied to `AnthropicFreeRequest`.)

Send sites:
```rust
let body = AnthropicRequest {
    model,
    max_tokens: self.config.max_tokens,
    temperature: self.config.temperature,
    system: build_system_block(system),
    ...
};
```

No predicate, no helper. The wire layer is a pass-through.

### Telemetry

Both `llm.anthropic` and `llm.anthropic.free` spans gain:

```rust
temperature_present = self.config.temperature.is_some(),
```

Recorded once at span open, alongside the existing `model` field. An operator investigating output quality drift can grep `events.log`:

```bash
grep '"temperature_present":false' events.log | head
```

and immediately see which calls omitted the field.

### Migration impact

- A user who previously had no `.loopr/config.yml` (relied on `LlmConfig::default()`) now sends no `temperature`. Anthropic's Sonnet 4.6 API default is 1.0. Output may shift in tone toward more variance. Users who want the old behavior add `temperature: 0.3` explicitly.
- A user with an explicit `temperature: 0.3` in YAML is unaffected.
- A user with an explicit `temperature: null` in YAML now works (previously failed deserialization).

The fresh-repo behavior change is acknowledged. The alternative (preserving 0.3 as a hidden in-code default) re-introduces the "missing key vs explicit 0.3 are indistinguishable" problem the Architect flagged, and breaks the "wire is dumb" property. The clean semantic wins.

### Test impact

Existing tests update from `temperature: 0.3` to `temperature: Some(0.3)`:

- `crates/llm/tests/anthropic.rs` line 22 and the body assertion at line 102 (which still checks that 0.3 is serialized as 0.3 - that holds).
- `crates/llm/tests/span.rs` line 114, `tests/free.rs` line 16, `tests/cache_smoke.rs` line 28.
- `crates/llm/src/config/tests.rs` line 10 - assertion compares against `Some(0.3)`.
- `crates/loopr/src/config/tests.rs` line 43 - assertion compares against `Some(0.5)`.
- `crates/llm/src/config/tests.rs::default_values_match_scope_memo` - assertion changes from `(cfg.temperature - 0.3).abs() < EPSILON` to `cfg.temperature == None`.

Three new tests in `crates/llm/tests/anthropic.rs`:

1. `temperature_omitted_when_config_is_none` - `LlmConfig.temperature = None` produces a body where `temperature` key is absent (`body.get("temperature").is_none()`).
2. `temperature_present_when_config_is_some` - `LlmConfig.temperature = Some(0.7)` produces `body["temperature"] == 0.7`.
3. `yaml_missing_temperature_deserializes_to_none` - parsing YAML without a `temperature:` key yields `LlmConfig.temperature == None` (test in `crates/llm/src/config/tests.rs`).

## Implementation Plan

Single phase, single commit.

1. Edit `crates/llm/src/config.rs`:
   a. Change `pub temperature: f32` to `pub temperature: Option<f32>` with `#[serde(default, skip_serializing_if = "Option::is_none")]`.
   b. Update `Default::default()` to set `temperature: None`.
2. Edit `crates/llm/src/anthropic.rs`:
   a. Change `AnthropicRequest.temperature` and `AnthropicFreeRequest.temperature` to `Option<f32>` with `#[serde(skip_serializing_if = "Option::is_none")]`.
   b. Wire `self.config.temperature` directly into both send sites.
   c. Add `temperature_present` span field on both `llm.anthropic` and `llm.anthropic.free` info-spans.
3. Update `crates/llm/src/config/tests.rs`:
   a. `default_values_match_scope_memo`: assert `cfg.temperature == None`.
   b. Add `yaml_missing_temperature_deserializes_to_none`.
4. Update `crates/llm/tests/anthropic.rs`, `tests/span.rs`, `tests/free.rs`, `tests/cache_smoke.rs`: wrap `temperature: 0.3` / `0.0` in `Some(...)`.
5. Add `temperature_omitted_when_config_is_none` and `temperature_present_when_config_is_some` to `tests/anthropic.rs`.
6. Update `crates/loopr/src/config/tests.rs::config_load_parses_llm_section`: assertion compares to `Some(0.5)`.
7. `otto ci` at the repo root.
8. `cargo install --path crates/loopr` and re-run `bin/e2e python-api` to confirm Director no longer dies on the wire (out of band; not part of this PR).

## Acceptance Criteria

| Given... | When... | Then... |
|---|---|---|
| `LlmConfig.temperature = Some(0.3)` | a request body is built | the JSON body contains `"temperature": 0.3` |
| `LlmConfig.temperature = None` | a request body is built | the JSON body does not contain a `temperature` key |
| YAML config omits `temperature:` | the daemon loads | deserialization succeeds; `temperature == None` |
| YAML config sets `temperature: 0.5` | the daemon loads | `LlmConfig.temperature == Some(0.5)` |
| `LlmConfig::default()` is called in code | inspecting the value | `temperature == None` |
| Director runs against Opus 4.7 with no temperature in config | first iteration | does not get a 400 for `temperature deprecated`; iteration completes |
| Any LLM call | inspecting `events.log` | the span carries `temperature_present: bool` matching whether the field was sent |
