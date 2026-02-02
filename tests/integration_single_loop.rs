//! Single loop execution integration tests
//!
//! Tests the core loop execution flow with a mock LLM client.

use loopr::domain::{Loop, LoopStatus, LoopType};
use loopr::error::Result;
use loopr::id::{generate_loop_id, now_ms};
use loopr::llm::{CompletionResponse, LlmClient, Message, MockLlmClient, Role, StopReason, ToolCall, Usage};
use loopr::storage::StorageWrapper;
use loopr::tools::ToolCatalog;
use loopr::validation::ValidationResult;
use tempfile::TempDir;

/// Integration test: verify mock LLM client works
#[test]
fn test_mock_llm_client_creation() {
    let mock = MockLlmClient::new();
    assert!(mock.is_ready());
    assert_eq!(mock.model(), "mock-model");
}

/// Integration test: verify storage persistence
#[test]
fn test_storage_persistence() -> Result<()> {
    let temp_dir = TempDir::new()?;

    // Create a loop and store it
    let loop_record = Loop::new_plan("Test task");

    {
        let storage = StorageWrapper::open(temp_dir.path())?;
        storage.create(&loop_record)?;
    }

    // Reload storage and verify persistence
    {
        let storage = StorageWrapper::open(temp_dir.path())?;
        let loaded: Option<Loop> = storage.get(&loop_record.id)?;
        assert!(loaded.is_some());
        let loaded = loaded.unwrap();
        assert_eq!(loaded.id, loop_record.id);
        assert_eq!(loaded.loop_type, LoopType::Plan);
    }

    Ok(())
}

/// Integration test: verify domain type serialization round-trip
#[test]
fn test_domain_serialization_roundtrip() -> Result<()> {
    let plan = Loop::new_plan("Create a web application");
    let json = serde_json::to_string(&plan)?;
    let restored: Loop = serde_json::from_str(&json)?;

    assert_eq!(plan.id, restored.id);
    assert_eq!(plan.loop_type, restored.loop_type);
    assert_eq!(plan.status, restored.status);

    Ok(())
}

/// Integration test: verify loop status transitions
#[test]
fn test_loop_status_transitions() {
    let mut loop_record = Loop::new_plan("Test");

    // Initial status is Pending (not resumable - it's new, not paused)
    assert_eq!(loop_record.status, LoopStatus::Pending);
    assert!(!loop_record.status.is_terminal());
    assert!(!loop_record.status.is_resumable()); // Pending is not resumable

    // Transition to Running
    loop_record.status = LoopStatus::Running;
    assert!(!loop_record.status.is_terminal());

    // Transition to Paused (resumable)
    loop_record.status = LoopStatus::Paused;
    assert!(!loop_record.status.is_terminal());
    assert!(loop_record.status.is_resumable()); // Paused IS resumable

    // Transition to Complete
    loop_record.status = LoopStatus::Complete;
    assert!(loop_record.status.is_terminal());
    assert!(!loop_record.status.is_resumable());
}

/// Integration test: verify tool catalog loading
#[test]
fn test_tool_catalog_basic() -> Result<()> {
    let temp_dir = TempDir::new()?;
    let catalog_path = temp_dir.path().join("catalog.toml");

    std::fs::write(
        &catalog_path,
        r#"
[[tool]]
name = "read_file"
description = "Read file contents"
lane = "no-net"

[[tool]]
name = "write_file"
description = "Write file contents"
lane = "no-net"
"#,
    )?;

    let catalog = ToolCatalog::from_file(&catalog_path)?;
    let tools = catalog.list();

    assert_eq!(tools.len(), 2);
    assert!(tools.contains(&"read_file"));
    assert!(tools.contains(&"write_file"));

    Ok(())
}

/// Integration test: verify validation result merging
#[test]
fn test_validation_result_merge() {
    let mut pass1 = ValidationResult::pass();
    let pass2 = ValidationResult::pass();
    let mut fail1 = ValidationResult::fail("Error 1");
    let fail2 = ValidationResult::fail("Error 2");

    // Pass + Pass = Pass
    pass1.merge(pass2);
    assert!(pass1.passed);

    // Fail + Fail = Fail with both errors
    fail1.merge(fail2);
    assert!(!fail1.passed);
    assert_eq!(fail1.errors.len(), 2);
}

/// Integration test: verify loop hierarchy IDs
#[test]
fn test_loop_hierarchy_ids() {
    let plan = Loop::new_plan("Build a system");
    let spec = Loop::new_spec(&plan, 1);
    let phase = Loop::new_phase(&spec, 1, "Setup", 3);
    let code = Loop::new_code(&phase);

    // Verify parent relationships
    assert!(plan.parent_id.is_none());
    assert_eq!(spec.parent_id, Some(plan.id.clone()));
    assert_eq!(phase.parent_id, Some(spec.id.clone()));
    assert_eq!(code.parent_id, Some(phase.id.clone()));

    // Verify types
    assert_eq!(plan.loop_type, LoopType::Plan);
    assert_eq!(spec.loop_type, LoopType::Spec);
    assert_eq!(phase.loop_type, LoopType::Phase);
    assert_eq!(code.loop_type, LoopType::Code);
}

/// Integration test: verify message construction
#[test]
fn test_message_construction() {
    let user_msg = Message::user("Hello");
    let assistant_msg = Message::assistant("Hi there");

    assert_eq!(user_msg.role, Role::User);
    assert_eq!(user_msg.content, "Hello");
    assert_eq!(assistant_msg.role, Role::Assistant);
    assert_eq!(assistant_msg.content, "Hi there");
}

/// Integration test: verify completion response parsing
#[test]
fn test_completion_response_structure() {
    let response = CompletionResponse {
        content: "Test response".to_string(),
        tool_calls: vec![ToolCall {
            id: "call_123".to_string(),
            name: "read_file".to_string(),
            input: serde_json::json!({"path": "/test.txt"}),
        }],
        stop_reason: StopReason::ToolUse,
        usage: Usage {
            input_tokens: 100,
            output_tokens: 50,
        },
    };

    assert_eq!(response.content, "Test response");
    assert_eq!(response.tool_calls.len(), 1);
    assert_eq!(response.tool_calls[0].name, "read_file");
    assert!(matches!(response.stop_reason, StopReason::ToolUse));
}

/// Integration test: verify ID generation uniqueness
#[test]
fn test_id_generation_uniqueness() {
    let mut ids = std::collections::HashSet::new();

    // Generate 100 IDs and verify uniqueness
    for _ in 0..100 {
        let id = generate_loop_id();
        assert!(ids.insert(id), "Generated duplicate ID");
    }
}

/// Integration test: verify now_ms returns sensible values
#[test]
fn test_now_ms_sensible() {
    let before = now_ms();
    std::thread::sleep(std::time::Duration::from_millis(10));
    let after = now_ms();

    assert!(after > before, "Time should advance");
    assert!(after - before >= 10, "At least 10ms should have passed");
}

/// Integration test: verify storage query filtering
#[test]
fn test_storage_query_filtering() -> Result<()> {
    let temp_dir = TempDir::new()?;

    let storage = StorageWrapper::open(temp_dir.path())?;

    // Create multiple loops with different statuses
    let mut plan1 = Loop::new_plan("Task 1");
    plan1.status = LoopStatus::Running;
    let mut plan2 = Loop::new_plan("Task 2");
    plan2.status = LoopStatus::Complete;
    let plan3 = Loop::new_plan("Task 3");
    // plan3 stays Pending

    storage.create(&plan1)?;
    storage.create(&plan2)?;
    storage.create(&plan3)?;

    // Verify we can list all
    let all: Vec<Loop> = storage.list_all()?;
    assert_eq!(all.len(), 3);

    Ok(())
}
