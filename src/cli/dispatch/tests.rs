use super::*;

#[test]
fn test_plan_create_mapping() {
    let cmd = Command::Plan {
        cmd: CrudCmd::Create {
            title: "My Plan".to_string(),

            parent: None,

            files: vec![],
            acceptance_criteria: vec![],
            dependencies: vec![],
        },
    };
    let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
    assert_eq!(method, "plan.create");
    assert_eq!(params["title"], "My Plan");
}

#[test]
fn test_spec_create_mapping_with_parent() {
    let cmd = Command::Spec {
        cmd: CrudCmd::Create {
            title: "My Spec".to_string(),

            parent: Some("plan-1".to_string()),

            files: vec![],
            acceptance_criteria: vec![],
            dependencies: vec![],
        },
    };
    let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
    assert_eq!(method, "spec.create");
    assert_eq!(params["parent_id"], "plan-1");
}

#[test]
fn test_phase_create_mapping_with_parent() {
    let cmd = Command::Phase {
        cmd: CrudCmd::Create {
            title: "Phase 1".to_string(),
            parent: Some("spec-1".to_string()),
            files: vec![],
            acceptance_criteria: vec![],
            dependencies: vec![],
        },
    };
    let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
    assert_eq!(method, "phase.create");
    assert_eq!(params["parent_id"], "spec-1");
}

#[test]
fn test_work_transition_mapping() {
    let cmd = Command::Work {
        cmd: CrudCmd::Transition {
            id: "wi-1".to_string(),
            status: "Ready".to_string(),
            skip_validation: false,
        },
    };
    let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
    assert_eq!(method, "work.transition");
    assert_eq!(params["id"], "wi-1");
    assert_eq!(params["target_status"], "Ready");
    assert_eq!(params["role"], "coordinator");
}

#[test]
fn test_bundle_create_mapping() {
    let cmd = Command::Bundle {
        cmd: BundleCmd::Create {
            work_id: "wi-1".to_string(),
            branch: "feature/foo".to_string(),
            description: "A bundle".to_string(),
            base_tick_id: Some("tick-1".to_string()),
            claims: vec![],
            paths: vec![],
        },
    };
    let (method, params) = command_to_ipc(&cmd, Role::Implementer);
    assert_eq!(method, "bundle.create");
    assert_eq!(params["work_id"], "wi-1");
    assert_eq!(params["branch_name"], "feature/foo");
    assert_eq!(params["base_tick_id"], "tick-1");
}

#[test]
fn test_tick_validate_mapping() {
    let cmd = Command::Tick {
        cmd: TickCmd::Validate { id: "t-1".to_string() },
    };
    let (method, params) = command_to_ipc(&cmd, Role::Integrator);
    assert_eq!(method, "integrator.validate");
    assert_eq!(params["tick_id"], "t-1");
}

#[test]
fn test_tick_publish_mapping() {
    let cmd = Command::Tick {
        cmd: TickCmd::Publish { id: "t-1".to_string() },
    };
    let (method, params) = command_to_ipc(&cmd, Role::Integrator);
    assert_eq!(method, "integrator.publish");
    assert_eq!(params["tick_id"], "t-1");
}

#[test]
fn test_worktree_create_mapping() {
    let cmd = Command::Worktree {
        cmd: WorktreeCmd::Create {
            work_id: "wi-1".to_string(),
            git_ref: "main".to_string(),
        },
    };
    let (method, params) = command_to_ipc(&cmd, Role::Implementer);
    assert_eq!(method, "worktree.create");
    assert_eq!(params["work_id"], "wi-1");
    assert_eq!(params["ref"], "main");
}

#[test]
fn test_worktree_list_mapping() {
    let cmd = Command::Worktree { cmd: WorktreeCmd::List };
    let (method, _params) = command_to_ipc(&cmd, Role::Implementer);
    assert_eq!(method, "worktree.list");
}

#[test]
fn test_learning_reinforce_mapping() {
    let cmd = Command::Learning {
        cmd: LearningCmd::Reinforce { id: "l-1".to_string() },
    };
    let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
    assert_eq!(method, "learning.reinforce");
    assert_eq!(params["id"], "l-1");
}

#[test]
fn test_lock_list_with_filters() {
    let cmd = Command::Lock {
        cmd: LockCmd::List {
            resource: Some("src/main.rs".to_string()),
            holder_id: None,
            active_only: true,
        },
    };
    let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
    assert_eq!(method, "lock.list");
    assert_eq!(params["resource"], "src/main.rs");
    assert_eq!(params["active_only"], true);
}

#[test]
fn test_status_mapping() {
    let cmd = Command::Status;
    let (method, _) = command_to_ipc(&cmd, Role::Coordinator);
    assert_eq!(method, "system.status");
}

#[test]
fn test_shutdown_mapping() {
    let cmd = Command::Shutdown;
    let (method, _) = command_to_ipc(&cmd, Role::Coordinator);
    assert_eq!(method, "system.shutdown");
}

#[test]
fn test_get_and_list_mappings() {
    let cmd = Command::Plan {
        cmd: CrudCmd::Get { id: "p-1".to_string() },
    };
    let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
    assert_eq!(method, "plan.get");
    assert_eq!(params["id"], "p-1");

    let cmd = Command::Plan {
        cmd: CrudCmd::List { parent: None },
    };
    let (method, _) = command_to_ipc(&cmd, Role::Coordinator);
    assert_eq!(method, "plan.list");
}

#[test]
fn test_transition_role_is_serde_compatible() {
    // Regression: dispatch must emit role values that handlers can deserialize.
    use crate::domain::role::Role;
    for role in [Role::Coordinator, Role::Integrator, Role::Implementer] {
        let cmd = Command::Plan {
            cmd: CrudCmd::Transition {
                id: "p-1".to_string(),
                status: "active".to_string(),
                skip_validation: false,
            },
        };
        let (_, params) = command_to_ipc(&cmd, role);
        let role_str = params["role"].as_str().unwrap();
        let quoted = format!("\"{}\"", role_str);
        let parsed: Role =
            serde_json::from_str(&quoted).unwrap_or_else(|e| panic!("role '{}' not deserializable: {}", role_str, e));
        assert_eq!(role, parsed);
    }
}

#[test]
fn test_plan_transition_normalizes_status_to_lowercase() {
    // Regression: plan/spec/phase use serde(rename_all = "lowercase"),
    // so dispatch must lowercase the user-provided status string.
    for input in ["Active", "ACTIVE", "active", "AcTiVe"] {
        let cmd = Command::Plan {
            cmd: CrudCmd::Transition {
                id: "p-1".to_string(),
                status: input.to_string(),
                skip_validation: false,
            },
        };
        let (_, params) = command_to_ipc(&cmd, Role::Coordinator);
        assert_eq!(
            params["target_status"], "active",
            "input '{}' should normalize to 'active'",
            input
        );
    }
}

#[test]
fn test_spec_transition_normalizes_status_to_lowercase() {
    let cmd = Command::Spec {
        cmd: CrudCmd::Transition {
            id: "s-1".to_string(),
            status: "Active".to_string(),
            skip_validation: false,
        },
    };
    let (_, params) = command_to_ipc(&cmd, Role::Coordinator);
    assert_eq!(params["target_status"], "active");
}

#[test]
fn test_phase_transition_normalizes_status_to_lowercase() {
    let cmd = Command::Phase {
        cmd: CrudCmd::Transition {
            id: "ph-1".to_string(),
            status: "Complete".to_string(),
            skip_validation: false,
        },
    };
    let (_, params) = command_to_ipc(&cmd, Role::Coordinator);
    assert_eq!(params["target_status"], "complete");
}

#[test]
fn test_work_transition_preserves_status_casing() {
    // WorkStatus uses default serde (PascalCase), so dispatch must NOT lowercase.
    let cmd = Command::Work {
        cmd: CrudCmd::Transition {
            id: "wi-1".to_string(),
            status: "InProgress".to_string(),
            skip_validation: false,
        },
    };
    let (_, params) = command_to_ipc(&cmd, Role::Coordinator);
    assert_eq!(params["target_status"], "InProgress");
}

#[test]
fn test_bundle_transition_role_is_serde_compatible() {
    for role in [Role::Coordinator, Role::Integrator, Role::Implementer] {
        let cmd = Command::Bundle {
            cmd: BundleCmd::Transition {
                id: "b-1".to_string(),
                status: "Triaged".to_string(),
            },
        };
        let (_, params) = command_to_ipc(&cmd, role);
        let role_str = params["role"].as_str().unwrap();
        let quoted = format!("\"{}\"", role_str);
        let parsed: Role =
            serde_json::from_str(&quoted).unwrap_or_else(|e| panic!("role '{}' not deserializable: {}", role_str, e));
        assert_eq!(role, parsed);
    }
}

#[test]
fn test_tick_transition_role_is_serde_compatible() {
    let cmd = Command::Tick {
        cmd: TickCmd::Transition {
            id: "t-1".to_string(),
            status: "Sealing".to_string(),
        },
    };
    let (_, params) = command_to_ipc(&cmd, Role::Integrator);
    let role_str = params["role"].as_str().unwrap();
    let quoted = format!("\"{}\"", role_str);
    let parsed: Role =
        serde_json::from_str(&quoted).unwrap_or_else(|e| panic!("role '{}' not deserializable: {}", role_str, e));
    assert_eq!(Role::Integrator, parsed);
}

#[test]
fn test_init_mapping() {
    let cmd = Command::Init;
    let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
    assert_eq!(method, "system.init");
    assert_eq!(params, json!({}));
}

#[test]
fn test_validate_mapping() {
    let cmd = Command::Validate {
        collection: "plan".to_string(),
        id: "plan-1".to_string(),
    };
    let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
    assert_eq!(method, "validator.validate");
    assert_eq!(params["collection"], "plan");
    assert_eq!(params["id"], "plan-1");
}

#[test]
fn test_report_mapping() {
    let cmd = Command::Report { id: "vr-1".to_string() };
    let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
    assert_eq!(method, "validator.report");
    assert_eq!(params["id"], "vr-1");
}

#[test]
fn test_reports_mapping() {
    let cmd = Command::Reports {
        collection: "plans".to_string(),
        target_id: "plan-1".to_string(),
    };
    let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
    assert_eq!(method, "validator.reports");
    assert_eq!(params["collection"], "plans");
    assert_eq!(params["target_id"], "plan-1");
}

#[test]
fn test_agent_start_implementer_mapping() {
    let cmd = Command::Agent {
        cmd: AgentCmd::StartImplementer {
            work_id: "wi-1".to_string(),
        },
    };
    let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
    assert_eq!(method, "agent.start");
    assert_eq!(params["agent-type"], "implementer");
    assert_eq!(params["work-id"], "wi-1");
}

#[test]
fn test_agent_start_reviewer_mapping() {
    let cmd = Command::Agent {
        cmd: AgentCmd::StartReviewer {
            bundle_id: "b-1".to_string(),
        },
    };
    let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
    assert_eq!(method, "agent.start");
    assert_eq!(params["agent-type"], "reviewer");
    assert_eq!(params["bundle-id"], "b-1");
}

#[test]
fn test_agent_stop_mapping() {
    let cmd = Command::Agent {
        cmd: AgentCmd::Stop {
            session_id: "sess-1".to_string(),
        },
    };
    let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
    assert_eq!(method, "agent.stop");
    assert_eq!(params["session_id"], "sess-1");
}

#[test]
fn test_agent_pause_mapping() {
    let cmd = Command::Agent {
        cmd: AgentCmd::Pause {
            session_id: "sess-1".to_string(),
        },
    };
    let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
    assert_eq!(method, "agent.pause");
    assert_eq!(params["session_id"], "sess-1");
}

#[test]
fn test_agent_resume_mapping() {
    let cmd = Command::Agent {
        cmd: AgentCmd::Resume {
            session_id: "sess-1".to_string(),
        },
    };
    let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
    assert_eq!(method, "agent.resume");
    assert_eq!(params["session_id"], "sess-1");
}

#[test]
fn test_agent_status_mapping() {
    let cmd = Command::Agent {
        cmd: AgentCmd::Status {
            session_id: "sess-1".to_string(),
        },
    };
    let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
    assert_eq!(method, "agent.status");
    assert_eq!(params["session_id"], "sess-1");
}

#[test]
fn test_agent_list_mapping() {
    let cmd = Command::Agent {
        cmd: AgentCmd::List {
            status: None,
            agent_type: None,
        },
    };
    let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
    assert_eq!(method, "agent.list");
    assert_eq!(params, json!({}));
}

#[test]
fn test_agent_list_with_filters_mapping() {
    let cmd = Command::Agent {
        cmd: AgentCmd::List {
            status: Some("running".to_string()),
            agent_type: Some("implementer".to_string()),
        },
    };
    let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
    assert_eq!(method, "agent.list");
    assert_eq!(params["status"], "running");
    assert_eq!(params["agent_type"], "implementer");
}

#[test]
fn test_agent_start_coordinator_mapping() {
    let cmd = Command::Agent {
        cmd: AgentCmd::StartCoordinator,
    };
    let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
    assert_eq!(method, "agent.start");
    assert_eq!(params["agent-type"], "coordinator");
}

#[test]
fn test_agent_start_researcher_mapping() {
    let cmd = Command::Agent {
        cmd: AgentCmd::StartResearcher {
            query: "How does auth work?".to_string(),
            target_id: Some("wi-1".to_string()),
        },
    };
    let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
    assert_eq!(method, "agent.start");
    assert_eq!(params["agent-type"], "researcher");
    assert_eq!(params["query"], "How does auth work?");
    assert_eq!(params["target-id"], "wi-1");
}

#[test]
fn test_agent_start_researcher_no_target() {
    let cmd = Command::Agent {
        cmd: AgentCmd::StartResearcher {
            query: "test".to_string(),
            target_id: None,
        },
    };
    let (_, params) = command_to_ipc(&cmd, Role::Coordinator);
    assert!(params["target-id"].is_null());
}

#[test]
fn test_coordinator_get_goal_mapping() {
    let cmd = Command::Coordinator {
        cmd: CoordinatorCmd::Status,
    };
    let (method, _) = command_to_ipc(&cmd, Role::Coordinator);
    assert_eq!(method, "coordinator.get_goal");
}

#[test]
fn test_crud_spec_with_parent() {
    let cmd = Command::Spec {
        cmd: CrudCmd::Create {
            title: "Auth Spec".to_string(),
            parent: Some("plan-42".to_string()),
            files: vec![],
            acceptance_criteria: vec![],
            dependencies: vec![],
        },
    };
    let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
    assert_eq!(method, "spec.create");
    assert_eq!(params["title"], "Auth Spec");
    assert_eq!(params["parent_id"], "plan-42");
}

#[test]
fn test_tick_list_with_status_filter() {
    let cmd = Command::Tick {
        cmd: TickCmd::List {
            status: Some("Published".to_string()),
        },
    };
    let (method, params) = command_to_ipc(&cmd, Role::Integrator);
    assert_eq!(method, "tick.list");
    assert_eq!(params["status"], "Published");
}

#[test]
fn test_lock_list_multiple_filters() {
    // All three filters set: resource, holder_id, and active_only
    let cmd = Command::Lock {
        cmd: LockCmd::List {
            resource: Some("src/lib.rs".to_string()),
            holder_id: Some("wi-7".to_string()),
            active_only: true,
        },
    };
    let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
    assert_eq!(method, "lock.list");
    assert_eq!(params["resource"], "src/lib.rs");
    assert_eq!(params["holder_id"], "wi-7");
    assert_eq!(params["active_only"], true);
}

#[test]
fn test_bundle_create_with_base_tick_id() {
    // Bundle create without base_tick_id — key should be absent
    let cmd = Command::Bundle {
        cmd: BundleCmd::Create {
            work_id: "wi-5".to_string(),
            branch: "feat/bar".to_string(),
            description: "No tick".to_string(),
            base_tick_id: None,
            claims: vec![],
            paths: vec![],
        },
    };
    let (method, params) = command_to_ipc(&cmd, Role::Implementer);
    assert_eq!(method, "bundle.create");
    assert_eq!(params["work_id"], "wi-5");
    assert_eq!(params["branch_name"], "feat/bar");
    assert_eq!(params["description"], "No tick");
    assert!(params.get("base_tick_id").is_none() || params["base_tick_id"].is_null());
}

#[test]
fn test_worktree_cleanup_mapping() {
    let cmd = Command::Worktree {
        cmd: WorktreeCmd::Cleanup {
            work_id: "wi-cleanup".to_string(),
        },
    };
    let (method, params) = command_to_ipc(&cmd, Role::Implementer);
    assert_eq!(method, "worktree.cleanup");
    assert_eq!(params["work_id"], "wi-cleanup");
}

#[test]
fn test_worktree_refresh_mapping() {
    let cmd = Command::Worktree {
        cmd: WorktreeCmd::Refresh {
            work_id: "wi-refresh".to_string(),
            git_ref: "main".to_string(),
        },
    };
    let (method, params) = command_to_ipc(&cmd, Role::Implementer);
    assert_eq!(method, "worktree.refresh");
    assert_eq!(params["work_id"], "wi-refresh");
    assert_eq!(params["ref"], "main");
}

#[test]
fn test_agent_start_with_all_optional_params() {
    // StartResearcher with target_id set — all optional params populated
    let cmd = Command::Agent {
        cmd: AgentCmd::StartResearcher {
            query: "What API patterns exist?".to_string(),
            target_id: Some("spec-9".to_string()),
        },
    };
    let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
    assert_eq!(method, "agent.start");
    assert_eq!(params["agent-type"], "researcher");
    assert_eq!(params["query"], "What API patterns exist?");
    assert_eq!(params["target-id"], "spec-9");

    // Also test integrator start (no optional params — different path)
    let cmd2 = Command::Agent {
        cmd: AgentCmd::StartIntegrator,
    };
    let (method2, params2) = command_to_ipc(&cmd2, Role::Integrator);
    assert_eq!(method2, "agent.start");
    assert_eq!(params2["agent-type"], "integrator");
}

// --- Coverage gap tests for uncovered branches ---

#[test]
fn test_work_create_with_parent_uses_parent_id() {
    let cmd = Command::Work {
        cmd: CrudCmd::Create {
            title: "Implement auth".to_string(),
            parent: Some("phase-1".to_string()),

            files: vec!["src/".to_string()],
            acceptance_criteria: vec!["tests pass".to_string()],
            dependencies: vec![],
        },
    };
    let (method, params) = command_to_ipc(&cmd, Role::Implementer);
    assert_eq!(method, "work.create");
    assert_eq!(params["parent_id"], "phase-1");
    assert_eq!(params["files"], json!(["src/"]));
    assert_eq!(params["acceptance_criteria"], json!(["tests pass"]));
}

#[test]
fn test_work_create_skips_work_fields_for_non_work() {
    // files should NOT appear in plan.create params
    let cmd = Command::Plan {
        cmd: CrudCmd::Create {
            title: "Plan".to_string(),

            parent: None,

            files: vec!["should-be-ignored".to_string()],
            acceptance_criteria: vec![],
            dependencies: vec![],
        },
    };
    let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
    assert_eq!(method, "plan.create");
    assert!(params.get("files").is_none());
}

#[test]
fn test_bundle_create_with_claims_and_paths() {
    let cmd = Command::Bundle {
        cmd: BundleCmd::Create {
            work_id: "wi-1".to_string(),
            branch: "feature/auth".to_string(),
            description: "".to_string(),
            base_tick_id: None,
            claims: vec!["Add JWT".to_string()],
            paths: vec!["src/auth.rs".to_string()],
        },
    };
    let (method, params) = command_to_ipc(&cmd, Role::Implementer);
    assert_eq!(method, "bundle.create");
    assert_eq!(params["claims"], json!(["Add JWT"]));
    assert_eq!(params["paths"], json!(["src/auth.rs"]));
}

#[test]
fn test_spec_list_with_parent_uses_parent_id() {
    let cmd = Command::Spec {
        cmd: CrudCmd::List {
            parent: Some("plan-1".to_string()),
        },
    };
    let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
    assert_eq!(method, "spec.list");
    assert_eq!(params["parent_id"], "plan-1");
}

#[test]
fn test_phase_list_with_parent_uses_parent_id() {
    let cmd = Command::Phase {
        cmd: CrudCmd::List {
            parent: Some("spec-1".to_string()),
        },
    };
    let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
    assert_eq!(method, "phase.list");
    assert_eq!(params["parent_id"], "spec-1");
}

#[test]
fn test_work_list_with_parent_uses_parent_id() {
    let cmd = Command::Work {
        cmd: CrudCmd::List {
            parent: Some("phase-1".to_string()),
        },
    };
    let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
    assert_eq!(method, "work.list");
    assert_eq!(params["parent_id"], "phase-1");
}

#[test]
fn test_transition_skip_validation_flag() {
    let cmd = Command::Plan {
        cmd: CrudCmd::Transition {
            id: "p-1".to_string(),
            status: "active".to_string(),
            skip_validation: true,
        },
    };
    let (_, params) = command_to_ipc(&cmd, Role::Coordinator);
    assert_eq!(params["skip_validation"], true);
}

#[test]
fn test_bundle_get_mapping() {
    let cmd = Command::Bundle {
        cmd: BundleCmd::Get { id: "b-1".to_string() },
    };
    let (method, params) = command_to_ipc(&cmd, Role::Implementer);
    assert_eq!(method, "bundle.get");
    assert_eq!(params["id"], "b-1");
}

#[test]
fn test_bundle_list_with_work_filter() {
    let cmd = Command::Bundle {
        cmd: BundleCmd::List {
            work_id: Some("wi-1".to_string()),
        },
    };
    let (method, params) = command_to_ipc(&cmd, Role::Implementer);
    assert_eq!(method, "bundle.list");
    assert_eq!(params["work_id"], "wi-1");
}

#[test]
fn test_bundle_list_no_filter() {
    let cmd = Command::Bundle {
        cmd: BundleCmd::List { work_id: None },
    };
    let (method, params) = command_to_ipc(&cmd, Role::Implementer);
    assert_eq!(method, "bundle.list");
    assert_eq!(params, json!({}));
}

#[test]
fn test_tick_create_mapping() {
    let cmd = Command::Tick {
        cmd: TickCmd::Create { number: 42 },
    };
    let (method, params) = command_to_ipc(&cmd, Role::Integrator);
    assert_eq!(method, "tick.create");
    assert_eq!(params["number"], 42);
}

#[test]
fn test_tick_get_mapping() {
    let cmd = Command::Tick {
        cmd: TickCmd::Get { id: "t-5".to_string() },
    };
    let (method, params) = command_to_ipc(&cmd, Role::Integrator);
    assert_eq!(method, "tick.get");
    assert_eq!(params["id"], "t-5");
}

#[test]
fn test_learning_create_mapping() {
    let cmd = Command::Learning {
        cmd: LearningCmd::Create {
            source_id: "wi-1".to_string(),
            scope: "global".to_string(),
            content: "Always use snake_case".to_string(),
        },
    };
    let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
    assert_eq!(method, "learning.create");
    assert_eq!(params["source_id"], "wi-1");
    assert_eq!(params["scope"], "global");
    assert_eq!(params["content"], "Always use snake_case");
}

#[test]
fn test_learning_get_mapping() {
    let cmd = Command::Learning {
        cmd: LearningCmd::Get { id: "l-1".to_string() },
    };
    let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
    assert_eq!(method, "learning.get");
    assert_eq!(params["id"], "l-1");
}

#[test]
fn test_learning_list_mapping() {
    let cmd = Command::Learning { cmd: LearningCmd::List };
    let (method, _) = command_to_ipc(&cmd, Role::Coordinator);
    assert_eq!(method, "learning.list");
}

#[test]
fn test_learning_contradict_mapping() {
    let cmd = Command::Learning {
        cmd: LearningCmd::Contradict { id: "l-1".to_string() },
    };
    let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
    assert_eq!(method, "learning.contradict");
    assert_eq!(params["id"], "l-1");
}

#[test]
fn test_learning_promote_mapping() {
    let cmd = Command::Learning {
        cmd: LearningCmd::Promote { id: "l-1".to_string() },
    };
    let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
    assert_eq!(method, "learning.promote");
    assert_eq!(params["id"], "l-1");
}

#[test]
fn test_learning_demote_mapping() {
    let cmd = Command::Learning {
        cmd: LearningCmd::Demote { id: "l-1".to_string() },
    };
    let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
    assert_eq!(method, "learning.demote");
    assert_eq!(params["id"], "l-1");
}

#[test]
fn test_lock_create_mapping() {
    let cmd = Command::Lock {
        cmd: LockCmd::Create {
            resource: "src/main.rs".to_string(),
            holder_id: "wi-1".to_string(),
            granted_by: "coordinator".to_string(),
        },
    };
    let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
    assert_eq!(method, "lock.create");
    assert_eq!(params["resource"], "src/main.rs");
    assert_eq!(params["holder_id"], "wi-1");
    assert_eq!(params["granted_by"], "coordinator");
}

#[test]
fn test_lock_get_mapping() {
    let cmd = Command::Lock {
        cmd: LockCmd::Get { id: "lk-1".to_string() },
    };
    let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
    assert_eq!(method, "lock.get");
    assert_eq!(params["id"], "lk-1");
}

#[test]
fn test_lock_release_mapping() {
    let cmd = Command::Lock {
        cmd: LockCmd::Release { id: "lk-1".to_string() },
    };
    let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
    assert_eq!(method, "lock.release");
    assert_eq!(params["id"], "lk-1");
}

#[test]
fn test_lock_expire_mapping() {
    let cmd = Command::Lock {
        cmd: LockCmd::Expire { id: "lk-1".to_string() },
    };
    let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
    assert_eq!(method, "lock.expire");
    assert_eq!(params["id"], "lk-1");
}

#[test]
fn test_tick_list_no_filter() {
    let cmd = Command::Tick {
        cmd: TickCmd::List { status: None },
    };
    let (method, params) = command_to_ipc(&cmd, Role::Integrator);
    assert_eq!(method, "tick.list");
    assert_eq!(params, json!({}));
}

#[test]
fn test_lock_list_holder_id_only() {
    let cmd = Command::Lock {
        cmd: LockCmd::List {
            resource: None,
            holder_id: Some("wi-3".to_string()),
            active_only: false,
        },
    };
    let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
    assert_eq!(method, "lock.list");
    assert_eq!(params["holder_id"], "wi-3");
    assert!(params.get("resource").is_none() || params["resource"].is_null());
    assert!(params.get("active_only").is_none() || params["active_only"].is_null());
}

#[test]
fn test_agent_output_mapping() {
    let cmd = Command::Agent {
        cmd: AgentCmd::Output {
            session_id: "sess-42".to_string(),
            since: 5,
        },
    };
    let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
    assert_eq!(method, "agent.output");
    assert_eq!(params["session_id"], "sess-42");
    assert_eq!(params["since"], 5);
}

#[test]
fn test_agent_output_default_since() {
    let cmd = Command::Agent {
        cmd: AgentCmd::Output {
            session_id: "sess-1".to_string(),
            since: 0,
        },
    };
    let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
    assert_eq!(method, "agent.output");
    assert_eq!(params["session_id"], "sess-1");
    assert_eq!(params["since"], 0);
}

// --- Clarity gate bypass tests ---

/// Helper: compute whether the clarity gate should be skipped.
fn should_skip_gate(plan_text: Option<&str>, skip_flag: bool, enabled: bool) -> bool {
    plan_text.is_some() || skip_flag || !enabled
}

#[test]
fn test_gate_skip_when_plan_provided() {
    assert!(should_skip_gate(Some("my plan"), false, true));
}

#[test]
fn test_gate_skip_when_flag_set() {
    assert!(should_skip_gate(None, true, true));
}

#[test]
fn test_gate_skip_when_disabled_in_config() {
    assert!(should_skip_gate(None, false, false));
}

#[test]
fn test_gate_runs_when_no_bypass() {
    assert!(!should_skip_gate(None, false, true));
}

#[test]
fn test_gate_skip_all_bypass_conditions() {
    // All bypass conditions active at once
    assert!(should_skip_gate(Some("plan"), true, false));
}
