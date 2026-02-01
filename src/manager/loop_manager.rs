//! Loop Manager implementation
//!
//! LoopManager orchestrates loop lifecycle - creation, execution, child spawning.
//! It owns the core dependencies and manages running loops.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use tokio::sync::RwLock;
use tokio::task::JoinHandle;

use crate::coordination::SignalManager;
use crate::domain::{Loop, LoopStatus, LoopType, SignalType};
use crate::error::{LooprError, Result};
use crate::id::now_ms;
use crate::llm::LlmClient;
use crate::runner::{LoopOutcome, LoopRunner, LoopRunnerConfig, SignalChecker};
use crate::storage::{Filter, HasId, Storage};
use crate::tools::ToolRouter;
use crate::worktree::WorktreeManager;

/// Collection name for loops in storage
const LOOPS_COLLECTION: &str = "loops";

/// Configuration for the LoopManager
#[derive(Debug, Clone)]
pub struct LoopManagerConfig {
    /// Maximum number of concurrent loops
    pub max_concurrent_loops: usize,
    /// Default maximum iterations for a loop
    pub default_max_iterations: u32,
    /// Path to prompts directory
    pub prompts_dir: PathBuf,
    /// Repository root path
    pub repo_root: PathBuf,
}

impl Default for LoopManagerConfig {
    fn default() -> Self {
        Self {
            max_concurrent_loops: 4,
            default_max_iterations: 10,
            prompts_dir: PathBuf::from("prompts"),
            repo_root: PathBuf::from("."),
        }
    }
}

/// Signal checker that uses SignalManager
struct StorageSignalChecker<S: Storage> {
    signal_manager: Arc<SignalManager<S>>,
}

#[async_trait]
impl<S: Storage + Send + Sync + 'static> SignalChecker for StorageSignalChecker<S> {
    async fn should_stop(&self, loop_id: &str) -> Result<bool> {
        if let Some(signal) = self.signal_manager.check(loop_id)? {
            if signal.signal_type == SignalType::Stop {
                self.signal_manager.acknowledge(&signal.id)?;
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn should_pause(&self, loop_id: &str) -> Result<bool> {
        if let Some(signal) = self.signal_manager.check(loop_id)? {
            if signal.signal_type == SignalType::Pause {
                return Ok(true);
            }
        }
        Ok(false)
    }

    async fn is_invalidated(&self, loop_id: &str) -> Result<bool> {
        if let Some(signal) = self.signal_manager.check(loop_id)? {
            if signal.signal_type == SignalType::Invalidate {
                self.signal_manager.acknowledge(&signal.id)?;
                return Ok(true);
            }
        }
        Ok(false)
    }
}

/// Manages loop lifecycle - creation, execution, child spawning
pub struct LoopManager<S: Storage, L: LlmClient, T: ToolRouter> {
    storage: Arc<S>,
    llm_client: Arc<L>,
    tool_router: Arc<T>,
    worktree_manager: Arc<WorktreeManager>,
    signal_manager: Arc<SignalManager<S>>,
    config: LoopManagerConfig,
    running_loops: RwLock<HashMap<String, JoinHandle<Result<LoopOutcome>>>>,
}

impl<S: Storage + Send + Sync + 'static, L: LlmClient + 'static, T: ToolRouter + 'static>
    LoopManager<S, L, T>
{
    /// Create a new LoopManager with the given dependencies
    pub fn new(
        storage: Arc<S>,
        llm_client: Arc<L>,
        tool_router: Arc<T>,
        worktree_manager: Arc<WorktreeManager>,
        signal_manager: Arc<SignalManager<S>>,
        config: LoopManagerConfig,
    ) -> Self {
        Self {
            storage,
            llm_client,
            tool_router,
            worktree_manager,
            signal_manager,
            config,
            running_loops: RwLock::new(HashMap::new()),
        }
    }

    /// Create and persist a new loop
    pub async fn create_loop(&self, loop_type: LoopType, task: &str) -> Result<Loop> {
        let loop_instance = match loop_type {
            LoopType::Plan => Loop::new_plan(task),
            _ => {
                return Err(LooprError::InvalidState(
                    "Only Plan loops can be created directly".into(),
                ));
            }
        };

        // Persist the new loop
        self.storage.create(LOOPS_COLLECTION, &loop_instance)?;

        Ok(loop_instance)
    }

    /// Create a child loop from a parent
    pub async fn create_child_loop(
        &self,
        parent: &Loop,
        loop_type: LoopType,
        index: u32,
    ) -> Result<Loop> {
        let loop_instance = match loop_type {
            LoopType::Spec => Loop::new_spec(parent, index),
            LoopType::Phase => Loop::new_phase(parent, index, "Phase", 1),
            LoopType::Code => Loop::new_code(parent),
            LoopType::Plan => {
                return Err(LooprError::InvalidState(
                    "Plan loops cannot be spawned as children".into(),
                ));
            }
        };

        self.storage.create(LOOPS_COLLECTION, &loop_instance)?;

        Ok(loop_instance)
    }

    /// Start executing a loop (spawns tokio task)
    pub async fn start_loop(&self, loop_id: &str) -> Result<()> {
        // Get the loop from storage
        let loop_instance: Option<Loop> = self.storage.get(LOOPS_COLLECTION, loop_id)?;
        let mut loop_instance =
            loop_instance.ok_or_else(|| LooprError::LoopNotFound(loop_id.to_string()))?;

        // Check if already running
        {
            let running = self.running_loops.read().await;
            if running.contains_key(loop_id) {
                return Err(LooprError::InvalidState(format!(
                    "Loop {} is already running",
                    loop_id
                )));
            }
        }

        // Update status to running
        loop_instance.status = LoopStatus::Running;
        loop_instance.updated_at = now_ms();
        self.storage
            .update(LOOPS_COLLECTION, loop_id, &loop_instance)?;

        // Create worktree for this loop
        let worktree_path = self.worktree_manager.create(loop_id).await?;
        loop_instance.worktree = worktree_path;

        // Build the runner config
        let runner_config = LoopRunnerConfig {
            max_iterations: loop_instance.max_iterations,
            prompts_dir: self.config.prompts_dir.clone(),
        };

        // Create signal checker
        let signal_checker = Arc::new(StorageSignalChecker {
            signal_manager: self.signal_manager.clone(),
        });

        // Clone Arc references for the task
        let storage = self.storage.clone();
        let llm_client = self.llm_client.clone();
        let tool_router = self.tool_router.clone();
        let loop_id_owned = loop_id.to_string();

        // Spawn the execution task
        let handle = tokio::spawn(async move {
            let runner = LoopRunner::new(
                llm_client,
                tool_router,
                signal_checker,
                runner_config,
            );

            let outcome = runner.run(&mut loop_instance).await?;

            // Update final status in storage
            storage.update(LOOPS_COLLECTION, &loop_id_owned, &loop_instance)?;

            Ok(outcome)
        });

        // Track the running task
        {
            let mut running = self.running_loops.write().await;
            running.insert(loop_id.to_string(), handle);
        }

        Ok(())
    }

    /// Stop a running loop
    pub async fn stop_loop(&self, loop_id: &str) -> Result<()> {
        self.signal_manager
            .send_stop(loop_id, "User requested stop")?;
        Ok(())
    }

    /// Pause a running loop
    pub async fn pause_loop(&self, loop_id: &str) -> Result<()> {
        self.signal_manager.send_pause(loop_id)?;
        Ok(())
    }

    /// Resume a paused loop
    pub async fn resume_loop(&self, loop_id: &str) -> Result<()> {
        self.signal_manager.send_resume(loop_id)?;
        Ok(())
    }

    /// Handle loop completion - spawn children if needed
    pub async fn on_loop_complete(&self, loop_id: &str) -> Result<()> {
        let loop_instance: Option<Loop> = self.storage.get(LOOPS_COLLECTION, loop_id)?;
        let loop_instance =
            loop_instance.ok_or_else(|| LooprError::LoopNotFound(loop_id.to_string()))?;

        // Clean up the running task tracking
        {
            let mut running = self.running_loops.write().await;
            running.remove(loop_id);
        }

        // Handle child spawning based on loop type
        match loop_instance.loop_type {
            LoopType::Plan => {
                // Plan loop needs user approval before spawning specs
                // This is handled by the daemon/UI layer
            }
            LoopType::Spec => {
                // Parse spec to find phases and spawn PhaseLoops
                // This will be implemented by the spawner module
            }
            LoopType::Phase => {
                // Spawn CodeLoop
                self.create_child_loop(&loop_instance, LoopType::Code, 0)
                    .await?;
            }
            LoopType::Code => {
                // Code loops are leaf nodes, check if ready to merge
            }
        }

        // Clean up worktree
        self.worktree_manager.cleanup(loop_id, true).await?;

        Ok(())
    }

    /// Get a loop by ID
    pub async fn get_loop(&self, loop_id: &str) -> Result<Option<Loop>> {
        self.storage.get(LOOPS_COLLECTION, loop_id)
    }

    /// List all loops
    pub async fn list_loops(&self) -> Result<Vec<Loop>> {
        self.storage.list(LOOPS_COLLECTION)
    }

    /// Find loops by status
    pub async fn find_by_status(&self, status: LoopStatus) -> Result<Vec<Loop>> {
        let status_str = serde_json::to_value(status)?;
        let filters = vec![Filter::eq("status", status_str)];
        self.storage.query(LOOPS_COLLECTION, &filters)
    }

    /// Find loops by parent
    pub async fn find_by_parent(&self, parent_id: &str) -> Result<Vec<Loop>> {
        let filters = vec![Filter::eq("parent_id", parent_id)];
        self.storage.query(LOOPS_COLLECTION, &filters)
    }

    /// Get count of running loops
    pub async fn running_count(&self) -> usize {
        let running = self.running_loops.read().await;
        running.len()
    }

    /// Get available slots for new loops
    pub async fn available_slots(&self) -> usize {
        let count = self.running_count().await;
        self.config.max_concurrent_loops.saturating_sub(count)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::llm::client::MockLlmClient;
    use crate::llm::types::{CompletionResponse, StopReason, Usage};
    use crate::storage::jsonl::JsonlStorage;
    use crate::tools::router::MockToolRouter;
    use tempfile::TempDir;

    fn create_test_deps() -> (
        TempDir,
        Arc<JsonlStorage>,
        Arc<MockLlmClient>,
        Arc<MockToolRouter>,
        Arc<WorktreeManager>,
        Arc<SignalManager<JsonlStorage>>,
    ) {
        let temp_dir = TempDir::new().unwrap();
        let storage = Arc::new(JsonlStorage::new(temp_dir.path()).unwrap());
        let llm_client = Arc::new(MockLlmClient::new());
        let tool_router = Arc::new(MockToolRouter::new());
        let worktree_manager = Arc::new(WorktreeManager::new(
            temp_dir.path().to_path_buf(),
            temp_dir.path().to_path_buf(),
        ));
        let signal_manager = Arc::new(SignalManager::new(storage.clone()));

        (
            temp_dir,
            storage,
            llm_client,
            tool_router,
            worktree_manager,
            signal_manager,
        )
    }

    #[test]
    fn test_config_default() {
        let config = LoopManagerConfig::default();
        assert_eq!(config.max_concurrent_loops, 4);
        assert_eq!(config.default_max_iterations, 10);
    }

    #[tokio::test]
    async fn test_create_plan_loop() {
        let (_temp, storage, llm_client, tool_router, worktree_manager, signal_manager) =
            create_test_deps();
        let config = LoopManagerConfig::default();
        let manager = LoopManager::new(
            storage.clone(),
            llm_client,
            tool_router,
            worktree_manager,
            signal_manager,
            config,
        );

        let loop_instance = manager
            .create_loop(LoopType::Plan, "Build a test app")
            .await
            .unwrap();

        assert_eq!(loop_instance.loop_type, LoopType::Plan);
        assert_eq!(loop_instance.status, LoopStatus::Pending);
        assert!(loop_instance.parent_id.is_none());
    }

    #[tokio::test]
    async fn test_create_spec_loop_error() {
        let (_temp, storage, llm_client, tool_router, worktree_manager, signal_manager) =
            create_test_deps();
        let config = LoopManagerConfig::default();
        let manager = LoopManager::new(
            storage,
            llm_client,
            tool_router,
            worktree_manager,
            signal_manager,
            config,
        );

        let result = manager.create_loop(LoopType::Spec, "task").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_get_loop() {
        let (_temp, storage, llm_client, tool_router, worktree_manager, signal_manager) =
            create_test_deps();
        let config = LoopManagerConfig::default();
        let manager = LoopManager::new(
            storage.clone(),
            llm_client,
            tool_router,
            worktree_manager,
            signal_manager,
            config,
        );

        let created = manager
            .create_loop(LoopType::Plan, "task")
            .await
            .unwrap();
        let fetched = manager.get_loop(&created.id).await.unwrap();

        assert!(fetched.is_some());
        assert_eq!(fetched.unwrap().id, created.id);
    }

    #[tokio::test]
    async fn test_list_loops() {
        let (_temp, storage, llm_client, tool_router, worktree_manager, signal_manager) =
            create_test_deps();
        let config = LoopManagerConfig::default();
        let manager = LoopManager::new(
            storage.clone(),
            llm_client,
            tool_router,
            worktree_manager,
            signal_manager,
            config,
        );

        manager
            .create_loop(LoopType::Plan, "task1")
            .await
            .unwrap();
        manager
            .create_loop(LoopType::Plan, "task2")
            .await
            .unwrap();

        let loops = manager.list_loops().await.unwrap();
        assert_eq!(loops.len(), 2);
    }

    #[tokio::test]
    async fn test_find_by_status() {
        let (_temp, storage, llm_client, tool_router, worktree_manager, signal_manager) =
            create_test_deps();
        let config = LoopManagerConfig::default();
        let manager = LoopManager::new(
            storage.clone(),
            llm_client,
            tool_router,
            worktree_manager,
            signal_manager,
            config,
        );

        manager
            .create_loop(LoopType::Plan, "task")
            .await
            .unwrap();

        let pending = manager.find_by_status(LoopStatus::Pending).await.unwrap();
        assert_eq!(pending.len(), 1);

        let running = manager.find_by_status(LoopStatus::Running).await.unwrap();
        assert_eq!(running.len(), 0);
    }

    #[tokio::test]
    async fn test_available_slots() {
        let (_temp, storage, llm_client, tool_router, worktree_manager, signal_manager) =
            create_test_deps();
        let config = LoopManagerConfig {
            max_concurrent_loops: 3,
            ..Default::default()
        };
        let manager = LoopManager::new(
            storage,
            llm_client,
            tool_router,
            worktree_manager,
            signal_manager,
            config,
        );

        assert_eq!(manager.available_slots().await, 3);
        assert_eq!(manager.running_count().await, 0);
    }

    #[tokio::test]
    async fn test_create_child_loop() {
        let (_temp, storage, llm_client, tool_router, worktree_manager, signal_manager) =
            create_test_deps();
        let config = LoopManagerConfig::default();
        let manager = LoopManager::new(
            storage.clone(),
            llm_client,
            tool_router,
            worktree_manager,
            signal_manager,
            config,
        );

        let parent = manager
            .create_loop(LoopType::Plan, "task")
            .await
            .unwrap();
        let child = manager
            .create_child_loop(&parent, LoopType::Spec, 1)
            .await
            .unwrap();

        assert_eq!(child.loop_type, LoopType::Spec);
        assert_eq!(child.parent_id, Some(parent.id.clone()));
    }

    #[tokio::test]
    async fn test_create_plan_as_child_error() {
        let (_temp, storage, llm_client, tool_router, worktree_manager, signal_manager) =
            create_test_deps();
        let config = LoopManagerConfig::default();
        let manager = LoopManager::new(
            storage.clone(),
            llm_client,
            tool_router,
            worktree_manager,
            signal_manager,
            config,
        );

        let parent = manager
            .create_loop(LoopType::Plan, "task")
            .await
            .unwrap();
        let result = manager.create_child_loop(&parent, LoopType::Plan, 1).await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_stop_loop_sends_signal() {
        let (_temp, storage, llm_client, tool_router, worktree_manager, signal_manager) =
            create_test_deps();
        let config = LoopManagerConfig::default();
        let manager = LoopManager::new(
            storage.clone(),
            llm_client,
            tool_router,
            worktree_manager,
            signal_manager.clone(),
            config,
        );

        let loop_instance = manager
            .create_loop(LoopType::Plan, "task")
            .await
            .unwrap();
        manager.stop_loop(&loop_instance.id).await.unwrap();

        // Check that a stop signal was sent
        let signal = signal_manager.check(&loop_instance.id).unwrap();
        assert!(signal.is_some());
        assert_eq!(signal.unwrap().signal_type, SignalType::Stop);
    }

    #[tokio::test]
    async fn test_pause_loop_sends_signal() {
        let (_temp, storage, llm_client, tool_router, worktree_manager, signal_manager) =
            create_test_deps();
        let config = LoopManagerConfig::default();
        let manager = LoopManager::new(
            storage.clone(),
            llm_client,
            tool_router,
            worktree_manager,
            signal_manager.clone(),
            config,
        );

        let loop_instance = manager
            .create_loop(LoopType::Plan, "task")
            .await
            .unwrap();
        manager.pause_loop(&loop_instance.id).await.unwrap();

        let signal = signal_manager.check(&loop_instance.id).unwrap();
        assert!(signal.is_some());
        assert_eq!(signal.unwrap().signal_type, SignalType::Pause);
    }

    #[tokio::test]
    async fn test_resume_loop_sends_signal() {
        let (_temp, storage, llm_client, tool_router, worktree_manager, signal_manager) =
            create_test_deps();
        let config = LoopManagerConfig::default();
        let manager = LoopManager::new(
            storage.clone(),
            llm_client,
            tool_router,
            worktree_manager,
            signal_manager.clone(),
            config,
        );

        let loop_instance = manager
            .create_loop(LoopType::Plan, "task")
            .await
            .unwrap();
        manager.resume_loop(&loop_instance.id).await.unwrap();

        let signal = signal_manager.check(&loop_instance.id).unwrap();
        assert!(signal.is_some());
        assert_eq!(signal.unwrap().signal_type, SignalType::Resume);
    }

    #[tokio::test]
    async fn test_find_by_parent() {
        let (_temp, storage, llm_client, tool_router, worktree_manager, signal_manager) =
            create_test_deps();
        let config = LoopManagerConfig::default();
        let manager = LoopManager::new(
            storage.clone(),
            llm_client,
            tool_router,
            worktree_manager,
            signal_manager,
            config,
        );

        let parent = manager
            .create_loop(LoopType::Plan, "task")
            .await
            .unwrap();
        manager
            .create_child_loop(&parent, LoopType::Spec, 1)
            .await
            .unwrap();
        manager
            .create_child_loop(&parent, LoopType::Spec, 2)
            .await
            .unwrap();

        let children = manager.find_by_parent(&parent.id).await.unwrap();
        assert_eq!(children.len(), 2);
    }

    #[tokio::test]
    async fn test_start_loop_not_found() {
        let (_temp, storage, llm_client, tool_router, worktree_manager, signal_manager) =
            create_test_deps();
        let config = LoopManagerConfig::default();
        let manager = LoopManager::new(
            storage,
            llm_client,
            tool_router,
            worktree_manager,
            signal_manager,
            config,
        );

        let result = manager.start_loop("nonexistent").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_get_loop_not_found() {
        let (_temp, storage, llm_client, tool_router, worktree_manager, signal_manager) =
            create_test_deps();
        let config = LoopManagerConfig::default();
        let manager = LoopManager::new(
            storage,
            llm_client,
            tool_router,
            worktree_manager,
            signal_manager,
            config,
        );

        let result = manager.get_loop("nonexistent").await.unwrap();
        assert!(result.is_none());
    }
}
