use serde::{Deserialize, Serialize};

fn default_tool_timeout() -> u64 {
    300
}

fn default_tool_worktree() -> bool {
    true
}

/// Deserialize a JSON value that is either a single string or an array of strings
/// into a Vec<String>. Handles LLM deviations where a string is sent instead of an array.
fn string_or_vec<'de, D>(deserializer: D) -> std::result::Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    use serde::de;

    struct StringOrVecVisitor;

    impl<'de> de::Visitor<'de> for StringOrVecVisitor {
        type Value = Vec<String>;

        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a string or array of strings")
        }

        fn visit_str<E: de::Error>(self, v: &str) -> std::result::Result<Self::Value, E> {
            Ok(vec![v.to_owned()])
        }

        fn visit_seq<A: de::SeqAccess<'de>>(self, seq: A) -> std::result::Result<Self::Value, A::Error> {
            serde::Deserialize::deserialize(de::value::SeqAccessDeserializer::new(seq))
        }

        fn visit_unit<E: de::Error>(self) -> std::result::Result<Self::Value, E> {
            Ok(Vec::new())
        }

        /// LLMs sometimes send `"args": {}` instead of `"args": []` -- treat empty map as empty vec.
        fn visit_map<A: de::MapAccess<'de>>(self, mut map: A) -> std::result::Result<Self::Value, A::Error> {
            // Drain any entries (shouldn't be any for empty {}) and return empty vec
            while map.next_entry::<de::IgnoredAny, de::IgnoredAny>()?.is_some() {}
            Ok(Vec::new())
        }
    }

    deserializer.deserialize_any(StringOrVecVisitor)
}

/// Structured actions that an LLM agent can request.
/// The agent's response is parsed into a sequence of these.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum AgentAction {
    // === Shared actions (all agent types) ===
    RunTool {
        tool: String,
        #[serde(default, deserialize_with = "string_or_vec")]
        args: Vec<String>,
    },
    WriteFile {
        path: String,
        content: String,
    },
    ReadFile {
        path: String,
        #[serde(default)]
        offset: Option<u64>,
        #[serde(default)]
        limit: Option<u64>,
    },
    EditFile {
        path: String,
        old_string: String,
        new_string: String,
    },
    Commit {
        message: String,
        #[serde(default, alias = "files", deserialize_with = "string_or_vec")]
        paths: Vec<String>,
    },
    ProposeBundle {
        #[serde(default, alias = "summary")]
        description: String,
        #[serde(default, deserialize_with = "string_or_vec")]
        claims: Vec<String>,
        #[serde(default)]
        noop_reason: Option<String>,
    },
    Transition {
        collection: String,
        id: String,
        target_status: String,
        /// If None, role is inferred from agent_type via AgentKind::default_role().
        #[serde(default)]
        role: Option<String>,
    },
    CreateLearning {
        content: String,
        scope: String,
        source_id: String,
        /// Roles this learning is relevant to. None = all roles.
        #[serde(default)]
        applicable_roles: Option<Vec<String>>,
        /// Resource tags for scoped selection (file paths, module names).
        #[serde(default)]
        resource_tags: Option<Vec<String>>,
    },
    Done {
        #[serde(default)]
        summary: String,
    },
    NeedHelp {
        reason: String,
    },
    RegisterTool {
        name: String,
        command: String,
        #[serde(default = "default_tool_timeout")]
        timeout_secs: u64,
        #[serde(default = "default_tool_worktree")]
        worktree: bool,
    },

    // === Coordinator-only actions ===
    CreatePlan {
        title: String,
        description: String,
        acceptance_criteria: String,
    },
    CreateSpec {
        parent_id: String,
        title: String,
        description: String,
    },
    CreatePhase {
        parent_id: String,
        title: String,
        description: String,
        order: u32,
    },
    CreateWork {
        parent_id: String,
        title: String,
        description: String,
        #[serde(default, deserialize_with = "string_or_vec")]
        resource_tags: Vec<String>,
        #[serde(default, deserialize_with = "string_or_vec")]
        acceptance_criteria: Vec<String>,
        #[serde(default, deserialize_with = "string_or_vec")]
        dependencies: Vec<String>,
    },
    AssignAgent {
        agent_type: String,
        target_id: String,
    },
    SpawnResearcher {
        query: String,
        scope_id: String,
    },
    ValidateDocument {
        collection: String,
        id: String,
    },
    AcquireLock {
        resource: String,
        holder_id: String,
    },
    ReleaseLock {
        lock_id: String,
    },
    TriageBundle {
        bundle_id: String,
    },
    AcceptBundle {
        bundle_id: String,
    },
    OverrideWork {
        work_id: String,
        target_status: String,
        reason: String,
    },
    EvaluateCoverage {
        parent_collection: String,
        parent_id: String,
    },
    ReviseParent {
        collection: String,
        id: String,
        reason: String,
        diagnostic: String,
    },
    InterviewQuestion {
        #[serde(default, deserialize_with = "string_or_vec")]
        questions: Vec<String>,
    },
    ProposePlan {
        title: String,
        description: String,
        acceptance_criteria: String,
    },

    // === Researcher-only actions ===
    SearchCode {
        pattern: String,
        #[serde(default)]
        glob: Option<String>,
        #[serde(default)]
        path: Option<String>,
    },
    SearchFiles {
        pattern: String,
        #[serde(default)]
        path: Option<String>,
    },
    ListDirectory {
        path: String,
    },
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_agent_action_run_tool_serde() {
        let action = AgentAction::RunTool {
            tool: "test".to_string(),
            args: vec![],
        };
        let json = serde_json::to_string(&action).unwrap();
        let deserialized: AgentAction = serde_json::from_str(&json).unwrap();
        if let AgentAction::RunTool { tool, args } = deserialized {
            assert_eq!(tool, "test");
            assert!(args.is_empty());
        } else {
            panic!("expected RunTool");
        }
    }

    #[test]
    fn test_agent_action_write_file_serde() {
        let action = AgentAction::WriteFile {
            path: "src/main.rs".to_string(),
            content: "fn main() {}".to_string(),
        };
        let json = serde_json::to_string(&action).unwrap();
        let deserialized: AgentAction = serde_json::from_str(&json).unwrap();
        if let AgentAction::WriteFile { path, content } = deserialized {
            assert_eq!(path, "src/main.rs");
            assert_eq!(content, "fn main() {}");
        } else {
            panic!("expected WriteFile");
        }
    }

    #[test]
    fn test_agent_action_done_serde() {
        let action = AgentAction::Done {
            summary: "All tests pass".to_string(),
        };
        let json = serde_json::to_string(&action).unwrap();
        let deserialized: AgentAction = serde_json::from_str(&json).unwrap();
        if let AgentAction::Done { summary } = deserialized {
            assert_eq!(summary, "All tests pass");
        } else {
            panic!("expected Done");
        }
    }

    #[test]
    fn test_agent_action_parse_from_llm_json() {
        let llm_output = r#"[
            {"action": "write_file", "path": "src/foo.rs", "content": "pub fn foo() {}"},
            {"action": "run_tool", "tool": "test", "args": []},
            {"action": "commit", "message": "feat: add foo", "paths": ["src/foo.rs"]},
            {"action": "done", "summary": "Implemented foo"}
        ]"#;
        let actions: Vec<AgentAction> = serde_json::from_str(llm_output).unwrap();
        assert_eq!(actions.len(), 4);
        assert!(matches!(actions[0], AgentAction::WriteFile { .. }));
        assert!(matches!(actions[1], AgentAction::RunTool { .. }));
        assert!(matches!(actions[2], AgentAction::Commit { .. }));
        assert!(matches!(actions[3], AgentAction::Done { .. }));
    }

    #[test]
    fn test_agent_action_need_help_serde() {
        let action = AgentAction::NeedHelp {
            reason: "Ambiguous requirement".to_string(),
        };
        let json = serde_json::to_string(&action).unwrap();
        let deserialized: AgentAction = serde_json::from_str(&json).unwrap();
        if let AgentAction::NeedHelp { reason } = deserialized {
            assert_eq!(reason, "Ambiguous requirement");
        } else {
            panic!("expected NeedHelp");
        }
    }

    #[test]
    fn test_agent_action_propose_bundle_serde() {
        let action = AgentAction::ProposeBundle {
            description: "Add error handling".to_string(),
            claims: vec!["src/error.rs".to_string()],
            noop_reason: None,
        };
        let json = serde_json::to_string(&action).unwrap();
        let deserialized: AgentAction = serde_json::from_str(&json).unwrap();
        if let AgentAction::ProposeBundle {
            description,
            claims,
            noop_reason,
        } = deserialized
        {
            assert_eq!(description, "Add error handling");
            assert_eq!(claims, vec!["src/error.rs"]);
            assert!(noop_reason.is_none());
        } else {
            panic!("expected ProposeBundle");
        }
    }

    #[test]
    fn test_agent_action_create_learning_serde() {
        let action = AgentAction::CreateLearning {
            content: "Parser needs error recovery".to_string(),
            scope: "work".to_string(),
            source_id: "wi-1".to_string(),
            applicable_roles: Some(vec!["implementer".to_string()]),
            resource_tags: Some(vec!["src/parser.rs".to_string()]),
        };
        let json = serde_json::to_string(&action).unwrap();
        let deserialized: AgentAction = serde_json::from_str(&json).unwrap();
        if let AgentAction::CreateLearning {
            content,
            scope,
            source_id,
            applicable_roles,
            resource_tags,
        } = deserialized
        {
            assert_eq!(content, "Parser needs error recovery");
            assert_eq!(scope, "work");
            assert_eq!(source_id, "wi-1");
            assert_eq!(applicable_roles, Some(vec!["implementer".to_string()]));
            assert_eq!(resource_tags, Some(vec!["src/parser.rs".to_string()]));
        } else {
            panic!("expected CreateLearning");
        }
    }

    #[test]
    fn test_agent_action_create_learning_backward_compat() {
        // Old JSON without applicable_roles/resource_tags should deserialize
        let json = r#"{"action":"create_learning","content":"x","scope":"global","source_id":"s1"}"#;
        let action: AgentAction = serde_json::from_str(json).unwrap();
        if let AgentAction::CreateLearning {
            applicable_roles,
            resource_tags,
            ..
        } = action
        {
            assert!(applicable_roles.is_none());
            assert!(resource_tags.is_none());
        } else {
            panic!("expected CreateLearning");
        }
    }

    #[test]
    fn test_agent_action_transition_with_role() {
        let action = AgentAction::Transition {
            collection: "work".to_string(),
            id: "wi-1".to_string(),
            target_status: "in_progress".to_string(),
            role: Some("implementer".to_string()),
        };
        let json = serde_json::to_string(&action).unwrap();
        let deserialized: AgentAction = serde_json::from_str(&json).unwrap();
        if let AgentAction::Transition { role, .. } = deserialized {
            assert_eq!(role, Some("implementer".to_string()));
        } else {
            panic!("expected Transition");
        }
    }

    #[test]
    fn test_agent_action_transition_without_role_backward_compat() {
        let json = r#"{"action":"transition","collection":"work","id":"wi-1","target_status":"done"}"#;
        let action: AgentAction = serde_json::from_str(json).unwrap();
        if let AgentAction::Transition { role, .. } = action {
            assert!(role.is_none());
        } else {
            panic!("expected Transition");
        }
    }

    #[test]
    fn test_agent_action_create_plan_serde() {
        let action = AgentAction::CreatePlan {
            title: "Auth overhaul".to_string(),
            description: "Rewrite auth".to_string(),
            acceptance_criteria: "All tests pass".to_string(),
        };
        let json = serde_json::to_string(&action).unwrap();
        let deserialized: AgentAction = serde_json::from_str(&json).unwrap();
        assert!(matches!(deserialized, AgentAction::CreatePlan { .. }));
    }

    #[test]
    fn test_agent_action_create_spec_serde() {
        let action = AgentAction::CreateSpec {
            parent_id: "p-1".to_string(),
            title: "JWT tokens".to_string(),
            description: "Implement JWT".to_string(),
        };
        let json = serde_json::to_string(&action).unwrap();
        let deserialized: AgentAction = serde_json::from_str(&json).unwrap();
        assert!(matches!(deserialized, AgentAction::CreateSpec { .. }));
    }

    #[test]
    fn test_agent_action_create_phase_serde() {
        let action = AgentAction::CreatePhase {
            parent_id: "s-1".to_string(),
            title: "Phase 1".to_string(),
            description: "Foundation".to_string(),
            order: 1,
        };
        let json = serde_json::to_string(&action).unwrap();
        let deserialized: AgentAction = serde_json::from_str(&json).unwrap();
        assert!(matches!(deserialized, AgentAction::CreatePhase { .. }));
    }

    #[test]
    fn test_agent_action_create_work_serde() {
        let action = AgentAction::CreateWork {
            parent_id: "ph-1".to_string(),
            title: "Add login".to_string(),
            description: "Add login endpoint".to_string(),
            resource_tags: vec!["src/".to_string()],
            acceptance_criteria: vec!["tests pass".to_string()],
            dependencies: vec!["wi-0".to_string()],
        };
        let json = serde_json::to_string(&action).unwrap();
        let deserialized: AgentAction = serde_json::from_str(&json).unwrap();
        assert!(matches!(deserialized, AgentAction::CreateWork { .. }));
    }

    #[test]
    fn test_agent_action_assign_agent_serde() {
        let action = AgentAction::AssignAgent {
            agent_type: "implementer".to_string(),
            target_id: "wi-1".to_string(),
        };
        let json = serde_json::to_string(&action).unwrap();
        let deserialized: AgentAction = serde_json::from_str(&json).unwrap();
        assert!(matches!(deserialized, AgentAction::AssignAgent { .. }));
    }

    #[test]
    fn test_agent_action_spawn_researcher_serde() {
        let action = AgentAction::SpawnResearcher {
            query: "Investigate auth module".to_string(),
            scope_id: "wi-1".to_string(),
        };
        let json = serde_json::to_string(&action).unwrap();
        let deserialized: AgentAction = serde_json::from_str(&json).unwrap();
        assert!(matches!(deserialized, AgentAction::SpawnResearcher { .. }));
    }

    #[test]
    fn test_agent_action_validate_document_serde() {
        let action = AgentAction::ValidateDocument {
            collection: "plan".to_string(),
            id: "p-1".to_string(),
        };
        let json = serde_json::to_string(&action).unwrap();
        let deserialized: AgentAction = serde_json::from_str(&json).unwrap();
        assert!(matches!(deserialized, AgentAction::ValidateDocument { .. }));
    }

    #[test]
    fn test_agent_action_acquire_lock_serde() {
        let action = AgentAction::AcquireLock {
            resource: "src/main.rs".to_string(),
            holder_id: "wi-1".to_string(),
        };
        let json = serde_json::to_string(&action).unwrap();
        let deserialized: AgentAction = serde_json::from_str(&json).unwrap();
        assert!(matches!(deserialized, AgentAction::AcquireLock { .. }));
    }

    #[test]
    fn test_agent_action_release_lock_serde() {
        let action = AgentAction::ReleaseLock {
            lock_id: "lock-1".to_string(),
        };
        let json = serde_json::to_string(&action).unwrap();
        let deserialized: AgentAction = serde_json::from_str(&json).unwrap();
        assert!(matches!(deserialized, AgentAction::ReleaseLock { .. }));
    }

    #[test]
    fn test_agent_action_triage_bundle_serde() {
        let action = AgentAction::TriageBundle {
            bundle_id: "b-1".to_string(),
        };
        let json = serde_json::to_string(&action).unwrap();
        let deserialized: AgentAction = serde_json::from_str(&json).unwrap();
        assert!(matches!(deserialized, AgentAction::TriageBundle { .. }));
    }

    #[test]
    fn test_agent_action_accept_bundle_serde() {
        let action = AgentAction::AcceptBundle {
            bundle_id: "b-1".to_string(),
        };
        let json = serde_json::to_string(&action).unwrap();
        let deserialized: AgentAction = serde_json::from_str(&json).unwrap();
        assert!(matches!(deserialized, AgentAction::AcceptBundle { .. }));
    }

    #[test]
    fn test_agent_action_search_code_serde() {
        let action = AgentAction::SearchCode {
            pattern: "fn main".to_string(),
            glob: Some("*.rs".to_string()),
            path: None,
        };
        let json = serde_json::to_string(&action).unwrap();
        let deserialized: AgentAction = serde_json::from_str(&json).unwrap();
        if let AgentAction::SearchCode { pattern, glob, path } = deserialized {
            assert_eq!(pattern, "fn main");
            assert_eq!(glob, Some("*.rs".to_string()));
            assert!(path.is_none());
        } else {
            panic!("expected SearchCode");
        }
    }

    #[test]
    fn test_agent_action_search_files_serde() {
        let action = AgentAction::SearchFiles {
            pattern: "*.rs".to_string(),
            path: Some("src/".to_string()),
        };
        let json = serde_json::to_string(&action).unwrap();
        let deserialized: AgentAction = serde_json::from_str(&json).unwrap();
        assert!(matches!(deserialized, AgentAction::SearchFiles { .. }));
    }

    #[test]
    fn test_agent_action_list_directory_serde() {
        let action = AgentAction::ListDirectory {
            path: "src/agents".to_string(),
        };
        let json = serde_json::to_string(&action).unwrap();
        let deserialized: AgentAction = serde_json::from_str(&json).unwrap();
        assert!(matches!(deserialized, AgentAction::ListDirectory { .. }));
    }

    #[test]
    fn test_agent_action_override_work_serde() {
        let action = AgentAction::OverrideWork {
            work_id: "wi-123".to_string(),
            target_status: "ready".to_string(),
            reason: "SLA breached: 3/3 attempts, 45min/30min".to_string(),
        };
        let json = serde_json::to_string(&action).unwrap();
        let deserialized: AgentAction = serde_json::from_str(&json).unwrap();
        if let AgentAction::OverrideWork {
            work_id,
            target_status,
            reason,
        } = deserialized
        {
            assert_eq!(work_id, "wi-123");
            assert_eq!(target_status, "ready");
            assert!(reason.contains("SLA breached"));
        } else {
            panic!("expected OverrideWork");
        }
    }

    #[test]
    fn test_agent_action_override_work_parse_from_llm_json() {
        let json = r#"{"action": "override_work", "work_id": "wi-456", "target_status": "abandoned", "reason": "stuck in InProgress for 60min"}"#;
        let action: AgentAction = serde_json::from_str(json).unwrap();
        assert!(matches!(action, AgentAction::OverrideWork { .. }));
    }

    // --- string_or_vec deserialization tests ---

    #[test]
    fn test_string_or_vec_string_input() {
        let json = r#"{"action": "run_tool", "tool": "pytest", "args": "--collect-only"}"#;
        let action: AgentAction = serde_json::from_str(json).unwrap();
        if let AgentAction::RunTool { args, .. } = action {
            assert_eq!(args, vec!["--collect-only"]);
        } else {
            panic!("expected RunTool");
        }
    }

    #[test]
    fn test_string_or_vec_array_input() {
        let json = r#"{"action": "run_tool", "tool": "pytest", "args": ["--collect-only", "-v"]}"#;
        let action: AgentAction = serde_json::from_str(json).unwrap();
        if let AgentAction::RunTool { args, .. } = action {
            assert_eq!(args, vec!["--collect-only", "-v"]);
        } else {
            panic!("expected RunTool");
        }
    }

    #[test]
    fn test_string_or_vec_missing_field() {
        let json = r#"{"action": "run_tool", "tool": "pytest"}"#;
        let action: AgentAction = serde_json::from_str(json).unwrap();
        if let AgentAction::RunTool { args, .. } = action {
            assert!(args.is_empty());
        } else {
            panic!("expected RunTool");
        }
    }

    #[test]
    fn test_string_or_vec_empty_array() {
        let json = r#"{"action": "run_tool", "tool": "pytest", "args": []}"#;
        let action: AgentAction = serde_json::from_str(json).unwrap();
        if let AgentAction::RunTool { args, .. } = action {
            assert!(args.is_empty());
        } else {
            panic!("expected RunTool");
        }
    }

    #[test]
    fn test_string_or_vec_null_input() {
        let json = r#"{"action": "run_tool", "tool": "pytest", "args": null}"#;
        let action: AgentAction = serde_json::from_str(json).unwrap();
        if let AgentAction::RunTool { args, .. } = action {
            assert!(args.is_empty());
        } else {
            panic!("expected RunTool");
        }
    }

    #[test]
    fn test_string_or_vec_empty_object() {
        // LLMs sometimes send "args": {} instead of "args": [] -- should parse as empty vec
        let json = r#"{"action": "run_tool", "tool": "test", "args": {}}"#;
        let action: AgentAction = serde_json::from_str(json).unwrap();
        if let AgentAction::RunTool { args, .. } = action {
            assert!(args.is_empty());
        } else {
            panic!("expected RunTool");
        }
    }

    #[test]
    fn test_string_or_vec_on_commit_paths() {
        let json = r#"{"action": "commit", "message": "fix bug", "paths": "src/main.rs"}"#;
        let action: AgentAction = serde_json::from_str(json).unwrap();
        if let AgentAction::Commit { paths, .. } = action {
            assert_eq!(paths, vec!["src/main.rs"]);
        } else {
            panic!("expected Commit");
        }
    }

    #[test]
    fn test_string_or_vec_on_create_work() {
        let json = r#"{
            "action": "create_work",
            "parent_id": "p1",
            "title": "Test",
            "description": "desc",
            "resource_tags": "src/lib.rs",
            "acceptance_criteria": "it works",
            "dependencies": "wi-001"
        }"#;
        let action: AgentAction = serde_json::from_str(json).unwrap();
        if let AgentAction::CreateWork {
            resource_tags,
            acceptance_criteria,
            dependencies,
            ..
        } = action
        {
            assert_eq!(resource_tags, vec!["src/lib.rs"]);
            assert_eq!(acceptance_criteria, vec!["it works"]);
            assert_eq!(dependencies, vec!["wi-001"]);
        } else {
            panic!("expected CreateWork");
        }
    }
}
