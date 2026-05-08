//! Integration tests for `AnthropicClient` using `wiremock` as a
//! hermetic local HTTP mock. No network; no real Anthropic key
//! needed. Each test stands up a fresh mock server, points
//! `LlmConfig.api_base_url` at its URI, and exercises one slice of
//! the status-to-LlmError mapping or the request-body shape.
//!
//! Keep these tests seam-shaped: assert one observable at a time
//! (request body, response mapping, header value). Don't bury
//! multiple behaviors behind a single test name.

#![allow(clippy::unwrap_used)]

use llm::{AnthropicClient, FatalReason, LlmClient, LlmConfig, LlmError, Message, ToolSchema};
use serde_json::{Value, json};
use wiremock::matchers::{header, method, path};
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

fn test_tool() -> ToolSchema {
    ToolSchema {
        name: "submit_decomposition".into(),
        description: "Submit a plan decomposition".into(),
        input_schema: json!({
            "type": "object",
            "properties": { "works": { "type": "array" } },
            "required": ["works"]
        }),
    }
}

fn valid_tool_use_body(tool_name: &str, input: Value) -> Value {
    json!({
        "id": "msg_01ABC",
        "type": "message",
        "role": "assistant",
        "model": "claude-sonnet-4-6-20260115",
        "content": [{
            "type": "tool_use",
            "id": "toolu_01XYZ",
            "name": tool_name,
            "input": input,
        }],
        "stop_reason": "tool_use",
        "usage": { "input_tokens": 100, "output_tokens": 42 }
    })
}

#[tokio::test]
async fn happy_path_returns_tool_call() {
    let server = MockServer::start().await;
    let input = json!({ "works": [{ "title": "Build it" }] });
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(valid_tool_use_body("submit_decomposition", input.clone())),
        )
        .mount(&server)
        .await;

    let client = AnthropicClient::new(test_config(server.uri()), "test-key".into()).unwrap();
    let (result, _usage) = client.complete_with_tool("sys", "usr", test_tool()).await.unwrap();

    assert_eq!(result.tool_name, "submit_decomposition");
    assert_eq!(result.input, input);
}

#[tokio::test]
async fn request_body_shape_matches_messages_api() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(valid_tool_use_body("submit_decomposition", json!({"works": []}))),
        )
        .mount(&server)
        .await;

    let client = AnthropicClient::new(test_config(server.uri()), "test-key".into()).unwrap();
    client
        .complete_with_tool("SYS-PROMPT", "USR-PROMPT", test_tool())
        .await
        .unwrap();

    let requests = server.received_requests().await.unwrap();
    assert_eq!(requests.len(), 1);
    let body: Value = serde_json::from_slice(&requests[0].body).unwrap();

    assert_eq!(body["model"], "claude-sonnet-4-6");
    assert_eq!(body["max_tokens"], 8192);
    assert_eq!(body["temperature"], 0.3);
    // Phase 4: `system` is an array of content blocks carrying
    // `cache_control`, not a bare string.
    let system = body["system"].as_array().expect("system must be an array");
    assert_eq!(system.len(), 1);
    assert_eq!(system[0]["type"], "text");
    assert_eq!(system[0]["text"], "SYS-PROMPT");
    assert_eq!(system[0]["cache_control"]["type"], "ephemeral");
    assert_eq!(body["messages"][0]["role"], "user");
    assert_eq!(body["messages"][0]["content"], "USR-PROMPT");
    assert_eq!(body["tools"][0]["name"], "submit_decomposition");
    assert_eq!(body["tool_choice"]["type"], "tool");
    assert_eq!(body["tool_choice"]["name"], "submit_decomposition");
}

#[tokio::test]
async fn required_headers_are_sent() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .and(header("x-api-key", "test-key-xyz"))
        .and(header("anthropic-version", "2023-06-01"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(valid_tool_use_body("submit_decomposition", json!({"works": []}))),
        )
        .mount(&server)
        .await;

    let client = AnthropicClient::new(test_config(server.uri()), "test-key-xyz".into()).unwrap();
    client.complete_with_tool("s", "u", test_tool()).await.unwrap();
    // If headers did not match, wiremock would return 404 and the
    // client would return a Fatal(BadRequest) error; reaching here
    // means all three expectations passed.
}

#[tokio::test]
async fn http_401_maps_to_fatal_auth() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(401).set_body_string("{\"error\": \"auth failed\"}"))
        .mount(&server)
        .await;

    let client = AnthropicClient::new(test_config(server.uri()), "test-key".into()).unwrap();
    let err = client.complete_with_tool("s", "u", test_tool()).await.unwrap_err();
    assert!(
        matches!(
            err,
            LlmError::Fatal {
                reason: FatalReason::Auth(_)
            }
        ),
        "got: {err:?}"
    );
}

#[tokio::test]
async fn http_429_maps_to_retryable() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(429).set_body_string("rate limited"))
        .mount(&server)
        .await;

    let client = AnthropicClient::new(test_config(server.uri()), "test-key".into()).unwrap();
    let err = client.complete_with_tool("s", "u", test_tool()).await.unwrap_err();
    assert!(matches!(err, LlmError::Retryable { .. }), "got: {err:?}");
}

#[tokio::test]
async fn http_500_maps_to_retryable() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(500).set_body_string("internal"))
        .mount(&server)
        .await;

    let client = AnthropicClient::new(test_config(server.uri()), "test-key".into()).unwrap();
    let err = client.complete_with_tool("s", "u", test_tool()).await.unwrap_err();
    assert!(matches!(err, LlmError::Retryable { .. }), "got: {err:?}");
}

#[tokio::test]
async fn stop_reason_max_tokens_maps_to_context_exhausted() {
    let server = MockServer::start().await;
    let body = json!({
        "id": "msg_01",
        "type": "message",
        "role": "assistant",
        "model": "claude-sonnet-4-6-20260115",
        "content": [{ "type": "tool_use", "id": "toolu_01", "name": "submit_decomposition", "input": {} }],
        "stop_reason": "max_tokens",
        "usage": { "input_tokens": 100, "output_tokens": 8192 }
    });
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;

    let client = AnthropicClient::new(test_config(server.uri()), "test-key".into()).unwrap();
    let err = client.complete_with_tool("s", "u", test_tool()).await.unwrap_err();
    match err {
        LlmError::Fatal {
            reason: FatalReason::ContextExhausted { used, limit },
        } => {
            assert_eq!(used, 8192);
            assert_eq!(limit, 8192);
        }
        other => panic!("expected ContextExhausted, got: {other:?}"),
    }
}

#[tokio::test]
async fn missing_tool_use_block_maps_to_schema_validation() {
    let server = MockServer::start().await;
    let body = json!({
        "id": "msg_01",
        "type": "message",
        "role": "assistant",
        "model": "claude-sonnet-4-6-20260115",
        "content": [{ "type": "text", "text": "I refuse to use the tool" }],
        "stop_reason": "end_turn",
        "usage": { "input_tokens": 100, "output_tokens": 10 }
    });
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;

    let client = AnthropicClient::new(test_config(server.uri()), "test-key".into()).unwrap();
    let err = client.complete_with_tool("s", "u", test_tool()).await.unwrap_err();
    assert!(
        matches!(
            err,
            LlmError::Fatal {
                reason: FatalReason::SchemaValidation(_)
            }
        ),
        "got: {err:?}"
    );
}

#[tokio::test]
async fn malformed_tool_input_maps_to_schema_validation() {
    let server = MockServer::start().await;
    let body = json!({
        "id": "msg_01",
        "type": "message",
        "role": "assistant",
        "model": "claude-sonnet-4-6-20260115",
        "content": [{
            "type": "tool_use",
            "id": "toolu_01",
            "name": "submit_decomposition",
            "input": "this-should-be-an-object-not-a-string"
        }],
        "stop_reason": "tool_use",
        "usage": { "input_tokens": 100, "output_tokens": 42 }
    });
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body))
        .mount(&server)
        .await;

    let client = AnthropicClient::new(test_config(server.uri()), "test-key".into()).unwrap();
    let err = client.complete_with_tool("s", "u", test_tool()).await.unwrap_err();
    assert!(
        matches!(
            err,
            LlmError::Fatal {
                reason: FatalReason::SchemaValidation(_)
            }
        ),
        "got: {err:?}"
    );
}

#[tokio::test]
async fn html_body_with_200_status_maps_to_retryable() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("<html><body>Hello from corporate proxy</body></html>")
                .insert_header("content-type", "text/html"),
        )
        .mount(&server)
        .await;

    let client = AnthropicClient::new(test_config(server.uri()), "test-key".into()).unwrap();
    let err = client.complete_with_tool("s", "u", test_tool()).await.unwrap_err();
    assert!(matches!(err, LlmError::Retryable { .. }), "got: {err:?}");
}

#[test]
fn new_rejects_empty_api_key() {
    // `AnthropicClient` intentionally does not derive `Debug` (the reqwest
    // client's auto-redaction is nice-to-have, not a guarantee), so
    // `unwrap_err` is unavailable; match on the Result directly.
    let result = AnthropicClient::new(test_config("http://127.0.0.1:0".into()), String::new());
    match result {
        Ok(_) => panic!("expected empty-key rejection"),
        Err(LlmError::Fatal {
            reason: FatalReason::ConfigInvalid(_),
        }) => {}
        Err(other) => panic!("expected ConfigInvalid, got: {other:?}"),
    }
}

#[test]
fn new_rejects_key_with_control_chars() {
    let result = AnthropicClient::new(test_config("http://127.0.0.1:0".into()), "bad\nkey".into());
    match result {
        Ok(_) => panic!("expected control-char rejection"),
        Err(LlmError::Fatal {
            reason: FatalReason::ConfigInvalid(_),
        }) => {}
        Err(other) => panic!("expected ConfigInvalid, got: {other:?}"),
    }
}

#[test]
fn new_rejects_empty_base_url() {
    let result = AnthropicClient::new(test_config(String::new()), "test-key".into());
    match result {
        Ok(_) => panic!("expected empty-URL rejection"),
        Err(LlmError::Fatal {
            reason: FatalReason::ConfigInvalid(msg),
        }) => assert!(msg.contains("empty"), "msg: {msg}"),
        Err(other) => panic!("expected ConfigInvalid, got: {other:?}"),
    }
}

#[test]
fn new_rejects_base_url_with_trailing_slash() {
    let result = AnthropicClient::new(test_config("https://api.anthropic.com/".into()), "test-key".into());
    match result {
        Ok(_) => panic!("expected trailing-slash rejection"),
        Err(LlmError::Fatal {
            reason: FatalReason::ConfigInvalid(msg),
        }) => assert!(msg.contains("trailing slash"), "msg: {msg}"),
        Err(other) => panic!("expected ConfigInvalid, got: {other:?}"),
    }
}

#[test]
fn new_rejects_base_url_with_control_chars() {
    let result = AnthropicClient::new(test_config("https://api.anthropic.com\n".into()), "test-key".into());
    match result {
        Ok(_) => panic!("expected control-char rejection"),
        Err(LlmError::Fatal {
            reason: FatalReason::ConfigInvalid(msg),
        }) => assert!(msg.contains("control"), "msg: {msg}"),
        Err(other) => panic!("expected ConfigInvalid, got: {other:?}"),
    }
}

#[test]
fn new_rejects_non_http_scheme() {
    let result = AnthropicClient::new(test_config("ftp://api.anthropic.com".into()), "test-key".into());
    match result {
        Ok(_) => panic!("expected non-http-scheme rejection"),
        Err(LlmError::Fatal {
            reason: FatalReason::ConfigInvalid(msg),
        }) => assert!(msg.contains("scheme"), "msg: {msg}"),
        Err(other) => panic!("expected ConfigInvalid, got: {other:?}"),
    }
}

#[test]
fn new_rejects_unparseable_base_url() {
    let result = AnthropicClient::new(test_config("not-a-url".into()), "test-key".into());
    match result {
        Ok(_) => panic!("expected unparseable-URL rejection"),
        Err(LlmError::Fatal {
            reason: FatalReason::ConfigInvalid(_),
        }) => {}
        Err(other) => panic!("expected ConfigInvalid, got: {other:?}"),
    }
}

fn text_response(text: &str) -> Value {
    json!({
        "id": "msg_01ABC",
        "type": "message",
        "role": "assistant",
        "model": "claude-sonnet-4-6-20260115",
        "content": [{ "type": "text", "text": text }],
        "stop_reason": "end_turn",
        "usage": { "input_tokens": 100, "output_tokens": 10 }
    })
}

/// Phase 4 seam test: `complete_free` with a multi-turn history sends
/// the correct wire body — both turns present, correct roles, bare
/// string content (single-Text optimisation), no `tools` or
/// `tool_choice` fields.
#[tokio::test]
async fn complete_free_multi_turn_wire_body() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/messages"))
        .respond_with(ResponseTemplate::new(200).set_body_json(text_response("ok")))
        .mount(&server)
        .await;

    let client = AnthropicClient::new(test_config(server.uri()), "test-key".into()).unwrap();
    let messages = vec![
        Message::user("first user turn"),
        Message::assistant("assistant reply"),
        Message::user("second user turn"),
    ];
    let (text, _usage) = client.complete_free("system text", &messages).await.unwrap();
    assert_eq!(text, "ok");

    let reqs = server.received_requests().await.unwrap();
    assert_eq!(reqs.len(), 1);
    let body: Value = serde_json::from_slice(&reqs[0].body).unwrap();

    // No tools or tool_choice on free requests
    assert!(body.get("tools").is_none(), "free must not carry tools");
    assert!(body.get("tool_choice").is_none(), "free must not carry tool_choice");

    // System block is the cache-annotated array form
    let system = body["system"].as_array().expect("system must be an array");
    assert_eq!(system[0]["type"], "text");
    assert_eq!(system[0]["text"], "system text");
    assert_eq!(system[0]["cache_control"]["type"], "ephemeral");

    // All three turns present with correct roles and bare-string content
    let msgs = body["messages"].as_array().expect("messages must be array");
    assert_eq!(msgs.len(), 3, "expected 3 turns in wire body");
    assert_eq!(msgs[0]["role"], "user");
    assert_eq!(msgs[0]["content"], "first user turn");
    assert_eq!(msgs[1]["role"], "assistant");
    assert_eq!(msgs[1]["content"], "assistant reply");
    assert_eq!(msgs[2]["role"], "user");
    assert_eq!(msgs[2]["content"], "second user turn");
}
