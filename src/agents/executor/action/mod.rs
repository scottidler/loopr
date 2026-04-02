mod bundle;
mod file;
mod learning;
mod lock;
mod record;
mod tool;
mod validation;
mod work;

use std::path::Path;

use eyre::Result;

use super::result::ActionResult;
use crate::agents::{AgentAction, AgentContext};

/// Execute a single agent action. Used by the agent loop to process parsed LLM responses.
///
/// `ctx.session.agent_type` is used for role inference on Transition actions (when role is None).
pub async fn execute_action(
    action: &AgentAction,
    ctx: &AgentContext,
    worktree_path: &Path,
    work_id: Option<&str>,
) -> Result<ActionResult> {
    let agent_log = &ctx.log;
    agent_log.debug(&format!("execute_action(action={:?})", action));
    match action {
        AgentAction::RunTool { tool, args } => tool::handle_run_tool(ctx, worktree_path, tool, args).await,
        AgentAction::RegisterTool {
            name,
            command,
            timeout_secs,
            worktree,
        } => Ok(tool::handle_register_tool(
            ctx,
            worktree_path,
            name,
            command,
            *timeout_secs,
            *worktree,
        )?),
        AgentAction::WriteFile { path, content } => {
            file::handle_write_file(ctx, worktree_path, work_id, path, content).await
        }
        AgentAction::EditFile {
            path,
            old_string,
            new_string,
        } => file::handle_edit_file(ctx, worktree_path, work_id, path, old_string, new_string).await,
        AgentAction::ReadFile { path, offset, limit } => {
            file::handle_read_file(ctx, worktree_path, path, offset, limit).await
        }
        AgentAction::Commit { message, paths } => file::handle_commit(worktree_path, message, paths).await,
        AgentAction::ProposeBundle {
            description,
            claims,
            noop_reason,
        } => {
            bundle::handle_propose_bundle(ctx, worktree_path, work_id, description, claims, noop_reason.as_deref())
                .await
        }
        AgentAction::Transition {
            collection,
            id,
            target_status,
            role,
        } => work::handle_transition(ctx, collection, id, target_status, role.as_deref()),
        AgentAction::CreateLearning {
            content,
            scope,
            source_id,
            applicable_roles,
            resource_tags,
        } => learning::handle_create_learning(
            ctx,
            work_id,
            content,
            scope,
            source_id,
            applicable_roles.as_deref(),
            resource_tags.as_deref(),
        ),
        AgentAction::Done { summary } => Ok(ActionResult::Done(summary.clone())),
        AgentAction::NeedHelp { reason } => Ok(ActionResult::NeedHelp(reason.clone())),

        // --- Coordinator document-creation actions ---
        AgentAction::CreatePlan {
            title,
            description,
            acceptance_criteria,
        } => record::handle_create_plan(ctx, title, description, acceptance_criteria),
        AgentAction::CreateSpec {
            plan_id,
            title,
            description,
        } => record::handle_create_spec(ctx, plan_id, title, description),
        AgentAction::CreatePhase {
            spec_id,
            title,
            description,
            order,
        } => record::handle_create_phase(ctx, spec_id, title, description, *order),
        AgentAction::CreateWork {
            phase_id,
            title,
            description,
            resource_tags,
            acceptance_criteria,
            dependencies,
        } => work::handle_create_work(
            ctx,
            phase_id,
            title,
            description,
            resource_tags,
            acceptance_criteria,
            dependencies,
        ),

        // --- Coordinator agent management actions ---
        AgentAction::AssignAgent { agent_type, target_id } => work::handle_assign_agent(ctx, agent_type, target_id),
        AgentAction::SpawnResearcher { query, scope_id } => work::handle_spawn_researcher(ctx, query, scope_id),
        AgentAction::ValidateDocument { collection, id } => validation::handle_validate_document(ctx, collection, id),
        AgentAction::EvaluateCoverage {
            parent_collection,
            parent_id,
        } => validation::handle_evaluate_coverage(ctx, parent_collection, parent_id),
        AgentAction::InterviewQuestion { questions } => record::handle_interview_question(ctx, questions),
        AgentAction::ProposePlan {
            title,
            description,
            acceptance_criteria,
        } => record::handle_propose_plan(ctx, title, description, acceptance_criteria),
        AgentAction::ReviseParent {
            collection,
            id,
            reason,
            diagnostic,
        } => record::handle_revise_parent(ctx, collection, id, reason, diagnostic),
        AgentAction::AcquireLock { resource, holder_id } => lock::handle_acquire_lock(ctx, resource, holder_id),
        AgentAction::ReleaseLock { lock_id } => lock::handle_release_lock(ctx, lock_id),
        AgentAction::TriageBundle { bundle_id } => bundle::handle_triage_bundle(ctx, bundle_id),
        AgentAction::AcceptBundle { bundle_id } => bundle::handle_accept_bundle(ctx, bundle_id),
        AgentAction::OverrideWork {
            work_id,
            target_status,
            reason,
        } => work::handle_override_work(ctx, work_id, target_status, reason),

        // --- Researcher actions ---
        AgentAction::SearchCode { pattern, glob, path } => {
            file::handle_search_code(worktree_path, agent_log, pattern, glob.as_deref(), path.as_deref()).await
        }
        AgentAction::SearchFiles { pattern, path } => {
            file::handle_search_files(worktree_path, agent_log, pattern, path.as_deref()).await
        }
        AgentAction::ListDirectory { path } => file::handle_list_directory(worktree_path, agent_log, path).await,
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    
    
    use crate::agents::executor::{execute_action, ActionResult};
    use crate::agents::executor::tests::{
        test_stores, test_agent_context,
    };
    use crate::agents::{AgentAction, AgentKind};
    
    
    
    use crate::test_util::TestDir;
    
    
    
    
    
    
    

    #[tokio::test]
    async fn test_execute_action_done() {
        let dir = TestDir::new("loopr-exec-done");

        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentKind::Implementer);

        let action = AgentAction::Done {
            summary: "All done".to_string(),
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        if let ActionResult::Done(summary) = result {
            assert_eq!(summary, "All done");
        } else {
            panic!("expected Done result");
        }
    }

    #[tokio::test]
    async fn test_execute_action_need_help() {
        let dir = TestDir::new("loopr-exec-help");

        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentKind::Implementer);

        let action = AgentAction::NeedHelp {
            reason: "Ambiguous spec".to_string(),
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        if let ActionResult::NeedHelp(reason) = result {
            assert_eq!(reason, "Ambiguous spec");
        } else {
            panic!("expected NeedHelp result");
        }
    }

    #[tokio::test]
    async fn test_execute_action_unknown_tool() {
        let dir = TestDir::new("loopr-exec-unk");

        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentKind::Implementer);

        let action = AgentAction::RunTool {
            tool: "nonexistent".to_string(),
            args: vec![],
        };
        let result = execute_action(&action, &ctx, &dir, None).await;
        assert!(result.is_err());
        let err = result.unwrap_err().to_string();
        assert!(err.contains("not found"), "Expected 'not found' in: {err}");
        assert!(err.contains("register_tool"), "Expected 'register_tool' hint in: {err}");
    }
}
