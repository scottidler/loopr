use std::path::Path;

use eyre::{Result, eyre};

use crate::agents::AgentContext;
use crate::agents::executor::action::scope;
use crate::agents::executor::result::ActionResult;
use crate::agents::executor::util::resolve_latest_published_tick_id;
use crate::domain::bundle::BundleStatus;

/// Handle ProposeBundle action.
pub(super) async fn handle_propose_bundle(
    ctx: &AgentContext,
    worktree_path: &Path,
    work_id: Option<&str>,
    description: &str,
    claims: &[String],
    noop_reason: Option<&str>,
    noop_paths: &[String],
) -> Result<ActionResult> {
    let bridge = &ctx.bridge;

    let mut is_noop = noop_reason.is_some();
    let wi_id = work_id.ok_or_else(|| eyre!("propose_bundle requires work_id"))?;

    // Guard: reject noop bundles with uncommitted changes.
    // The LLM may have written files via write_file but then
    // incorrectly taken the noop path. Return an error so a
    // fresh session can self-correct with a normal bundle.
    if is_noop {
        let status_output = tokio::process::Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(worktree_path)
            .output()
            .await;
        if let Ok(output) = status_output {
            let status = String::from_utf8_lossy(&output.stdout);
            if !status.trim().is_empty() {
                ctx.warn(&format!(
                    "Rejected noop bundle: worktree has uncommitted changes:\n{}",
                    status.trim()
                ));
                return Ok(ActionResult::ActionError(format!(
                    "Cannot propose a noop bundle while the worktree has uncommitted \
                     changes:\n{}\n\
                     You wrote files in this session that are not committed. \
                     Use `commit` first, then propose a normal bundle (without \
                     noop_reason). Do NOT use noop_reason if you made any changes.",
                    status.trim()
                )));
            }
        }
    }

    // Fix 1 (bundle-state-alignment): Auto-convert false noops to normal bundles.
    // When the agent branch has commits that differ from the integration HEAD,
    // this is NOT a noop - the previous session's work exists on this branch.
    // Auto-converting captures branch_name and head_commit, sending the bundle
    // through the normal review path where the reviewer gets a real git diff.
    if is_noop {
        let base_ref = ctx.session.base_ref.as_deref().unwrap_or_default().to_string();
        if !base_ref.is_empty() {
            let diff_check = tokio::process::Command::new("git")
                .args(["diff", "--quiet", &base_ref, "HEAD"])
                .current_dir(worktree_path)
                .output()
                .await;
            if let Ok(output) = diff_check
                && !output.status.success()
            {
                // Exit code 1 = there ARE differences between commits
                ctx.info(
                    "Auto-converting false noop to normal bundle: agent branch \
                     has commits that differ from integration HEAD",
                );
                is_noop = false;
                // Fall through to normal bundle path below, which captures
                // branch_name and head_commit automatically
            }
        }
    }

    // For normal bundles: auto-commit any pending changes before creating
    // the bundle. Stage all non-artifact dirty files. Skip for noop bundles.
    if !is_noop {
        let status_output = tokio::process::Command::new("git")
            .args(["status", "--porcelain"])
            .current_dir(worktree_path)
            .output()
            .await;
        if let Ok(output) = status_output {
            let dirty_files = scope::parse_porcelain_status(&String::from_utf8_lossy(&output.stdout));
            // Empty scope = all non-artifact files are in-scope.
            let (in_scope, _) = scope::partition_by_scope(&dirty_files, &[]);

            if !in_scope.is_empty() {
                let add_out = tokio::process::Command::new("git")
                    .arg("add")
                    .args(&in_scope)
                    .current_dir(worktree_path)
                    .output()
                    .await;
                if let Ok(out) = add_out
                    && out.status.success()
                {
                    let commit_out = tokio::process::Command::new("git")
                        .args(["commit", "-m", &format!("impl: {}", description)])
                        .current_dir(worktree_path)
                        .output()
                        .await;
                    match commit_out {
                        Ok(out) if out.status.success() => {
                            ctx.info("Auto-committed pending changes before propose_bundle");
                        }
                        _ => {
                            ctx.info("No pending changes to auto-commit before propose_bundle");
                        }
                    }
                }
            }
        }
    }

    // F2: Derive branch name deterministically from work_id - matches
    // WorktreeManager::create() which uses format!("agent/{}", work_id).
    // For noop bundles, use empty branch_name (Integrator skips merge).
    let branch_name = if is_noop {
        ctx.info(&format!("Noop bundle: {}", noop_reason.unwrap_or("(no reason)")));
        String::new()
    } else {
        format!("agent/{}", wi_id)
    };

    // Capture the current HEAD SHA for audit and pre-merge verification
    let head_commit = if !is_noop {
        tokio::process::Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(worktree_path)
            .output()
            .await
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
    } else {
        None
    };

    // Compute paths: all files this bundle affects.
    // For normal bundles: git diff --name-only <base_ref>...HEAD
    // For noop bundles: use explicit noop_paths or fall back to Work context extraction.
    let paths: Vec<String> = if !is_noop {
        let base = ctx.session.base_ref.as_deref().unwrap_or("main");
        let range = format!("{}...HEAD", base);
        tokio::process::Command::new("git")
            .args(["diff", "--name-only", &range])
            .current_dir(worktree_path)
            .output()
            .await
            .ok()
            .filter(|o| o.status.success())
            .map(|o| {
                String::from_utf8_lossy(&o.stdout)
                    .lines()
                    .filter(|l| !l.is_empty())
                    .map(String::from)
                    .collect()
            })
            .unwrap_or_default()
    } else {
        // Use explicit noop_paths or fall back to extracting paths from Work title/AC
        let candidates: Vec<String> = if !noop_paths.is_empty() {
            noop_paths.to_vec()
        } else {
            extract_paths_from_work(bridge.stores(), wi_id)
        };
        // Validate: keep only paths that exist in the worktree
        let valid: Vec<String> = candidates
            .into_iter()
            .filter(|p| {
                let full = worktree_path.join(p);
                if full.exists() {
                    true
                } else {
                    ctx.warn(&format!("noop_paths: {} does not exist in worktree, skipping", p));
                    false
                }
            })
            .collect();
        // Fail if no valid paths - prevents a dead-on-arrival bundle
        if valid.is_empty() {
            return Ok(ActionResult::ActionError(
                "Noop bundle requires at least one valid file path. \
                 Provide noop_paths listing the files you verified satisfy the acceptance criteria. \
                 Example: \"noop_paths\": [\"src/main.rs\"]"
                    .to_string(),
            ));
        }
        valid
    };
    if !paths.is_empty() {
        ctx.info(&format!("Touched paths: {:?}", paths));
    }

    // Fix #1: Resolve base_tick_id from latest Published Tick
    let base_tick_id = resolve_latest_published_tick_id(bridge.stores());

    let mut params = serde_json::json!({
        "work_id": wi_id,
        "branch_name": branch_name,
        "claims": claims,
        "description": description,
    });
    if let Some(ref sha) = head_commit {
        params["head_commit"] = serde_json::Value::String(sha.clone());
    }
    if let Some(reason) = noop_reason {
        params["noop_reason"] = serde_json::Value::String(reason.to_string());
    }
    if let Some(tick_id) = &base_tick_id {
        params["base_tick_id"] = serde_json::Value::String(tick_id.clone());
    }
    if !paths.is_empty() {
        params["paths"] = serde_json::json!(paths);
    }

    let resp = bridge.request("bundle.create", params.clone());
    if resp.is_error() {
        // Fix #7: On stale bundle rejection (-32002), retry once
        if let Some(ref err) = resp.error
            && err.code == -32002
        {
            ctx.warn(&format!(
                "Bundle stale ({}), retrying with fresh base_tick_id",
                err.message
            ));
            let new_base_tick_id = resolve_latest_published_tick_id(bridge.stores());
            if let Some(tick_id) = &new_base_tick_id {
                params["base_tick_id"] = serde_json::Value::String(tick_id.clone());
            }
            let retry_resp = bridge.request("bundle.create", params);
            if retry_resp.is_error() {
                return Err(eyre!("propose bundle failed after retry: {:?}", retry_resp.error));
            }
            return Ok(ActionResult::BundleProposed(description.to_string()));
        }
        return Err(eyre!("propose bundle failed: {:?}", resp.error));
    }
    Ok(ActionResult::BundleProposed(description.to_string()))
}

/// Extract file-like path tokens from a Work's title and acceptance criteria.
/// Used as fallback when the implementer omits noop_paths.
/// A token is file-like if it contains `.` or `/`.
fn extract_paths_from_work(stores: &crate::daemon::context::Stores, work_id: &str) -> Vec<String> {
    let Ok(works) = stores.read_works() else {
        return vec![];
    };
    let Some(work) = works.get(work_id) else {
        return vec![];
    };
    let mut tokens: Vec<String> = Vec::new();
    for text in std::iter::once(work.title.as_str()).chain(work.acceptance_criteria.0.iter().map(String::as_str)) {
        for word in text.split_whitespace() {
            let clean =
                word.trim_matches(|c: char| !c.is_alphanumeric() && c != '.' && c != '/' && c != '_' && c != '-');
            if !clean.is_empty() && (clean.contains('.') || clean.contains('/')) {
                tokens.push(clean.to_string());
            }
        }
    }
    tokens
}

/// Handle AcceptBundle action.
pub(super) fn handle_accept_bundle(ctx: &AgentContext, bundle_id: &str) -> Result<ActionResult> {
    let bridge = &ctx.bridge;

    if !bundle_id.starts_with("bd-") {
        return Ok(ActionResult::ActionError(format!(
            "accept_bundle: '{}' is not a bundle ID (expected bd-* prefix). Use the bundle ID, not the work ID.",
            bundle_id
        )));
    }
    // Pre-flight status guard: only Triaged or Reviewed bundles can be accepted
    let bundles = bridge.stores().read_bundles()?;
    match bundles.get(bundle_id) {
        None => {
            return Ok(ActionResult::ActionError(format!(
                "accept_bundle: bundle {} not found",
                bundle_id
            )));
        }
        Some(bundle) if !matches!(bundle.status(), BundleStatus::Triaged | BundleStatus::Reviewed) => {
            let hint = match bundle.status() {
                BundleStatus::Proposed => "Bundle is still being triaged by the system.",
                _ => "No accept action needed.",
            };
            return Ok(ActionResult::ActionError(format!(
                "accept_bundle: bundle {} is {} not Triaged/Reviewed. {}",
                bundle_id,
                bundle.status(),
                hint
            )));
        }
        _ => {}
    }
    drop(bundles);
    let resp = bridge.request(
        "bundle.transition",
        serde_json::json!({
            "id": bundle_id,
            "target_status": "Accepted",
            "role": "coordinator",
        }),
    );
    if resp.is_error() {
        let msg = resp.error.as_ref().map(|e| e.message.clone()).unwrap_or_default();
        return Ok(ActionResult::ActionError(format!("accept_bundle failed: {}", msg)));
    }
    ctx.info(&format!("AcceptBundle: {} -> Accepted", bundle_id));
    Ok(ActionResult::Transitioned(format!("bundle/{} -> Accepted", bundle_id)))
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {

    use crate::agents::executor::tests::{create_test_hierarchy, test_agent_context, test_stores};
    use crate::agents::executor::{ActionResult, execute_action};
    use crate::agents::{AgentAction, AgentKind};

    use crate::domain::tick::TickStatus;
    use crate::test_util::TestDir;

    #[tokio::test(flavor = "multi_thread")]
    async fn test_execute_propose_bundle() {
        let dir = TestDir::new("loopr-exec-propbundle");

        tokio::process::Command::new("git")
            .args(["init"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["config", "commit.gpgsign", "false"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        std::fs::write(dir.join("init.txt"), "init").unwrap();
        tokio::process::Command::new("git")
            .args(["add", "."])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentKind::Coordinator);

        let (_, _, _, wi_id) = create_test_hierarchy(&ctx.bridge);

        let action = AgentAction::ProposeBundle {
            description: "My bundle".to_string(),
            claims: vec!["Implemented feature X".to_string()],
            noop_reason: None,
            noop_paths: vec![],
        };
        let result = execute_action(&action, &ctx, &dir, Some(&wi_id)).await.unwrap();
        assert!(
            matches!(result, ActionResult::BundleProposed(ref desc) if desc == "My bundle"),
            "expected BundleProposed, got: {:?}",
            result
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_execute_propose_bundle_no_work() {
        let dir = TestDir::new("loopr-exec-propnowi");

        tokio::process::Command::new("git")
            .args(["init"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        std::fs::write(dir.join("init.txt"), "init").unwrap();
        tokio::process::Command::new("git")
            .args(["add", "."])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["config", "commit.gpgsign", "false"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        let stores = test_stores(&dir);

        let action = AgentAction::ProposeBundle {
            description: "My bundle".to_string(),
            claims: vec![],
            noop_reason: None,
            noop_paths: vec![],
        };
        let (ctx, _) = test_agent_context(&dir, &stores, AgentKind::Coordinator);
        let result = execute_action(&action, &ctx, &dir, None).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("work_id"));
    }

    // --- Group C: Agent management + domain actions ---

    #[tokio::test(flavor = "multi_thread")]
    async fn test_execute_accept_bundle() {
        let dir = TestDir::new("loopr-exec-accept");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentKind::Coordinator);

        let (_, _, _, wi_id) = create_test_hierarchy(&ctx.bridge);

        let bundle_resp = ctx.bridge.request(
            "bundle.create",
            serde_json::json!({
                "work_id": wi_id,
                "branch_name": "feature/accept",
                "description": "Accept test",
            }),
        );
        let bundle_id = bundle_resp.result.as_ref().unwrap()["id"].as_str().unwrap().to_string();
        ctx.bridge.request(
            "bundle.transition",
            serde_json::json!({"id": bundle_id, "target_status": "Triaged", "role": "coordinator"}),
        );
        ctx.bridge.request(
            "bundle.transition",
            serde_json::json!({"id": bundle_id, "target_status": "Reviewed", "role": "reviewer", "verification": "tests passed"}),
        );

        let action = AgentAction::AcceptBundle {
            bundle_id: bundle_id.clone(),
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(
            matches!(result, ActionResult::Transitioned(ref msg) if msg.contains("Accepted")),
            "expected Transitioned to Accepted, got: {:?}",
            result
        );
    }

    // --- Group D: Lifecycle paths ---

    #[tokio::test(flavor = "multi_thread")]
    async fn test_execute_accept_bundle_rejects_work_id() {
        let dir = TestDir::new("loopr-exec-accept-wkid");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentKind::Coordinator);

        let action = AgentAction::AcceptBundle {
            bundle_id: "wk-12345".to_string(),
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(
            matches!(result, ActionResult::ActionError(ref msg) if msg.contains("not a bundle ID")),
            "expected prefix validation error, got: {:?}",
            result
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_execute_accept_bundle_full() {
        let dir = TestDir::new("loopr-exec-acceptfull");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentKind::Coordinator);

        let (_, _, _, wi_id) = create_test_hierarchy(&ctx.bridge);

        let bundle_resp = ctx.bridge.request(
            "bundle.create",
            serde_json::json!({
                "work_id": wi_id,
                "branch_name": "feature/accepterr",
                "description": "Accept err test",
            }),
        );
        let bundle_id = bundle_resp.result.as_ref().unwrap()["id"].as_str().unwrap().to_string();

        let action = AgentAction::AcceptBundle {
            bundle_id: bundle_id.clone(),
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(
            matches!(result, ActionResult::ActionError(ref msg) if msg.contains("being triaged by the system")),
            "expected corrective hint for Proposed bundle, got: {:?}",
            result
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_accept_bundle_rejects_proposed_bundle() {
        let dir = TestDir::new("loopr-exec-accept-prop");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentKind::Coordinator);

        let (_, _, _, wi_id) = create_test_hierarchy(&ctx.bridge);

        let bundle_resp = ctx.bridge.request(
            "bundle.create",
            serde_json::json!({
                "work_id": wi_id,
                "branch_name": "feature/accept-prop",
                "description": "Accept proposed test",
            }),
        );
        let bundle_id = bundle_resp.result.as_ref().unwrap()["id"].as_str().unwrap().to_string();

        let action = AgentAction::AcceptBundle {
            bundle_id: bundle_id.clone(),
        };
        let result = execute_action(&action, &ctx, &dir, None).await.unwrap();
        assert!(
            matches!(result, ActionResult::ActionError(ref msg) if msg.contains("being triaged by the system")),
            "expected corrective hint for Proposed bundle, got: {:?}",
            result
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_propose_bundle_includes_base_tick_id() {
        let dir = TestDir::new("loopr-exec-propbase");

        tokio::process::Command::new("git")
            .args(["init"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["config", "commit.gpgsign", "false"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        std::fs::write(dir.join("init.txt"), "init").unwrap();
        tokio::process::Command::new("git")
            .args(["add", "."])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentKind::Coordinator);

        let mut tick = crate::domain::tick::Tick::new(1);
        tick.force_status(TickStatus::Published);
        tick.integration_sha = Some("abc123".to_string());
        stores.ticks.write().unwrap().insert(tick.id.clone(), tick);

        let (_, _, _, wi_id) = create_test_hierarchy(&ctx.bridge);

        let action = AgentAction::ProposeBundle {
            description: "Bundle with base tick".to_string(),
            claims: vec!["claim".to_string()],
            noop_reason: None,
            noop_paths: vec![],
        };
        let result = execute_action(&action, &ctx, &dir, Some(&wi_id)).await.unwrap();
        assert!(
            matches!(result, ActionResult::BundleProposed(ref d) if d == "Bundle with base tick"),
            "expected BundleProposed, got: {:?}",
            result
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_propose_bundle_uses_deterministic_branch_name() {
        let dir = TestDir::new("loopr-exec-f2branch");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentKind::Coordinator);

        let (_, _, _, wi_id) = create_test_hierarchy(&ctx.bridge);
        ctx.bridge.request(
            "work.transition",
            serde_json::json!({"id": wi_id, "target_status": "InProgress", "role": "coordinator"}),
        );

        let _ = tokio::process::Command::new("git")
            .args(["init"])
            .current_dir(&dir)
            .output()
            .await;
        let _ = tokio::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(&dir)
            .output()
            .await;
        let _ = tokio::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(&dir)
            .output()
            .await;
        let _ = tokio::process::Command::new("git")
            .args(["config", "commit.gpgsign", "false"])
            .current_dir(&dir)
            .output()
            .await;
        std::fs::write(dir.join("impl.txt"), "implementation").unwrap();
        let _ = tokio::process::Command::new("git")
            .args(["add", "."])
            .current_dir(&dir)
            .output()
            .await;
        let _ = tokio::process::Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(&dir)
            .output()
            .await;

        let action = AgentAction::ProposeBundle {
            description: "Test bundle".to_string(),
            claims: vec!["implemented feature".to_string()],
            noop_reason: None,
            noop_paths: vec![],
        };
        let result = execute_action(&action, &ctx, &dir, Some(&wi_id)).await.unwrap();
        assert!(
            matches!(result, ActionResult::BundleProposed(_)),
            "expected BundleProposed, got: {:?}",
            result
        );
        let bundles = stores.bundles.read().unwrap();
        let bundle = bundles.values().next().expect("should have one bundle");
        assert_eq!(
            bundle.branch_name,
            format!("agent/{}", wi_id),
            "bundle branch should be deterministic agent/<work_id>"
        );
    }

    // --- determine_work_handback tests ---

    #[tokio::test(flavor = "multi_thread")]
    async fn test_noop_propose_bundle_creates_empty_branch() {
        let dir = TestDir::new("loopr-exec-noop-branch");
        // Create a real file so noop_paths validation passes
        std::fs::write(dir.join("main.rs"), "fn main() {}").unwrap();
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentKind::Implementer);

        let (_, _, _, wi_id) = create_test_hierarchy(&ctx.bridge);

        let action = AgentAction::ProposeBundle {
            description: "Work already complete".to_string(),
            claims: vec!["criteria satisfied".to_string()],
            noop_reason: Some("Phase 1 already implemented this".to_string()),
            noop_paths: vec!["main.rs".to_string()],
        };
        let result = execute_action(&action, &ctx, &dir, Some(&wi_id)).await.unwrap();
        assert!(
            matches!(result, ActionResult::BundleProposed(ref d) if d == "Work already complete"),
            "expected BundleProposed, got: {:?}",
            result
        );

        let bundles = stores.bundles.read().unwrap();
        let bundle = bundles
            .values()
            .find(|b| b.work_id == wi_id)
            .expect("should have bundle");
        assert!(
            bundle.branch_name.is_empty(),
            "noop bundle should have empty branch_name"
        );
        assert_eq!(bundle.noop_reason.as_deref(), Some("Phase 1 already implemented this"));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_noop_bundle_handler_rejects_empty_branch_without_noop() {
        let dir = TestDir::new("loopr-exec-noop-reject");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentKind::Implementer);

        let (_, _, _, wi_id) = create_test_hierarchy(&ctx.bridge);

        let resp = ctx.bridge.request(
            "bundle.create",
            serde_json::json!({
                "work_id": wi_id,
                "branch_name": "",
                "description": "should fail",
            }),
        );
        assert!(resp.is_error(), "empty branch_name without noop_reason should fail");
        let err_msg = resp.error.as_ref().unwrap().message.clone();
        assert!(
            err_msg.contains("branch_name"),
            "error should mention branch_name: {}",
            err_msg
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_noop_guard_rejects_dirty_worktree() {
        let dir = TestDir::new("loopr-exec-noop-dirty");

        tokio::process::Command::new("git")
            .args(["init"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["config", "commit.gpgsign", "false"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        std::fs::write(dir.join("init.txt"), "init").unwrap();
        tokio::process::Command::new("git")
            .args(["add", "."])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();

        std::fs::write(dir.join("todo.lua"), "print('hello')").unwrap();

        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentKind::Implementer);
        let (_, _, _, wi_id) = create_test_hierarchy(&ctx.bridge);

        let action = AgentAction::ProposeBundle {
            description: "Work already complete".to_string(),
            claims: vec!["criteria satisfied".to_string()],
            noop_reason: Some("already done".to_string()),
            noop_paths: vec![],
        };
        let result = execute_action(&action, &ctx, &dir, Some(&wi_id)).await.unwrap();
        assert!(
            matches!(result, ActionResult::ActionError(ref msg) if msg.contains("uncommitted changes")),
            "expected ActionError for dirty worktree noop, got: {:?}",
            result
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_noop_guard_allows_clean_worktree() {
        let dir = TestDir::new("loopr-exec-noop-clean");

        tokio::process::Command::new("git")
            .args(["init"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["config", "user.email", "test@test.com"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["config", "user.name", "Test"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["config", "commit.gpgsign", "false"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        std::fs::write(dir.join("init.txt"), "init").unwrap();
        tokio::process::Command::new("git")
            .args(["add", "."])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["commit", "-m", "initial"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();

        std::fs::write(dir.join(".gitignore"), ".taskstore/\n*.log\ndocs/\n").unwrap();
        tokio::process::Command::new("git")
            .args(["add", ".gitignore"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["commit", "-m", "add gitignore"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();

        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentKind::Implementer);
        let (_, _, _, wi_id) = create_test_hierarchy(&ctx.bridge);

        let action = AgentAction::ProposeBundle {
            description: "Work already complete".to_string(),
            claims: vec!["criteria satisfied".to_string()],
            noop_reason: Some("Phase 1 already implemented this".to_string()),
            noop_paths: vec!["init.txt".to_string()],
        };
        let result = execute_action(&action, &ctx, &dir, Some(&wi_id)).await.unwrap();
        assert!(
            matches!(result, ActionResult::BundleProposed(ref d) if d == "Work already complete"),
            "expected BundleProposed for clean noop, got: {:?}",
            result
        );
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_noop_bundle_handler_allows_empty_branch_with_noop() {
        let dir = TestDir::new("loopr-exec-noop-allow");
        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentKind::Implementer);

        let (_, _, _, wi_id) = create_test_hierarchy(&ctx.bridge);

        let resp = ctx.bridge.request(
            "bundle.create",
            serde_json::json!({
                "work_id": wi_id,
                "branch_name": "",
                "noop_reason": "already done by phase 1",
                "description": "noop bundle",
            }),
        );
        assert!(
            !resp.is_error(),
            "empty branch_name with noop_reason should succeed: {:?}",
            resp.error
        );
        let bundle_id = resp.result.as_ref().unwrap()["id"].as_str().unwrap();
        let bundles = stores.bundles.read().unwrap();
        let bundle = bundles.get(bundle_id).expect("bundle should exist");
        assert!(bundle.branch_name.is_empty());
        assert_eq!(bundle.noop_reason.as_deref(), Some("already done by phase 1"));
    }

    // --- noop_paths tests (Fix 1) ---

    /// Verify that explicit noop_paths are stored on the bundle when the implementer provides them.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_noop_bundle_uses_explicit_paths() {
        let dir = TestDir::new("loopr-exec-noop-explicit");

        // Init git repo
        for args in [
            vec!["init"],
            vec!["config", "user.email", "test@test.com"],
            vec!["config", "user.name", "Test"],
        ] {
            tokio::process::Command::new("git")
                .args(&args)
                .current_dir(&dir)
                .output()
                .await
                .unwrap();
        }
        // Ignore test artifacts so noop guard doesn't trigger
        std::fs::write(dir.join(".gitignore"), ".taskstore/\ndocs/\n").unwrap();
        // Create the file that noop_paths will reference
        std::fs::write(dir.join("requirements.txt"), "flask==2.0").unwrap();
        tokio::process::Command::new("git")
            .args(["add", "."])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();

        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentKind::Implementer);
        let (_, _, _, wi_id) = create_test_hierarchy(&ctx.bridge);

        let action = AgentAction::ProposeBundle {
            description: "Requirements already pinned".to_string(),
            claims: vec!["requirements.txt exists with pinned versions".to_string()],
            noop_reason: Some("File already exists from scaffold".to_string()),
            noop_paths: vec!["requirements.txt".to_string()],
        };
        let result = execute_action(&action, &ctx, &dir, Some(&wi_id)).await.unwrap();
        assert!(
            matches!(result, ActionResult::BundleProposed(_)),
            "expected BundleProposed, got: {:?}",
            result
        );

        // Bundle should have paths set to the explicit noop_paths value
        let bundles = stores.bundles.read().unwrap();
        let bundle = bundles.values().find(|b| b.work_id == wi_id).unwrap();
        assert_eq!(bundle.paths, vec!["requirements.txt".to_string()]);
    }

    /// Verify that the fallback path extractor pulls file tokens from Work title/AC.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_noop_bundle_fallback_paths_from_work() {
        let dir = TestDir::new("loopr-exec-noop-fallback");

        for args in [
            vec!["init"],
            vec!["config", "user.email", "test@test.com"],
            vec!["config", "user.name", "Test"],
        ] {
            tokio::process::Command::new("git")
                .args(&args)
                .current_dir(&dir)
                .output()
                .await
                .unwrap();
        }
        // Ignore test artifacts so noop guard doesn't trigger
        std::fs::write(dir.join(".gitignore"), ".taskstore/\ndocs/\n").unwrap();
        // Create the file referenced in the work title
        std::fs::write(dir.join("setup.py"), "# setup").unwrap();
        tokio::process::Command::new("git")
            .args(["add", "."])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();

        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentKind::Implementer);

        // Create a work with "setup.py" in the title so the fallback extractor finds it
        let (_, _, _, _) = create_test_hierarchy(&ctx.bridge);
        // Override to create a custom work that mentions setup.py in title
        let wi_resp = ctx.bridge.request(
            "work.create",
            serde_json::json!({
                "parent_id": "ph-placeholder",
                "title": "Create setup.py package configuration",
                "acceptance_criteria": ["setup.py must define package metadata"]
            }),
        );
        // Use create_test_hierarchy work but modify stores directly
        let stores_arc = ctx.bridge.stores();
        let work_id = {
            let works = stores_arc.read_works().unwrap();
            // Find any work - use the one from create_test_hierarchy
            works.keys().next().unwrap().clone()
        };
        {
            let mut works = stores_arc.works.write().unwrap();
            if let Some(w) = works.get_mut(&work_id) {
                w.title = "Create setup.py package configuration".to_string();
                w.acceptance_criteria = crate::domain::criteria::AcceptanceCriteria(vec![
                    "setup.py must define package metadata".to_string(),
                ]);
            }
        }
        let _ = wi_resp; // suppress unused warning

        let action = AgentAction::ProposeBundle {
            description: "Setup.py already exists".to_string(),
            claims: vec!["setup.py already in repo".to_string()],
            noop_reason: Some("File pre-exists".to_string()),
            noop_paths: vec![], // empty - fallback to Work title/AC extraction
        };
        let result = execute_action(&action, &ctx, &dir, Some(&work_id)).await.unwrap();
        assert!(
            matches!(result, ActionResult::BundleProposed(_)),
            "expected BundleProposed from fallback, got: {:?}",
            result
        );

        let bundles = stores.bundles.read().unwrap();
        let bundle = bundles.values().find(|b| b.work_id == work_id).unwrap();
        assert!(!bundle.paths.is_empty(), "fallback should have found setup.py");
        assert!(
            bundle.paths.iter().any(|p| p.contains("setup.py")),
            "fallback paths should include setup.py, got: {:?}",
            bundle.paths
        );
    }

    /// Verify that a noop bundle with no valid paths returns ActionError.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_noop_bundle_filters_nonexistent_paths() {
        let dir = TestDir::new("loopr-exec-noop-filter");

        for args in [
            vec!["init"],
            vec!["config", "user.email", "test@test.com"],
            vec!["config", "user.name", "Test"],
        ] {
            tokio::process::Command::new("git")
                .args(&args)
                .current_dir(&dir)
                .output()
                .await
                .unwrap();
        }
        // Ignore test artifacts so noop guard doesn't trigger
        std::fs::write(dir.join(".gitignore"), ".taskstore/\ndocs/\n").unwrap();
        std::fs::write(dir.join("README.md"), "# test").unwrap();
        tokio::process::Command::new("git")
            .args(["add", "."])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();
        tokio::process::Command::new("git")
            .args(["commit", "-m", "init"])
            .current_dir(&dir)
            .output()
            .await
            .unwrap();

        let stores = test_stores(&dir);
        let (ctx, _) = test_agent_context(&dir, &stores, AgentKind::Implementer);
        let (_, _, _, wi_id) = create_test_hierarchy(&ctx.bridge);

        let action = AgentAction::ProposeBundle {
            description: "Noop with bad paths".to_string(),
            claims: vec!["done".to_string()],
            noop_reason: Some("already done".to_string()),
            noop_paths: vec!["nonexistent/file.rs".to_string(), "also/missing.py".to_string()],
        };
        let result = execute_action(&action, &ctx, &dir, Some(&wi_id)).await.unwrap();
        assert!(
            matches!(result, ActionResult::ActionError(ref msg) if msg.contains("at least one valid file path")),
            "expected ActionError for all-invalid noop_paths, got: {:?}",
            result
        );
    }
}
