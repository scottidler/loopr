//! Live integration smoke for Anthropic prompt caching.
//!
//! Phase 4 of the Tier-1 cleanup wires `cache_control: ephemeral` on
//! the system block. This test issues two back-to-back `complete_free`
//! calls with the same byte-stable system prompt against the real
//! Messages API and asserts the second response reports
//! `cache_read_input_tokens > 0`.
//!
//! Gated `#[ignore]` so the unit phases survive without an
//! `ANTHROPIC_API_KEY`. Run manually with:
//!
//! ```text
//! ANTHROPIC_API_KEY=... cargo test -p llm --test cache_smoke -- --ignored
//! ```
//!
//! The system prompt is intentionally bulked past Sonnet's 2048-token
//! cache minimum so the cache is actually populated; below threshold,
//! Anthropic accepts the request but silently no-ops the cache.

#![allow(clippy::unwrap_used)]

use llm::{AnthropicClient, ChatMessage, LlmClient, LlmConfig};

fn live_config() -> LlmConfig {
    LlmConfig {
        model: "claude-sonnet-4-6".into(),
        max_tokens: 1024,
        temperature: 0.0,
        api_key_env: "ANTHROPIC_API_KEY".into(),
        api_base_url: "https://api.anthropic.com".into(),
    }
}

/// Bulk a baseline string up to `target_tokens` worth of text so the
/// system prompt crosses the per-model cache minimum (2048 tokens on
/// Sonnet 4.x). One token ~= 4 chars in English; we round up.
fn bulk_system_prompt(target_tokens: usize) -> String {
    let baseline = "You are a deterministic assistant that responds with the literal string 'OK' to any question. \
                    This system prompt is intentionally repeated to exceed the per-model cache minimum so prompt \
                    caching engages. Do not deviate from the response contract under any circumstances. ";
    let needed_chars = target_tokens.saturating_mul(4);
    let mut buf = String::with_capacity(needed_chars + baseline.len());
    while buf.len() < needed_chars {
        buf.push_str(baseline);
    }
    buf
}

#[tokio::test]
#[ignore = "requires ANTHROPIC_API_KEY; opt-in only"]
async fn cache_read_tokens_increment_on_second_call() {
    let api_key = std::env::var("ANTHROPIC_API_KEY").expect("ANTHROPIC_API_KEY must be set for this test");
    let client = AnthropicClient::new(live_config(), api_key).expect("client");

    let system = bulk_system_prompt(2500);
    let messages = vec![ChatMessage::user("respond with OK".to_string())];

    // Call 1: cache miss; the API populates cache_creation_input_tokens.
    let (_first, first_usage) = client.complete_free(&system, &messages).await.expect("first call");
    println!(
        "first: input={} cache_create={} cache_read={}",
        first_usage.input_tokens, first_usage.cache_creation_input_tokens, first_usage.cache_read_input_tokens
    );
    assert!(
        first_usage.cache_creation_input_tokens > 0,
        "first call should populate the cache; got usage={first_usage:?}"
    );

    // Call 2: cache hit; cache_read_input_tokens should be > 0.
    let (_second, second_usage) = client.complete_free(&system, &messages).await.expect("second call");
    println!(
        "second: input={} cache_create={} cache_read={}",
        second_usage.input_tokens, second_usage.cache_creation_input_tokens, second_usage.cache_read_input_tokens
    );
    assert!(
        second_usage.cache_read_input_tokens > 0,
        "second call should hit the cache; got usage={second_usage:?}"
    );
}
