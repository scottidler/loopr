//! Integration tests for `AnthropicClient::complete_free` via
//! wiremock. Exercises the text-block extraction path and the
//! request-body shape (no `tools`, no `tool_choice`).

#![allow(clippy::unwrap_used)]

use llm::{AnthropicClient, ChatMessage, FatalReason, LlmClient, LlmConfig, LlmError};
use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn test_config(base_url: String) -> LlmConfig {
    LlmConfig {
        model: "claude-sonnet-4-6".into(),
        max_tokens: 8192,
        temperature: 0.3,
        api_key_env: "ANTHROPIC_API_KEY".into(),
        api_base_url: base_url,
    }
}

fn text_response(text: &str) -> Value {
    json!({
        "id": "msg_01ABC",
        "type": "message",
        "role": "assistant",
        "model": "claude-sonnet-4-6-20260115",
        "content": [{
            "type": "text",
            "text": text,
        }],
        "stop_reason": "end_turn",
        "usage": { "input_tokens": 100, "output_tokens": 42 }
    })
}

#[tokio::test]
async fn free_happy_path_returns_text() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(text_response("hello world")))
        .mount(&server)
        .await;

    let client = AnthropicClient::new(test_config(server.uri()), "test-key".into()).unwrap();
    let messages = vec![ChatMessage::user("say hi")];
    let (result, _usage) = client.complete_free("sys", &messages).await.unwrap();

    assert_eq!(result, "hello world");
}

#[tokio::test]
async fn free_request_body_has_no_tools_or_tool_choice() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(text_response("ok")))
        .mount(&server)
        .await;

    let client = AnthropicClient::new(test_config(server.uri()), "test-key".into()).unwrap();
    let messages = vec![
        ChatMessage::user("first"),
        ChatMessage::assistant("intermediate"),
        ChatMessage::user("last"),
    ];
    client.complete_free("S", &messages).await.unwrap();

    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 1);
    let body: Value = serde_json::from_slice(&reqs[0].body).unwrap();
    // Phase 4: `system` is now an array of content blocks carrying
    // `cache_control`, not a bare string. Assert the new shape.
    let system = body["system"].as_array().expect("system must be an array");
    assert_eq!(system.len(), 1);
    assert_eq!(system[0]["type"], "text");
    assert_eq!(system[0]["text"], "S");
    assert_eq!(system[0]["cache_control"]["type"], "ephemeral");
    assert!(body.get("tools").is_none(), "free request must not carry tools");
    assert!(
        body.get("tool_choice").is_none(),
        "free request must not carry tool_choice"
    );
    let msgs = body["messages"].as_array().unwrap();
    assert_eq!(msgs.len(), 3);
    assert_eq!(msgs[0]["role"], "user");
    assert_eq!(msgs[0]["content"], "first");
    assert_eq!(msgs[1]["role"], "assistant");
    assert_eq!(msgs[1]["content"], "intermediate");
    assert_eq!(msgs[2]["role"], "user");
    assert_eq!(msgs[2]["content"], "last");
}

#[tokio::test]
async fn free_picks_first_text_block_skipping_thinking() {
    // The design-doc invariant: thinking blocks are discarded; the
    // first `{"type": "text"}` block wins.
    let server = MockServer::start().await;
    let body = json!({
        "id": "msg_01",
        "type": "message",
        "role": "assistant",
        "model": "claude-sonnet-4-6",
        "content": [
            { "type": "thinking", "thinking": "reasoning text" },
            { "type": "text", "text": "the answer" },
            { "type": "text", "text": "second text block" },
        ],
        "stop_reason": "end_turn",
        "usage": { "input_tokens": 10, "output_tokens": 20 }
    });
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;

    let client = AnthropicClient::new(test_config(server.uri()), "test-key".into()).unwrap();
    let msgs = vec![ChatMessage::user("q")];
    let (out, _usage) = client.complete_free("s", &msgs).await.unwrap();
    assert_eq!(out, "the answer", "first text block wins, thinking skipped");
}

#[tokio::test]
async fn free_no_text_block_maps_to_schema_validation() {
    let server = MockServer::start().await;
    let body = json!({
        "id": "msg_01",
        "type": "message",
        "role": "assistant",
        "model": "claude-sonnet-4-6",
        "content": [
            { "type": "thinking", "thinking": "only thinking" },
        ],
        "stop_reason": "end_turn",
        "usage": { "input_tokens": 1, "output_tokens": 1 }
    });
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;

    let client = AnthropicClient::new(test_config(server.uri()), "test-key".into()).unwrap();
    let err = client.complete_free("s", &[ChatMessage::user("q")]).await.unwrap_err();
    match err {
        LlmError::Fatal {
            reason: FatalReason::SchemaValidation(msg),
        } => assert!(msg.contains("no text content block"), "got: {msg}"),
        other => panic!("expected SchemaValidation, got {other:?}"),
    }
}

#[tokio::test]
async fn free_max_tokens_maps_to_context_exhausted() {
    let server = MockServer::start().await;
    let body = json!({
        "id": "msg_01",
        "type": "message",
        "role": "assistant",
        "model": "claude-sonnet-4-6",
        "content": [{ "type": "text", "text": "partial..." }],
        "stop_reason": "max_tokens",
        "usage": { "input_tokens": 100, "output_tokens": 8192 }
    });
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;

    let client = AnthropicClient::new(test_config(server.uri()), "test-key".into()).unwrap();
    let err = client.complete_free("s", &[ChatMessage::user("q")]).await.unwrap_err();
    match err {
        LlmError::Fatal {
            reason: FatalReason::ContextExhausted { used, limit },
        } => {
            assert_eq!(used, 8192);
            assert_eq!(limit, 8192);
        }
        other => panic!("expected ContextExhausted, got {other:?}"),
    }
}

#[tokio::test]
async fn free_http_500_maps_to_retryable() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(500).set_body_string("upstream dead"))
        .mount(&server)
        .await;

    let client = AnthropicClient::new(test_config(server.uri()), "test-key".into()).unwrap();
    let err = client.complete_free("s", &[ChatMessage::user("q")]).await.unwrap_err();
    match err {
        LlmError::Retryable { reason } => assert!(reason.contains("500"), "got: {reason}"),
        other => panic!("expected Retryable, got {other:?}"),
    }
}

#[tokio::test]
async fn free_http_401_maps_to_fatal_auth() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(401).set_body_string("invalid key"))
        .mount(&server)
        .await;

    let client = AnthropicClient::new(test_config(server.uri()), "test-key".into()).unwrap();
    let err = client.complete_free("s", &[ChatMessage::user("q")]).await.unwrap_err();
    match err {
        LlmError::Fatal {
            reason: FatalReason::Auth(_),
        } => {}
        other => panic!("expected Fatal::Auth, got {other:?}"),
    }
}
