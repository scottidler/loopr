//! Tool catalog loading from TOML configuration
//!
//! Loads tool definitions from a TOML file and provides lookup methods.

use std::collections::HashMap;
use std::path::Path;

use serde::Deserialize;
use serde_json::Value;

use crate::error::{LooprError, Result};

use super::definition::{Tool, ToolLane};

/// TOML representation of a tool parameter
#[derive(Debug, Deserialize)]
struct TomlParam {
    #[serde(rename = "type")]
    param_type: String,
    description: Option<String>,
}

/// TOML representation of a tool definition
#[derive(Debug, Deserialize)]
struct TomlTool {
    name: String,
    description: String,
    lane: Option<String>,
    timeout_ms: Option<u64>,
    requires_worktree: Option<bool>,
    max_output_bytes: Option<u64>,
    #[serde(default)]
    params: HashMap<String, TomlParam>,
    #[serde(default)]
    required: Vec<String>,
}

/// TOML file structure
#[derive(Debug, Deserialize)]
struct TomlCatalog {
    #[serde(rename = "tool")]
    tools: Vec<TomlTool>,
}

/// Catalog of tool definitions loaded from TOML
#[derive(Debug, Clone)]
pub struct ToolCatalog {
    tools: HashMap<String, Tool>,
}

impl ToolCatalog {
    /// Create an empty catalog
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
        }
    }

    /// Load catalog from a TOML file
    pub fn from_file(path: impl AsRef<Path>) -> Result<Self> {
        let content = std::fs::read_to_string(path.as_ref()).map_err(|e| {
            LooprError::Storage(format!("Failed to read catalog file: {}", e))
        })?;
        Self::from_toml(&content)
    }

    /// Load catalog from TOML string
    pub fn from_toml(content: &str) -> Result<Self> {
        let catalog: TomlCatalog = toml::from_str(content)
            .map_err(|e| LooprError::Storage(format!("Failed to parse TOML: {}", e)))?;

        let mut tools = HashMap::new();
        for toml_tool in catalog.tools {
            let tool = Self::convert_toml_tool(toml_tool)?;
            tools.insert(tool.name.clone(), tool);
        }

        Ok(Self { tools })
    }

    /// Convert TOML tool to internal Tool struct
    fn convert_toml_tool(toml_tool: TomlTool) -> Result<Tool> {
        // Build JSON schema from params
        let mut properties = serde_json::Map::new();
        for (name, param) in toml_tool.params {
            let mut prop = serde_json::Map::new();
            prop.insert("type".to_string(), Value::String(param.param_type));
            if let Some(desc) = param.description {
                prop.insert("description".to_string(), Value::String(desc));
            }
            properties.insert(name, Value::Object(prop));
        }

        let schema = serde_json::json!({
            "type": "object",
            "properties": properties,
            "required": toml_tool.required
        });

        let lane = toml_tool
            .lane
            .as_deref()
            .map(ToolLane::from_str)
            .unwrap_or(Some(ToolLane::NoNet))
            .ok_or_else(|| {
                LooprError::Storage(format!(
                    "Invalid lane '{}' for tool '{}'",
                    toml_tool.lane.unwrap_or_default(),
                    toml_tool.name
                ))
            })?;

        let mut tool = Tool::new(toml_tool.name, toml_tool.description)
            .with_schema(schema)
            .with_lane(lane);

        if let Some(timeout) = toml_tool.timeout_ms {
            tool = tool.with_timeout(timeout);
        }
        if toml_tool.requires_worktree.unwrap_or(false) {
            tool = tool.with_worktree_required();
        }
        if let Some(max_output) = toml_tool.max_output_bytes {
            tool = tool.with_max_output(max_output);
        }

        Ok(tool)
    }

    /// Get a tool by name
    pub fn get(&self, name: &str) -> Option<&Tool> {
        self.tools.get(name)
    }

    /// List all tool names
    pub fn list(&self) -> Vec<&str> {
        self.tools.keys().map(|s| s.as_str()).collect()
    }

    /// Get the lane for a tool
    pub fn get_lane(&self, name: &str) -> Option<ToolLane> {
        self.tools.get(name).map(|t| t.lane)
    }

    /// Get all tools
    pub fn all(&self) -> impl Iterator<Item = &Tool> {
        self.tools.values()
    }

    /// Get number of tools
    pub fn len(&self) -> usize {
        self.tools.len()
    }

    /// Check if catalog is empty
    pub fn is_empty(&self) -> bool {
        self.tools.is_empty()
    }

    /// Add a tool to the catalog
    pub fn add(&mut self, tool: Tool) {
        self.tools.insert(tool.name.clone(), tool);
    }

    /// Remove a tool from the catalog
    pub fn remove(&mut self, name: &str) -> Option<Tool> {
        self.tools.remove(name)
    }

    /// Get tools filtered by lane
    pub fn by_lane(&self, lane: ToolLane) -> Vec<&Tool> {
        self.tools.values().filter(|t| t.lane == lane).collect()
    }

    /// Check if a tool exists
    pub fn contains(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }
}

impl Default for ToolCatalog {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_TOML: &str = r#"
[[tool]]
name = "read_file"
description = "Read file contents"
lane = "no-net"

[tool.params.path]
type = "string"
description = "File path relative to worktree"

[tool.params.offset]
type = "integer"
description = "Start line (1-indexed)"

[[tool]]
name = "write_file"
description = "Write content to file"
lane = "no-net"
requires_worktree = true

[tool.params.path]
type = "string"
description = "File path"

[tool.params.content]
type = "string"
description = "Content to write"

[[tool]]
name = "bash"
description = "Execute bash command"
lane = "no-net"
timeout_ms = 60000

[tool.params.command]
type = "string"
description = "Command to execute"

[[tool]]
name = "web_fetch"
description = "Fetch URL content"
lane = "net"
timeout_ms = 30000
max_output_bytes = 100000

[tool.params.url]
type = "string"
description = "URL to fetch"
"#;

    #[test]
    fn test_catalog_new_empty() {
        let catalog = ToolCatalog::new();
        assert!(catalog.is_empty());
        assert_eq!(catalog.len(), 0);
    }

    #[test]
    fn test_catalog_from_toml() {
        let catalog = ToolCatalog::from_toml(SAMPLE_TOML).unwrap();
        assert_eq!(catalog.len(), 4);
        assert!(catalog.contains("read_file"));
        assert!(catalog.contains("write_file"));
        assert!(catalog.contains("bash"));
        assert!(catalog.contains("web_fetch"));
    }

    #[test]
    fn test_catalog_get() {
        let catalog = ToolCatalog::from_toml(SAMPLE_TOML).unwrap();

        let tool = catalog.get("read_file").unwrap();
        assert_eq!(tool.name, "read_file");
        assert_eq!(tool.description, "Read file contents");
        assert_eq!(tool.lane, ToolLane::NoNet);
    }

    #[test]
    fn test_catalog_get_nonexistent() {
        let catalog = ToolCatalog::from_toml(SAMPLE_TOML).unwrap();
        assert!(catalog.get("nonexistent").is_none());
    }

    #[test]
    fn test_catalog_list() {
        let catalog = ToolCatalog::from_toml(SAMPLE_TOML).unwrap();
        let tools = catalog.list();
        assert_eq!(tools.len(), 4);
        assert!(tools.contains(&"read_file"));
        assert!(tools.contains(&"write_file"));
        assert!(tools.contains(&"bash"));
        assert!(tools.contains(&"web_fetch"));
    }

    #[test]
    fn test_catalog_get_lane() {
        let catalog = ToolCatalog::from_toml(SAMPLE_TOML).unwrap();

        assert_eq!(catalog.get_lane("read_file"), Some(ToolLane::NoNet));
        assert_eq!(catalog.get_lane("web_fetch"), Some(ToolLane::Net));
        assert_eq!(catalog.get_lane("nonexistent"), None);
    }

    #[test]
    fn test_catalog_by_lane() {
        let catalog = ToolCatalog::from_toml(SAMPLE_TOML).unwrap();

        let nonet_tools = catalog.by_lane(ToolLane::NoNet);
        assert_eq!(nonet_tools.len(), 3);

        let net_tools = catalog.by_lane(ToolLane::Net);
        assert_eq!(net_tools.len(), 1);
        assert_eq!(net_tools[0].name, "web_fetch");

        let heavy_tools = catalog.by_lane(ToolLane::Heavy);
        assert!(heavy_tools.is_empty());
    }

    #[test]
    fn test_catalog_tool_timeout() {
        let catalog = ToolCatalog::from_toml(SAMPLE_TOML).unwrap();

        let bash = catalog.get("bash").unwrap();
        assert_eq!(bash.timeout_ms, Some(60000));
        assert_eq!(bash.effective_timeout_ms(), 60000);

        let read = catalog.get("read_file").unwrap();
        assert!(read.timeout_ms.is_none());
        assert_eq!(read.effective_timeout_ms(), 10_000); // NoNet default
    }

    #[test]
    fn test_catalog_tool_requires_worktree() {
        let catalog = ToolCatalog::from_toml(SAMPLE_TOML).unwrap();

        let write = catalog.get("write_file").unwrap();
        assert!(write.requires_worktree);

        let bash = catalog.get("bash").unwrap();
        assert!(!bash.requires_worktree);
    }

    #[test]
    fn test_catalog_tool_max_output() {
        let catalog = ToolCatalog::from_toml(SAMPLE_TOML).unwrap();

        let fetch = catalog.get("web_fetch").unwrap();
        assert_eq!(fetch.max_output_bytes, Some(100000));

        let read = catalog.get("read_file").unwrap();
        assert!(read.max_output_bytes.is_none());
    }

    #[test]
    fn test_catalog_tool_schema() {
        let catalog = ToolCatalog::from_toml(SAMPLE_TOML).unwrap();

        let tool = catalog.get("read_file").unwrap();
        let schema = &tool.input_schema;

        assert_eq!(schema["type"], "object");
        assert!(schema["properties"]["path"].is_object());
        assert_eq!(schema["properties"]["path"]["type"], "string");
    }

    #[test]
    fn test_catalog_add() {
        let mut catalog = ToolCatalog::new();
        let tool = Tool::new("custom", "Custom tool");
        catalog.add(tool);

        assert!(catalog.contains("custom"));
        assert_eq!(catalog.len(), 1);
    }

    #[test]
    fn test_catalog_remove() {
        let mut catalog = ToolCatalog::from_toml(SAMPLE_TOML).unwrap();
        let removed = catalog.remove("bash");

        assert!(removed.is_some());
        assert_eq!(removed.unwrap().name, "bash");
        assert!(!catalog.contains("bash"));
        assert_eq!(catalog.len(), 3);
    }

    #[test]
    fn test_catalog_all() {
        let catalog = ToolCatalog::from_toml(SAMPLE_TOML).unwrap();
        let all: Vec<_> = catalog.all().collect();
        assert_eq!(all.len(), 4);
    }

    #[test]
    fn test_catalog_invalid_toml() {
        let result = ToolCatalog::from_toml("invalid { toml }");
        assert!(result.is_err());
    }

    #[test]
    fn test_catalog_invalid_lane() {
        let toml = r#"
[[tool]]
name = "bad"
description = "Bad tool"
lane = "invalid_lane"
"#;
        let result = ToolCatalog::from_toml(toml);
        assert!(result.is_err());
    }

    #[test]
    fn test_catalog_default_lane() {
        let toml = r#"
[[tool]]
name = "simple"
description = "Simple tool with default lane"
"#;
        let catalog = ToolCatalog::from_toml(toml).unwrap();
        let tool = catalog.get("simple").unwrap();
        assert_eq!(tool.lane, ToolLane::NoNet);
    }

    #[test]
    fn test_catalog_to_llm_definitions() {
        let catalog = ToolCatalog::from_toml(SAMPLE_TOML).unwrap();

        for tool in catalog.all() {
            let llm_def = tool.to_llm_definition();
            assert!(!llm_def.name.is_empty());
            assert!(!llm_def.description.is_empty());
        }
    }

    #[test]
    fn test_catalog_required_params() {
        let toml = r#"
[[tool]]
name = "test"
description = "Test tool"
required = ["path"]

[tool.params.path]
type = "string"
description = "Required path"

[tool.params.optional]
type = "integer"
description = "Optional int"
"#;
        let catalog = ToolCatalog::from_toml(toml).unwrap();
        let tool = catalog.get("test").unwrap();

        let required = tool.input_schema["required"].as_array().unwrap();
        assert_eq!(required.len(), 1);
        assert_eq!(required[0], "path");
    }

    #[test]
    fn test_catalog_heavy_lane() {
        let toml = r#"
[[tool]]
name = "build"
description = "Run full build"
lane = "heavy"
timeout_ms = 600000
"#;
        let catalog = ToolCatalog::from_toml(toml).unwrap();
        let tool = catalog.get("build").unwrap();

        assert_eq!(tool.lane, ToolLane::Heavy);
        assert!(tool.lane.allows_network());
    }

    #[test]
    fn test_catalog_default() {
        let catalog = ToolCatalog::default();
        assert!(catalog.is_empty());
    }
}
