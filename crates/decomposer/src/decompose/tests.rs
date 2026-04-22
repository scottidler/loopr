use std::path::Path;
use std::sync::Mutex;

use serde_json::json;
use tempfile::TempDir;

use domain::Plan;
use llm::{ChatMessage, FatalReason, LlmClient, LlmError, ToolCall, ToolSchema};

use crate::error::DecomposerError;

/// In-process `LlmClient` stand-in for unit tests. Holds a queue of
/// canned `Result<ToolCall, LlmError>` responses served in call order.
/// Each call to `complete_with_tool` pops the front of the queue;
/// calling with an empty queue panics loudly so the test fails
/// rather than hanging.
struct MockLlmClient {
    responses: Mutex<Vec<Result<ToolCall, LlmError>>>,
}

impl MockLlmClient {
    fn new(responses: Vec<Result<ToolCall, LlmError>>) -> Self {
        Self {
            responses: Mutex::new(responses),
        }
    }
}

impl LlmClient for MockLlmClient {
    #[allow(clippy::manual_async_fn)] // explicit `+ Send` bound required by the trait
    fn complete_with_tool<'a>(
        &'a self,
        _system: &'a str,
        _user: &'a str,
        _tool: ToolSchema,
    ) -> impl std::future::Future<Output = Result<ToolCall, LlmError>> + Send + 'a {
        async move {
            let mut q = self.responses.lock().expect("mock response queue lock");
            if q.is_empty() {
                panic!("MockLlmClient: response queue exhausted");
            }
            q.remove(0)
        }
    }

    #[allow(clippy::manual_async_fn)]
    fn complete_free<'a>(
        &'a self,
        _system: &'a str,
        _messages: &'a [ChatMessage],
    ) -> impl std::future::Future<Output = Result<String, LlmError>> + Send + 'a {
        async move {
            panic!("MockLlmClient in decomposer tests does not implement complete_free");
        }
    }
}

fn tool_call(input: serde_json::Value) -> ToolCall {
    ToolCall {
        tool_name: "submit_decomposition".to_string(),
        input,
    }
}

fn ok(call: ToolCall) -> Result<ToolCall, LlmError> {
    Ok(call)
}

fn schema_validation_err(msg: &str) -> Result<ToolCall, LlmError> {
    Err(LlmError::Fatal {
        reason: FatalReason::SchemaValidation(msg.to_string()),
    })
}

fn retryable_err(msg: &str) -> Result<ToolCall, LlmError> {
    Err(LlmError::Retryable {
        reason: msg.to_string(),
    })
}

fn fresh_plan(goal: &str) -> Plan {
    Plan::new(goal.to_string())
}

async fn run_decompose(
    responses: Vec<Result<ToolCall, LlmError>>,
    target: &Path,
) -> Result<Vec<domain::Work>, DecomposerError> {
    let plan = fresh_plan("decompose me");
    let llm = MockLlmClient::new(responses);
    super::decompose(&plan, target, &llm).await
}

#[tokio::test]
async fn happy_path_single_child_no_deps() {
    let dir = TempDir::new().expect("tempdir");
    let response = tool_call(json!({
        "children": [
            {
                "title": "Add --version flag",
                "content": "Implement --version via clap.",
                "dependencies": [],
                "acceptance_criteria": ["assert --version prints the version"]
            }
        ]
    }));
    let works = run_decompose(vec![ok(response)], dir.path()).await.expect("ok");
    assert_eq!(works.len(), 1);
    assert_eq!(works[0].title, "Add --version flag");
    assert_eq!(works[0].acceptance_criteria.len(), 1);
    assert!(works[0].dependencies.is_empty());
}

#[tokio::test]
async fn happy_path_two_children_with_one_dep() {
    let dir = TempDir::new().expect("tempdir");
    let response = tool_call(json!({
        "children": [
            {
                "title": "Build CLI",
                "content": "scaffold CLI",
                "dependencies": [],
                "acceptance_criteria": ["assert scaffold exists"]
            },
            {
                "title": "Add Tests",
                "content": "CLI tests",
                "dependencies": ["Build CLI"],
                "acceptance_criteria": ["assert tests pass"]
            }
        ]
    }));
    let works = run_decompose(vec![ok(response)], dir.path()).await.expect("ok");
    assert_eq!(works.len(), 2);
    let titles: Vec<&str> = works.iter().map(|w| w.title.as_str()).collect();
    assert!(titles.contains(&"Build CLI"));
    assert!(titles.contains(&"Add Tests"));

    // Find the Test Work and assert its dep points at the Build Work.
    let tests_work = works.iter().find(|w| w.title == "Add Tests").expect("tests work");
    let cli_work = works.iter().find(|w| w.title == "Build CLI").expect("cli work");
    assert_eq!(tests_work.dependencies.len(), 1);
    assert_eq!(tests_work.dependencies[0], cli_work.id);
    assert!(cli_work.dependencies.is_empty());
}

#[tokio::test]
async fn retry_fires_on_first_llm_error_and_second_succeeds() {
    let dir = TempDir::new().expect("tempdir");
    let response = tool_call(json!({
        "children": [
            {
                "title": "A",
                "content": "aaa",
                "acceptance_criteria": ["assert a works"]
            }
        ]
    }));
    let works = run_decompose(vec![schema_validation_err("no tool_use"), ok(response)], dir.path())
        .await
        .expect("ok after retry");
    assert_eq!(works.len(), 1);
    assert_eq!(works[0].title, "A");
}

#[tokio::test]
async fn retry_failure_propagates_final_error() {
    let dir = TempDir::new().expect("tempdir");
    let err = run_decompose(vec![retryable_err("first"), retryable_err("second")], dir.path())
        .await
        .expect_err("should fail after retry");
    match err {
        DecomposerError::LlmFailed(LlmError::Retryable { reason }) => {
            assert_eq!(reason, "second", "retry's error propagates, not first");
        }
        other => panic!("expected LlmFailed(Retryable), got {other:?}"),
    }
}

#[tokio::test]
async fn zero_children_errors_zero_children() {
    let dir = TempDir::new().expect("tempdir");
    let response = tool_call(json!({"children": []}));
    let err = run_decompose(vec![ok(response)], dir.path())
        .await
        .expect_err("zero children");
    assert!(matches!(err, DecomposerError::ZeroChildren(_)));
}

#[tokio::test]
async fn malformed_tool_input_errors_malformed_children() {
    let dir = TempDir::new().expect("tempdir");
    // Valid JSON object but missing `children` field.
    let response = tool_call(json!({"not_children": []}));
    let err = run_decompose(vec![ok(response)], dir.path())
        .await
        .expect_err("malformed");
    assert!(matches!(err, DecomposerError::MalformedChildren(_)), "got: {err:?}");
}

#[tokio::test]
async fn cycle_detected_between_two_children() {
    let dir = TempDir::new().expect("tempdir");
    let response = tool_call(json!({
        "children": [
            {
                "title": "A",
                "content": "a",
                "dependencies": ["B"],
                "acceptance_criteria": ["assert a"]
            },
            {
                "title": "B",
                "content": "b",
                "dependencies": ["A"],
                "acceptance_criteria": ["assert b"]
            }
        ]
    }));
    let err = run_decompose(vec![ok(response)], dir.path()).await.expect_err("cycle");
    assert!(matches!(err, DecomposerError::CycleDetected(_)), "got: {err:?}");
}

#[tokio::test]
async fn self_loop_is_cycle() {
    let dir = TempDir::new().expect("tempdir");
    let response = tool_call(json!({
        "children": [
            {
                "title": "A",
                "content": "a",
                "dependencies": ["A"],
                "acceptance_criteria": ["assert a"]
            }
        ]
    }));
    let err = run_decompose(vec![ok(response)], dir.path())
        .await
        .expect_err("self-loop");
    assert!(matches!(err, DecomposerError::CycleDetected(_)), "got: {err:?}");
}

#[tokio::test]
async fn duplicate_titles_after_normalization_errors() {
    let dir = TempDir::new().expect("tempdir");
    let response = tool_call(json!({
        "children": [
            {
                "title": "Build CLI",
                "content": "a",
                "acceptance_criteria": ["assert a"]
            },
            {
                "title": "build cli",
                "content": "b",
                "acceptance_criteria": ["assert b"]
            }
        ]
    }));
    let err = run_decompose(vec![ok(response)], dir.path()).await.expect_err("dupes");
    match err {
        DecomposerError::DuplicateTitles(titles) => {
            assert_eq!(titles, vec!["build cli".to_string()]);
        }
        other => panic!("expected DuplicateTitles, got {other:?}"),
    }
}

#[tokio::test]
async fn unresolved_dep_errors() {
    let dir = TempDir::new().expect("tempdir");
    let response = tool_call(json!({
        "children": [
            {
                "title": "A",
                "content": "a",
                "dependencies": ["NotThere"],
                "acceptance_criteria": ["assert a"]
            }
        ]
    }));
    let err = run_decompose(vec![ok(response)], dir.path())
        .await
        .expect_err("unresolved");
    assert!(matches!(err, DecomposerError::UnresolvedDeps(_)), "got: {err:?}");
}

#[tokio::test]
async fn empty_title_errors_empty_title_with_index() {
    let dir = TempDir::new().expect("tempdir");
    let response = tool_call(json!({
        "children": [
            {
                "title": "first",
                "content": "",
                "acceptance_criteria": ["assert first"]
            },
            {
                "title": "   ",
                "content": "x",
                "acceptance_criteria": ["assert second"]
            }
        ]
    }));
    let err = run_decompose(vec![ok(response)], dir.path())
        .await
        .expect_err("empty title");
    match err {
        DecomposerError::EmptyTitle(idx) => assert_eq!(idx, 1),
        other => panic!("expected EmptyTitle(1), got {other:?}"),
    }
}

#[tokio::test]
async fn empty_ac_in_both_array_and_content_errors_empty_acceptance_criteria() {
    let dir = TempDir::new().expect("tempdir");
    let response = tool_call(json!({
        "children": [
            {
                "title": "Foo",
                "content": "no AC section here",
                "acceptance_criteria": []
            }
        ]
    }));
    let err = run_decompose(vec![ok(response)], dir.path())
        .await
        .expect_err("empty AC");
    match err {
        DecomposerError::EmptyAcceptanceCriteria(title) => assert_eq!(title, "Foo"),
        other => panic!("expected EmptyAcceptanceCriteria, got {other:?}"),
    }
}

#[tokio::test]
async fn ac_fallback_extracts_from_markdown_when_array_empty() {
    let dir = TempDir::new().expect("tempdir");
    let content = "# Foo\n\n## Acceptance Criteria\n\n- assert one thing\n- assert another\n\n## Next Section\nno\n";
    let response = tool_call(json!({
        "children": [
            {
                "title": "Foo",
                "content": content,
                "acceptance_criteria": []
            }
        ]
    }));
    let works = run_decompose(vec![ok(response)], dir.path()).await.expect("ok");
    assert_eq!(works.len(), 1);
    assert_eq!(
        works[0].acceptance_criteria.len(),
        2,
        "AC should fall back to markdown extraction"
    );
}

#[tokio::test]
async fn every_work_has_parent_id_equal_to_plan_id() {
    let dir = TempDir::new().expect("tempdir");
    let response = tool_call(json!({
        "children": [
            {"title": "A", "content": "a", "acceptance_criteria": ["assert a"]},
            {"title": "B", "content": "b", "acceptance_criteria": ["assert b"]}
        ]
    }));
    let plan = fresh_plan("roots");
    let llm = MockLlmClient::new(vec![ok(response)]);
    let works = super::decompose(&plan, dir.path(), &llm).await.expect("ok");
    for w in &works {
        assert_eq!(w.parent_id, plan.id);
        assert_eq!(w.status, domain::WorkStatus::Pending);
    }
}
