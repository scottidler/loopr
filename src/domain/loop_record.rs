//! Loop record and related types
//!
//! The Loop is the core abstraction in Loopr. It implements the Ralph Wiggum pattern:
//! an iterative loop that calls an LLM with fresh context on each iteration until
//! validation passes.
//!
//! Per domain-types.md, Loop is self-contained with its own `run()` method.
//! There is no separate LoopRunner - that was unnecessary indirection.

use std::path::PathBuf;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::domain::LoopOutcome;
use crate::error::Result;
use crate::id::{generate_child_id, generate_loop_id, now_ms};
use crate::llm::{CompletionRequest, LlmClient, Message, ToolDefinition};
use crate::prompt::PromptRenderer;
use crate::storage::HasId;
use crate::tools::ToolRouter;
use crate::validation::Validator;

/// The core Loop struct representing a single loop instance
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Loop {
    //=== Identity ===
    /// Unique identifier (timestamp + random suffix: "1738300800123-a1b2")
    pub id: String,

    /// What kind of loop: Plan, Spec, Phase, or Code
    pub loop_type: LoopType,

    /// Parent loop that spawned this one (None for root PlanLoop)
    pub parent_id: Option<String>,

    //=== Artifacts ===
    /// The artifact this loop consumes (parent's output)
    pub input_artifact: Option<PathBuf>,

    /// The artifact(s) this loop produces
    pub output_artifacts: Vec<PathBuf>,

    //=== Behavior Configuration ===
    /// Path to the prompt template for this loop type
    pub prompt_path: PathBuf,

    /// Command to validate this loop's output
    pub validation_command: String,

    /// Maximum iterations before failure
    pub max_iterations: u32,

    //=== Workspace ===
    /// Git worktree path for this loop's work
    pub worktree: PathBuf,

    //=== Runtime State ===
    /// Current iteration number (0-indexed, increments on failure)
    pub iteration: u32,

    /// Current status
    pub status: LoopStatus,

    /// Accumulated feedback from failed iterations
    pub progress: String,

    /// Loop-type-specific context data
    pub context: serde_json::Value,

    //=== Timestamps ===
    pub created_at: i64,
    pub updated_at: i64,
}

/// The four types of loops in the hierarchy
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LoopType {
    Plan,
    Spec,
    Phase,
    Code,
}

/// Status of a loop's execution
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LoopStatus {
    /// Waiting to start
    Pending,
    /// Actively iterating
    Running,
    /// User-initiated pause (resumable)
    Paused,
    /// Stopped for rebase after sibling merge
    Rebasing,
    /// Validation passed, artifacts produced
    Complete,
    /// Max iterations exhausted
    Failed,
    /// Parent re-iterated, this loop's work is stale
    Invalidated,
}

impl LoopStatus {
    /// Returns true if the loop is in a terminal state
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            LoopStatus::Complete | LoopStatus::Failed | LoopStatus::Invalidated
        )
    }

    /// Returns true if the loop can be resumed
    pub fn is_resumable(&self) -> bool {
        matches!(self, LoopStatus::Paused | LoopStatus::Rebasing)
    }
}

impl Loop {
    /// Create a new PlanLoop from a user task description
    pub fn new_plan(task: &str) -> Self {
        let id = generate_loop_id();
        let now = now_ms();

        Self {
            id: id.clone(),
            loop_type: LoopType::Plan,
            parent_id: None,
            input_artifact: None,
            output_artifacts: vec![],
            prompt_path: PathBuf::from("prompts/plan.md"),
            validation_command: "loopr validate plan".to_string(),
            max_iterations: 10,
            worktree: PathBuf::from(format!(".loopr/worktrees/plan-{}", &id)),
            iteration: 0,
            status: LoopStatus::Pending,
            progress: String::new(),
            context: serde_json::json!({ "task": task }),
            created_at: now,
            updated_at: now,
        }
    }

    /// Create a new SpecLoop as a child of a PlanLoop
    pub fn new_spec(parent: &Loop, index: u32) -> Self {
        let id = generate_child_id(&parent.id, index);
        let now = now_ms();

        Self {
            id: id.clone(),
            loop_type: LoopType::Spec,
            parent_id: Some(parent.id.clone()),
            input_artifact: parent.output_artifacts.first().cloned(),
            output_artifacts: vec![],
            prompt_path: PathBuf::from("prompts/spec.md"),
            validation_command: "loopr validate spec".to_string(),
            max_iterations: 10,
            worktree: PathBuf::from(format!(".loopr/worktrees/spec-{}", &id)),
            iteration: 0,
            status: LoopStatus::Pending,
            progress: String::new(),
            context: serde_json::json!({
                "plan_content": "",
                "spec_index": index
            }),
            created_at: now,
            updated_at: now,
        }
    }

    /// Create a new PhaseLoop as a child of a SpecLoop
    pub fn new_phase(parent: &Loop, index: u32, name: &str, total: u32) -> Self {
        let id = generate_child_id(&parent.id, index);
        let now = now_ms();

        Self {
            id: id.clone(),
            loop_type: LoopType::Phase,
            parent_id: Some(parent.id.clone()),
            input_artifact: parent.output_artifacts.first().cloned(),
            output_artifacts: vec![],
            prompt_path: PathBuf::from("prompts/phase.md"),
            validation_command: "loopr validate phase".to_string(),
            max_iterations: 10,
            worktree: PathBuf::from(format!(".loopr/worktrees/phase-{}", &id)),
            iteration: 0,
            status: LoopStatus::Pending,
            progress: String::new(),
            context: serde_json::json!({
                "spec_content": "",
                "phase_number": index,
                "phase_name": name,
                "total_phases": total
            }),
            created_at: now,
            updated_at: now,
        }
    }

    /// Create a new CodeLoop as a child of a PhaseLoop
    pub fn new_code(parent: &Loop) -> Self {
        let id = generate_child_id(&parent.id, 1);
        let now = now_ms();

        Self {
            id: id.clone(),
            loop_type: LoopType::Code,
            parent_id: Some(parent.id.clone()),
            input_artifact: parent.output_artifacts.first().cloned(),
            output_artifacts: vec![], // CodeLoop produces code, not artifacts
            prompt_path: PathBuf::from("prompts/code.md"),
            validation_command: "otto ci".to_string(),
            max_iterations: 100,
            worktree: PathBuf::from(format!(".loopr/worktrees/code-{}", &id)),
            iteration: 0,
            status: LoopStatus::Pending,
            progress: String::new(),
            context: serde_json::json!({
                "phase_content": "",
                "task": ""
            }),
            created_at: now,
            updated_at: now,
        }
    }

    /// Update the timestamp
    pub fn touch(&mut self) {
        self.updated_at = now_ms();
    }

    /// Run the loop until completion or failure.
    ///
    /// This implements the Ralph Wiggum pattern:
    /// - Fresh context (new messages array) each iteration
    /// - Feedback accumulated in the prompt, not conversation history
    /// - Iterate until validation passes or max iterations reached
    ///
    /// Per domain-types.md, Loop is self-contained. The `run()` method
    /// takes dependencies as parameters rather than having a separate
    /// LoopRunner struct.
    pub async fn run<L, T, V>(
        &mut self,
        llm: Arc<L>,
        tool_router: Arc<T>,
        validator: Arc<V>,
    ) -> Result<LoopOutcome>
    where
        L: LlmClient,
        T: ToolRouter,
        V: Validator,
    {
        self.run_with_config(llm, tool_router, validator, LoopRunConfig::default()).await
    }

    /// Run the loop with custom configuration.
    pub async fn run_with_config<L, T, V>(
        &mut self,
        llm: Arc<L>,
        tool_router: Arc<T>,
        validator: Arc<V>,
        config: LoopRunConfig,
    ) -> Result<LoopOutcome>
    where
        L: LlmClient,
        T: ToolRouter,
        V: Validator,
    {
        self.status = LoopStatus::Running;
        let prompt_renderer = PromptRenderer::new();

        while self.iteration < self.max_iterations {
            // 1. Build prompt with accumulated feedback (FRESH CONTEXT)
            let system_prompt = self.build_system_prompt(&prompt_renderer)?;
            let user_message = self.build_user_message();

            // 2. Call LLM - NEW messages array each time
            let tools = self.get_tools_for_loop_type(&*tool_router);
            let request = CompletionRequest {
                system: system_prompt,
                messages: vec![Message::user(&user_message)],
                tools,
                max_tokens: Some(config.max_tokens),
                ..Default::default()
            };

            let response = llm.complete(request).await?;

            // 3. Execute tool calls
            for call in &response.tool_calls {
                let result = tool_router.execute(call.clone(), &self.worktree).await?;

                // If tool execution fails, add to progress
                if result.is_error {
                    self.progress
                        .push_str(&format!("\nTool {} failed: {}\n", call.name, result.content));
                }
            }

            // 4. Validate output
            let artifact_path = self.get_artifact_path();
            let validation_result = validator.validate(&artifact_path, &self.worktree).await?;

            if validation_result.passed {
                self.status = LoopStatus::Complete;
                return Ok(LoopOutcome::Complete);
            }

            // 5. Accumulate feedback for next iteration
            self.progress.push_str(&format!(
                "\n---\n## Iteration {} Failed\n{}\n",
                self.iteration + 1,
                validation_result.output
            ));

            for error in &validation_result.errors {
                self.progress.push_str(&format!("- {}\n", error));
            }

            self.iteration += 1;

            // ITERATION ENDS - Context is discarded
            // Next iteration starts completely fresh
        }

        // Max iterations exhausted
        self.status = LoopStatus::Failed;
        Ok(LoopOutcome::Failed("Max iterations exhausted".into()))
    }

    /// Build the system prompt for this loop.
    fn build_system_prompt(&self, prompt_renderer: &PromptRenderer) -> Result<String> {
        // Render the prompt template with the loop's context
        prompt_renderer.render_json(&self.prompt_path.to_string_lossy(), &self.context)
    }

    /// Build the user message with task and accumulated feedback.
    fn build_user_message(&self) -> String {
        let task = self.context.get("task").and_then(|v| v.as_str()).unwrap_or("");

        if self.progress.is_empty() {
            task.to_string()
        } else {
            format!("{}\n\n## Previous Iteration Feedback\n{}", task, self.progress)
        }
    }

    /// Get tool definitions appropriate for the loop type.
    fn get_tools_for_loop_type<T: ToolRouter + ?Sized>(&self, tool_router: &T) -> Vec<ToolDefinition> {
        // For now, return all available tools
        // In the future, this could filter based on loop type
        let tool_names = tool_router.available_tools();
        tool_names
            .iter()
            .map(|name| ToolDefinition {
                name: name.clone(),
                description: format!("Tool: {}", name),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {}
                }),
            })
            .collect()
    }

    /// Get the path to the output artifact for validation.
    fn get_artifact_path(&self) -> PathBuf {
        if let Some(artifact) = self.output_artifacts.first() {
            artifact.clone()
        } else {
            // For CodeLoop, validate the worktree itself
            self.worktree.clone()
        }
    }
}

/// Configuration for loop execution.
#[derive(Debug, Clone)]
pub struct LoopRunConfig {
    /// Maximum tokens for LLM responses
    pub max_tokens: u32,
    /// Whether to use streaming for LLM calls
    pub use_streaming: bool,
}

impl Default for LoopRunConfig {
    fn default() -> Self {
        Self {
            max_tokens: 8192,
            use_streaming: false,
        }
    }
}

impl HasId for Loop {
    fn id(&self) -> &str {
        &self.id
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_loop_status_is_terminal() {
        assert!(LoopStatus::Complete.is_terminal());
        assert!(LoopStatus::Failed.is_terminal());
        assert!(LoopStatus::Invalidated.is_terminal());
        assert!(!LoopStatus::Pending.is_terminal());
        assert!(!LoopStatus::Running.is_terminal());
        assert!(!LoopStatus::Paused.is_terminal());
        assert!(!LoopStatus::Rebasing.is_terminal());
    }

    #[test]
    fn test_loop_status_is_resumable() {
        assert!(LoopStatus::Paused.is_resumable());
        assert!(LoopStatus::Rebasing.is_resumable());
        assert!(!LoopStatus::Pending.is_resumable());
        assert!(!LoopStatus::Running.is_resumable());
        assert!(!LoopStatus::Complete.is_resumable());
        assert!(!LoopStatus::Failed.is_resumable());
        assert!(!LoopStatus::Invalidated.is_resumable());
    }

    #[test]
    fn test_new_plan_creates_correct_fields() {
        let plan = Loop::new_plan("Add OAuth authentication");

        assert_eq!(plan.loop_type, LoopType::Plan);
        assert!(plan.parent_id.is_none());
        assert!(plan.input_artifact.is_none());
        assert!(plan.output_artifacts.is_empty());
        assert_eq!(plan.prompt_path, PathBuf::from("prompts/plan.md"));
        assert_eq!(plan.validation_command, "loopr validate plan");
        assert_eq!(plan.max_iterations, 10);
        assert_eq!(plan.iteration, 0);
        assert_eq!(plan.status, LoopStatus::Pending);
        assert!(plan.progress.is_empty());
        assert_eq!(plan.context["task"], "Add OAuth authentication");
    }

    #[test]
    fn test_new_spec_creates_child_of_plan() {
        let plan = Loop::new_plan("Add OAuth");
        let spec = Loop::new_spec(&plan, 1);

        assert_eq!(spec.loop_type, LoopType::Spec);
        assert_eq!(spec.parent_id, Some(plan.id.clone()));
        assert!(spec.id.starts_with(&plan.id));
        assert_eq!(spec.prompt_path, PathBuf::from("prompts/spec.md"));
        assert_eq!(spec.context["spec_index"], 1);
    }

    #[test]
    fn test_new_phase_creates_child_of_spec() {
        let plan = Loop::new_plan("Add OAuth");
        let spec = Loop::new_spec(&plan, 1);
        let phase = Loop::new_phase(&spec, 1, "Create migrations", 3);

        assert_eq!(phase.loop_type, LoopType::Phase);
        assert_eq!(phase.parent_id, Some(spec.id.clone()));
        assert!(phase.id.starts_with(&spec.id));
        assert_eq!(phase.prompt_path, PathBuf::from("prompts/phase.md"));
        assert_eq!(phase.context["phase_number"], 1);
        assert_eq!(phase.context["phase_name"], "Create migrations");
        assert_eq!(phase.context["total_phases"], 3);
    }

    #[test]
    fn test_new_code_creates_child_of_phase() {
        let plan = Loop::new_plan("Add OAuth");
        let spec = Loop::new_spec(&plan, 1);
        let phase = Loop::new_phase(&spec, 1, "Create migrations", 3);
        let code = Loop::new_code(&phase);

        assert_eq!(code.loop_type, LoopType::Code);
        assert_eq!(code.parent_id, Some(phase.id.clone()));
        assert_eq!(code.prompt_path, PathBuf::from("prompts/code.md"));
        assert_eq!(code.validation_command, "otto ci");
        assert_eq!(code.max_iterations, 100);
    }

    #[test]
    fn test_loop_serialization_roundtrip() {
        let plan = Loop::new_plan("Test task");
        let json = serde_json::to_string(&plan).expect("serialize");
        let parsed: Loop = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(parsed.id, plan.id);
        assert_eq!(parsed.loop_type, plan.loop_type);
        assert_eq!(parsed.status, plan.status);
    }

    #[test]
    fn test_loop_type_serialization() {
        assert_eq!(serde_json::to_string(&LoopType::Plan).unwrap(), "\"plan\"");
        assert_eq!(serde_json::to_string(&LoopType::Spec).unwrap(), "\"spec\"");
        assert_eq!(serde_json::to_string(&LoopType::Phase).unwrap(), "\"phase\"");
        assert_eq!(serde_json::to_string(&LoopType::Code).unwrap(), "\"code\"");
    }

    #[test]
    fn test_loop_status_serialization() {
        assert_eq!(serde_json::to_string(&LoopStatus::Pending).unwrap(), "\"pending\"");
        assert_eq!(serde_json::to_string(&LoopStatus::Running).unwrap(), "\"running\"");
        assert_eq!(serde_json::to_string(&LoopStatus::Complete).unwrap(), "\"complete\"");
    }

    #[test]
    fn test_touch_updates_timestamp() {
        let mut plan = Loop::new_plan("Test");
        let original = plan.updated_at;

        // Small sleep to ensure different timestamp
        std::thread::sleep(std::time::Duration::from_millis(2));
        plan.touch();

        assert!(plan.updated_at >= original);
    }

    #[test]
    fn test_plan_worktree_path_format() {
        let plan = Loop::new_plan("Test");
        let worktree_str = plan.worktree.to_string_lossy();

        assert!(worktree_str.starts_with(".loopr/worktrees/plan-"));
        assert!(worktree_str.contains(&plan.id));
    }
}
