//! Tool definitions and lane classification
//!
//! Defines tools with their properties for routing to appropriate runners.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::llm::ToolDefinition;

/// Lane determines which runner executes a tool
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ToolLane {
    /// No network access - safe file operations and isolated commands
    NoNet,
    /// Network access - web fetch, API calls, build commands
    Net,
    /// Heavy operations - long-running builds, full test suites
    Heavy,
}

impl Default for ToolLane {
    fn default() -> Self {
        Self::NoNet
    }
}

impl ToolLane {
    /// Parse from string representation
    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "no-net" | "nonet" | "no_net" => Some(Self::NoNet),
            "net" => Some(Self::Net),
            "heavy" => Some(Self::Heavy),
            _ => None,
        }
    }

    /// Check if lane allows network access
    pub fn allows_network(&self) -> bool {
        matches!(self, Self::Net | Self::Heavy)
    }

    /// Get default timeout in ms for this lane
    pub fn default_timeout_ms(&self) -> u64 {
        match self {
            Self::NoNet => 10_000,
            Self::Net => 30_000,
            Self::Heavy => 600_000,
        }
    }
}

/// A tool definition with execution metadata
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tool {
    /// Tool name (e.g., "read_file", "write_file")
    pub name: String,
    /// Human-readable description for LLM
    pub description: String,
    /// JSON schema for input parameters
    pub input_schema: Value,
    /// Runner lane for execution
    #[serde(default)]
    pub lane: ToolLane,
    /// Timeout in milliseconds
    #[serde(default)]
    pub timeout_ms: Option<u64>,
    /// Whether tool requires worktree context
    #[serde(default)]
    pub requires_worktree: bool,
    /// Maximum output size in bytes
    #[serde(default)]
    pub max_output_bytes: Option<u64>,
}

impl Tool {
    /// Create a new tool definition
    pub fn new(name: impl Into<String>, description: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
            lane: ToolLane::NoNet,
            timeout_ms: None,
            requires_worktree: false,
            max_output_bytes: None,
        }
    }

    /// Set input schema
    pub fn with_schema(mut self, schema: Value) -> Self {
        self.input_schema = schema;
        self
    }

    /// Set lane
    pub fn with_lane(mut self, lane: ToolLane) -> Self {
        self.lane = lane;
        self
    }

    /// Set timeout
    pub fn with_timeout(mut self, timeout_ms: u64) -> Self {
        self.timeout_ms = Some(timeout_ms);
        self
    }

    /// Set requires worktree
    pub fn with_worktree_required(mut self) -> Self {
        self.requires_worktree = true;
        self
    }

    /// Set max output bytes
    pub fn with_max_output(mut self, max_bytes: u64) -> Self {
        self.max_output_bytes = Some(max_bytes);
        self
    }

    /// Get effective timeout (uses lane default if not specified)
    pub fn effective_timeout_ms(&self) -> u64 {
        self.timeout_ms.unwrap_or_else(|| self.lane.default_timeout_ms())
    }

    /// Convert to LLM ToolDefinition for API calls
    pub fn to_llm_definition(&self) -> ToolDefinition {
        ToolDefinition::new(self.name.clone(), self.description.clone(), self.input_schema.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tool_lane_from_str() {
        assert_eq!(ToolLane::from_str("no-net"), Some(ToolLane::NoNet));
        assert_eq!(ToolLane::from_str("nonet"), Some(ToolLane::NoNet));
        assert_eq!(ToolLane::from_str("no_net"), Some(ToolLane::NoNet));
        assert_eq!(ToolLane::from_str("net"), Some(ToolLane::Net));
        assert_eq!(ToolLane::from_str("heavy"), Some(ToolLane::Heavy));
        assert_eq!(ToolLane::from_str("unknown"), None);
    }

    #[test]
    fn test_tool_lane_allows_network() {
        assert!(!ToolLane::NoNet.allows_network());
        assert!(ToolLane::Net.allows_network());
        assert!(ToolLane::Heavy.allows_network());
    }

    #[test]
    fn test_tool_lane_default_timeout() {
        assert_eq!(ToolLane::NoNet.default_timeout_ms(), 10_000);
        assert_eq!(ToolLane::Net.default_timeout_ms(), 30_000);
        assert_eq!(ToolLane::Heavy.default_timeout_ms(), 600_000);
    }

    #[test]
    fn test_tool_lane_default() {
        let lane = ToolLane::default();
        assert_eq!(lane, ToolLane::NoNet);
    }

    #[test]
    fn test_tool_lane_serialization() {
        let json = serde_json::to_string(&ToolLane::NoNet).unwrap();
        assert_eq!(json, "\"no-net\"");
        let json = serde_json::to_string(&ToolLane::Net).unwrap();
        assert_eq!(json, "\"net\"");
        let json = serde_json::to_string(&ToolLane::Heavy).unwrap();
        assert_eq!(json, "\"heavy\"");
    }

    #[test]
    fn test_tool_lane_deserialization() {
        let lane: ToolLane = serde_json::from_str("\"no-net\"").unwrap();
        assert_eq!(lane, ToolLane::NoNet);
        let lane: ToolLane = serde_json::from_str("\"net\"").unwrap();
        assert_eq!(lane, ToolLane::Net);
        let lane: ToolLane = serde_json::from_str("\"heavy\"").unwrap();
        assert_eq!(lane, ToolLane::Heavy);
    }

    #[test]
    fn test_tool_new() {
        let tool = Tool::new("read_file", "Read file contents");
        assert_eq!(tool.name, "read_file");
        assert_eq!(tool.description, "Read file contents");
        assert_eq!(tool.lane, ToolLane::NoNet);
        assert!(tool.timeout_ms.is_none());
        assert!(!tool.requires_worktree);
    }

    #[test]
    fn test_tool_builder() {
        let tool = Tool::new("run_command", "Execute shell command")
            .with_lane(ToolLane::Net)
            .with_timeout(120_000)
            .with_worktree_required()
            .with_max_output(100_000)
            .with_schema(serde_json::json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string" }
                },
                "required": ["command"]
            }));

        assert_eq!(tool.name, "run_command");
        assert_eq!(tool.lane, ToolLane::Net);
        assert_eq!(tool.timeout_ms, Some(120_000));
        assert!(tool.requires_worktree);
        assert_eq!(tool.max_output_bytes, Some(100_000));
        assert!(tool.input_schema["properties"]["command"].is_object());
    }

    #[test]
    fn test_tool_effective_timeout_custom() {
        let tool = Tool::new("test", "test").with_timeout(5000);
        assert_eq!(tool.effective_timeout_ms(), 5000);
    }

    #[test]
    fn test_tool_effective_timeout_default_nonet() {
        let tool = Tool::new("test", "test").with_lane(ToolLane::NoNet);
        assert_eq!(tool.effective_timeout_ms(), 10_000);
    }

    #[test]
    fn test_tool_effective_timeout_default_net() {
        let tool = Tool::new("test", "test").with_lane(ToolLane::Net);
        assert_eq!(tool.effective_timeout_ms(), 30_000);
    }

    #[test]
    fn test_tool_effective_timeout_default_heavy() {
        let tool = Tool::new("test", "test").with_lane(ToolLane::Heavy);
        assert_eq!(tool.effective_timeout_ms(), 600_000);
    }

    #[test]
    fn test_tool_to_llm_definition() {
        let tool = Tool::new("read_file", "Read file contents").with_schema(serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "File path" }
            },
            "required": ["path"]
        }));

        let llm_def = tool.to_llm_definition();
        assert_eq!(llm_def.name, "read_file");
        assert_eq!(llm_def.description, "Read file contents");
        assert!(llm_def.input_schema["properties"]["path"].is_object());
    }

    #[test]
    fn test_tool_serialization() {
        let tool = Tool::new("test", "Test tool")
            .with_lane(ToolLane::Net)
            .with_timeout(5000);

        let json = serde_json::to_string(&tool).unwrap();
        assert!(json.contains("\"name\":\"test\""));
        assert!(json.contains("\"lane\":\"net\""));
        assert!(json.contains("\"timeout_ms\":5000"));
    }

    #[test]
    fn test_tool_deserialization() {
        let json = r#"{
            "name": "write_file",
            "description": "Write content to file",
            "input_schema": {
                "type": "object",
                "properties": {},
                "required": []
            },
            "lane": "no-net",
            "timeout_ms": 10000,
            "requires_worktree": true
        }"#;

        let tool: Tool = serde_json::from_str(json).unwrap();
        assert_eq!(tool.name, "write_file");
        assert_eq!(tool.description, "Write content to file");
        assert_eq!(tool.lane, ToolLane::NoNet);
        assert_eq!(tool.timeout_ms, Some(10000));
        assert!(tool.requires_worktree);
    }

    #[test]
    fn test_tool_deserialization_defaults() {
        let json = r#"{
            "name": "simple",
            "description": "Simple tool",
            "input_schema": {}
        }"#;

        let tool: Tool = serde_json::from_str(json).unwrap();
        assert_eq!(tool.name, "simple");
        assert_eq!(tool.lane, ToolLane::NoNet);
        assert!(tool.timeout_ms.is_none());
        assert!(!tool.requires_worktree);
    }

    #[test]
    fn test_read_file_tool() {
        let tool = Tool::new("read_file", "Read file contents with line numbers")
            .with_lane(ToolLane::NoNet)
            .with_timeout(10_000)
            .with_max_output(100_000)
            .with_worktree_required()
            .with_schema(serde_json::json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "File path relative to worktree" },
                    "offset": { "type": "integer", "description": "Start line (1-indexed)" },
                    "limit": { "type": "integer", "description": "Max lines (default: 2000)" }
                },
                "required": ["path"]
            }));

        let llm_def = tool.to_llm_definition();
        assert_eq!(llm_def.input_schema["required"][0], "path");
    }

    #[test]
    fn test_run_command_tool() {
        let tool = Tool::new("run_command", "Execute shell command")
            .with_lane(ToolLane::Net)
            .with_timeout(120_000)
            .with_max_output(100_000)
            .with_worktree_required()
            .with_schema(serde_json::json!({
                "type": "object",
                "properties": {
                    "command": { "type": "string" },
                    "timeout_ms": { "type": "integer" }
                },
                "required": ["command"]
            }));

        assert!(tool.lane.allows_network());
        assert_eq!(tool.effective_timeout_ms(), 120_000);
    }

    #[test]
    fn test_build_tool() {
        let tool = Tool::new("build", "Run build command")
            .with_lane(ToolLane::Heavy)
            .with_timeout(600_000)
            .with_max_output(1_000_000);

        assert!(tool.lane.allows_network());
        assert_eq!(tool.effective_timeout_ms(), 600_000);
        assert_eq!(tool.max_output_bytes, Some(1_000_000));
    }
}
