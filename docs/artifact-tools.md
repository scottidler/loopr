# Artifact Tools: Structured Output via tool_use

**Author:** Scott A. Idler
**Date:** 2026-01-31
**Status:** Implementation Spec

---

## Summary

Loopr uses Anthropic's native `tool_use` mechanism for structured artifact creation. Instead of the LLM outputting markdown that gets parsed with regex, the LLM calls artifact tools with structured parameters. This approach is inspired by Claude Code, which uses tool_use exclusively for structured output.

**Key insight:** The artifact (plan.md, spec.md, phase.md) can still be written as human-readable markdown, but the **structured data comes from tool_use parameters**, not parsed from the markdown.

---

## Why tool_use Instead of Markdown Parsing?

| Approach | Pros | Cons |
|----------|------|------|
| **Regex markdown parsing** | Human-readable output | Brittle, variable LLM formatting breaks parsing |
| **JSON blocks in markdown** | Structured data | LLM may forget format, still needs extraction |
| **tool_use** | Guaranteed structure, API-enforced schema | Requires tool definitions |

Claude Code uses tool_use for all structured operations. The API guarantees the response will match the input schema - no regex needed.

---

## Artifact Tool Definitions

### create_plan

Called by PlanLoop to define the plan structure.

```rust
ToolDefinition {
    name: "create_plan",
    description: "Create a plan with specs. Call this to define what specs will be created.",
    input_schema: json!({
        "type": "object",
        "properties": {
            "title": {
                "type": "string",
                "description": "Plan title"
            },
            "overview": {
                "type": "string",
                "description": "High-level description of the plan"
            },
            "specs": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "name": {
                            "type": "string",
                            "description": "Spec identifier (kebab-case, e.g., 'user-auth')"
                        },
                        "title": {
                            "type": "string",
                            "description": "Human-readable spec title"
                        },
                        "description": {
                            "type": "string",
                            "description": "What this spec accomplishes"
                        },
                        "dependencies": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Names of specs this depends on"
                        }
                    },
                    "required": ["name", "title", "description"]
                },
                "minItems": 1,
                "maxItems": 10,
                "description": "Specs that will be created from this plan"
            },
            "non_goals": {
                "type": "array",
                "items": { "type": "string" },
                "description": "What is explicitly out of scope"
            },
            "risks": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Known risks and mitigations"
            }
        },
        "required": ["title", "overview", "specs"]
    }),
}
```

### create_spec

Called by SpecLoop to define the spec structure.

```rust
ToolDefinition {
    name: "create_spec",
    description: "Create a spec with phases. Call this to define what phases will be created.",
    input_schema: json!({
        "type": "object",
        "properties": {
            "name": {
                "type": "string",
                "description": "Spec identifier (matches plan reference)"
            },
            "title": {
                "type": "string",
                "description": "Human-readable spec title"
            },
            "overview": {
                "type": "string",
                "description": "Detailed description of what this spec accomplishes"
            },
            "phases": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "name": {
                            "type": "string",
                            "description": "Phase identifier (kebab-case)"
                        },
                        "title": {
                            "type": "string",
                            "description": "Human-readable phase title"
                        },
                        "description": {
                            "type": "string",
                            "description": "What this phase accomplishes"
                        },
                        "files": {
                            "type": "array",
                            "items": { "type": "string" },
                            "description": "Files this phase will create or modify"
                        },
                        "validation": {
                            "type": "string",
                            "description": "How to verify this phase is complete"
                        }
                    },
                    "required": ["name", "title", "description"]
                },
                "minItems": 3,
                "maxItems": 7,
                "description": "Phases (Rule of Five: 3-7 phases per spec)"
            },
            "acceptance_criteria": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Criteria that must be met for spec completion"
            },
            "test_strategy": {
                "type": "string",
                "description": "How the spec will be tested"
            }
        },
        "required": ["name", "title", "overview", "phases"]
    }),
}
```

### create_phase

Called by PhaseLoop to define the phase details.

```rust
ToolDefinition {
    name: "create_phase",
    description: "Create a phase with implementation details. Call this to define what the CodeLoop will implement.",
    input_schema: json!({
        "type": "object",
        "properties": {
            "name": {
                "type": "string",
                "description": "Phase identifier (matches spec reference)"
            },
            "title": {
                "type": "string",
                "description": "Human-readable phase title"
            },
            "objective": {
                "type": "string",
                "description": "What this phase accomplishes"
            },
            "tasks": {
                "type": "array",
                "items": {
                    "type": "object",
                    "properties": {
                        "description": {
                            "type": "string",
                            "description": "What needs to be done"
                        },
                        "file": {
                            "type": "string",
                            "description": "File to create or modify"
                        },
                        "action": {
                            "type": "string",
                            "enum": ["create", "modify", "delete"],
                            "description": "Type of file operation"
                        }
                    },
                    "required": ["description"]
                },
                "description": "Specific tasks for the CodeLoop"
            },
            "validation_command": {
                "type": "string",
                "description": "Command to run to validate phase completion (e.g., 'cargo test')"
            },
            "success_criteria": {
                "type": "array",
                "items": { "type": "string" },
                "description": "How to know the phase is complete"
            }
        },
        "required": ["name", "title", "objective", "tasks", "validation_command"]
    }),
}
```

---

## How It Works

### 1. LLM Calls Artifact Tool

When PlanLoop asks the LLM to create a plan, the LLM responds with a tool_use block:

```json
{
    "type": "tool_use",
    "id": "toolu_01XYZ",
    "name": "create_plan",
    "input": {
        "title": "Add OAuth Authentication",
        "overview": "Implement OAuth 2.0 authentication with JWT tokens...",
        "specs": [
            {
                "name": "db-schema",
                "title": "Database Schema",
                "description": "Add tables for OAuth tokens and sessions",
                "dependencies": []
            },
            {
                "name": "oauth-endpoints",
                "title": "OAuth API Endpoints",
                "description": "Implement /auth/login, /auth/token, /auth/refresh",
                "dependencies": ["db-schema"]
            }
        ],
        "non_goals": ["Social login (OAuth providers)"],
        "risks": ["Token storage security"]
    }
}
```

### 2. Daemon Handles Tool Call

```rust
impl LoopManager {
    async fn handle_artifact_tool(
        &self,
        loop_id: &str,
        tool_call: &ToolCall,
    ) -> Result<ToolResult> {
        match tool_call.name.as_str() {
            "create_plan" => {
                let plan: PlanArtifact = serde_json::from_value(tool_call.input.clone())?;

                // Store structured data for spawning children later
                self.store_plan_artifact(loop_id, &plan).await?;

                // Also write human-readable markdown
                let markdown = render_plan_markdown(&plan);
                let path = self.artifact_path(loop_id, "plan.md");
                fs::write(&path, &markdown)?;

                Ok(ToolResult {
                    output: format!("Plan created with {} specs", plan.specs.len()),
                    ..Default::default()
                })
            }
            "create_spec" => {
                let spec: SpecArtifact = serde_json::from_value(tool_call.input.clone())?;
                self.store_spec_artifact(loop_id, &spec).await?;

                let markdown = render_spec_markdown(&spec);
                let path = self.artifact_path(loop_id, "spec.md");
                fs::write(&path, &markdown)?;

                Ok(ToolResult {
                    output: format!("Spec created with {} phases", spec.phases.len()),
                    ..Default::default()
                })
            }
            "create_phase" => {
                let phase: PhaseArtifact = serde_json::from_value(tool_call.input.clone())?;
                self.store_phase_artifact(loop_id, &phase).await?;

                let markdown = render_phase_markdown(&phase);
                let path = self.artifact_path(loop_id, "phase.md");
                fs::write(&path, &markdown)?;

                Ok(ToolResult {
                    output: format!("Phase created with {} tasks", phase.tasks.len()),
                    ..Default::default()
                })
            }
            _ => Err(eyre!("Unknown artifact tool: {}", tool_call.name)),
        }
    }
}
```

### 3. Spawning Children Uses Structured Data

When spawning child loops, we use the stored structured data - not parsed markdown:

```rust
impl LoopManager {
    async fn spawn_children_for_plan(&self, plan_id: &str) -> Result<Vec<String>> {
        // Get structured artifact data (NOT parsed from markdown)
        let plan: PlanArtifact = self.get_plan_artifact(plan_id).await?;

        let mut child_ids = Vec::new();
        for spec in &plan.specs {
            let child = Loop {
                id: generate_loop_id(),
                loop_type: LoopType::Spec,
                parent_id: Some(plan_id.to_string()),
                context: json!({
                    "spec_name": spec.name,
                    "spec_title": spec.title,
                    "spec_description": spec.description,
                    "dependencies": spec.dependencies,
                }),
                ..Default::default()
            };
            self.store.create(&child)?;
            child_ids.push(child.id);
        }

        Ok(child_ids)
    }
}
```

---

## Artifact Storage

Artifacts are stored in two forms:

### 1. Structured (for machine use)

```
~/.loopr/<project>/loops/<loop-id>/
└── artifacts/
    └── plan.json    # Structured data from tool_use
```

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanArtifact {
    pub title: String,
    pub overview: String,
    pub specs: Vec<SpecReference>,
    pub non_goals: Vec<String>,
    pub risks: Vec<String>,
    pub created_at: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SpecReference {
    pub name: String,
    pub title: String,
    pub description: String,
    pub dependencies: Vec<String>,
}
```

### 2. Markdown (for human use)

```
~/.loopr/<project>/loops/<loop-id>/
└── artifacts/
    └── plan.md      # Human-readable rendering
```

The markdown is generated from the structured data:

```rust
fn render_plan_markdown(plan: &PlanArtifact) -> String {
    let mut md = String::new();

    writeln!(md, "# {}", plan.title);
    writeln!(md);
    writeln!(md, "## Overview");
    writeln!(md, "{}", plan.overview);
    writeln!(md);
    writeln!(md, "## Specs");

    for (i, spec) in plan.specs.iter().enumerate() {
        writeln!(md, "### {}. {}", i + 1, spec.title);
        writeln!(md, "**Name:** `{}`", spec.name);
        writeln!(md, "{}", spec.description);
        if !spec.dependencies.is_empty() {
            writeln!(md, "**Dependencies:** {}", spec.dependencies.join(", "));
        }
        writeln!(md);
    }

    if !plan.non_goals.is_empty() {
        writeln!(md, "## Non-Goals");
        for ng in &plan.non_goals {
            writeln!(md, "- {}", ng);
        }
    }

    if !plan.risks.is_empty() {
        writeln!(md, "## Risks");
        for risk in &plan.risks {
            writeln!(md, "- {}", risk);
        }
    }

    md
}
```

---

## Prompt Instructions

Loop prompts must instruct the LLM to use the artifact tools:

```markdown
## Creating Your Output

You MUST use the `create_plan` tool to define your plan structure.
Do NOT write a plan.md file directly - use the tool instead.

The tool will:
1. Validate your plan structure
2. Store the structured data for spawning child loops
3. Generate a human-readable plan.md automatically

Example:
```
tool_use: create_plan
input:
  title: "Add Feature X"
  overview: "Description..."
  specs:
    - name: "component-a"
      title: "Component A"
      description: "..."
```
```

---

## Validation

Artifact tools can enforce constraints:

```rust
impl ToolCatalog {
    fn validate_artifact(&self, tool_name: &str, input: &Value) -> Result<()> {
        match tool_name {
            "create_plan" => {
                let plan: PlanArtifact = serde_json::from_value(input.clone())?;

                // Check spec count
                if plan.specs.is_empty() {
                    return Err(eyre!("Plan must have at least one spec"));
                }
                if plan.specs.len() > 10 {
                    return Err(eyre!("Plan has too many specs (max 10)"));
                }

                // Check for duplicate names
                let names: HashSet<_> = plan.specs.iter().map(|s| &s.name).collect();
                if names.len() != plan.specs.len() {
                    return Err(eyre!("Duplicate spec names"));
                }

                // Validate dependencies reference existing specs
                for spec in &plan.specs {
                    for dep in &spec.dependencies {
                        if !names.contains(dep) {
                            return Err(eyre!("Unknown dependency: {}", dep));
                        }
                    }
                }

                Ok(())
            }
            "create_spec" => {
                let spec: SpecArtifact = serde_json::from_value(input.clone())?;

                // Rule of Five: 3-7 phases
                if spec.phases.len() < 3 {
                    return Err(eyre!("Spec needs at least 3 phases (Rule of Five)"));
                }
                if spec.phases.len() > 7 {
                    return Err(eyre!("Spec has too many phases (max 7, Rule of Five)"));
                }

                Ok(())
            }
            _ => Ok(()),
        }
    }
}
```

---

## Benefits

1. **No regex parsing** - API guarantees structure matches schema
2. **Validation at creation** - Catch errors before spawning children
3. **Human-readable output** - Markdown still generated for user review
4. **Type-safe** - Rust structs from serde_json, not string manipulation
5. **Consistent** - Same pattern as Claude Code uses

---

## References

- [tools.md](tools.md) - Tool system overview
- [loop-architecture.md](loop-architecture.md) - Loop hierarchy
- [rule-of-five.md](rule-of-five.md) - Phase count constraints
