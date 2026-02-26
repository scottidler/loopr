use std::path::Path;

use eyre::{Context, Result, bail};
use serde_json::json;

use crate::domain::role::Role;
use crate::ipc::client::IpcClient;

use super::{BundleCmd, Command, CrudCmd, LearningCmd, LockCmd, TickCmd, WorktreeCmd};

/// Connect to the daemon, send the IPC request for the given CLI command,
/// print the result, and exit.
pub async fn run(command: &Command, socket_path: &Path, role: Role) -> Result<()> {
    let mut client = IpcClient::connect(socket_path)
        .await
        .context("failed to connect to daemon — is it running?")?;

    // Perform handshake first
    let handshake = client.handshake(env!("CARGO_PKG_VERSION")).await;
    if let Err(e) = handshake {
        bail!("handshake failed: {e}");
    }

    let (method, params) = command_to_ipc(command, role);

    // Shutdown is fire-and-forget — the daemon may not respond
    if method == "system.shutdown" {
        client.send(&method, params).await.context("failed to send shutdown")?;
        println!("Shutdown signal sent.");
        return Ok(());
    }

    let (resp, _events) = client.request(&method, params).await.context("IPC request failed")?;

    if resp.is_error() {
        let err = resp.error.unwrap();
        bail!("error ({}): {}", err.code, err.message);
    }

    // Pretty-print the result
    if let Some(result) = resp.result {
        let pretty = serde_json::to_string_pretty(&result)?;
        println!("{pretty}");
    }

    Ok(())
}

/// Map a CLI Command to an IPC (method, params) pair.
fn command_to_ipc(command: &Command, role: Role) -> (String, serde_json::Value) {
    match command {
        Command::Status => ("system.status".to_string(), json!(null)),

        Command::Shutdown => ("system.shutdown".to_string(), json!(null)),

        Command::Plan { cmd } => crud_to_ipc("plan", cmd, role),
        Command::Spec { cmd } => crud_to_ipc("spec", cmd, role),
        Command::Phase { cmd } => crud_to_ipc("phase", cmd, role),
        Command::WorkItem { cmd } => crud_to_ipc("work_item", cmd, role),

        Command::Bundle { cmd } => bundle_to_ipc(cmd, role),
        Command::Tick { cmd } => tick_to_ipc(cmd, role),
        Command::Worktree { cmd } => worktree_to_ipc(cmd),
        Command::Learning { cmd } => learning_to_ipc(cmd),
        Command::Lock { cmd } => lock_to_ipc(cmd),

        Command::Init => ("system.init".to_string(), json!({})),
        Command::Validate { collection, id } => (
            "validator.validate".to_string(),
            json!({ "collection": collection, "id": id }),
        ),
        Command::Report { id } => ("validator.report".to_string(), json!({ "id": id })),
        Command::Reports { collection, target_id } => (
            "validator.reports".to_string(),
            json!({ "collection": collection, "target_id": target_id }),
        ),

        // Tui and Daemon are handled by main before dispatch
        Command::Tui | Command::Daemon => unreachable!("tui/daemon handled before dispatch"),
    }
}

/// Map CrudCmd to IPC for Plan/Spec/Phase/WorkItem.
fn crud_to_ipc(collection: &str, cmd: &CrudCmd, role: Role) -> (String, serde_json::Value) {
    match cmd {
        CrudCmd::Create {
            title,
            description,
            parent,
            order,
        } => {
            let mut params = json!({
                "title": title,
                "description": description,
            });
            if let Some(parent_id) = parent {
                // Use the correct parent key based on collection
                let key = match collection {
                    "spec" => "plan_id",
                    "phase" => "spec_id",
                    "work_item" => "phase_id",
                    _ => "parent_id",
                };
                params[key] = json!(parent_id);
            }
            if let Some(order) = order {
                params["order"] = json!(order);
            }
            (format!("{collection}.create"), params)
        }
        CrudCmd::Get { id } => (format!("{collection}.get"), json!({ "id": id })),
        CrudCmd::List { parent } => {
            let mut params = json!({});
            if let Some(parent_id) = parent {
                let key = match collection {
                    "spec" => "plan_id",
                    "phase" => "spec_id",
                    "work_item" => "phase_id",
                    _ => "parent_id",
                };
                params[key] = json!(parent_id);
            }
            (format!("{collection}.list"), params)
        }
        CrudCmd::Transition { id, status, skip_validation } => {
            // Plan/Spec/Phase use serde(rename_all = "lowercase"), so normalize
            let normalized_status = match collection {
                "plan" | "spec" | "phase" => status.to_lowercase(),
                _ => status.clone(),
            };
            let mut params = json!({
                "id": id,
                "target_status": normalized_status,
                "role": role.to_string(),
            });
            if *skip_validation {
                params["skip_validation"] = json!(true);
            }
            (format!("{collection}.transition"), params)
        }
    }
}

fn bundle_to_ipc(cmd: &BundleCmd, role: Role) -> (String, serde_json::Value) {
    match cmd {
        BundleCmd::Create {
            work_item_id,
            branch,
            description,
            base_tick_id,
        } => {
            let mut params = json!({
                "work_item_id": work_item_id,
                "branch_name": branch,
                "description": description,
            });
            if let Some(tick_id) = base_tick_id {
                params["base_tick_id"] = json!(tick_id);
            }
            ("bundle.create".to_string(), params)
        }
        BundleCmd::Get { id } => ("bundle.get".to_string(), json!({ "id": id })),
        BundleCmd::List { work_item_id } => {
            let mut params = json!({});
            if let Some(wi_id) = work_item_id {
                params["work_item_id"] = json!(wi_id);
            }
            ("bundle.list".to_string(), params)
        }
        BundleCmd::Transition { id, status } => (
            "bundle.transition".to_string(),
            json!({
                "id": id,
                "target_status": status,
                "role": role.to_string(),
            }),
        ),
    }
}

fn tick_to_ipc(cmd: &TickCmd, role: Role) -> (String, serde_json::Value) {
    match cmd {
        TickCmd::Create { number } => ("tick.create".to_string(), json!({ "number": number })),
        TickCmd::Get { id } => ("tick.get".to_string(), json!({ "id": id })),
        TickCmd::List { status } => {
            let mut params = json!({});
            if let Some(s) = status {
                params["status"] = json!(s);
            }
            ("tick.list".to_string(), params)
        }
        TickCmd::Transition { id, status } => (
            "tick.transition".to_string(),
            json!({
                "id": id,
                "target_status": status,
                "role": role.to_string(),
            }),
        ),
        TickCmd::Validate { id } => ("integrator.validate".to_string(), json!({ "tick_id": id })),
        TickCmd::Publish { id } => ("integrator.publish".to_string(), json!({ "tick_id": id })),
    }
}

fn worktree_to_ipc(cmd: &WorktreeCmd) -> (String, serde_json::Value) {
    match cmd {
        WorktreeCmd::Create { work_item_id, git_ref } => (
            "worktree.create".to_string(),
            json!({ "work_item_id": work_item_id, "ref": git_ref }),
        ),
        WorktreeCmd::List => ("worktree.list".to_string(), json!(null)),
        WorktreeCmd::Cleanup { work_item_id } => {
            ("worktree.cleanup".to_string(), json!({ "work_item_id": work_item_id }))
        }
        WorktreeCmd::Refresh { work_item_id, git_ref } => (
            "worktree.refresh".to_string(),
            json!({ "work_item_id": work_item_id, "ref": git_ref }),
        ),
    }
}

fn learning_to_ipc(cmd: &LearningCmd) -> (String, serde_json::Value) {
    match cmd {
        LearningCmd::Create {
            source_id,
            scope,
            content,
        } => (
            "learning.create".to_string(),
            json!({ "source_id": source_id, "scope": scope, "content": content }),
        ),
        LearningCmd::Get { id } => ("learning.get".to_string(), json!({ "id": id })),
        LearningCmd::List => ("learning.list".to_string(), json!(null)),
        LearningCmd::Reinforce { id } => ("learning.reinforce".to_string(), json!({ "id": id })),
        LearningCmd::Contradict { id } => ("learning.contradict".to_string(), json!({ "id": id })),
        LearningCmd::Promote { id } => ("learning.promote".to_string(), json!({ "id": id })),
        LearningCmd::Demote { id } => ("learning.demote".to_string(), json!({ "id": id })),
    }
}

fn lock_to_ipc(cmd: &LockCmd) -> (String, serde_json::Value) {
    match cmd {
        LockCmd::Create {
            resource,
            holder_id,
            granted_by,
        } => (
            "lock.create".to_string(),
            json!({ "resource": resource, "holder_id": holder_id, "granted_by": granted_by }),
        ),
        LockCmd::Get { id } => ("lock.get".to_string(), json!({ "id": id })),
        LockCmd::List {
            resource,
            holder_id,
            active_only,
        } => {
            let mut params = json!({});
            if let Some(r) = resource {
                params["resource"] = json!(r);
            }
            if let Some(h) = holder_id {
                params["holder_id"] = json!(h);
            }
            if *active_only {
                params["active_only"] = json!(true);
            }
            ("lock.list".to_string(), params)
        }
        LockCmd::Release { id } => ("lock.release".to_string(), json!({ "id": id })),
        LockCmd::Expire { id } => ("lock.expire".to_string(), json!({ "id": id })),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_plan_create_mapping() {
        let cmd = Command::Plan {
            cmd: CrudCmd::Create {
                title: "My Plan".to_string(),
                description: "A test".to_string(),
                parent: None,
                order: None,
            },
        };
        let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
        assert_eq!(method, "plan.create");
        assert_eq!(params["title"], "My Plan");
        assert_eq!(params["description"], "A test");
    }

    #[test]
    fn test_spec_create_mapping_with_parent() {
        let cmd = Command::Spec {
            cmd: CrudCmd::Create {
                title: "My Spec".to_string(),
                description: "".to_string(),
                parent: Some("plan-1".to_string()),
                order: None,
            },
        };
        let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
        assert_eq!(method, "spec.create");
        assert_eq!(params["plan_id"], "plan-1");
    }

    #[test]
    fn test_phase_create_mapping_with_order() {
        let cmd = Command::Phase {
            cmd: CrudCmd::Create {
                title: "Phase 1".to_string(),
                description: "".to_string(),
                parent: Some("spec-1".to_string()),
                order: Some(1),
            },
        };
        let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
        assert_eq!(method, "phase.create");
        assert_eq!(params["spec_id"], "spec-1");
        assert_eq!(params["order"], 1);
    }

    #[test]
    fn test_work_item_transition_mapping() {
        let cmd = Command::WorkItem {
            cmd: CrudCmd::Transition {
                id: "wi-1".to_string(),
                status: "Ready".to_string(),
                skip_validation: false,
            },
        };
        let (method, params) = command_to_ipc(&cmd, Role::Coordinator);
        assert_eq!(method, "work_item.transition");
        assert_eq!(params["id"], "wi-1");
        assert_eq!(params["target_status"], "Ready");
        assert_eq!(params["role"], "coordinator");
    }

    #[test]
    fn test_bundle_create_mapping() {
        let cmd = Command::Bundle {
            cmd: BundleCmd::Create {
                work_item_id: "wi-1".to_string(),
                branch: "feature/foo".to_string(),
                description: "A bundle".to_string(),
                base_tick_id: Some("tick-1".to_string()),
            },
        };
        let (method, params) = command_to_ipc(&cmd, Role::Implementer);
        assert_eq!(method, "bundle.create");
        assert_eq!(params["work_item_id"], "wi-1");
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
                work_item_id: "wi-1".to_string(),
                git_ref: "main".to_string(),
            },
        };
        let (method, params) = command_to_ipc(&cmd, Role::Implementer);
        assert_eq!(method, "worktree.create");
        assert_eq!(params["work_item_id"], "wi-1");
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
            let parsed: Role = serde_json::from_str(&quoted)
                .unwrap_or_else(|e| panic!("role '{}' not deserializable: {}", role_str, e));
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
    fn test_work_item_transition_preserves_status_casing() {
        // WorkItemStatus uses default serde (PascalCase), so dispatch must NOT lowercase.
        let cmd = Command::WorkItem {
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
            let parsed: Role = serde_json::from_str(&quoted)
                .unwrap_or_else(|e| panic!("role '{}' not deserializable: {}", role_str, e));
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
}
