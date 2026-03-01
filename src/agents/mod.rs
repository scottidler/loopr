pub mod agent_logger;
pub mod bridge;
pub mod context;
pub mod coordinator;
pub mod executor;
pub mod generation;
pub mod implementer;
pub mod integrator;
pub mod lifeguard;
pub mod llm_client;
pub mod researcher;
pub mod reviewer;
pub mod sandbox;

use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;

use async_trait::async_trait;
use eyre::Result;
use serde::{Deserialize, Serialize};
use taskstore::record::{IndexValue, Record};
use tokio::sync::broadcast;

use crate::agents::agent_logger::AgentLogger;
use crate::agents::bridge::AgentIpcBridge;
use crate::daemon::context::Stores;
use crate::id;
use crate::ipc::protocol::DaemonEvent;
use crate::tools::ToolRunner;
use crate::worktree::manager::WorktreeManager;

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

        /// LLMs sometimes send `"args": {}` instead of `"args": []` — treat empty map as empty vec.
        fn visit_map<A: de::MapAccess<'de>>(self, mut map: A) -> std::result::Result<Self::Value, A::Error> {
            // Drain any entries (shouldn't be any for empty {}) and return empty vec
            while map.next_entry::<de::IgnoredAny, de::IgnoredAny>()?.is_some() {}
            Ok(Vec::new())
        }
    }

    deserializer.deserialize_any(StringOrVecVisitor)
}

/// Common trait for all agent implementations.
#[async_trait]
pub trait Agent: Send {
    /// Run the agent's main loop to completion.
    async fn run(&mut self) -> Result<()>;

    /// Agent type for dispatch and logging.
    fn agent_type(&self) -> AgentType;
}

/// Shared cross-cutting fields for all agents.
pub struct AgentContext {
    pub session: AgentSession,
    pub stores: Arc<Stores>,
    pub bridge: AgentIpcBridge,
    pub event_tx: broadcast::Sender<DaemonEvent>,
    pub tool_runner: Arc<ToolRunner>,
    pub log: AgentLogger,
}

impl AgentContext {
    /// Create an AgentContext by cloning the session from stores.
    pub fn from_session_id(
        session_id: &str,
        agent_type: AgentType,
        stores: Arc<Stores>,
        event_tx: broadcast::Sender<DaemonEvent>,
    ) -> eyre::Result<Self> {
        let session = {
            let sessions = stores.agent_sessions.read().unwrap();
            sessions
                .get(session_id)
                .ok_or_else(|| eyre::eyre!("session not found: {}", session_id))?
                .clone()
        };

        let bridge = AgentIpcBridge::new(
            stores.clone(),
            event_tx.clone(),
            WorktreeManager::new(
                stores.config.project.repo_path.clone(),
                stores.config.project.repo_path.join(".worktrees"),
            ),
            stores.config.clone(),
        );
        let log = AgentLogger::new(agent_type, session_id)?;

        Ok(Self {
            session,
            stores: stores.clone(),
            bridge,
            event_tx,
            tool_runner: stores.tool_runner.clone(),
            log,
        })
    }

    pub fn info(&self, msg: &str) {
        self.log.info(msg)
    }
    pub fn warn(&self, msg: &str) {
        self.log.warn(msg)
    }
    pub fn debug(&self, msg: &str) {
        self.log.debug(msg)
    }
    pub fn error(&self, msg: &str) {
        self.log.error(msg)
    }

    /// Check if this agent's session has been cancelled.
    pub fn is_cancelled(&self) -> bool {
        let sessions = self.stores.agent_sessions.read().unwrap();
        sessions
            .get(&self.session.id)
            .map(|s| s.status == AgentStatus::Cancelled)
            .unwrap_or(true)
    }

    /// Persist current iteration count to stores.
    pub fn persist_iteration(&self) {
        let mut sessions = self.stores.agent_sessions.write().unwrap();
        if let Some(s) = sessions.get_mut(&self.session.id) {
            s.iteration = self.session.iteration;
        }
    }

    /// Emit an iteration-completed event.
    pub fn emit_iteration_completed(&self, iteration: u32, summary: &str) {
        let _ = self.event_tx.send(DaemonEvent::agent_iteration_completed(
            &self.session.id,
            iteration,
            summary,
        ));
    }
}

/// The type of agent — determines behavior and prompt.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentType {
    Implementer,
    Reviewer,
    Coordinator,
    Researcher,
    Integrator,
}

impl AgentType {
    /// Returns the default Role corresponding to this agent type.
    pub fn default_role(&self) -> crate::domain::role::Role {
        match self {
            AgentType::Implementer => crate::domain::role::Role::Implementer,
            AgentType::Reviewer => crate::domain::role::Role::Reviewer,
            AgentType::Coordinator => crate::domain::role::Role::Coordinator,
            AgentType::Researcher => crate::domain::role::Role::Researcher,
            AgentType::Integrator => crate::domain::role::Role::Integrator,
        }
    }

    /// Returns true if this agent type operates in the "thinking plane" (no worktree needed).
    pub fn is_thinking_plane(&self) -> bool {
        matches!(
            self,
            AgentType::Coordinator | AgentType::Researcher | AgentType::Integrator | AgentType::Reviewer
        )
    }
}

impl fmt::Display for AgentType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AgentType::Implementer => write!(f, "implementer"),
            AgentType::Reviewer => write!(f, "reviewer"),
            AgentType::Coordinator => write!(f, "coordinator"),
            AgentType::Researcher => write!(f, "researcher"),
            AgentType::Integrator => write!(f, "integrator"),
        }
    }
}

/// Lifecycle status of an agent session.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentStatus {
    Starting,
    Running,
    WaitingForLlm,
    Paused,
    Completed,
    Failed,
    Cancelled,
}

impl AgentStatus {
    /// Returns true if this is a terminal status (no further transitions possible).
    pub fn is_terminal(&self) -> bool {
        matches!(
            self,
            AgentStatus::Completed | AgentStatus::Failed | AgentStatus::Cancelled
        )
    }

    /// Validate whether a status transition is allowed.
    pub fn can_transition_to(&self, target: AgentStatus) -> bool {
        matches!(
            (self, target),
            // Starting transitions
            (AgentStatus::Starting, AgentStatus::Running)
            | (AgentStatus::Starting, AgentStatus::Failed)
            | (AgentStatus::Starting, AgentStatus::Cancelled)
            // Running transitions
            | (AgentStatus::Running, AgentStatus::WaitingForLlm)
            | (AgentStatus::Running, AgentStatus::Paused)
            | (AgentStatus::Running, AgentStatus::Completed)
            | (AgentStatus::Running, AgentStatus::Failed)
            | (AgentStatus::Running, AgentStatus::Cancelled)
            // WaitingForLlm transitions
            | (AgentStatus::WaitingForLlm, AgentStatus::Running)
            | (AgentStatus::WaitingForLlm, AgentStatus::Failed)
            | (AgentStatus::WaitingForLlm, AgentStatus::Cancelled)
            // Paused transitions
            | (AgentStatus::Paused, AgentStatus::Running)
            | (AgentStatus::Paused, AgentStatus::Cancelled)
        )
    }
}

impl fmt::Display for AgentStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            AgentStatus::Starting => write!(f, "starting"),
            AgentStatus::Running => write!(f, "running"),
            AgentStatus::WaitingForLlm => write!(f, "waitingforllm"),
            AgentStatus::Paused => write!(f, "paused"),
            AgentStatus::Completed => write!(f, "completed"),
            AgentStatus::Failed => write!(f, "failed"),
            AgentStatus::Cancelled => write!(f, "cancelled"),
        }
    }
}

/// A persistent record tracking an agent's lifecycle and metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSession {
    pub id: String,
    pub agent_type: AgentType,
    pub work_id: Option<String>,
    pub bundle_id: Option<String>,
    pub status: AgentStatus,
    pub iteration: u32,
    pub model: String,
    pub worktree_path: Option<String>,
    pub error_message: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    /// Generic target ID for agents that don't target Works or Bundles.
    /// Coordinator: None (operates globally).
    /// Researcher: the scope_id (Plan/Spec/Phase/Work ID being researched).
    /// Integrator: None (operates on whatever Accepted Bundles exist).
    #[serde(default)]
    pub target_id: Option<String>,
    /// Query string for Researcher agents. Set by SpawnResearcher action.
    #[serde(default)]
    pub query: Option<String>,
}

impl AgentSession {
    pub fn new(agent_type: AgentType, model: String) -> Self {
        let now = id::now_millis();
        Self {
            id: id::generate_id(),
            agent_type,
            work_id: None,
            bundle_id: None,
            status: AgentStatus::Starting,
            iteration: 0,
            model,
            worktree_path: None,
            error_message: None,
            created_at: now,
            updated_at: now,
            target_id: None,
            query: None,
        }
    }

    /// Transition the agent to a new status, updating the timestamp.
    /// Returns Err if the transition is not allowed.
    pub fn transition_to(&mut self, target: AgentStatus) -> Result<(), String> {
        if !self.status.can_transition_to(target) {
            return Err(format!("invalid agent status transition: {} → {}", self.status, target));
        }
        self.status = target;
        self.updated_at = id::now_millis();
        Ok(())
    }
}

impl Record for AgentSession {
    fn id(&self) -> &str {
        &self.id
    }

    fn updated_at(&self) -> i64 {
        self.updated_at
    }

    fn collection_name() -> &'static str {
        "agent_sessions"
    }

    fn indexed_fields(&self) -> HashMap<String, IndexValue> {
        let mut m = HashMap::new();
        m.insert("status".into(), IndexValue::String(self.status.to_string()));
        m.insert("agent_type".into(), IndexValue::String(self.agent_type.to_string()));
        if let Some(ref wi_id) = self.work_id {
            m.insert("work_id".into(), IndexValue::String(wi_id.clone()));
        }
        if let Some(ref b_id) = self.bundle_id {
            m.insert("bundle_id".into(), IndexValue::String(b_id.clone()));
        }
        if let Some(ref tid) = self.target_id {
            m.insert("target_id".into(), IndexValue::String(tid.clone()));
        }
        m
    }
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
    },
    Transition {
        collection: String,
        id: String,
        target_status: String,
        /// If None, role is inferred from agent_type via AgentType::default_role().
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

    // === Coordinator-only actions ===
    CreatePlan {
        title: String,
        description: String,
        acceptance_criteria: String,
    },
    CreateSpec {
        plan_id: String,
        title: String,
        description: String,
    },
    CreatePhase {
        spec_id: String,
        title: String,
        description: String,
        order: u32,
    },
    CreateWork {
        phase_id: String,
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

/// Events emitted by agents, broadcast through the daemon event system.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AgentEvent {
    StatusChange {
        session_id: String,
        status: AgentStatus,
    },
    LlmOutput {
        session_id: String,
        chunk: String,
        is_final: bool,
    },
    ToolStarted {
        session_id: String,
        tool: String,
    },
    ToolCompleted {
        session_id: String,
        tool: String,
        exit_code: i32,
        duration_ms: u64,
    },
    ActionCompleted {
        session_id: String,
        action_summary: String,
    },
    IterationCompleted {
        session_id: String,
        iteration: u32,
        summary: String,
    },
    StalenessDetected {
        session_id: String,
        new_tick_id: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::TestDir;
    use std::path::Path;
    use std::sync::Mutex as StdMutex;
    use taskstore::Store;

    // --- Test helpers ---

    fn make_test_dir(label: &str) -> TestDir {
        TestDir::new(&format!("loopr-mod-{label}"))
    }

    fn test_agent_logger(dir: &Path) -> AgentLogger {
        let file_path = dir.join("test-mod.log");
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&file_path)
            .unwrap();
        AgentLogger::_new_for_test(AgentType::Coordinator, "test-session", file, file_path)
    }

    fn test_stores_with_dir(dir: &Path) -> Arc<Stores> {
        use crate::config::{Config, ProjectConfig};
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

    fn test_agent_context(
        dir: &Path,
        stores: &Arc<Stores>,
        agent_type: AgentType,
    ) -> (AgentContext, broadcast::Receiver<DaemonEvent>) {
        let (event_tx, event_rx) = broadcast::channel(16);
        let worktree_mgr = WorktreeManager::new(dir.to_path_buf(), dir.join(".worktrees"));
        let bridge = AgentIpcBridge::new(stores.clone(), event_tx.clone(), worktree_mgr, stores.config.clone());
        let agent_log = test_agent_logger(dir);
        let session = AgentSession::new(agent_type, "test-model".into());
        let ctx = AgentContext {
            session,
            stores: stores.clone(),
            bridge,
            event_tx,
            tool_runner: stores.tool_runner.clone(),
            log: agent_log,
        };
        (ctx, event_rx)
    }

    // --- AgentContext tests ---

    #[test]
    fn test_agent_context_from_session_id_success() {
        let dir = make_test_dir("from-session-ok");
        let stores = test_stores_with_dir(&dir);

        // Insert a session into stores
        let session = AgentSession::new(AgentType::Implementer, "test-model".into());
        let session_id = session.id.clone();
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session_id.clone(), session);

        let (event_tx, _event_rx) = broadcast::channel(16);
        let ctx = AgentContext::from_session_id(&session_id, AgentType::Implementer, stores.clone(), event_tx);
        assert!(ctx.is_ok());
        let ctx = ctx.unwrap();
        assert_eq!(ctx.session.id, session_id);
        assert_eq!(ctx.session.agent_type, AgentType::Implementer);
    }

    #[test]
    fn test_agent_context_from_session_id_not_found() {
        let dir = make_test_dir("from-session-missing");
        let stores = test_stores_with_dir(&dir);

        let (event_tx, _event_rx) = broadcast::channel(16);
        let result = AgentContext::from_session_id("nonexistent", AgentType::Coordinator, stores, event_tx);
        let err_msg = result.err().expect("expected Err").to_string();
        assert!(
            err_msg.contains("session not found"),
            "expected 'session not found', got: {err_msg}"
        );
    }

    #[test]
    fn test_agent_context_log_delegates() {
        let dir = make_test_dir("log-delegates");
        let stores = test_stores_with_dir(&dir);
        let (ctx, _rx) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        // These should not panic — they delegate to AgentLogger
        ctx.info("info message");
        ctx.warn("warn message");
        ctx.debug("debug message");
        ctx.error("error message");

        // Verify messages were written to the log file
        let log_content = std::fs::read_to_string(ctx.log.file_path()).unwrap();
        assert!(log_content.contains("info message"));
        assert!(log_content.contains("warn message"));
        assert!(log_content.contains("debug message"));
        assert!(log_content.contains("error message"));
    }

    #[test]
    fn test_agent_context_is_cancelled_false_when_running() {
        let dir = make_test_dir("cancel-false");
        let stores = test_stores_with_dir(&dir);
        let (ctx, _rx) = test_agent_context(&dir, &stores, AgentType::Implementer);

        // Insert session into stores with Starting status
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(ctx.session.id.clone(), ctx.session.clone());

        assert!(!ctx.is_cancelled());
    }

    #[test]
    fn test_agent_context_is_cancelled_true_when_cancelled() {
        let dir = make_test_dir("cancel-true");
        let stores = test_stores_with_dir(&dir);
        let (ctx, _rx) = test_agent_context(&dir, &stores, AgentType::Implementer);

        // Insert session with Cancelled status
        let mut session = ctx.session.clone();
        session.status = AgentStatus::Cancelled;
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(session.id.clone(), session);

        assert!(ctx.is_cancelled());
    }

    #[test]
    fn test_agent_context_is_cancelled_true_when_session_missing() {
        let dir = make_test_dir("cancel-missing");
        let stores = test_stores_with_dir(&dir);
        let (ctx, _rx) = test_agent_context(&dir, &stores, AgentType::Implementer);

        // Don't insert session — simulates a removed/expired session
        assert!(ctx.is_cancelled());
    }

    #[test]
    fn test_agent_context_persist_iteration() {
        let dir = make_test_dir("persist-iter");
        let stores = test_stores_with_dir(&dir);
        let (mut ctx, _rx) = test_agent_context(&dir, &stores, AgentType::Implementer);

        // Insert session with iteration 0
        stores
            .agent_sessions
            .write()
            .unwrap()
            .insert(ctx.session.id.clone(), ctx.session.clone());

        // Bump iteration locally and persist
        ctx.session.iteration = 5;
        ctx.persist_iteration();

        // Verify stores reflect the update
        let sessions = stores.agent_sessions.read().unwrap();
        let stored = sessions.get(&ctx.session.id).unwrap();
        assert_eq!(stored.iteration, 5);
    }

    #[test]
    fn test_agent_context_persist_iteration_noop_when_missing() {
        let dir = make_test_dir("persist-iter-noop");
        let stores = test_stores_with_dir(&dir);
        let (mut ctx, _rx) = test_agent_context(&dir, &stores, AgentType::Implementer);

        // Don't insert session — persist_iteration should silently no-op
        ctx.session.iteration = 10;
        ctx.persist_iteration(); // should not panic
    }

    #[test]
    fn test_agent_context_emit_iteration_completed() {
        let dir = make_test_dir("emit-iter");
        let stores = test_stores_with_dir(&dir);
        let (ctx, mut rx) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        ctx.emit_iteration_completed(3, "Planned 5 specs");

        // Verify the event was received
        let event = rx.try_recv().unwrap();
        assert_eq!(event.event, "agent.iteration_completed");

        // Verify payload contents
        let data = event.data;
        assert_eq!(data["session_id"], ctx.session.id);
        assert_eq!(data["iteration"], 3);
        assert_eq!(data["summary"], "Planned 5 specs");
    }

    #[test]
    fn test_agent_context_emit_iteration_completed_no_receivers() {
        let dir = make_test_dir("emit-no-rx");
        let stores = test_stores_with_dir(&dir);
        let (ctx, rx) = test_agent_context(&dir, &stores, AgentType::Coordinator);

        // Drop receiver — emit should not panic (uses `let _ =`)
        drop(rx);
        ctx.emit_iteration_completed(1, "test summary");
    }

    // --- AgentType tests ---

    const ALL_AGENT_TYPES: [AgentType; 5] = [
        AgentType::Implementer,
        AgentType::Reviewer,
        AgentType::Coordinator,
        AgentType::Researcher,
        AgentType::Integrator,
    ];

    #[test]
    fn test_agent_type_display() {
        assert_eq!(AgentType::Implementer.to_string(), "implementer");
        assert_eq!(AgentType::Reviewer.to_string(), "reviewer");
        assert_eq!(AgentType::Coordinator.to_string(), "coordinator");
        assert_eq!(AgentType::Researcher.to_string(), "researcher");
        assert_eq!(AgentType::Integrator.to_string(), "integrator");
    }

    #[test]
    fn test_agent_type_serde_roundtrip() {
        for at in ALL_AGENT_TYPES {
            let json = serde_json::to_string(&at).unwrap();
            let deserialized: AgentType = serde_json::from_str(&json).unwrap();
            assert_eq!(at, deserialized);
        }
    }

    #[test]
    fn test_agent_type_display_matches_serde() {
        for at in ALL_AGENT_TYPES {
            let display = at.to_string();
            let quoted = format!("\"{display}\"");
            let deserialized: AgentType = serde_json::from_str(&quoted).unwrap();
            assert_eq!(at, deserialized);
        }
    }

    #[test]
    fn test_agent_type_default_role() {
        use crate::domain::role::Role;
        assert_eq!(AgentType::Implementer.default_role(), Role::Implementer);
        assert_eq!(AgentType::Reviewer.default_role(), Role::Reviewer);
        assert_eq!(AgentType::Coordinator.default_role(), Role::Coordinator);
        assert_eq!(AgentType::Researcher.default_role(), Role::Researcher);
        assert_eq!(AgentType::Integrator.default_role(), Role::Integrator);
    }

    #[test]
    fn test_agent_type_is_thinking_plane() {
        assert!(!AgentType::Implementer.is_thinking_plane());
        assert!(AgentType::Reviewer.is_thinking_plane());
        assert!(AgentType::Coordinator.is_thinking_plane());
        assert!(AgentType::Researcher.is_thinking_plane());
        assert!(AgentType::Integrator.is_thinking_plane());
    }

    // --- AgentStatus tests ---

    #[test]
    fn test_agent_status_display() {
        assert_eq!(AgentStatus::Starting.to_string(), "starting");
        assert_eq!(AgentStatus::Running.to_string(), "running");
        assert_eq!(AgentStatus::WaitingForLlm.to_string(), "waitingforllm");
        assert_eq!(AgentStatus::Paused.to_string(), "paused");
        assert_eq!(AgentStatus::Completed.to_string(), "completed");
        assert_eq!(AgentStatus::Failed.to_string(), "failed");
        assert_eq!(AgentStatus::Cancelled.to_string(), "cancelled");
    }

    #[test]
    fn test_agent_status_serde_roundtrip() {
        for status in [
            AgentStatus::Starting,
            AgentStatus::Running,
            AgentStatus::WaitingForLlm,
            AgentStatus::Paused,
            AgentStatus::Completed,
            AgentStatus::Failed,
            AgentStatus::Cancelled,
        ] {
            let json = serde_json::to_string(&status).unwrap();
            let deserialized: AgentStatus = serde_json::from_str(&json).unwrap();
            assert_eq!(status, deserialized);
        }
    }

    #[test]
    fn test_agent_status_is_terminal() {
        assert!(!AgentStatus::Starting.is_terminal());
        assert!(!AgentStatus::Running.is_terminal());
        assert!(!AgentStatus::WaitingForLlm.is_terminal());
        assert!(!AgentStatus::Paused.is_terminal());
        assert!(AgentStatus::Completed.is_terminal());
        assert!(AgentStatus::Failed.is_terminal());
        assert!(AgentStatus::Cancelled.is_terminal());
    }

    #[test]
    fn test_agent_status_valid_transitions() {
        // Starting transitions
        assert!(AgentStatus::Starting.can_transition_to(AgentStatus::Running));
        assert!(AgentStatus::Starting.can_transition_to(AgentStatus::Failed));
        assert!(AgentStatus::Starting.can_transition_to(AgentStatus::Cancelled));

        // Running transitions
        assert!(AgentStatus::Running.can_transition_to(AgentStatus::WaitingForLlm));
        assert!(AgentStatus::Running.can_transition_to(AgentStatus::Paused));
        assert!(AgentStatus::Running.can_transition_to(AgentStatus::Completed));
        assert!(AgentStatus::Running.can_transition_to(AgentStatus::Failed));
        assert!(AgentStatus::Running.can_transition_to(AgentStatus::Cancelled));

        // WaitingForLlm transitions
        assert!(AgentStatus::WaitingForLlm.can_transition_to(AgentStatus::Running));
        assert!(AgentStatus::WaitingForLlm.can_transition_to(AgentStatus::Failed));
        assert!(AgentStatus::WaitingForLlm.can_transition_to(AgentStatus::Cancelled));

        // Paused transitions
        assert!(AgentStatus::Paused.can_transition_to(AgentStatus::Running));
        assert!(AgentStatus::Paused.can_transition_to(AgentStatus::Cancelled));
    }

    #[test]
    fn test_agent_status_invalid_transitions() {
        // Terminal states cannot transition
        assert!(!AgentStatus::Completed.can_transition_to(AgentStatus::Running));
        assert!(!AgentStatus::Failed.can_transition_to(AgentStatus::Running));
        assert!(!AgentStatus::Cancelled.can_transition_to(AgentStatus::Running));

        // Cannot skip states
        assert!(!AgentStatus::Starting.can_transition_to(AgentStatus::Completed));
        assert!(!AgentStatus::Starting.can_transition_to(AgentStatus::Paused));
        assert!(!AgentStatus::Paused.can_transition_to(AgentStatus::Completed));
        assert!(!AgentStatus::Paused.can_transition_to(AgentStatus::WaitingForLlm));
    }

    // --- AgentSession tests ---

    #[test]
    fn test_agent_session_new() {
        let session = AgentSession::new(AgentType::Implementer, "claude-sonnet-4-6".to_string());
        assert!(!session.id.is_empty());
        assert_eq!(session.agent_type, AgentType::Implementer);
        assert_eq!(session.status, AgentStatus::Starting);
        assert_eq!(session.iteration, 0);
        assert_eq!(session.model, "claude-sonnet-4-6");
        assert!(session.work_id.is_none());
        assert!(session.bundle_id.is_none());
        assert!(session.worktree_path.is_none());
        assert!(session.error_message.is_none());
        assert!(session.target_id.is_none());
        assert!(session.query.is_none());
        assert!(session.created_at > 0);
        assert_eq!(session.created_at, session.updated_at);
    }

    #[test]
    fn test_agent_session_new_researcher_with_target() {
        let mut session = AgentSession::new(AgentType::Researcher, "model".to_string());
        session.target_id = Some("wi-123".to_string());
        session.query = Some("Investigate auth module".to_string());
        assert_eq!(session.agent_type, AgentType::Researcher);
        assert_eq!(session.target_id.as_deref(), Some("wi-123"));
        assert_eq!(session.query.as_deref(), Some("Investigate auth module"));
    }

    #[test]
    fn test_agent_session_new_coordinator() {
        let session = AgentSession::new(AgentType::Coordinator, "model".to_string());
        assert_eq!(session.agent_type, AgentType::Coordinator);
        assert!(session.target_id.is_none());
        assert!(session.query.is_none());
    }

    #[test]
    fn test_agent_session_unique_ids() {
        let s1 = AgentSession::new(AgentType::Implementer, "m".to_string());
        let s2 = AgentSession::new(AgentType::Implementer, "m".to_string());
        assert_ne!(s1.id, s2.id);
    }

    #[test]
    fn test_agent_session_transition_valid() {
        let mut session = AgentSession::new(AgentType::Implementer, "m".to_string());
        assert!(session.transition_to(AgentStatus::Running).is_ok());
        assert_eq!(session.status, AgentStatus::Running);
        assert!(session.updated_at >= session.created_at);
    }

    #[test]
    fn test_agent_session_transition_invalid() {
        let mut session = AgentSession::new(AgentType::Implementer, "m".to_string());
        let result = session.transition_to(AgentStatus::Completed);
        assert!(result.is_err());
        assert_eq!(session.status, AgentStatus::Starting); // unchanged
    }

    #[test]
    fn test_agent_session_transition_chain() {
        let mut session = AgentSession::new(AgentType::Reviewer, "m".to_string());
        assert!(session.transition_to(AgentStatus::Running).is_ok());
        assert!(session.transition_to(AgentStatus::WaitingForLlm).is_ok());
        assert!(session.transition_to(AgentStatus::Running).is_ok());
        assert!(session.transition_to(AgentStatus::Completed).is_ok());
        assert!(session.status.is_terminal());
    }

    #[test]
    fn test_agent_session_serde_roundtrip() {
        let mut session = AgentSession::new(AgentType::Implementer, "claude-sonnet-4-6".to_string());
        session.work_id = Some("wi-123".to_string());
        session.worktree_path = Some("/tmp/worktree".to_string());
        let json = serde_json::to_string(&session).unwrap();
        let deserialized: AgentSession = serde_json::from_str(&json).unwrap();
        assert_eq!(session.id, deserialized.id);
        assert_eq!(session.agent_type, deserialized.agent_type);
        assert_eq!(session.status, deserialized.status);
        assert_eq!(session.work_id, deserialized.work_id);
        assert_eq!(session.worktree_path, deserialized.worktree_path);
        assert_eq!(session.target_id, deserialized.target_id);
        assert_eq!(session.query, deserialized.query);
    }

    #[test]
    fn test_agent_session_serde_backward_compat() {
        // Old JSON without target_id/query should deserialize with defaults (None)
        let json = r#"{
            "id": "test-1", "agent_type": "implementer", "work_id": null,
            "bundle_id": null, "status": "starting", "iteration": 0,
            "model": "m", "worktree_path": null, "error_message": null,
            "created_at": 1000, "updated_at": 1000
        }"#;
        let session: AgentSession = serde_json::from_str(json).unwrap();
        assert!(session.target_id.is_none());
        assert!(session.query.is_none());
    }

    // --- Record trait tests ---

    #[test]
    fn test_agent_session_record_id() {
        let session = AgentSession::new(AgentType::Implementer, "m".to_string());
        assert_eq!(Record::id(&session), session.id.as_str());
    }

    #[test]
    fn test_agent_session_record_updated_at() {
        let session = AgentSession::new(AgentType::Implementer, "m".to_string());
        assert_eq!(Record::updated_at(&session), session.updated_at);
    }

    #[test]
    fn test_agent_session_record_collection_name() {
        assert_eq!(AgentSession::collection_name(), "agent_sessions");
    }

    #[test]
    fn test_agent_session_record_indexed_fields() {
        let mut session = AgentSession::new(AgentType::Implementer, "m".to_string());
        session.work_id = Some("wi-1".to_string());

        let fields = session.indexed_fields();
        assert_eq!(fields.get("status"), Some(&IndexValue::String("starting".to_string())));
        assert_eq!(
            fields.get("agent_type"),
            Some(&IndexValue::String("implementer".to_string()))
        );
        assert_eq!(fields.get("work_id"), Some(&IndexValue::String("wi-1".to_string())));
        assert!(!fields.contains_key("bundle_id"));
    }

    #[test]
    fn test_agent_session_record_indexed_fields_reviewer() {
        let mut session = AgentSession::new(AgentType::Reviewer, "m".to_string());
        session.bundle_id = Some("b-1".to_string());

        let fields = session.indexed_fields();
        assert_eq!(
            fields.get("agent_type"),
            Some(&IndexValue::String("reviewer".to_string()))
        );
        assert_eq!(fields.get("bundle_id"), Some(&IndexValue::String("b-1".to_string())));
        assert!(!fields.contains_key("work_id"));
    }

    #[test]
    fn test_agent_session_record_indexed_fields_with_target_id() {
        let mut session = AgentSession::new(AgentType::Researcher, "m".to_string());
        session.target_id = Some("wi-42".to_string());

        let fields = session.indexed_fields();
        assert_eq!(
            fields.get("agent_type"),
            Some(&IndexValue::String("researcher".to_string()))
        );
        assert_eq!(fields.get("target_id"), Some(&IndexValue::String("wi-42".to_string())));
    }

    // --- AgentAction tests ---

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
        };
        let json = serde_json::to_string(&action).unwrap();
        let deserialized: AgentAction = serde_json::from_str(&json).unwrap();
        if let AgentAction::ProposeBundle { description, claims } = deserialized {
            assert_eq!(description, "Add error handling");
            assert_eq!(claims, vec!["src/error.rs"]);
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

    // --- Coordinator action tests ---

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
            plan_id: "p-1".to_string(),
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
            spec_id: "s-1".to_string(),
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
            phase_id: "ph-1".to_string(),
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

    // --- Researcher action tests ---

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

    // --- AgentEvent tests ---

    #[test]
    fn test_agent_event_status_change_serde() {
        let event = AgentEvent::StatusChange {
            session_id: "s1".to_string(),
            status: AgentStatus::Running,
        };
        let json = serde_json::to_string(&event).unwrap();
        let deserialized: AgentEvent = serde_json::from_str(&json).unwrap();
        if let AgentEvent::StatusChange { session_id, status } = deserialized {
            assert_eq!(session_id, "s1");
            assert_eq!(status, AgentStatus::Running);
        } else {
            panic!("expected StatusChange");
        }
    }

    #[test]
    fn test_agent_event_llm_output_serde() {
        let event = AgentEvent::LlmOutput {
            session_id: "s1".to_string(),
            chunk: "Hello".to_string(),
            is_final: false,
        };
        let json = serde_json::to_string(&event).unwrap();
        let deserialized: AgentEvent = serde_json::from_str(&json).unwrap();
        if let AgentEvent::LlmOutput {
            session_id,
            chunk,
            is_final,
        } = deserialized
        {
            assert_eq!(session_id, "s1");
            assert_eq!(chunk, "Hello");
            assert!(!is_final);
        } else {
            panic!("expected LlmOutput");
        }
    }

    #[test]
    fn test_agent_event_tool_completed_serde() {
        let event = AgentEvent::ToolCompleted {
            session_id: "s1".to_string(),
            tool: "test".to_string(),
            exit_code: 0,
            duration_ms: 1500,
        };
        let json = serde_json::to_string(&event).unwrap();
        let deserialized: AgentEvent = serde_json::from_str(&json).unwrap();
        if let AgentEvent::ToolCompleted {
            session_id,
            tool,
            exit_code,
            duration_ms,
        } = deserialized
        {
            assert_eq!(session_id, "s1");
            assert_eq!(tool, "test");
            assert_eq!(exit_code, 0);
            assert_eq!(duration_ms, 1500);
        } else {
            panic!("expected ToolCompleted");
        }
    }

    #[test]
    fn test_agent_event_staleness_detected_serde() {
        let event = AgentEvent::StalenessDetected {
            session_id: "s1".to_string(),
            new_tick_id: "tick-42".to_string(),
        };
        let json = serde_json::to_string(&event).unwrap();
        let deserialized: AgentEvent = serde_json::from_str(&json).unwrap();
        if let AgentEvent::StalenessDetected {
            session_id,
            new_tick_id,
        } = deserialized
        {
            assert_eq!(session_id, "s1");
            assert_eq!(new_tick_id, "tick-42");
        } else {
            panic!("expected StalenessDetected");
        }
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
        // LLMs sometimes send "args": {} instead of "args": [] — should parse as empty vec
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
            "phase_id": "p1",
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
