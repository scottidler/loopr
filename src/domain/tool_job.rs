//! Tool job records for audit trail of tool executions
//!
//! ToolJobRecord tracks each tool execution within a loop iteration,
//! providing an audit trail for debugging and observability.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use taskstore::{IndexValue, Record};

use crate::id::{generate_job_id, now_ms};

/// Status of a tool job execution
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum ToolJobStatus {
    /// Waiting to be executed
    Pending,
    /// Currently executing
    Running,
    /// Completed successfully
    Success,
    /// Execution failed
    Failed,
    /// Execution timed out
    Timeout,
    /// Execution was cancelled
    Cancelled,
}

impl ToolJobStatus {
    /// Check if this status represents a terminal state
    pub fn is_terminal(&self) -> bool {
        matches!(self, Self::Success | Self::Failed | Self::Timeout | Self::Cancelled)
    }

    /// Check if this status represents a successful completion
    pub fn is_success(&self) -> bool {
        matches!(self, Self::Success)
    }

    /// Check if this status represents a failure
    pub fn is_failure(&self) -> bool {
        matches!(self, Self::Failed | Self::Timeout | Self::Cancelled)
    }
}

/// Audit trail record for a tool execution
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolJobRecord {
    /// Unique identifier for this job
    pub id: String,

    /// The loop that executed this tool
    pub loop_id: String,

    /// Which iteration of the loop this was executed in
    pub iteration: u32,

    /// Name of the tool that was executed
    pub tool_name: String,

    /// Lane the tool runs in ("no-net", "net", "heavy")
    pub lane: String,

    /// Truncated summary of the input provided to the tool
    pub input_summary: String,

    /// Truncated summary of the tool's output
    pub output_summary: String,

    /// Current status of the job
    pub status: ToolJobStatus,

    /// Exit code if the tool was a command
    pub exit_code: Option<i32>,

    /// How long the execution took in milliseconds
    pub duration_ms: u64,

    /// When this job was created (Unix ms)
    pub created_at: i64,

    /// When this job completed (Unix ms)
    pub completed_at: Option<i64>,
}

impl ToolJobRecord {
    /// Create a new tool job record
    pub fn new(loop_id: &str, iteration: u32, tool_name: &str, lane: &str) -> Self {
        Self {
            id: generate_job_id(loop_id, iteration),
            loop_id: loop_id.to_string(),
            iteration,
            tool_name: tool_name.to_string(),
            lane: lane.to_string(),
            input_summary: String::new(),
            output_summary: String::new(),
            status: ToolJobStatus::Pending,
            exit_code: None,
            duration_ms: 0,
            created_at: now_ms(),
            completed_at: None,
        }
    }

    /// Set the input summary (truncated if needed)
    pub fn with_input(mut self, input: &str) -> Self {
        self.input_summary = truncate_string(input, 1000);
        self
    }

    /// Mark job as running
    pub fn mark_running(&mut self) {
        self.status = ToolJobStatus::Running;
    }

    /// Mark job as successfully completed
    pub fn mark_success(&mut self, output: &str, duration_ms: u64) {
        self.status = ToolJobStatus::Success;
        self.output_summary = truncate_string(output, 1000);
        self.duration_ms = duration_ms;
        self.completed_at = Some(now_ms());
    }

    /// Mark job as failed
    pub fn mark_failed(&mut self, output: &str, exit_code: Option<i32>, duration_ms: u64) {
        self.status = ToolJobStatus::Failed;
        self.output_summary = truncate_string(output, 1000);
        self.exit_code = exit_code;
        self.duration_ms = duration_ms;
        self.completed_at = Some(now_ms());
    }

    /// Mark job as timed out
    pub fn mark_timeout(&mut self, output: &str, duration_ms: u64) {
        self.status = ToolJobStatus::Timeout;
        self.output_summary = truncate_string(output, 1000);
        self.duration_ms = duration_ms;
        self.completed_at = Some(now_ms());
    }

    /// Mark job as cancelled
    pub fn mark_cancelled(&mut self) {
        self.status = ToolJobStatus::Cancelled;
        self.completed_at = Some(now_ms());
    }
}

impl Record for ToolJobRecord {
    fn id(&self) -> &str {
        &self.id
    }

    fn updated_at(&self) -> i64 {
        self.completed_at.unwrap_or(self.created_at)
    }

    fn collection_name() -> &'static str {
        "tool_jobs"
    }

    fn indexed_fields(&self) -> HashMap<String, IndexValue> {
        let mut fields = HashMap::new();
        fields.insert("loop_id".to_string(), IndexValue::String(self.loop_id.clone()));
        fields.insert("iteration".to_string(), IndexValue::Int(self.iteration as i64));
        fields.insert("tool_name".to_string(), IndexValue::String(self.tool_name.clone()));
        fields.insert(
            "status".to_string(),
            IndexValue::String(serde_json::to_string(&self.status).unwrap_or_default()),
        );
        fields
    }
}

/// Truncate a string to a maximum length, adding ellipsis if truncated
fn truncate_string(s: &str, max_len: usize) -> String {
    if s.len() <= max_len {
        s.to_string()
    } else {
        format!("{}...", &s[..max_len.saturating_sub(3)])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tool_job_status_is_terminal() {
        assert!(!ToolJobStatus::Pending.is_terminal());
        assert!(!ToolJobStatus::Running.is_terminal());
        assert!(ToolJobStatus::Success.is_terminal());
        assert!(ToolJobStatus::Failed.is_terminal());
        assert!(ToolJobStatus::Timeout.is_terminal());
        assert!(ToolJobStatus::Cancelled.is_terminal());
    }

    #[test]
    fn test_tool_job_status_is_success() {
        assert!(!ToolJobStatus::Pending.is_success());
        assert!(!ToolJobStatus::Running.is_success());
        assert!(ToolJobStatus::Success.is_success());
        assert!(!ToolJobStatus::Failed.is_success());
        assert!(!ToolJobStatus::Timeout.is_success());
        assert!(!ToolJobStatus::Cancelled.is_success());
    }

    #[test]
    fn test_tool_job_status_is_failure() {
        assert!(!ToolJobStatus::Pending.is_failure());
        assert!(!ToolJobStatus::Running.is_failure());
        assert!(!ToolJobStatus::Success.is_failure());
        assert!(ToolJobStatus::Failed.is_failure());
        assert!(ToolJobStatus::Timeout.is_failure());
        assert!(ToolJobStatus::Cancelled.is_failure());
    }

    #[test]
    fn test_tool_job_record_new() {
        let job = ToolJobRecord::new("loop-123", 1, "read_file", "no-net");

        assert!(job.id.starts_with("job-"));
        assert_eq!(job.loop_id, "loop-123");
        assert_eq!(job.iteration, 1);
        assert_eq!(job.tool_name, "read_file");
        assert_eq!(job.lane, "no-net");
        assert_eq!(job.status, ToolJobStatus::Pending);
        assert!(job.input_summary.is_empty());
        assert!(job.output_summary.is_empty());
        assert_eq!(job.duration_ms, 0);
        assert!(job.exit_code.is_none());
        assert!(job.completed_at.is_none());
    }

    #[test]
    fn test_tool_job_record_with_input() {
        let job = ToolJobRecord::new("loop-123", 1, "write_file", "no-net").with_input("some input data");

        assert_eq!(job.input_summary, "some input data");
    }

    #[test]
    fn test_tool_job_record_with_long_input_truncated() {
        let long_input = "x".repeat(2000);
        let job = ToolJobRecord::new("loop-123", 1, "write_file", "no-net").with_input(&long_input);

        assert!(job.input_summary.len() <= 1000);
        assert!(job.input_summary.ends_with("..."));
    }

    #[test]
    fn test_tool_job_record_mark_running() {
        let mut job = ToolJobRecord::new("loop-123", 1, "bash", "no-net");

        job.mark_running();

        assert_eq!(job.status, ToolJobStatus::Running);
    }

    #[test]
    fn test_tool_job_record_mark_success() {
        let mut job = ToolJobRecord::new("loop-123", 1, "bash", "no-net");

        job.mark_success("output data", 150);

        assert_eq!(job.status, ToolJobStatus::Success);
        assert_eq!(job.output_summary, "output data");
        assert_eq!(job.duration_ms, 150);
        assert!(job.completed_at.is_some());
    }

    #[test]
    fn test_tool_job_record_mark_failed() {
        let mut job = ToolJobRecord::new("loop-123", 1, "bash", "no-net");

        job.mark_failed("error message", Some(1), 200);

        assert_eq!(job.status, ToolJobStatus::Failed);
        assert_eq!(job.output_summary, "error message");
        assert_eq!(job.exit_code, Some(1));
        assert_eq!(job.duration_ms, 200);
        assert!(job.completed_at.is_some());
    }

    #[test]
    fn test_tool_job_record_mark_timeout() {
        let mut job = ToolJobRecord::new("loop-123", 1, "bash", "no-net");

        job.mark_timeout("partial output", 60000);

        assert_eq!(job.status, ToolJobStatus::Timeout);
        assert_eq!(job.output_summary, "partial output");
        assert_eq!(job.duration_ms, 60000);
        assert!(job.completed_at.is_some());
    }

    #[test]
    fn test_tool_job_record_mark_cancelled() {
        let mut job = ToolJobRecord::new("loop-123", 1, "bash", "no-net");

        job.mark_cancelled();

        assert_eq!(job.status, ToolJobStatus::Cancelled);
        assert!(job.completed_at.is_some());
    }

    #[test]
    fn test_tool_job_record_serialization() {
        let job = ToolJobRecord::new("loop-123", 1, "read_file", "no-net");

        let json = serde_json::to_string(&job).expect("serialize");
        let deserialized: ToolJobRecord = serde_json::from_str(&json).expect("deserialize");

        assert_eq!(deserialized.loop_id, job.loop_id);
        assert_eq!(deserialized.tool_name, job.tool_name);
        assert_eq!(deserialized.status, job.status);
    }

    #[test]
    fn test_truncate_string_short() {
        assert_eq!(truncate_string("hello", 10), "hello");
    }

    #[test]
    fn test_truncate_string_long() {
        let result = truncate_string("hello world this is long", 10);
        assert_eq!(result.len(), 10);
        assert!(result.ends_with("..."));
    }

    #[test]
    fn test_truncate_string_exact_length() {
        assert_eq!(truncate_string("hello", 5), "hello");
    }
}
