//! `submit_decomposition` tool schema plus the `DecomposeChild` /
//! `DecomposeResponse` shapes the LLM's tool-use input deserializes
//! into.
//!
//! The schema shape matches v3's `decomposition_tool_schema` verbatim
//! (v3 `decomposer.rs:197-236`): `children` array with per-item
//! `{title, content, dependencies, acceptance_criteria}`; only `title`
//! and `content` are required per-item.

use serde::Deserialize;
use serde_json::json;

use llm::ToolSchema;

/// The model's per-child item. Field names match the schema. Optional
/// arrays default to empty so the LLM may omit them when a Work has
/// no deps or no pre-extracted AC.
#[derive(Debug, Clone, Deserialize)]
pub(crate) struct DecomposeChild {
    pub title: String,
    #[serde(default)]
    pub content: String,
    #[serde(default)]
    pub dependencies: Vec<String>,
    #[serde(default)]
    pub acceptance_criteria: Vec<String>,
    /// Files this Work is expected to create or modify, relative to
    /// the worktree root. Used as the staging allow-list at commit
    /// time. Empty falls back to artifact-only filtering.
    #[serde(default)]
    pub files: Vec<String>,
}

/// The top-level shape of the tool call's `input` JSON.
#[derive(Debug, Deserialize)]
pub(crate) struct DecomposeResponse {
    pub children: Vec<DecomposeChild>,
}

/// Build the `submit_decomposition` tool schema.
pub(crate) fn submit_decomposition_schema() -> ToolSchema {
    ToolSchema {
        name: "submit_decomposition".to_string(),
        description: "Submit the decomposed child Works. Call this exactly once with all children.".to_string(),
        input_schema: json!({
            "type": "object",
            "properties": {
                "children": {
                    "type": "array",
                    "description": "The decomposed child Works",
                    "items": {
                        "type": "object",
                        "properties": {
                            "title": {
                                "type": "string",
                                "description": "Short descriptive title of the Work"
                            },
                            "content": {
                                "type": "string",
                                "description": "Full markdown content of the Work"
                            },
                            "dependencies": {
                                "type": "array",
                                "items": {"type": "string"},
                                "description": "Titles of sibling Works this one depends on"
                            },
                            "acceptance_criteria": {
                                "type": "array",
                                "items": {"type": "string"},
                                "description": "Concrete, testable assertions for this Work"
                            },
                            "files": {
                                "type": "array",
                                "items": {"type": "string"},
                                "description": "Files this Work will create or modify (paths relative to repo root). The implementer's commit will be restricted to these paths; out-of-scope edits will be flagged in the iteration result."
                            }
                        },
                        "required": ["title", "content"]
                    }
                }
            },
            "required": ["children"]
        }),
    }
}
