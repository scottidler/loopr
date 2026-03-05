use std::path::{Path, PathBuf};

use async_trait::async_trait;
use eyre::{Result, eyre};

use crate::agents::agent_logger::AgentLogger;
use crate::agents::context::ContextBuilder;
use crate::agents::executor::{ActionResult, execute_action};
use crate::agents::implementer::{self, ChatMessage, IterationOutcome, LlmClient};
use crate::agents::lifeguard::{self, Lifeguard, Verdict};
use crate::agents::sandbox;
use crate::agents::{Agent, AgentAction, AgentContext, AgentStatus, AgentType};
use crate::config::AgentRoleConfig;
use crate::domain::role::Role;
use crate::ipc::protocol::DaemonEvent;

/// Validate that a path is safe for the Researcher to access.
/// Delegates to the shared sandbox module with denylist enabled.
pub fn validate_path(repo_root: &Path, relative: &str, agent_log: &AgentLogger) -> Result<PathBuf> {
    agent_log.debug(&format!(
        "validate_path(repo_root={}, relative={})",
        repo_root.display(),
        relative
    ));
    sandbox::validate_sandboxed_path(repo_root, relative, true)
}

/// Build the Researcher system prompt with the query injected.
fn build_system_prompt(query: &str, agent_log: &AgentLogger) -> String {
    agent_log.debug(&format!("build_system_prompt(query_len={})", query.len()));
    crate::prompts::store().researcher.replace("{query}", query)
}

/// Execute a SearchCode action: use grep/ripgrep to search file contents.
pub async fn execute_search_code(
    repo_root: &Path,
    pattern: &str,
    glob_filter: Option<&str>,
    search_path: Option<&str>,
    agent_log: &AgentLogger,
) -> Result<String> {
    agent_log.debug(&format!(
        "execute_search_code(pattern={}, glob={:?}, path={:?})",
        pattern, glob_filter, search_path
    ));
    let search_dir = match search_path {
        Some(p) => validate_path(repo_root, p, agent_log)?,
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
pub async fn execute_search_files(
    repo_root: &Path,
    pattern: &str,
    search_path: Option<&str>,
    agent_log: &AgentLogger,
) -> Result<String> {
    let search_dir = match search_path {
        Some(p) => validate_path(repo_root, p, agent_log)?,
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
                    if validate_path(repo_root, &rel_str, agent_log).is_ok() {
                        results.push(rel_str.to_string());
                    }
                }
            }
            Err(e) => {
                agent_log.warn(&format!("glob error: {}", e));
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
pub async fn execute_list_directory(repo_root: &Path, path: &str, agent_log: &AgentLogger) -> Result<String> {
    let dir = validate_path(repo_root, path, agent_log)?;

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

/// The Researcher agent — multi-iteration LLM agent for codebase investigation.
pub struct ResearcherAgent {
    pub ctx: AgentContext,
    llm: Box<dyn LlmClient>,
    config: AgentRoleConfig,
    previous_summary: Option<String>,
}

impl ResearcherAgent {
    pub fn new(ctx: AgentContext, llm: Box<dyn LlmClient>, config: AgentRoleConfig) -> Self {
        Self {
            ctx,
            llm,
            config,
            previous_summary: None,
        }
    }

    async fn run_iteration(&self, iteration: u32, guard: &mut Lifeguard) -> Result<IterationOutcome> {
        let query = self.ctx.session.query.as_deref().unwrap_or("(no query specified)");
        let system_prompt = build_system_prompt(query, &self.ctx.log);

        let builder = ContextBuilder::new(&self.ctx.stores, Role::Researcher).with_guidance(&self.ctx.stores.guidance);

        let footer = format!(
            "Iteration {}: Investigate the codebase to answer the query. Respond with a JSON array of actions.",
            iteration
        );

        let builder = builder
            .with_previous_summary(self.previous_summary.clone())
            .with_iteration(iteration)
            .with_footer(footer);

        let assembled = builder.build(&system_prompt);

        self.ctx.info(&format!(
            "iteration {} context: ~{} tokens",
            iteration, assembled.token_estimate
        ));

        log::debug!("[agent_status] {}: -> WaitingForLlm", self.ctx.session.id);
        let _ = self.ctx.event_tx.send(DaemonEvent::agent_status_changed(
            &self.ctx.session.id,
            AgentStatus::WaitingForLlm,
        ));

        // Self-correction loop: re-prompt on parse failure up to max_requeries times
        let mut messages = vec![ChatMessage::user(&assembled.user_message)];
        let mut requeries = 0u32;
        let max_requeries = self.config.max_requeries;

        let actions = loop {
            let response = self.llm.call_with_history(&assembled.system_prompt, &messages).await?;

            match implementer::parse_actions(&response, &self.ctx.log) {
                Ok(actions) => break actions,
                Err(parse_err) => {
                    requeries += 1;
                    if requeries > max_requeries {
                        return Err(parse_err);
                    }
                    self.ctx.info(&format!(
                        "parse failed (requery {}/{}): {}",
                        requeries, max_requeries, parse_err
                    ));
                    messages.push(ChatMessage::assistant(&response));
                    messages.push(ChatMessage::user(&format!(
                        "Your response could not be parsed as a valid JSON action array.\n\
                         Error: {}\n\n\
                         Please respond with ONLY a valid JSON array of actions. \
                         Do not include any text before or after the JSON.",
                        parse_err
                    )));
                }
            }
        };

        log::debug!("[agent_status] {}: -> Running (LLM complete)", self.ctx.session.id);
        let _ = self.ctx.event_tx.send(DaemonEvent::agent_status_changed(
            &self.ctx.session.id,
            AgentStatus::Running,
        ));

        if actions.is_empty() {
            return Ok(IterationOutcome::Done("No actions returned".to_string()));
        }

        guard.reset_parse_failures();

        let repo_root = &self.ctx.stores.config.project.repo_path;

        let mut last_summary = String::new();
        for action in &actions {
            // Lifeguard: check for repeated identical actions
            let action_hash = lifeguard::hash_action(action);
            if let Verdict::Escalate(reason) = guard.check_action(action_hash) {
                self.ctx.warn(&format!("lifeguard: {}", reason));
                return Ok(IterationOutcome::NeedHelp(format!("lifeguard: {}", reason)));
            }

            if !is_allowed_researcher_action(action) {
                self.ctx.warn(&format!("attempted disallowed action: {:?}", action));
                last_summary = format!("ERROR: action not allowed for Researcher: {:?}", action);
                continue;
            }

            let result = match action {
                AgentAction::SearchCode { pattern, glob, path } => {
                    match execute_search_code(repo_root, pattern, glob.as_deref(), path.as_deref(), &self.ctx.log).await
                    {
                        Ok(output) => ActionResult::FileRead(output),
                        Err(e) => ActionResult::ActionError(e.to_string()),
                    }
                }
                AgentAction::SearchFiles { pattern, path } => {
                    match execute_search_files(repo_root, pattern, path.as_deref(), &self.ctx.log).await {
                        Ok(output) => ActionResult::FileRead(output),
                        Err(e) => ActionResult::ActionError(e.to_string()),
                    }
                }
                AgentAction::ListDirectory { path } => {
                    match execute_list_directory(repo_root, path, &self.ctx.log).await {
                        Ok(output) => ActionResult::FileRead(output),
                        Err(e) => ActionResult::ActionError(e.to_string()),
                    }
                }
                AgentAction::ReadFile { path } => match validate_path(repo_root, path, &self.ctx.log) {
                    Ok(full_path) => match tokio::fs::read_to_string(&full_path).await {
                        Ok(content) => {
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
                },
                _ => match execute_action(action, &self.ctx, repo_root, None).await {
                    Ok(r) => r,
                    Err(e) => {
                        self.ctx.warn(&format!("action failed (non-fatal): {e}"));
                        ActionResult::ActionError(e.to_string())
                    }
                },
            };

            let summary = format_action_summary(action, &result);
            let _ = self
                .ctx
                .event_tx
                .send(DaemonEvent::agent_action_completed(&self.ctx.session.id, &summary));

            match &result {
                ActionResult::Done(s) => return Ok(IterationOutcome::Done(s.clone())),
                ActionResult::NeedHelp(reason) => return Ok(IterationOutcome::NeedHelp(reason.clone())),
                _ => {}
            }
            last_summary = summary;
        }

        Ok(IterationOutcome::Continue(last_summary))
    }
}

#[async_trait]
impl Agent for ResearcherAgent {
    async fn run(&mut self) -> Result<()> {
        self.ctx.debug(&format!("run(session_id={})", self.ctx.session.id));
        self.ctx.info(&format!(
            "Researcher starting (query: {:?}, target: {:?})",
            self.ctx.session.query.as_deref().unwrap_or("none"),
            self.ctx.session.target_id.as_deref().unwrap_or("none"),
        ));

        let mut guard = Lifeguard::new();

        for iteration in 1..=self.config.max_iterations {
            if self.ctx.is_cancelled() {
                self.ctx.info("cancelled, exiting loop");
                return Ok(());
            }

            self.ctx.session.iteration = iteration;
            self.ctx
                .info(&format!("iteration {}/{}", iteration, self.config.max_iterations));

            let outcome = self.run_iteration(iteration, &mut guard).await;

            match outcome {
                Ok(IterationOutcome::Done(summary)) => {
                    self.ctx.emit_iteration_completed(iteration, &summary);
                    self.ctx.info(&format!("done: {}", summary));
                    return Ok(());
                }
                Ok(IterationOutcome::Continue(summary)) => {
                    self.ctx.emit_iteration_completed(iteration, &summary);
                    self.ctx.info(&format!("continue: {}", summary));
                    self.previous_summary = Some(summary);
                }
                Ok(IterationOutcome::NeedHelp(reason)) => {
                    self.ctx.emit_iteration_completed(iteration, &reason);
                    self.ctx.warn(&format!("needs help: {}", reason));
                    return Err(eyre!("researcher needs help: {}", reason));
                }
                Err(e) => {
                    self.ctx.warn(&format!("iteration {} failed: {}", iteration, e));
                    return Err(eyre!("researcher iteration failed: {}", e));
                }
            }
        }

        self.ctx
            .warn(&format!("reached max iterations ({})", self.config.max_iterations));
        Err(eyre!(
            "researcher reached max iterations ({})",
            self.config.max_iterations
        ))
    }

    fn agent_type(&self) -> AgentType {
        AgentType::Researcher
    }
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

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::agents::bridge::AgentIpcBridge;
    use crate::agents::{AgentContext, AgentSession, AgentType};
    use crate::config::{AgentRoleConfig, Config, ProjectConfig};
    use crate::daemon::context::Stores;
    use crate::test_util::TestDir;
    use crate::tools::ToolRunner;
    use crate::worktree::manager::WorktreeManager;
    use async_trait::async_trait;
    use std::path::Path;
    use std::sync::{Arc, Mutex as StdMutex};
    use taskstore::Store;
    use tokio::sync::broadcast;

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
        let file_path = dir.join("test-researcher.log");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&file_path)
            .unwrap();
        AgentLogger::_new_for_test(AgentType::Researcher, "test-session", file, file_path)
    }

    fn test_researcher_agent(dir: &Path, stores: Arc<Stores>, llm: Box<dyn LlmClient>) -> ResearcherAgent {
        let (event_tx, _) = broadcast::channel(64);
        let worktree_mgr = WorktreeManager::new(dir.to_path_buf(), dir.join(".worktrees"));
        let bridge = AgentIpcBridge::new(stores.clone(), event_tx.clone(), worktree_mgr, stores.config.clone());
        let agent_log = test_agent_logger(dir);
        let config = AgentRoleConfig::default_researcher();
        let mut session = AgentSession::new(AgentType::Researcher, "test".into());
        session.query = Some("test query".into());
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session.id.clone(), session.clone());
        let ctx = AgentContext {
            session,
            stores,
            bridge,
            event_tx,
            tool_runner: Arc::new(ToolRunner::new(&[])),
            tool_executor: Arc::new(crate::tools::ToolExecutor::standard(&[])),
            log: agent_log,
        };
        ResearcherAgent::new(ctx, llm, config)
    }

    // --- Path sandboxing tests ---

    #[test]
    fn test_validate_path_relative_ok() {
        let dir = TestDir::new("loopr-res-pathok");
        std::fs::write(dir.join("test.rs"), "fn main() {}").unwrap();
        let agent_log = test_agent_logger(&dir);

        let result = validate_path(&dir, "test.rs", &agent_log);
        assert!(result.is_ok());
    }

    #[test]
    fn test_validate_path_rejects_absolute() {
        let dir = TestDir::new("loopr-res-pathabs");
        let agent_log = test_agent_logger(&dir);

        let result = validate_path(&dir, "/etc/passwd", &agent_log);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("absolute"));
    }

    #[test]
    fn test_validate_path_rejects_env_file() {
        let dir = TestDir::new("loopr-res-pathenv");
        let agent_log = test_agent_logger(&dir);

        let result = validate_path(&dir, ".env", &agent_log);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("security policy"));
    }

    #[test]
    fn test_validate_path_rejects_key_file() {
        let dir = TestDir::new("loopr-res-pathkey");
        let agent_log = test_agent_logger(&dir);

        let result = validate_path(&dir, "server.key", &agent_log);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("security policy"));
    }

    #[test]
    fn test_validate_path_rejects_pem_file() {
        let dir = TestDir::new("loopr-res-pathpem");
        let agent_log = test_agent_logger(&dir);

        let result = validate_path(&dir, "cert.pem", &agent_log);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("security policy"));
    }

    #[test]
    fn test_validate_path_rejects_credentials() {
        let dir = TestDir::new("loopr-res-pathcred");
        let agent_log = test_agent_logger(&dir);

        let result = validate_path(&dir, "credentials.json", &agent_log);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("security policy"));
    }

    #[test]
    fn test_validate_path_rejects_secret_file() {
        let dir = TestDir::new("loopr-res-pathsec");
        let agent_log = test_agent_logger(&dir);

        let result = validate_path(&dir, "my_secret_config.yml", &agent_log);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("security policy"));
    }

    #[test]
    fn test_validate_path_allows_normal_files() {
        let dir = TestDir::new("loopr-res-pathnorm");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/main.rs"), "fn main() {}").unwrap();
        let agent_log = test_agent_logger(&dir);

        assert!(validate_path(&dir, "src/main.rs", &agent_log).is_ok());
        assert!(validate_path(&dir, "Cargo.toml", &agent_log).is_ok());
    }

    // --- System prompt tests ---

    #[test]
    fn test_system_prompt_contains_query_placeholder() {
        crate::prompts::init_defaults();
        assert!(crate::prompts::store().researcher.contains("{query}"));
    }

    #[test]
    fn test_build_system_prompt_injects_query() {
        let dir = TestDir::new("loopr-res-sysprompt");
        let agent_log = test_agent_logger(&dir);
        let prompt = build_system_prompt("Find all error handling patterns", &agent_log);
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
        let dir = TestDir::new("loopr-res-sc");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/test.rs"), "fn hello_world() {}\nfn goodbye() {}").unwrap();
        let agent_log = test_agent_logger(&dir);

        let result = execute_search_code(&dir, "hello_world", Some("*.rs"), Some("src"), &agent_log).await;
        assert!(result.is_ok());
        let output = result.unwrap();
        assert!(output.contains("hello_world"));
    }

    #[tokio::test]
    async fn test_execute_search_code_no_matches() {
        let dir = TestDir::new("loopr-res-scnone");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/test.rs"), "fn main() {}").unwrap();
        let agent_log = test_agent_logger(&dir);

        let result = execute_search_code(&dir, "nonexistent_pattern_xyz", None, Some("src"), &agent_log).await;
        assert!(result.is_ok());
        assert!(result.unwrap().contains("No matches"));
    }

    #[tokio::test]
    async fn test_execute_search_files_finds_files() {
        let dir = TestDir::new("loopr-res-sf");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/foo.rs"), "").unwrap();
        std::fs::write(dir.join("src/bar.rs"), "").unwrap();
        let agent_log = test_agent_logger(&dir);

        let result = execute_search_files(&dir, "*.rs", Some("src"), &agent_log).await;
        assert!(result.is_ok());
        let output = result.unwrap();
        assert!(output.contains("foo.rs") || output.contains("bar.rs"));
    }

    #[tokio::test]
    async fn test_execute_search_files_no_matches() {
        let dir = TestDir::new("loopr-res-sfnone");
        let agent_log = test_agent_logger(&dir);

        let result = execute_search_files(&dir, "*.xyz", None, &agent_log).await;
        assert!(result.is_ok());
        assert!(result.unwrap().contains("No files found"));
    }

    #[tokio::test]
    async fn test_execute_list_directory() {
        let dir = TestDir::new("loopr-res-ld");
        std::fs::create_dir_all(dir.join("mydir")).unwrap();
        std::fs::write(dir.join("mydir/a.txt"), "").unwrap();
        std::fs::write(dir.join("mydir/b.txt"), "").unwrap();
        let agent_log = test_agent_logger(&dir);

        let result = execute_list_directory(&dir, "mydir", &agent_log).await;
        assert!(result.is_ok());
        let output = result.unwrap();
        assert!(output.contains("a.txt"));
        assert!(output.contains("b.txt"));
    }

    #[tokio::test]
    async fn test_execute_list_directory_not_a_dir() {
        let dir = TestDir::new("loopr-res-ldfile");
        std::fs::write(dir.join("file.txt"), "").unwrap();
        let agent_log = test_agent_logger(&dir);

        let result = execute_list_directory(&dir, "file.txt", &agent_log).await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not a directory"));
    }

    // --- ResearcherAgent tests ---

    #[tokio::test]
    async fn test_researcher_done_on_first_iteration() {
        let dir = TestDir::new("loopr-res-done");
        let stores = test_stores(&dir);

        let llm: Box<dyn LlmClient> = Box::new(MockLlm::new(vec![
            r#"[{"action": "done", "summary": "Found the pattern"}]"#.to_string(),
        ]));
        let mut agent = test_researcher_agent(&dir, stores, llm);
        let result = agent.run().await;
        assert!(result.is_ok());
        assert_eq!(agent.ctx.session.iteration, 1);
    }

    #[tokio::test]
    async fn test_researcher_need_help_exits() {
        let dir = TestDir::new("loopr-res-help");
        let stores = test_stores(&dir);

        let llm: Box<dyn LlmClient> = Box::new(MockLlm::new(vec![
            r#"[{"action": "need_help", "reason": "Cannot find module"}]"#.to_string(),
        ]));
        let mut agent = test_researcher_agent(&dir, stores, llm);
        let result = agent.run().await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("needs help"));
    }

    #[tokio::test]
    async fn test_researcher_max_iterations() {
        let dir = TestDir::new("loopr-res-maxiter");
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("src/test.rs"), "fn main() {}").unwrap();
        let stores = test_stores(&dir);

        let mut responses = Vec::new();
        for _ in 0..15 {
            responses.push(
                r#"[{"action": "search_code", "pattern": "fn main", "glob": "*.rs", "path": "src"}]"#.to_string(),
            );
        }
        let llm: Box<dyn LlmClient> = Box::new(MockLlm::new(responses));

        // Need custom config with max_iterations = 3
        let (event_tx, _) = broadcast::channel(64);
        let worktree_mgr = WorktreeManager::new(dir.to_path_buf(), dir.join(".worktrees"));
        let bridge = AgentIpcBridge::new(stores.clone(), event_tx.clone(), worktree_mgr, stores.config.clone());
        let agent_log = test_agent_logger(&dir);
        let mut config = AgentRoleConfig::default_researcher();
        config.max_iterations = 3;
        let mut session = AgentSession::new(AgentType::Researcher, "test".into());
        session.query = Some("Search forever".into());
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session.id.clone(), session.clone());
        let ctx = AgentContext {
            session,
            stores,
            bridge,
            event_tx,
            tool_runner: Arc::new(ToolRunner::new(&[])),
            tool_executor: Arc::new(crate::tools::ToolExecutor::standard(&[])),
            log: agent_log,
        };
        let mut agent = ResearcherAgent::new(ctx, llm, config);
        let result = agent.run().await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("max iterations"));
        assert_eq!(agent.ctx.session.iteration, 3);
    }

    #[tokio::test]
    async fn test_researcher_exits_on_cancellation() {
        let dir = TestDir::new("loopr-res-canc");
        let stores = test_stores(&dir);

        let llm: Box<dyn LlmClient> = Box::new(MockLlm::new(vec![]));
        let mut agent = test_researcher_agent(&dir, stores.clone(), llm);

        // Cancel the agent's session
        {
            let mut sessions = stores.agent_sessions.write().unwrap();
            if let Some(s) = sessions.get_mut(&agent.ctx.session.id) {
                let _ = s.transition_to(AgentStatus::Running);
                let _ = s.transition_to(AgentStatus::Cancelled);
            }
        }

        let result = agent.run().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_researcher_blocks_write_actions() {
        let dir = TestDir::new("loopr-res-block");
        let stores = test_stores(&dir);

        let llm: Box<dyn LlmClient> = Box::new(MockLlm::new(vec![
            r#"[{"action": "write_file", "path": "hack.txt", "content": "pwned"}, {"action": "done", "summary": "tried to write"}]"#.to_string(),
        ]));
        let mut agent = test_researcher_agent(&dir, stores, llm);
        let result = agent.run().await;
        assert!(result.is_ok());

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
        let dir = TestDir::new("loopr-res-parent");
        let agent_log = test_agent_logger(&dir);

        let result = validate_path(&dir, "../etc/passwd", &agent_log);
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("path traversal"));
    }

    #[test]
    fn test_validate_path_nonexistent_file_in_repo() {
        // A nonexistent file that is within the repo root should be accepted
        let dir = TestDir::new("loopr-res-nonexist");
        let agent_log = test_agent_logger(&dir);

        let result = validate_path(&dir, "src/does_not_exist.rs", &agent_log);
        assert!(result.is_ok());
        // The path should be within the repo root
        let full = result.unwrap();
        assert!(full.starts_with(&dir));
    }

    #[tokio::test]
    async fn test_execute_search_code_truncation_over_100_lines() {
        let dir = TestDir::new("loopr-res-sc-trunc");
        let agent_log = test_agent_logger(&dir);

        // Create a file with 150 lines that all match
        let content: String = (0..150).map(|i| format!("fn match_line_{i}() {{}}\n")).collect();
        std::fs::write(dir.join("big.rs"), &content).unwrap();

        let result = execute_search_code(&dir, "match_line", None, None, &agent_log)
            .await
            .unwrap();
        // Should be truncated — 100 lines + truncation message
        assert!(result.contains("truncated"));
        let line_count = result.lines().count();
        assert_eq!(line_count, 101); // 100 result lines + 1 truncation line
    }

    #[tokio::test]
    async fn test_execute_search_files_truncation_at_200() {
        let dir = TestDir::new("loopr-res-sf-trunc");
        let agent_log = test_agent_logger(&dir);

        // Create 210 files
        for i in 0..210 {
            std::fs::write(dir.join(format!("file_{i:03}.txt")), "").unwrap();
        }

        let result = execute_search_files(&dir, "*.txt", None, &agent_log).await.unwrap();
        // Should have truncation
        assert!(result.contains("truncated"));
        // Should have at most 201 lines (200 files + truncation message)
        let line_count = result.lines().count();
        assert!(line_count <= 201);
    }

    #[tokio::test]
    async fn test_execute_list_directory_empty_dir() {
        let dir = TestDir::new("loopr-res-ld-empty");
        std::fs::create_dir_all(dir.join("emptydir")).unwrap();
        let agent_log = test_agent_logger(&dir);

        let result = execute_list_directory(&dir, "emptydir", &agent_log).await.unwrap();
        assert_eq!(result, "(empty directory)");
    }

    #[tokio::test]
    async fn test_execute_list_directory_entry_truncation_at_500() {
        let dir = TestDir::new("loopr-res-ld-trunc");
        let sub = dir.join("bigdir");
        std::fs::create_dir_all(&sub).unwrap();
        let agent_log = test_agent_logger(&dir);

        // Create 510 files
        for i in 0..510 {
            std::fs::write(sub.join(format!("entry_{i:04}.txt")), "").unwrap();
        }

        let result = execute_list_directory(&dir, "bigdir", &agent_log).await.unwrap();
        assert!(result.contains("truncated"));
        // Sort happens after truncation; entries are capped at 501 (500 + truncation marker)
        let line_count = result.lines().count();
        assert!(line_count <= 501);
    }

    #[tokio::test]
    async fn test_run_researcher_iteration_empty_actions_returns_done() {
        let dir = TestDir::new("loopr-res-empty-act");
        let stores = test_stores(&dir);

        let llm: Box<dyn LlmClient> = Box::new(MockLlm::new(vec![r#"[]"#.to_string()]));
        let mut agent = test_researcher_agent(&dir, stores, llm);
        let result = agent.run().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_run_researcher_iteration_mixed_allowed_disallowed() {
        let dir = TestDir::new("loopr-res-mixed");
        let stores = test_stores(&dir);

        let llm: Box<dyn LlmClient> = Box::new(MockLlm::new(vec![
            r#"[{"action": "write_file", "path": "bad.txt", "content": "nope"}, {"action": "done", "summary": "mixed test done"}]"#.to_string(),
        ]));
        let mut agent = test_researcher_agent(&dir, stores, llm);
        let result = agent.run().await;
        assert!(result.is_ok());
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

    // --- Self-correction loop tests for Researcher ---

    #[tokio::test]
    async fn test_researcher_self_correction_parse_failure_then_success() {
        // First response is malformed, second is valid JSON.
        // Self-correction loop should re-prompt and succeed within the same iteration.
        let dir = TestDir::new("loopr-res-selfcorr1");
        let stores = test_stores(&dir);

        let llm: Box<dyn LlmClient> = Box::new(MockLlm::new(vec![
            "I'll investigate the codebase now.".to_string(), // malformed
            r#"[{"action": "done", "summary": "Self-corrected researcher"}]"#.to_string(), // valid
        ]));
        let mut agent = test_researcher_agent(&dir, stores, llm);

        let result = agent.run().await;
        assert!(
            result.is_ok(),
            "expected success after self-correction, got: {:?}",
            result.err()
        );
        // Should complete in 1 iteration (self-correction within iteration)
        assert_eq!(agent.ctx.session.iteration, 1);
    }

    #[tokio::test]
    async fn test_researcher_self_correction_max_requeries_exceeded() {
        // All responses are malformed. After max_requeries retries, should fail.
        let dir = TestDir::new("loopr-res-selfcorr2");
        let stores = test_stores(&dir);

        // max_requeries=3 default: initial + 3 retries = 4 responses, all bad
        let llm: Box<dyn LlmClient> = Box::new(MockLlm::new(vec![
            "bad 1".to_string(),
            "bad 2".to_string(),
            "bad 3".to_string(),
            "bad 4".to_string(),
        ]));

        let mut agent = test_researcher_agent(&dir, stores, llm);

        let result = agent.run().await;
        assert!(result.is_err(), "expected error when max_requeries exceeded");
        assert!(
            result.unwrap_err().to_string().contains("failed to parse"),
            "error should be a parse error"
        );
    }
}
