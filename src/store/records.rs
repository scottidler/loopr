//! Loop record types for TaskStore persistence.
//!
//! This module defines the unified `LoopRecord` type that stores all loop types
//! (plan, spec, phase, ralph) in a single JSONL file. The `loop_type` field
//! discriminates between them.

use chrono::Utc;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::time::{SystemTime, UNIX_EPOCH};

/// The unified loop record stored in TaskStore.
///
/// All loop types (plan, spec, phase, ralph) use this same record.
/// The `loop_type` field discriminates between them.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct LoopRecord {
    /// Timestamp-based ID: "1737802800"
    pub id: String,

    /// Discriminator: plan | spec | phase | ralph
    pub loop_type: LoopType,

    /// Parent loop ID (None for top-level plan loops)
    pub parent_loop: Option<String>,

    /// Path to artifact that spawned this loop
    pub triggered_by: Option<String>,

    /// TUI conversation reference
    pub conversation_id: Option<String>,

    /// Current status
    pub status: LoopStatus,

    /// Current iteration (1-indexed once started)
    pub iteration: u32,

    /// Limit before failure
    pub max_iterations: u32,

    /// Accumulated iteration feedback
    pub progress: String,

    /// Loop-type-specific data (template variables, task description, etc.)
    pub context: serde_json::Value,

    /// Unix timestamp in milliseconds
    pub created_at: i64,

    /// Unix timestamp in milliseconds
    pub updated_at: i64,
}

impl LoopRecord {
    /// Create a new loop record with the given type and context.
    pub fn new(loop_type: LoopType, context: serde_json::Value) -> Self {
        let now = now_ms();
        Self {
            id: generate_loop_id(),
            loop_type,
            parent_loop: None,
            triggered_by: None,
            conversation_id: None,
            status: LoopStatus::Pending,
            iteration: 0,
            max_iterations: 10,
            progress: String::new(),
            context,
            created_at: now,
            updated_at: now,
        }
    }

    /// Create a new plan loop.
    pub fn new_plan(task: &str, max_iterations: u32) -> Self {
        let context = serde_json::json!({
            "task": task,
            "review_pass": 1,
        });
        let mut record = Self::new(LoopType::Plan, context);
        record.max_iterations = max_iterations;
        record
    }

    /// Create a new spec loop from a plan.
    pub fn new_spec(parent_loop: &str, plan_content: &str, max_iterations: u32) -> Self {
        let context = serde_json::json!({
            "plan_content": plan_content,
            "plan_id": parent_loop,
        });
        let mut record = Self::new(LoopType::Spec, context);
        record.parent_loop = Some(parent_loop.to_string());
        record.max_iterations = max_iterations;
        record
    }

    /// Create a new phase loop from a spec.
    pub fn new_phase(
        parent_loop: &str,
        spec_content: &str,
        phase_number: u32,
        phase_name: &str,
        phases_total: u32,
        max_iterations: u32,
    ) -> Self {
        let context = serde_json::json!({
            "spec_content": spec_content,
            "spec_id": parent_loop,
            "phase_number": phase_number,
            "phase_name": phase_name,
            "phases_total": phases_total,
        });
        let mut record = Self::new(LoopType::Phase, context);
        record.parent_loop = Some(parent_loop.to_string());
        record.max_iterations = max_iterations;
        record
    }

    /// Create a new standalone ralph loop.
    pub fn new_ralph(task: &str, max_iterations: u32) -> Self {
        let context = serde_json::json!({
            "task": task,
        });
        let mut record = Self::new(LoopType::Ralph, context);
        record.max_iterations = max_iterations;
        record
    }

    /// Create a new ralph loop from a phase.
    pub fn new_ralph_from_phase(parent_loop: &str, phase_content: &str, task: &str, max_iterations: u32) -> Self {
        let context = serde_json::json!({
            "task": task,
            "phase_content": phase_content,
            "phase_id": parent_loop,
        });
        let mut record = Self::new(LoopType::Ralph, context);
        record.parent_loop = Some(parent_loop.to_string());
        record.max_iterations = max_iterations;
        record
    }

    /// Update the timestamp to now.
    pub fn touch(&mut self) {
        self.updated_at = now_ms();
    }

    /// Get indexed fields for SQLite queries.
    pub fn indexed_fields(&self) -> HashMap<String, IndexValue> {
        let mut fields = HashMap::new();
        fields.insert(
            "loop_type".into(),
            IndexValue::String(self.loop_type.as_str().to_string()),
        );
        fields.insert("status".into(), IndexValue::String(self.status.as_str().to_string()));
        if let Some(ref parent) = self.parent_loop {
            fields.insert("parent_loop".into(), IndexValue::String(parent.clone()));
        }
        fields
    }
}

/// Loop type discriminator.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum LoopType {
    Plan,
    Spec,
    Phase,
    Ralph,
}

impl LoopType {
    /// Get the string representation.
    pub fn as_str(&self) -> &'static str {
        match self {
            LoopType::Plan => "plan",
            LoopType::Spec => "spec",
            LoopType::Phase => "phase",
            LoopType::Ralph => "ralph",
        }
    }
}

impl std::fmt::Display for LoopType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Loop status state machine.
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "lowercase")]
pub enum LoopStatus {
    /// Waiting to start
    Pending,
    /// Actively iterating
    Running,
    /// User paused or rate limited
    Paused,
    /// Validation passed
    Complete,
    /// Max iterations or unrecoverable error
    Failed,
    /// Parent re-iterated, this loop is stale
    Invalidated,
}

impl LoopStatus {
    /// Get the string representation.
    pub fn as_str(&self) -> &'static str {
        match self {
            LoopStatus::Pending => "pending",
            LoopStatus::Running => "running",
            LoopStatus::Paused => "paused",
            LoopStatus::Complete => "complete",
            LoopStatus::Failed => "failed",
            LoopStatus::Invalidated => "invalidated",
        }
    }

    /// Check if this is a terminal status.
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            LoopStatus::Complete | LoopStatus::Failed | LoopStatus::Invalidated
        )
    }

    /// Check if this loop can be started.
    pub fn can_start(&self) -> bool {
        matches!(self, LoopStatus::Pending | LoopStatus::Paused)
    }
}

impl std::fmt::Display for LoopStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.as_str())
    }
}

/// Index value types for SQLite.
#[derive(Debug, Clone)]
pub enum IndexValue {
    String(String),
    Integer(i64),
}

/// Generate a unique loop ID based on timestamp with sub-second precision.
///
/// Format: seconds + microseconds suffix (e.g., "1737802800123456")
/// This ensures uniqueness even when creating multiple records per second.
pub fn generate_loop_id() -> String {
    use std::sync::atomic::{AtomicU32, Ordering};
    static COUNTER: AtomicU32 = AtomicU32::new(0);

    let duration = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("Time went backwards");

    // Use seconds + subsec_micros + atomic counter for uniqueness
    let secs = duration.as_secs();
    let micros = duration.subsec_micros();
    let counter = COUNTER.fetch_add(1, Ordering::Relaxed);

    format!("{}{:06}{:04}", secs, micros, counter % 10000)
}

/// Get current time in milliseconds since epoch.
pub fn now_ms() -> i64 {
    Utc::now().timestamp_millis()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_loop_type_as_str() {
        assert_eq!(LoopType::Plan.as_str(), "plan");
        assert_eq!(LoopType::Spec.as_str(), "spec");
        assert_eq!(LoopType::Phase.as_str(), "phase");
        assert_eq!(LoopType::Ralph.as_str(), "ralph");
    }

    #[test]
    fn test_loop_status_as_str() {
        assert_eq!(LoopStatus::Pending.as_str(), "pending");
        assert_eq!(LoopStatus::Running.as_str(), "running");
        assert_eq!(LoopStatus::Complete.as_str(), "complete");
    }

    #[test]
    fn test_loop_status_is_terminal() {
        assert!(!LoopStatus::Pending.is_terminal());
        assert!(!LoopStatus::Running.is_terminal());
        assert!(!LoopStatus::Paused.is_terminal());
        assert!(LoopStatus::Complete.is_terminal());
        assert!(LoopStatus::Failed.is_terminal());
        assert!(LoopStatus::Invalidated.is_terminal());
    }

    #[test]
    fn test_loop_status_can_start() {
        assert!(LoopStatus::Pending.can_start());
        assert!(LoopStatus::Paused.can_start());
        assert!(!LoopStatus::Running.can_start());
        assert!(!LoopStatus::Complete.can_start());
    }

    #[test]
    fn test_new_plan_loop() {
        let record = LoopRecord::new_plan("Build a REST API", 15);
        assert_eq!(record.loop_type, LoopType::Plan);
        assert_eq!(record.status, LoopStatus::Pending);
        assert_eq!(record.max_iterations, 15);
        assert!(record.parent_loop.is_none());
        assert_eq!(record.context["task"], "Build a REST API");
        assert_eq!(record.context["review_pass"], 1);
    }

    #[test]
    fn test_new_spec_loop() {
        let record = LoopRecord::new_spec("123456", "# Plan content", 10);
        assert_eq!(record.loop_type, LoopType::Spec);
        assert_eq!(record.parent_loop, Some("123456".to_string()));
        assert_eq!(record.context["plan_content"], "# Plan content");
        assert_eq!(record.context["plan_id"], "123456");
    }

    #[test]
    fn test_new_phase_loop() {
        let record = LoopRecord::new_phase("spec123", "# Spec", 2, "Implement tokens", 5, 10);
        assert_eq!(record.loop_type, LoopType::Phase);
        assert_eq!(record.parent_loop, Some("spec123".to_string()));
        assert_eq!(record.context["phase_number"], 2);
        assert_eq!(record.context["phase_name"], "Implement tokens");
        assert_eq!(record.context["phases_total"], 5);
    }

    #[test]
    fn test_new_ralph_standalone() {
        let record = LoopRecord::new_ralph("Fix bug in auth.rs", 5);
        assert_eq!(record.loop_type, LoopType::Ralph);
        assert!(record.parent_loop.is_none());
        assert_eq!(record.context["task"], "Fix bug in auth.rs");
    }

    #[test]
    fn test_new_ralph_from_phase() {
        let record = LoopRecord::new_ralph_from_phase("phase123", "# Phase", "Do the task", 5);
        assert_eq!(record.loop_type, LoopType::Ralph);
        assert_eq!(record.parent_loop, Some("phase123".to_string()));
        assert_eq!(record.context["phase_content"], "# Phase");
        assert_eq!(record.context["phase_id"], "phase123");
    }

    #[test]
    fn test_indexed_fields() {
        let mut record = LoopRecord::new_plan("Test", 10);
        record.parent_loop = Some("parent123".to_string());

        let fields = record.indexed_fields();
        assert!(matches!(
            fields.get("loop_type"),
            Some(IndexValue::String(s)) if s == "plan"
        ));
        assert!(matches!(
            fields.get("status"),
            Some(IndexValue::String(s)) if s == "pending"
        ));
        assert!(matches!(
            fields.get("parent_loop"),
            Some(IndexValue::String(s)) if s == "parent123"
        ));
    }

    #[test]
    fn test_touch_updates_timestamp() {
        let mut record = LoopRecord::new_plan("Test", 10);
        let original = record.updated_at;

        // Sleep briefly to ensure time advances
        std::thread::sleep(std::time::Duration::from_millis(2));

        record.touch();
        assert!(record.updated_at >= original);
    }

    #[test]
    fn test_loop_record_serialization() {
        let record = LoopRecord::new_plan("Test task", 10);
        let json = serde_json::to_string(&record).unwrap();
        let deserialized: LoopRecord = serde_json::from_str(&json).unwrap();

        assert_eq!(record.loop_type, deserialized.loop_type);
        assert_eq!(record.status, deserialized.status);
        assert_eq!(record.context, deserialized.context);
    }

    #[test]
    fn test_generate_loop_id_is_numeric() {
        let id = generate_loop_id();
        // ID is a large numeric string (seconds + micros + counter)
        assert!(id.chars().all(|c| c.is_ascii_digit()));
        assert!(id.len() >= 16); // At least seconds (10) + micros (6)
    }

    #[test]
    fn test_generate_loop_id_uniqueness() {
        let ids: Vec<String> = (0..100).map(|_| generate_loop_id()).collect();
        let unique: std::collections::HashSet<_> = ids.iter().collect();
        assert_eq!(ids.len(), unique.len(), "IDs should be unique");
    }
}
