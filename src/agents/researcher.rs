use std::path::{Path, PathBuf};
use std::sync::Arc;

use eyre::{Result, eyre};
use log::{debug, info, warn};
use tokio::sync::broadcast;

use crate::agents::agent_logger::AgentLogger;
use crate::agents::bridge::AgentIpcBridge;
use crate::agents::context::ContextBuilder;
use crate::agents::executor::{ActionResult, execute_action};
use crate::agents::implementer::{self, IterationOutcome, LlmClient};
use crate::agents::{AgentAction, AgentSession, AgentStatus, AgentType};
use crate::config::AgentRoleConfig;
use crate::daemon::context::Stores;
use crate::domain::role::Role;
use crate::ipc::protocol::DaemonEvent;

/// Denylist patterns for path sandboxing. Matched against the file name component.
const PATH_DENYLIST: &[&str] = &[".env", "credentials.", "secret"];
const EXT_DENYLIST: &[&str] = &["key", "pem"];

/// Validate that a path is safe for the Researcher to access.
///
/// Rules:
/// 1. Must be relative (no absolute paths)
/// 2. Must resolve within repo_root after canonicalization
/// 3. Must not match denylist patterns
pub fn validate_path(repo_root: &Path, relative: &str) -> Result<PathBuf> {
    debug!(
        "validate_path(repo_root={}, relative={})",
        repo_root.display(),
        relative
    );
    // Reject absolute paths
    if relative.starts_with('/') || relative.starts_with('\\') {
        return Err(eyre!("absolute paths not allowed: {}", relative));
    }

    // Reject path traversal components (..)
    for component in Path::new(relative).components() {
        if component == std::path::Component::ParentDir {
            return Err(eyre!("path traversal not allowed: {}", relative));
        }
    }

    let full = repo_root.join(relative);

    // Canonicalize — for files that don't yet exist, we validate the parent
    let canonical = if full.exists() {
        full.canonicalize().unwrap_or_else(|_| full.clone())
    } else {
        // File doesn't exist — still check the parent for escapes
        full.clone()
    };

    let root_canonical = repo_root.canonicalize().unwrap_or_else(|_| repo_root.to_path_buf());
    if !canonical.starts_with(&root_canonical) {
        return Err(eyre!("path escapes repo root: {}", relative));
    }

    // Check denylist against file name
    let file_name = full
        .file_name()
        .map(|n| n.to_string_lossy().to_lowercase())
        .unwrap_or_default();

    for pattern in PATH_DENYLIST {
        if file_name.contains(pattern) {
            return Err(eyre!("path denied by security policy: {}", relative));
        }
    }

    if let Some(ext) = full.extension() {
        let ext_lower = ext.to_string_lossy().to_lowercase();
        for denied_ext in EXT_DENYLIST {
            if ext_lower == *denied_ext {
                return Err(eyre!("file extension denied by security policy: {}", relative));
            }
        }
    }

    Ok(full)
}

/// Build the Researcher system prompt with the query injected.
fn build_system_prompt(query: &str) -> String {
    debug!("build_system_prompt(query_len={})", query.len());
    crate::prompts::store().researcher.replace("{query}", query)
}

/// Execute a SearchCode action: use grep/ripgrep to search file contents.
pub async fn execute_search_code(
    repo_root: &Path,
    pattern: &str,
    glob_filter: Option<&str>,
    search_path: Option<&str>,
) -> Result<String> {
    debug!(
        "execute_search_code(pattern={}, glob={:?}, path={:?})",
        pattern, glob_filter, search_path
    );
    let search_dir = match search_path {
        Some(p) => validate_path(repo_root, p)?,
        None => repo_root.to_path_buf(),
    };

    // Gap #32: Use rg (ripgrep) with --no-follow; fall back to grep
    let rg_available = std::process::Command::new("rg").arg("--version").output().is_ok();

    let mut cmd = if rg_available {
        let mut c = tokio::process::Command::new("rg");
        c.args(["--no-follow", "-n", pattern]);
        if let Some(g) = glob_filter {
            c.args(["--glob", g]);
        }
        c.arg(search_dir.to_string_lossy().as_ref());
        c
    } else {
        let mut c = tokio::process::Command::new("grep");
        c.args(["-rn", "-E", pattern]);
        if let Some(g) = glob_filter {
            c.arg(format!("--include={}", g));
        }
        c.arg(search_dir.to_string_lossy().as_ref());
        c
    };

    // Limit output to prevent overwhelming the context
    let output = tokio::time::timeout(std::time::Duration::from_secs(30), cmd.output())
        .await
        .map_err(|_| eyre!("search_code timed out after 30s"))?
        .map_err(|e| eyre!("search_code failed: {}", e))?;

    let stdout = String::from_utf8_lossy(&output.stdout);

    // Truncate to ~100 lines to fit token budget
    let lines: Vec<&str> = stdout.lines().take(100).collect();
    let mut result = lines.join("\n");
    if stdout.lines().count() > 100 {
        result.push_str("\n... (truncated, more results available)");
    }

    if result.is_empty() {
        result = "No matches found.".to_string();
    }

    Ok(result)
}

/// Execute a SearchFiles action: find files matching a glob pattern.
pub async fn execute_search_files(repo_root: &Path, pattern: &str, search_path: Option<&str>) -> Result<String> {
    let search_dir = match search_path {
        Some(p) => validate_path(repo_root, p)?,
        None => repo_root.to_path_buf(),
    };

    let glob_pattern = format!("{}/{}", search_dir.display(), pattern);
    let mut results = Vec::new();

    for entry in glob::glob(&glob_pattern).map_err(|e| eyre!("invalid glob pattern: {}", e))? {
        match entry {
            Ok(matched_path) => {
                // Validate each result path
                if let Ok(rel) = matched_path.strip_prefix(repo_root) {
                    let rel_str: std::borrow::Cow<'_, str> = rel.to_string_lossy();
                    // Apply denylist
                    if validate_path(repo_root, &rel_str).is_ok() {
                        results.push(rel_str.to_string());
                    }
                }
            }
            Err(e) => {
                warn!("glob error: {}", e);
            }
        }

        if results.len() >= 200 {
            results.push("... (truncated, more results available)".to_string());
            break;
        }
    }

    if results.is_empty() {
        Ok("No files found matching pattern.".to_string())
    } else {
        Ok(results.join("\n"))
    }
}

/// Execute a ListDirectory action: list files in a directory.
pub async fn execute_list_directory(repo_root: &Path, path: &str) -> Result<String> {
    let dir = validate_path(repo_root, path)?;

    if !dir.is_dir() {
        return Err(eyre!("not a directory: {}", path));
    }

    let mut entries = Vec::new();
    let mut reader = tokio::fs::read_dir(&dir)
        .await
        .map_err(|e| eyre!("list_directory '{}': {}", path, e))?;

    while let Some(entry) = reader.next_entry().await.map_err(|e| eyre!("read dir entry: {}", e))? {
        let name = entry.file_name().to_string_lossy().to_string();
        let meta = entry.metadata().await;
        let suffix = if meta.as_ref().map(|m| m.is_dir()).unwrap_or(false) { "/" } else { "" };
        entries.push(format!("{}{}", name, suffix));

        if entries.len() >= 500 {
            entries.push("... (truncated)".to_string());
            break;
        }
    }

    entries.sort();
    if entries.is_empty() {
        Ok("(empty directory)".to_string())
    } else {
        Ok(entries.join("\n"))
    }
}

/// Run a single researcher iteration: build context → call LLM → parse → execute.
async fn run_researcher_iteration(
    llm: &dyn LlmClient,
    session: &AgentSession,
    stores: &Arc<Stores>,
    bridge: &AgentIpcBridge,
    iteration: u32,
    previous_summary: Option<String>,
) -> Result<IterationOutcome> {
    let query = session.query.as_deref().unwrap_or("(no query specified)");
    let system_prompt = build_system_prompt(query);

    // Build scope chain from target_id if available
    let builder = ContextBuilder::new(stores, Role::Researcher);

    let footer = format!(
        "Iteration {}: Investigate the codebase to answer the query. Respond with a JSON array of actions.",
        iteration
    );

    let builder = builder
        .with_previous_summary(previous_summary)
        .with_iteration(iteration)
        .with_footer(footer);

    let assembled = builder.build(&system_prompt);

    let event_tx = bridge.event_tx();
    info!(
        "Researcher {} iteration {} context: ~{} tokens",
        session.id, iteration, assembled.token_estimate
    );

    let _ = event_tx.send(DaemonEvent::agent_status_changed(
        &session.id,
        AgentStatus::WaitingForLlm,
    ));

    let response = llm.call(&assembled.system_prompt, &assembled.user_message).await?;
    let actions = implementer::parse_actions(&response)?;

    let _ = event_tx.send(DaemonEvent::agent_status_changed(&session.id, AgentStatus::Running));

    if actions.is_empty() {
        return Ok(IterationOutcome::Done("No actions returned".to_string()));
    }

    let repo_root = &stores.config.project.repo_path;
    let tool_runner = &*stores.tool_runner;

    let mut last_summary = String::new();
    for action in &actions {
        // Validate: Researcher can only use read-only actions
        if !is_allowed_researcher_action(action) {
            warn!("Researcher {} attempted disallowed action: {:?}", session.id, action);
            last_summary = format!("ERROR: action not allowed for Researcher: {:?}", action);
            continue;
        }

        let result = match action {
            // Handle Researcher-specific actions inline
            AgentAction::SearchCode { pattern, glob, path } => {
                match execute_search_code(repo_root, pattern, glob.as_deref(), path.as_deref()).await {
                    Ok(output) => ActionResult::FileRead(output),
                    Err(e) => ActionResult::ActionError(e.to_string()),
                }
            }
            AgentAction::SearchFiles { pattern, path } => {
                match execute_search_files(repo_root, pattern, path.as_deref()).await {
                    Ok(output) => ActionResult::FileRead(output),
                    Err(e) => ActionResult::ActionError(e.to_string()),
                }
            }
            AgentAction::ListDirectory { path } => match execute_list_directory(repo_root, path).await {
                Ok(output) => ActionResult::FileRead(output),
                Err(e) => ActionResult::ActionError(e.to_string()),
            },
            AgentAction::ReadFile { path } => {
                // Apply path sandboxing for ReadFile
                match validate_path(repo_root, path) {
                    Ok(full_path) => match tokio::fs::read_to_string(&full_path).await {
                        Ok(content) => {
                            // Truncate to ~2000 lines
                            let lines: Vec<&str> = content.lines().take(2000).collect();
                            let mut truncated = lines.join("\n");
                            if content.lines().count() > 2000 {
                                truncated.push_str("\n... [truncated]");
                            }
                            ActionResult::FileRead(truncated)
                        }
                        Err(e) => ActionResult::ActionError(format!("read_file '{}': {}", path, e)),
                    },
                    Err(e) => ActionResult::ActionError(e.to_string()),
                }
            }
            // Shared actions delegated to executor
            _ => match execute_action(action, tool_runner, bridge, repo_root, None, AgentType::Researcher).await {
                Ok(r) => r,
                Err(e) => {
                    warn!("Researcher {} action failed (non-fatal): {e}", session.id);
                    ActionResult::ActionError(e.to_string())
                }
            },
        };

        let summary = format_action_summary(action, &result);
        let _ = event_tx.send(DaemonEvent::agent_action_completed(&session.id, &summary));

        match &result {
            ActionResult::Done(s) => return Ok(IterationOutcome::Done(s.clone())),
            ActionResult::NeedHelp(reason) => return Ok(IterationOutcome::NeedHelp(reason.clone())),
            _ => {}
        }
        last_summary = summary;
    }

    Ok(IterationOutcome::Continue(last_summary))
}

/// Check if the action is allowed for a Researcher (read-only + CreateLearning + Done/NeedHelp).
fn is_allowed_researcher_action(action: &AgentAction) -> bool {
    matches!(
        action,
        AgentAction::SearchCode { .. }
            | AgentAction::SearchFiles { .. }
            | AgentAction::ListDirectory { .. }
            | AgentAction::ReadFile { .. }
            | AgentAction::CreateLearning { .. }
            | AgentAction::Done { .. }
            | AgentAction::NeedHelp { .. }
    )
}

/// Run the Researcher's iteration loop.
///
/// Unlike Coordinator (indefinite), Researcher has a max_iterations cap.
/// Terminates on: Done, NeedHelp, max_iterations, or error.
pub async fn run_researcher(
    llm: &dyn LlmClient,
    session: &mut AgentSession,
    stores: &Arc<Stores>,
    bridge: &AgentIpcBridge,
    config: &AgentRoleConfig,
    event_tx: &broadcast::Sender<DaemonEvent>,
    agent_log: &AgentLogger,
) -> Result<()> {
    agent_log.debug(&format!("run_researcher(session_id={})", session.id));
    agent_log.info(&format!(
        "Researcher starting (query: {:?}, target: {:?})",
        session.query.as_deref().unwrap_or("none"),
        session.target_id.as_deref().unwrap_or("none"),
    ));

    let mut previous_summary: Option<String> = None;

    for iteration in 1..=config.max_iterations {
        // Check cancellation
        {
            let sessions = stores.agent_sessions.read().unwrap();
            if let Some(s) = sessions.get(&session.id)
                && s.status == AgentStatus::Cancelled
            {
                info!("Researcher {} cancelled, exiting loop", session.id);
                return Ok(());
            }
        }

        session.iteration = iteration;
        info!(
            "Researcher {} iteration {}/{}",
            session.id, iteration, config.max_iterations
        );

        let outcome = run_researcher_iteration(llm, session, stores, bridge, iteration, previous_summary.clone()).await;

        match outcome {
            Ok(IterationOutcome::Done(summary)) => {
                let _ = event_tx.send(DaemonEvent::agent_iteration_completed(&session.id, iteration, &summary));
                info!("Researcher {} done: {}", session.id, summary);
                return Ok(());
            }
            Ok(IterationOutcome::Continue(summary)) => {
                let _ = event_tx.send(DaemonEvent::agent_iteration_completed(&session.id, iteration, &summary));
                info!("Researcher {} continue: {}", session.id, summary);
                previous_summary = Some(summary);
            }
            Ok(IterationOutcome::NeedHelp(reason)) => {
                let _ = event_tx.send(DaemonEvent::agent_iteration_completed(&session.id, iteration, &reason));
                warn!("Researcher {} needs help: {}", session.id, reason);
                return Err(eyre!("researcher needs help: {}", reason));
            }
            Err(e) => {
                warn!("Researcher {} iteration {} failed: {}", session.id, iteration, e);
                return Err(eyre!("researcher iteration failed: {}", e));
            }
        }
    }

    warn!(
        "Researcher {} reached max iterations ({})",
        session.id, config.max_iterations
    );
    Err(eyre!("researcher reached max iterations ({})", config.max_iterations))
}

fn format_action_summary(action: &AgentAction, result: &ActionResult) -> String {
    match result {
        ActionResult::FileRead(content) => match action {
            AgentAction::SearchCode { pattern, .. } => {
                let line_count = content.lines().count();
                format!("search_code '{}': {} lines", pattern, line_count)
            }
            AgentAction::SearchFiles { pattern, .. } => {
                let file_count = content.lines().count();
                format!("search_files '{}': {} files", pattern, file_count)
            }
            AgentAction::ListDirectory { path } => {
                let entry_count = content.lines().count();
                format!("list_directory '{}': {} entries", path, entry_count)
            }
            AgentAction::ReadFile { path } => format!("read_file '{}' ({} bytes)", path, content.len()),
            _ => format!("read: {} bytes", content.len()),
        },
        ActionResult::LearningCreated(c) => format!("learning: {}", c),
        ActionResult::Done(s) => format!("done: {}", s),
        ActionResult::NeedHelp(r) => format!("need help: {}", r),
        ActionResult::ActionError(e) => format!("ERROR: {}", e),
        other => format!("{:?}", other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::AgentSession;
    use crate::config::{Config, ProjectConfig};
    use async_trait::async_trait;
    use std::sync::Mutex as StdMutex;
    use taskstore::Store;

    struct MockLlm {
        responses: StdMutex<Vec<String>>,
    }

    impl MockLlm {
        fn new(responses: Vec<String>) -> Self {
            Self {
                responses: StdMutex::new(responses),
            }
        }
    }

    #[async_trait]
    impl LlmClient for MockLlm {
        async fn call(&self, _system_prompt: &str, _user_message: &str) -> Result<String> {
            let mut responses = self.responses.lock().unwrap();
            if responses.is_empty() {
                Ok(r#"[{"action": "done", "summary": "No more responses"}]"#.to_string())
            } else {
                Ok(responses.remove(0))
            }
        }
    }

    fn test_stores(dir: &Path) -> Arc<Stores> {
        let config = Config {
            project: ProjectConfig {
                repo_path: dir.to_path_buf(),
                ..ProjectConfig::default()
            },
            ..Config::default()
        };
        let store = Store::open(dir).unwrap();
        let mut stores = Stores::new();
        stores.store = Some(Arc::new(StdMutex::new(store)));
        stores.config = config;
        Arc::new(stores)
    }

    fn test_agent_logger(dir: &Path) -> AgentLogger {
        use crate::agents::AgentType;
        let file_path = dir.join("test-researcher.log");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&file_path)
            .unwrap();
        AgentLogger::_new_for_test(AgentType::Researcher, "test-session", file, file_path)
    }

    // --- Path sandboxing tests ---

    #[test]
    fn test_validate_path_relative_ok() {
        let dir = std::env::temp_dir().join(format!("loopr-res-pathok-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("test.rs"), "fn main() {}").unwrap();

        let result = validate_path(&dir, "test.rs");
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_path_rejects_absolute() {
        let dir = std::env::temp_dir().join(format!("loopr-res-pathabs-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();

        let result = validate_path(&dir, "/etc/passwd");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("absolute"));
    }

    #[test]
    fn test_validate_path_rejects_env_file() {
        let dir = std::env::temp_dir().join(format!("loopr-res-pathenv-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();

        let result = validate_path(&dir, ".env");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("security policy"));
    }

    #[test]
    fn test_validate_path_rejects_key_file() {
        let dir = std::env::temp_dir().join(format!("loopr-res-pathkey-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();

        let result = validate_path(&dir, "server.key");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("security policy"));
    }

    #[test]
    fn test_validate_path_rejects_pem_file() {
        let dir = std::env::temp_dir().join(format!("loopr-res-pathpem-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();

        let result = validate_path(&dir, "cert.pem");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("security policy"));
    }

    #[test]
    fn test_validate_path_rejects_credentials() {
        let dir = std::env::temp_dir().join(format!("loopr-res-pathcred-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();

        let result = validate_path(&dir, "credentials.json");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("security policy"));
    }

    #[test]
    fn test_validate_path_rejects_secret_file() {
        let dir = std::env::temp_dir().join(format!("loopr-res-pathsec-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();

        let result = validate_path(&dir, "my_secret_config.yml");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("security policy"));
    }

    #[test]
    fn test_validate_path_allows_normal_files() {
        let dir = std::env::temp_dir().join(format!("loopr-res-pathnorm-{}", crate::id::generate_id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/main.rs"), "fn main() {}").unwrap();

        assert!(validate_path(&dir, "src/main.rs").is_ok());
        assert!(validate_path(&dir, "Cargo.toml").is_ok());
    }

    // --- System prompt tests ---

    #[test]
    fn test_system_prompt_contains_query_placeholder() {
        crate::prompts::init_defaults();
        assert!(crate::prompts::store().researcher.contains("{query}"));
    }

    #[test]
    fn test_build_system_prompt_injects_query() {
        let prompt = build_system_prompt("Find all error handling patterns");
        assert!(prompt.contains("Find all error handling patterns"));
        assert!(!prompt.contains("{query}"));
    }

    // --- is_allowed_researcher_action tests ---

    #[test]
    fn test_allowed_researcher_actions() {
        assert!(is_allowed_researcher_action(&AgentAction::SearchCode {
            pattern: "test".into(),
            glob: None,
            path: None,
        }));
        assert!(is_allowed_researcher_action(&AgentAction::SearchFiles {
            pattern: "*.rs".into(),
            path: None,
        }));
        assert!(is_allowed_researcher_action(&AgentAction::ListDirectory {
            path: "src".into(),
        }));
        assert!(is_allowed_researcher_action(&AgentAction::ReadFile {
            path: "src/main.rs".into(),
        }));
        assert!(is_allowed_researcher_action(&AgentAction::CreateLearning {
            content: "test".into(),
            scope: "global".into(),
            source_id: "s1".into(),
            applicable_roles: None,
            resource_tags: None,
        }));
        assert!(is_allowed_researcher_action(&AgentAction::Done {
            summary: "done".into(),
        }));
        assert!(is_allowed_researcher_action(&AgentAction::NeedHelp {
            reason: "stuck".into(),
        }));
    }

    #[test]
    fn test_disallowed_researcher_actions() {
        assert!(!is_allowed_researcher_action(&AgentAction::WriteFile {
            path: "x".into(),
            content: "y".into(),
        }));
        assert!(!is_allowed_researcher_action(&AgentAction::Commit {
            message: "m".into(),
            paths: vec![],
        }));
        assert!(!is_allowed_researcher_action(&AgentAction::RunTool {
            tool: "test".into(),
            args: vec![],
        }));
        assert!(!is_allowed_researcher_action(&AgentAction::ProposeBundle {
            description: "d".into(),
            claims: vec![],
        }));
    }

    // --- SearchCode / SearchFiles / ListDirectory tests ---

    #[tokio::test]
    async fn test_execute_search_code_finds_content() {
        let dir = std::env::temp_dir().join(format!("loopr-res-sc-{}", crate::id::generate_id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/test.rs"), "fn hello_world() {}\nfn goodbye() {}").unwrap();

        let result = execute_search_code(&dir, "hello_world", Some("*.rs"), Some("src")).await;
        assert!(result.is_ok());
        let output = result.unwrap();
        assert!(output.contains("hello_world"));
    }

    #[tokio::test]
    async fn test_execute_search_code_no_matches() {
        let dir = std::env::temp_dir().join(format!("loopr-res-scnone-{}", crate::id::generate_id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/test.rs"), "fn main() {}").unwrap();

        let result = execute_search_code(&dir, "nonexistent_pattern_xyz", None, None).await;
        assert!(result.is_ok());
        assert!(result.unwrap().contains("No matches"));
    }

    #[tokio::test]
    async fn test_execute_search_files_finds_files() {
        let dir = std::env::temp_dir().join(format!("loopr-res-sf-{}", crate::id::generate_id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/foo.rs"), "").unwrap();
        std::fs::write(dir.join("src/bar.rs"), "").unwrap();

        let result = execute_search_files(&dir, "*.rs", Some("src")).await;
        assert!(result.is_ok());
        let output = result.unwrap();
        assert!(output.contains("foo.rs") || output.contains("bar.rs"));
    }

    #[tokio::test]
    async fn test_execute_search_files_no_matches() {
        let dir = std::env::temp_dir().join(format!("loopr-res-sfnone-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();

        let result = execute_search_files(&dir, "*.xyz", None).await;
        assert!(result.is_ok());
        assert!(result.unwrap().contains("No files found"));
    }

    #[tokio::test]
    async fn test_execute_list_directory() {
        let dir = std::env::temp_dir().join(format!("loopr-res-ld-{}", crate::id::generate_id()));
        std::fs::create_dir_all(dir.join("mydir")).unwrap();
        std::fs::write(dir.join("mydir/a.txt"), "").unwrap();
        std::fs::write(dir.join("mydir/b.txt"), "").unwrap();

        let result = execute_list_directory(&dir, "mydir").await;
        assert!(result.is_ok());
        let output = result.unwrap();
        assert!(output.contains("a.txt"));
        assert!(output.contains("b.txt"));
    }

    #[tokio::test]
    async fn test_execute_list_directory_not_a_dir() {
        let dir = std::env::temp_dir().join(format!("loopr-res-ldfile-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("file.txt"), "").unwrap();

        let result = execute_list_directory(&dir, "file.txt").await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not a directory"));
    }

    // --- run_researcher tests ---

    #[tokio::test]
    async fn test_researcher_done_on_first_iteration() {
        let dir = std::env::temp_dir().join(format!("loopr-res-done-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = test_stores(&dir);
        let (event_tx, _rx) = broadcast::channel(16);

        let llm = MockLlm::new(vec![
            r#"[{"action": "done", "summary": "Found the pattern"}]"#.to_string(),
        ]);

        let mut session = AgentSession::new(AgentType::Researcher, "test-model".into());
        session.query = Some("Find error handling".into());
        session.target_id = Some("wi-1".into());
        let _ = session.transition_to(AgentStatus::Running);
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session.id.clone(), session.clone());

        let config = AgentRoleConfig::default_researcher();
        let worktree_mgr = crate::worktree::manager::WorktreeManager::new(dir.clone(), dir.join(".wt"));
        let bridge = AgentIpcBridge::new(stores.clone(), event_tx.clone(), worktree_mgr, stores.config.clone());

        let agent_log = test_agent_logger(&dir);
        let result = run_researcher(&llm, &mut session, &stores, &bridge, &config, &event_tx, &agent_log).await;
        assert!(result.is_ok());
        assert_eq!(session.iteration, 1);
    }

    #[tokio::test]
    async fn test_researcher_need_help_exits() {
        let dir = std::env::temp_dir().join(format!("loopr-res-help-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = test_stores(&dir);
        let (event_tx, _rx) = broadcast::channel(16);

        let llm = MockLlm::new(vec![
            r#"[{"action": "need_help", "reason": "Cannot find module"}]"#.to_string(),
        ]);

        let mut session = AgentSession::new(AgentType::Researcher, "test-model".into());
        session.query = Some("Find module X".into());
        let _ = session.transition_to(AgentStatus::Running);
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session.id.clone(), session.clone());

        let config = AgentRoleConfig::default_researcher();
        let worktree_mgr = crate::worktree::manager::WorktreeManager::new(dir.clone(), dir.join(".wt"));
        let bridge = AgentIpcBridge::new(stores.clone(), event_tx.clone(), worktree_mgr, stores.config.clone());

        let agent_log = test_agent_logger(&dir);
        let result = run_researcher(&llm, &mut session, &stores, &bridge, &config, &event_tx, &agent_log).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("needs help"));
    }

    #[tokio::test]
    async fn test_researcher_max_iterations() {
        let dir = std::env::temp_dir().join(format!("loopr-res-maxiter-{}", crate::id::generate_id()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/test.rs"), "fn main() {}").unwrap();
        let stores = test_stores(&dir);
        let (event_tx, _rx) = broadcast::channel(16);

        // Always return a continue action — should hit max iterations
        let mut responses = Vec::new();
        for _ in 0..15 {
            responses.push(
                r#"[{"action": "search_code", "pattern": "fn main", "glob": "*.rs", "path": "src"}]"#.to_string(),
            );
        }
        let llm = MockLlm::new(responses);

        let mut session = AgentSession::new(AgentType::Researcher, "test-model".into());
        session.query = Some("Search forever".into());
        let _ = session.transition_to(AgentStatus::Running);
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session.id.clone(), session.clone());

        let mut config = AgentRoleConfig::default_researcher();
        config.max_iterations = 3; // Low cap for testing

        let worktree_mgr = crate::worktree::manager::WorktreeManager::new(dir.clone(), dir.join(".wt"));
        let bridge = AgentIpcBridge::new(stores.clone(), event_tx.clone(), worktree_mgr, stores.config.clone());

        let agent_log = test_agent_logger(&dir);
        let result = run_researcher(&llm, &mut session, &stores, &bridge, &config, &event_tx, &agent_log).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("max iterations"));
        assert_eq!(session.iteration, 3);
    }

    #[tokio::test]
    async fn test_researcher_exits_on_cancellation() {
        let dir = std::env::temp_dir().join(format!("loopr-res-canc-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = test_stores(&dir);
        let (event_tx, _rx) = broadcast::channel(16);

        let llm = MockLlm::new(vec![]);

        let mut session = AgentSession::new(AgentType::Researcher, "test-model".into());
        let _ = session.transition_to(AgentStatus::Running);
        let _ = session.transition_to(AgentStatus::Cancelled);
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session.id.clone(), session.clone());

        let config = AgentRoleConfig::default_researcher();
        let worktree_mgr = crate::worktree::manager::WorktreeManager::new(dir.clone(), dir.join(".wt"));
        let bridge = AgentIpcBridge::new(stores.clone(), event_tx.clone(), worktree_mgr, stores.config.clone());

        let agent_log = test_agent_logger(&dir);
        let result = run_researcher(&llm, &mut session, &stores, &bridge, &config, &event_tx, &agent_log).await;
        assert!(result.is_ok()); // Cancelled = graceful exit
    }

    #[tokio::test]
    async fn test_researcher_blocks_write_actions() {
        let dir = std::env::temp_dir().join(format!("loopr-res-block-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = test_stores(&dir);
        let (event_tx, _rx) = broadcast::channel(16);

        // LLM tries to write a file — should be blocked
        let llm = MockLlm::new(vec![
            r#"[{"action": "write_file", "path": "hack.txt", "content": "pwned"}, {"action": "done", "summary": "tried to write"}]"#.to_string(),
        ]);

        let mut session = AgentSession::new(AgentType::Researcher, "test-model".into());
        session.query = Some("Test".into());
        let _ = session.transition_to(AgentStatus::Running);
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session.id.clone(), session.clone());

        let config = AgentRoleConfig::default_researcher();
        let worktree_mgr = crate::worktree::manager::WorktreeManager::new(dir.clone(), dir.join(".wt"));
        let bridge = AgentIpcBridge::new(stores.clone(), event_tx.clone(), worktree_mgr, stores.config.clone());

        let agent_log = test_agent_logger(&dir);
        let result = run_researcher(&llm, &mut session, &stores, &bridge, &config, &event_tx, &agent_log).await;
        assert!(result.is_ok()); // Completes with Done

        // Verify file was NOT written
        assert!(!dir.join("hack.txt").exists());
    }

    // --- format_action_summary tests ---

    #[test]
    fn test_format_action_summary_search_code() {
        let action = AgentAction::SearchCode {
            pattern: "fn test".into(),
            glob: None,
            path: None,
        };
        let result = ActionResult::FileRead("line1\nline2\nline3".into());
        let summary = format_action_summary(&action, &result);
        assert!(summary.contains("search_code"));
        assert!(summary.contains("3 lines"));
    }

    #[test]
    fn test_format_action_summary_list_dir() {
        let action = AgentAction::ListDirectory { path: "src".into() };
        let result = ActionResult::FileRead("mod.rs\nmain.rs".into());
        let summary = format_action_summary(&action, &result);
        assert!(summary.contains("list_directory"));
        assert!(summary.contains("2 entries"));
    }

    #[test]
    fn test_format_action_summary_done() {
        let action = AgentAction::Done {
            summary: "Complete".into(),
        };
        let result = ActionResult::Done("Complete".into());
        assert_eq!(format_action_summary(&action, &result), "done: Complete");
    }

    // --- New coverage tests ---

    #[test]
    fn test_validate_path_rejects_parent_dir() {
        let dir = std::env::temp_dir().join(format!("loopr-res-parent-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();

        let result = validate_path(&dir, "../etc/passwd");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("path traversal"));
    }

    #[test]
    fn test_validate_path_nonexistent_file_in_repo() {
        // A nonexistent file that is within the repo root should be accepted
        let dir = std::env::temp_dir().join(format!("loopr-res-nonexist-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();

        let result = validate_path(&dir, "src/does_not_exist.rs");
        assert!(result.is_ok());
        // The path should be within the repo root
        let full = result.unwrap();
        assert!(full.starts_with(&dir));
    }

    #[tokio::test]
    async fn test_execute_search_code_truncation_over_100_lines() {
        let dir = std::env::temp_dir().join(format!("loopr-res-sc-trunc-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();

        // Create a file with 150 lines that all match
        let content: String = (0..150).map(|i| format!("fn match_line_{i}() {{}}\n")).collect();
        std::fs::write(dir.join("big.rs"), &content).unwrap();

        let result = execute_search_code(&dir, "match_line", None, None).await.unwrap();
        // Should be truncated — 100 lines + truncation message
        assert!(result.contains("truncated"));
        let line_count = result.lines().count();
        assert_eq!(line_count, 101); // 100 result lines + 1 truncation line
    }

    #[tokio::test]
    async fn test_execute_search_files_truncation_at_200() {
        let dir = std::env::temp_dir().join(format!("loopr-res-sf-trunc-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();

        // Create 210 files
        for i in 0..210 {
            std::fs::write(dir.join(format!("file_{i:03}.txt")), "").unwrap();
        }

        let result = execute_search_files(&dir, "*.txt", None).await.unwrap();
        // Should have truncation
        assert!(result.contains("truncated"));
        // Should have at most 201 lines (200 files + truncation message)
        let line_count = result.lines().count();
        assert!(line_count <= 201);
    }

    #[tokio::test]
    async fn test_execute_list_directory_empty_dir() {
        let dir = std::env::temp_dir().join(format!("loopr-res-ld-empty-{}", crate::id::generate_id()));
        std::fs::create_dir_all(dir.join("emptydir")).unwrap();

        let result = execute_list_directory(&dir, "emptydir").await.unwrap();
        assert_eq!(result, "(empty directory)");
    }

    #[tokio::test]
    async fn test_execute_list_directory_entry_truncation_at_500() {
        let dir = std::env::temp_dir().join(format!("loopr-res-ld-trunc-{}", crate::id::generate_id()));
        let sub = dir.join("bigdir");
        std::fs::create_dir_all(&sub).unwrap();

        // Create 510 files
        for i in 0..510 {
            std::fs::write(sub.join(format!("entry_{i:04}.txt")), "").unwrap();
        }

        let result = execute_list_directory(&dir, "bigdir").await.unwrap();
        assert!(result.contains("truncated"));
        // Sort happens after truncation; entries are capped at 501 (500 + truncation marker)
        let line_count = result.lines().count();
        assert!(line_count <= 501);
    }

    #[tokio::test]
    async fn test_run_researcher_iteration_empty_actions_returns_done() {
        // When the LLM returns an empty action array, the researcher should finish (Done).
        let dir = std::env::temp_dir().join(format!("loopr-res-empty-act-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = test_stores(&dir);
        let (event_tx, _rx) = broadcast::channel(16);

        // Return empty actions array
        let llm = MockLlm::new(vec![r#"[]"#.to_string()]);

        let mut session = AgentSession::new(AgentType::Researcher, "test-model".into());
        session.query = Some("Find something".into());
        let _ = session.transition_to(AgentStatus::Running);
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session.id.clone(), session.clone());

        let config = AgentRoleConfig::default_researcher();
        let worktree_mgr = crate::worktree::manager::WorktreeManager::new(dir.clone(), dir.join(".wt"));
        let bridge = AgentIpcBridge::new(stores.clone(), event_tx.clone(), worktree_mgr, stores.config.clone());

        let agent_log = test_agent_logger(&dir);
        let result = run_researcher(&llm, &mut session, &stores, &bridge, &config, &event_tx, &agent_log).await;
        // Empty actions → Done("No actions returned") → Ok(())
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_run_researcher_iteration_mixed_allowed_disallowed() {
        // Mix of disallowed (write_file) and allowed (done) actions — should skip write, process done.
        let dir = std::env::temp_dir().join(format!("loopr-res-mixed-{}", crate::id::generate_id()));
        std::fs::create_dir_all(&dir).unwrap();
        let stores = test_stores(&dir);
        let (event_tx, _rx) = broadcast::channel(16);

        let llm = MockLlm::new(vec![
            r#"[{"action": "write_file", "path": "bad.txt", "content": "nope"}, {"action": "done", "summary": "mixed test done"}]"#.to_string(),
        ]);

        let mut session = AgentSession::new(AgentType::Researcher, "test-model".into());
        session.query = Some("Mixed test".into());
        let _ = session.transition_to(AgentStatus::Running);
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session.id.clone(), session.clone());

        let config = AgentRoleConfig::default_researcher();
        let worktree_mgr = crate::worktree::manager::WorktreeManager::new(dir.clone(), dir.join(".wt"));
        let bridge = AgentIpcBridge::new(stores.clone(), event_tx.clone(), worktree_mgr, stores.config.clone());

        let agent_log = test_agent_logger(&dir);
        let result = run_researcher(&llm, &mut session, &stores, &bridge, &config, &event_tx, &agent_log).await;
        assert!(result.is_ok());
        // Write file should NOT have been created
        assert!(!dir.join("bad.txt").exists());
    }

    #[test]
    fn test_format_action_summary_other_catch_all() {
        // Test the `other => format!("{:?}", other)` branch in format_action_summary.
        let action = AgentAction::SearchCode {
            pattern: "test".into(),
            glob: None,
            path: None,
        };
        // Use a variant that hits the catch-all: e.g., Committed
        let result = ActionResult::Committed("abc123".into());
        let summary = format_action_summary(&action, &result);
        assert!(summary.contains("Committed"));
    }
}
