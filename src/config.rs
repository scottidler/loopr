use eyre::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

// --- Strategy Knobs ---

/// How to handle stale Bundles when a new Tick is published.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StalePolicy {
    /// Agent rebases and re-tests at next safe point.
    #[default]
    ReplanAtSafePoint,
    /// Bundle rejected outright if stale.
    RejectIfStale,
    /// Daemon auto-rebases and re-runs validation.
    AutoReplayAndVerify,
}

/// How to handle resource conflicts between agents.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConflictPolicy {
    /// Locks checked, conflicts detected at merge time.
    #[default]
    LockAdvisory,
    /// File writes to locked paths rejected by executor.
    LockStrict,
}

/// When the Integrator creates Ticks.
#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "mode")]
pub enum TickCadence {
    /// Create a Tick as soon as any Bundle is Accepted.
    #[default]
    Continuous,
    /// Wait for N Accepted Bundles or a timeout before creating a Tick.
    Batched { min_bundles: u32, timeout_secs: u64 },
}

/// Limits on Bundle size to keep changes reviewable.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct BundleSizePolicy {
    pub max_files_touched: u32,
    pub max_loc_changed: u32,
}

impl Default for BundleSizePolicy {
    fn default() -> Self {
        Self {
            max_files_touched: 8,
            max_loc_changed: 300,
        }
    }
}

/// How strict the Doc Validator is about ambiguity.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ValidatorStrictness {
    /// Any ambiguity in doc = Fail.
    #[default]
    HardFailOnAnyAmbiguity,
    /// Ambiguity = Warn, not Fail.
    AllowAmbiguityWithFlags,
    /// All issues are Info, never Fail.
    SuggestOnly,
}

/// Policy for auto-promoting Learnings to Policies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct PromotionPolicy {
    pub min_reinforcements: u32,
    pub max_age_days: u32,
    pub auto_promote: bool,
}

impl Default for PromotionPolicy {
    fn default() -> Self {
        Self {
            min_reinforcements: 3,
            max_age_days: 30,
            auto_promote: true,
        }
    }
}

/// SLA thresholds for detecting stuck Work items.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct WorkSlaConfig {
    pub max_attempts: u32,
    pub max_wall_clock_minutes: u64,
}

impl Default for WorkSlaConfig {
    fn default() -> Self {
        Self {
            max_attempts: 3,
            max_wall_clock_minutes: 30,
        }
    }
}

/// How strict the Coverage Evaluator is about minor gaps.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CoverageStrictness {
    /// All children must fully cover the parent — no gaps allowed.
    #[default]
    RequireComplete,
    /// Minor gaps are allowed; only critical gaps block activation.
    AllowMinorGaps,
    /// Coverage evaluation runs but results are advisory only — never blocks.
    SuggestOnly,
}

/// Strategy knobs controlling system behavior.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct StrategyConfig {
    pub stale_policy: StalePolicy,
    pub conflict_policy: ConflictPolicy,
    pub tick_cadence: TickCadence,
    pub bundle_size: BundleSizePolicy,
    pub validator_strictness: ValidatorStrictness,
    pub promotion: PromotionPolicy,
    pub max_lock_ttl_minutes: u64,
    pub work_sla: WorkSlaConfig,
    /// Enable/disable coverage evaluation at decomposition boundaries.
    pub coverage_enabled: bool,
    /// How strict the Coverage Evaluator is about minor gaps.
    pub coverage_strictness: CoverageStrictness,
    /// Maximum decomposition attempts per parent before bubble-up.
    pub max_decomposition_attempts: u32,
    /// Maximum bubble-up depth to prevent infinite recursion.
    pub max_bubble_up_depth: u32,
    /// Enable/disable the collaborative Plan interview.
    pub plan_interview_enabled: bool,
    /// Require explicit user approval of the Plan before decomposition.
    pub plan_approval_required: bool,
}

impl Default for StrategyConfig {
    fn default() -> Self {
        Self {
            stale_policy: StalePolicy::default(),
            conflict_policy: ConflictPolicy::default(),
            tick_cadence: TickCadence::default(),
            bundle_size: BundleSizePolicy::default(),
            validator_strictness: ValidatorStrictness::default(),
            promotion: PromotionPolicy::default(),
            max_lock_ttl_minutes: 60,
            work_sla: WorkSlaConfig::default(),
            coverage_enabled: true,
            coverage_strictness: CoverageStrictness::default(),
            max_decomposition_attempts: 3,
            max_bubble_up_depth: 2,
            plan_interview_enabled: true,
            plan_approval_required: true,
        }
    }
}

/// How the Coordinator handles the interview phase.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InterviewMode {
    /// Default: Coordinator asks questions, human answers via TUI.
    #[default]
    Interactive,
    /// Coordinator generates questions then self-answers from goal + repo context.
    /// Auto-approves the resulting Plan.
    Auto,
    /// Skip Interviewing entirely. Start in Planning state.
    /// Auto-creates a Plan from the goal text.
    Skip,
}

/// Coordinator-specific config extending AgentRoleConfig.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct CoordinatorConfig {
    #[serde(flatten)]
    pub role: AgentRoleConfig,
    pub active_interval_secs: u64,
    pub idle_interval_secs: u64,
    pub max_validation_attempts: u32,
    pub max_work_retries: u32,
    pub phase_timeout_secs: u64,
    pub goal_timeout_secs: u64,
    #[serde(default)]
    pub interview_mode: InterviewMode,
}

impl Default for CoordinatorConfig {
    fn default() -> Self {
        Self {
            role: AgentRoleConfig {
                model: "claude-opus-4-6".to_string(),
                api_key_env: "ANTHROPIC_API_KEY".to_string(),
                max_tokens: 8192,
                max_iterations: u32::MAX,
                min_pool: 1,
                max_pool: 1,
                temperature: 0.2,
                session_timeout_secs: None, // Coordinator is long-lived
                max_requeries: 3,
            },
            active_interval_secs: 5,
            idle_interval_secs: 30,
            max_validation_attempts: 3,
            max_work_retries: 3,
            phase_timeout_secs: 3600,
            goal_timeout_secs: 14400,
            interview_mode: InterviewMode::default(),
        }
    }
}

/// Daemon-specific configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct DaemonConfig {
    pub socket_path: PathBuf,
    pub pid_path: PathBuf,
}

impl Default for DaemonConfig {
    fn default() -> Self {
        let base = dirs::data_local_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join("loopr");
        Self {
            socket_path: base.join("daemon.sock"),
            pid_path: base.join("daemon.pid"),
        }
    }
}

/// Project-specific configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ProjectConfig {
    pub repo_path: PathBuf,
    pub worktree_dir: PathBuf,
}

/// Integrator configuration — validation commands run during tick validation.
/// The Integrator is deterministic code (not an LLM agent).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct IntegratorConfig {
    pub validation_commands: Vec<String>,
    pub interval_secs: u64,
    pub enabled: bool,
    pub session_timeout_secs: Option<u64>,
}

/// Agent system configuration — LLM agents running as Tokio tasks.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct AgentConfig {
    pub enabled: bool,
    pub auto_start_implementer: bool,
    pub auto_start_reviewer: bool,
    pub auto_start_coordinator: bool,
    /// When true, persistent worker pool pulls Ready Works instead of
    /// push-based AssignAgent. Default false (feature flag).
    pub pull_based_workers: bool,
    /// Number of persistent worker tasks in the pull-based pool.
    pub worker_pool_size: u32,
    pub implementer: AgentRoleConfig,
    pub reviewer: AgentRoleConfig,
    pub coordinator: CoordinatorConfig,
    pub researcher: AgentRoleConfig,
    pub tools: Vec<ToolEntry>,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            auto_start_implementer: false,
            auto_start_reviewer: false,
            auto_start_coordinator: false,
            pull_based_workers: false,
            worker_pool_size: 2,
            implementer: AgentRoleConfig::default_implementer(),
            reviewer: AgentRoleConfig::default_reviewer(),
            coordinator: CoordinatorConfig::default(),
            researcher: AgentRoleConfig::default_researcher(),
            tools: vec![
                ToolEntry {
                    name: "test".to_string(),
                    command: "cargo test".to_string(),
                    timeout_secs: 300,
                    worktree: true,
                },
                ToolEntry {
                    name: "clippy".to_string(),
                    command: "cargo clippy -- -D warnings".to_string(),
                    timeout_secs: 120,
                    worktree: true,
                },
                ToolEntry {
                    name: "fmt-check".to_string(),
                    command: "cargo fmt --check".to_string(),
                    timeout_secs: 30,
                    worktree: true,
                },
                ToolEntry {
                    name: "fmt".to_string(),
                    command: "cargo fmt".to_string(),
                    timeout_secs: 30,
                    worktree: true,
                },
                ToolEntry {
                    name: "build".to_string(),
                    command: "cargo build".to_string(),
                    timeout_secs: 300,
                    worktree: true,
                },
            ],
        }
    }
}

/// Per-role agent configuration (Implementer or Reviewer).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct AgentRoleConfig {
    pub model: String,
    pub api_key_env: String,
    pub max_tokens: u32,
    pub max_iterations: u32,
    pub min_pool: u32,
    pub max_pool: u32,
    pub temperature: f32,
    pub session_timeout_secs: Option<u64>,
    /// Max re-prompts per iteration for self-correction (parse/tool errors). 0 = disabled.
    pub max_requeries: u32,
}

impl Default for AgentRoleConfig {
    fn default() -> Self {
        Self::default_implementer()
    }
}

impl AgentRoleConfig {
    pub fn default_implementer() -> Self {
        Self {
            model: "claude-sonnet-4-6".to_string(),
            api_key_env: "ANTHROPIC_API_KEY".to_string(),
            max_tokens: 8192,
            max_iterations: 20,
            min_pool: 2,
            max_pool: 6,
            temperature: 0.3,
            session_timeout_secs: Some(1800), // 30 min
            max_requeries: 3,
        }
    }

    pub fn default_reviewer() -> Self {
        Self {
            model: "claude-sonnet-4-6".to_string(),
            api_key_env: "ANTHROPIC_API_KEY".to_string(),
            max_tokens: 4096,
            max_iterations: 5,
            min_pool: 1,
            max_pool: 2,
            temperature: 0.1,
            session_timeout_secs: Some(600), // 10 min
            max_requeries: 3,
        }
    }

    pub fn default_researcher() -> Self {
        Self {
            model: "claude-sonnet-4-6".to_string(),
            api_key_env: "ANTHROPIC_API_KEY".to_string(),
            max_tokens: 4096,
            max_iterations: 10,
            min_pool: 1,
            max_pool: 4,
            temperature: 0.1,
            session_timeout_secs: Some(600), // 10 min
            max_requeries: 3,
        }
    }
}

/// A configured tool available to agents.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ToolEntry {
    pub name: String,
    pub command: String,
    pub timeout_secs: u64,
    pub worktree: bool,
}

/// Chat session configuration — model for interactive chat and delegate subagents.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ChatConfig {
    pub model: String,
    pub delegate_model: String,
    pub api_key_env: String,
    pub max_tokens: u32,
    pub temperature: f32,
    pub max_iterations: u32,
}

impl Default for ChatConfig {
    fn default() -> Self {
        Self {
            model: "claude-sonnet-4-6".to_string(),
            delegate_model: "claude-haiku-4-5-20251001".to_string(),
            api_key_env: "ANTHROPIC_API_KEY".to_string(),
            max_tokens: 8192,
            temperature: 0.3,
            max_iterations: 3,
        }
    }
}

impl ChatConfig {
    /// Build an `AgentRoleConfig` for the parent chat LLM.
    pub fn to_role_config(&self) -> AgentRoleConfig {
        AgentRoleConfig {
            model: self.model.clone(),
            api_key_env: self.api_key_env.clone(),
            max_tokens: self.max_tokens,
            max_iterations: self.max_iterations,
            temperature: self.temperature,
            ..AgentRoleConfig::default_implementer()
        }
    }

    /// Build an `AgentRoleConfig` for delegate subagents.
    pub fn to_delegate_role_config(&self) -> AgentRoleConfig {
        AgentRoleConfig {
            model: self.delegate_model.clone(),
            api_key_env: self.api_key_env.clone(),
            max_tokens: self.max_tokens,
            max_iterations: 20,
            temperature: self.temperature,
            ..AgentRoleConfig::default_implementer()
        }
    }
}

/// Doc Validator configuration — LLM-powered document validation for quality gates.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ValidatorConfig {
    pub enabled: bool,
    pub provider: String,
    pub model: String,
    pub api_key_env: String,
    pub max_tokens: u32,
    pub temperature: f32,
}

impl Default for ValidatorConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: "anthropic".to_string(),
            model: "claude-sonnet-4-6".to_string(),
            api_key_env: "ANTHROPIC_API_KEY".to_string(),
            max_tokens: 4096,
            temperature: 0.0,
        }
    }
}

/// Coverage Evaluator configuration — LLM-powered coverage evaluation at decomposition boundaries.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct EvaluatorConfig {
    pub enabled: bool,
    pub provider: String,
    pub model: String,
    pub api_key_env: String,
    pub max_tokens: u32,
    pub temperature: f32,
}

impl Default for EvaluatorConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            provider: "anthropic".to_string(),
            model: "claude-sonnet-4-6".to_string(),
            api_key_env: "ANTHROPIC_API_KEY".to_string(),
            max_tokens: 4096,
            temperature: 0.0,
        }
    }
}

impl Default for IntegratorConfig {
    fn default() -> Self {
        Self {
            validation_commands: vec![
                "cargo fmt --check".to_string(),
                "cargo clippy -- -D warnings".to_string(),
                "cargo test".to_string(),
            ],
            interval_secs: 15,
            enabled: false,
            session_timeout_secs: Some(1200), // 20 min
        }
    }
}

impl Default for ProjectConfig {
    fn default() -> Self {
        Self {
            repo_path: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            worktree_dir: PathBuf::from(".worktrees"),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct Config {
    pub name: String,
    pub debug: bool,
    pub log_level: Option<String>,
    pub daemon: DaemonConfig,
    pub project: ProjectConfig,
    pub chat: ChatConfig,
    pub integrator: IntegratorConfig,
    pub validator: ValidatorConfig,
    pub evaluator: EvaluatorConfig,
    pub agents: AgentConfig,
    pub strategy: StrategyConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            name: "loopr".to_string(),
            debug: false,
            log_level: None,
            daemon: DaemonConfig::default(),
            project: ProjectConfig::default(),
            chat: ChatConfig::default(),
            integrator: IntegratorConfig::default(),
            validator: ValidatorConfig::default(),
            evaluator: EvaluatorConfig::default(),
            agents: AgentConfig::default(),
            strategy: StrategyConfig::default(),
        }
    }
}

impl Config {
    /// Load configuration with fallback chain
    pub fn load(config_path: Option<&PathBuf>) -> Result<Self> {
        log::debug!("Config::load(config_path={:?})", config_path);
        // If explicit config path provided, try to load it
        if let Some(path) = config_path {
            return Self::load_from_file(path).context(format!("Failed to load config from {}", path.display()));
        }

        // Try primary location: ~/.config/<project>/<project>.yml
        if let Some(config_dir) = dirs::config_dir() {
            let project_name = env!("CARGO_PKG_NAME");
            let primary_config = config_dir.join(project_name).join(format!("{}.yml", project_name));
            if primary_config.exists() {
                match Self::load_from_file(&primary_config) {
                    Ok(config) => return Ok(config),
                    Err(e) => {
                        log::warn!("Failed to load config from {}: {}", primary_config.display(), e);
                    }
                }
            }
        }

        // Try fallback location: ./<project>.yml
        let project_name = env!("CARGO_PKG_NAME");
        let fallback_config = PathBuf::from(format!("{}.yml", project_name));
        if fallback_config.exists() {
            match Self::load_from_file(&fallback_config) {
                Ok(config) => return Ok(config),
                Err(e) => {
                    log::warn!("Failed to load config from {}: {}", fallback_config.display(), e);
                }
            }
        }

        // No config file found, use defaults
        log::info!("No config file found, using defaults");
        Ok(Self::default())
    }

    fn load_from_file<P: AsRef<Path>>(path: P) -> Result<Self> {
        log::debug!("Config::load_from_file(path={})", path.as_ref().display());
        let content = fs::read_to_string(&path).context("Failed to read config file")?;

        let config: Self = serde_yaml::from_str(&content).context("Failed to parse config file")?;

        log::info!("Loaded config from: {}", path.as_ref().display());
        Ok(config)
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_config_default() {
        let config = Config::default();
        assert_eq!(config.name, "loopr");
        assert!(!config.debug);
    }

    #[test]
    fn test_config_load_defaults_when_no_file() {
        // Run in a temp dir so we don't pick up ./loopr.yml from the project root
        let tmp = std::env::temp_dir().join("loopr_test_no_config");
        let _ = std::fs::create_dir_all(&tmp);
        let prev = std::env::current_dir().unwrap();
        std::env::set_current_dir(&tmp).unwrap();
        let config = Config::load(None).expect("should load defaults");
        std::env::set_current_dir(prev).unwrap();
        let _ = std::fs::remove_dir_all(&tmp);
        assert_eq!(config.name, "loopr");
    }

    #[test]
    fn test_daemon_config_default() {
        let dc = DaemonConfig::default();
        assert!(dc.socket_path.ends_with("daemon.sock"));
        assert!(dc.pid_path.ends_with("daemon.pid"));
    }

    #[test]
    fn test_project_config_default() {
        let pc = ProjectConfig::default();
        assert!(pc.repo_path.is_absolute() || pc.repo_path == std::path::Path::new("."));
    }

    #[test]
    fn test_config_has_daemon_and_project() {
        let config = Config::default();
        assert!(config.daemon.socket_path.ends_with("daemon.sock"));
        assert!(!config.project.repo_path.as_os_str().is_empty());
    }

    #[test]
    fn test_integrator_config_default() {
        let ic = IntegratorConfig::default();
        assert!(!ic.validation_commands.is_empty());
        assert!(ic.validation_commands.iter().any(|c| c.contains("cargo test")));
    }

    #[test]
    fn test_config_has_integrator() {
        let config = Config::default();
        assert!(!config.integrator.validation_commands.is_empty());
    }

    #[test]
    fn test_agent_config_default_disabled() {
        let ac = AgentConfig::default();
        assert!(!ac.enabled);
        assert!(!ac.auto_start_implementer);
        assert!(!ac.auto_start_reviewer);
        assert!(!ac.auto_start_coordinator);
    }

    #[test]
    fn test_agent_config_default_tools() {
        let ac = AgentConfig::default();
        assert_eq!(ac.tools.len(), 5);
        let names: Vec<&str> = ac.tools.iter().map(|t| t.name.as_str()).collect();
        assert!(names.contains(&"test"));
        assert!(names.contains(&"clippy"));
        assert!(names.contains(&"fmt-check"));
        assert!(names.contains(&"fmt"));
        assert!(names.contains(&"build"));
    }

    #[test]
    fn test_agent_role_config_implementer_defaults() {
        let rc = AgentRoleConfig::default_implementer();
        assert_eq!(rc.max_iterations, 20);
        assert_eq!(rc.max_pool, 6);
        assert_eq!(rc.max_tokens, 8192);
        assert!((rc.temperature - 0.3).abs() < f32::EPSILON);
        assert_eq!(rc.max_requeries, 3);
    }

    #[test]
    fn test_agent_role_config_reviewer_defaults() {
        let rc = AgentRoleConfig::default_reviewer();
        assert_eq!(rc.max_iterations, 5);
        assert_eq!(rc.max_pool, 2);
        assert_eq!(rc.max_tokens, 4096);
        assert!((rc.temperature - 0.1).abs() < f32::EPSILON);
        assert_eq!(rc.max_requeries, 3);
    }

    #[test]
    fn test_tool_entry_fields() {
        let tool = ToolEntry {
            name: "test".to_string(),
            command: "cargo test".to_string(),
            timeout_secs: 300,
            worktree: true,
        };
        assert_eq!(tool.name, "test");
        assert_eq!(tool.command, "cargo test");
        assert_eq!(tool.timeout_secs, 300);
        assert!(tool.worktree);
    }

    #[test]
    fn test_config_has_agents() {
        let config = Config::default();
        assert!(!config.agents.enabled);
        assert!(!config.agents.tools.is_empty());
    }

    #[test]
    fn test_agent_config_deserialize_from_yaml() {
        let yaml = r#"
enabled: true
auto_start_implementer: true
auto_start_reviewer: false
implementer:
  model: "claude-sonnet-4-6"
  api_key_env: "MY_KEY"
  max_tokens: 4096
  max_iterations: 10
  min_pool: 2
  max_pool: 3
  temperature: 0.5
reviewer:
  model: "claude-sonnet-4-6"
  api_key_env: "MY_KEY"
  max_tokens: 2048
  max_iterations: 3
  min_pool: 1
  max_pool: 1
  temperature: 0.0
tools:
  - name: "test"
    command: "cargo test"
    timeout_secs: 60
    worktree: true
"#;
        let ac: AgentConfig = serde_yaml::from_str(yaml).expect("should parse agent config");
        assert!(ac.enabled);
        assert!(ac.auto_start_implementer);
        assert!(!ac.auto_start_reviewer);
        assert_eq!(ac.implementer.max_pool, 3);
        assert_eq!(ac.reviewer.max_iterations, 3);
        assert_eq!(ac.tools.len(), 1);
        assert_eq!(ac.tools[0].name, "test");
    }

    // --- Strategy Knobs tests ---

    #[test]
    fn test_stale_policy_default() {
        assert_eq!(StalePolicy::default(), StalePolicy::ReplanAtSafePoint);
    }

    #[test]
    fn test_stale_policy_serde_roundtrip() {
        for policy in [
            StalePolicy::ReplanAtSafePoint,
            StalePolicy::RejectIfStale,
            StalePolicy::AutoReplayAndVerify,
        ] {
            let json = serde_json::to_string(&policy).unwrap();
            let deserialized: StalePolicy = serde_json::from_str(&json).unwrap();
            assert_eq!(policy, deserialized);
        }
    }

    #[test]
    fn test_conflict_policy_default() {
        assert_eq!(ConflictPolicy::default(), ConflictPolicy::LockAdvisory);
    }

    #[test]
    fn test_conflict_policy_serde_roundtrip() {
        for policy in [ConflictPolicy::LockAdvisory, ConflictPolicy::LockStrict] {
            let json = serde_json::to_string(&policy).unwrap();
            let deserialized: ConflictPolicy = serde_json::from_str(&json).unwrap();
            assert_eq!(policy, deserialized);
        }
    }

    #[test]
    fn test_tick_cadence_default() {
        assert_eq!(TickCadence::default(), TickCadence::Continuous);
    }

    #[test]
    fn test_tick_cadence_batched_serde() {
        let cadence = TickCadence::Batched {
            min_bundles: 3,
            timeout_secs: 300,
        };
        let json = serde_json::to_string(&cadence).unwrap();
        let deserialized: TickCadence = serde_json::from_str(&json).unwrap();
        assert_eq!(cadence, deserialized);
    }

    #[test]
    fn test_bundle_size_policy_default() {
        let bsp = BundleSizePolicy::default();
        assert_eq!(bsp.max_files_touched, 8);
        assert_eq!(bsp.max_loc_changed, 300);
    }

    #[test]
    fn test_validator_strictness_default() {
        assert_eq!(
            ValidatorStrictness::default(),
            ValidatorStrictness::HardFailOnAnyAmbiguity
        );
    }

    #[test]
    fn test_validator_strictness_serde_roundtrip() {
        for strictness in [
            ValidatorStrictness::HardFailOnAnyAmbiguity,
            ValidatorStrictness::AllowAmbiguityWithFlags,
            ValidatorStrictness::SuggestOnly,
        ] {
            let json = serde_json::to_string(&strictness).unwrap();
            let deserialized: ValidatorStrictness = serde_json::from_str(&json).unwrap();
            assert_eq!(strictness, deserialized);
        }
    }

    #[test]
    fn test_promotion_policy_default() {
        let pp = PromotionPolicy::default();
        assert_eq!(pp.min_reinforcements, 3);
        assert_eq!(pp.max_age_days, 30);
        assert!(pp.auto_promote);
    }

    #[test]
    fn test_work_sla_config_default() {
        let sla = WorkSlaConfig::default();
        assert_eq!(sla.max_attempts, 3);
        assert_eq!(sla.max_wall_clock_minutes, 30);
    }

    #[test]
    fn test_work_sla_config_serde_roundtrip() {
        let sla = WorkSlaConfig {
            max_attempts: 5,
            max_wall_clock_minutes: 60,
        };
        let json = serde_json::to_string(&sla).unwrap();
        let deserialized: WorkSlaConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(sla, deserialized);
    }

    #[test]
    fn test_work_sla_config_yaml_default_omitted() {
        // When not specified in YAML, should use defaults
        let yaml = r#"
stale_policy: replan_at_safe_point
"#;
        let sc: StrategyConfig = serde_yaml::from_str(yaml).expect("should parse with default work_sla");
        assert_eq!(sc.work_sla.max_attempts, 3);
        assert_eq!(sc.work_sla.max_wall_clock_minutes, 30);
    }

    #[test]
    fn test_strategy_config_default() {
        let sc = StrategyConfig::default();
        assert_eq!(sc.stale_policy, StalePolicy::ReplanAtSafePoint);
        assert_eq!(sc.conflict_policy, ConflictPolicy::LockAdvisory);
        assert_eq!(sc.tick_cadence, TickCadence::Continuous);
        assert_eq!(sc.bundle_size.max_files_touched, 8);
        assert_eq!(sc.validator_strictness, ValidatorStrictness::HardFailOnAnyAmbiguity);
        assert!(sc.promotion.auto_promote);
        assert_eq!(sc.max_lock_ttl_minutes, 60);
        assert_eq!(sc.work_sla.max_attempts, 3);
        assert_eq!(sc.work_sla.max_wall_clock_minutes, 30);
    }

    #[test]
    fn test_strategy_config_serde_roundtrip() {
        let sc = StrategyConfig::default();
        let json = serde_json::to_string(&sc).unwrap();
        let deserialized: StrategyConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(sc, deserialized);
    }

    #[test]
    fn test_config_has_strategy() {
        let config = Config::default();
        assert_eq!(config.strategy.max_lock_ttl_minutes, 60);
    }

    // --- CoordinatorConfig tests ---

    #[test]
    fn test_coordinator_config_default() {
        let cc = CoordinatorConfig::default();
        assert_eq!(cc.active_interval_secs, 5);
        assert_eq!(cc.idle_interval_secs, 30);
        assert_eq!(cc.max_validation_attempts, 3);
        assert_eq!(cc.role.max_pool, 1);
        assert!((cc.role.temperature - 0.2).abs() < f32::EPSILON);
    }

    #[test]
    fn test_agent_config_has_coordinator() {
        let ac = AgentConfig::default();
        assert_eq!(ac.coordinator.role.max_pool, 1);
        assert!(!ac.auto_start_coordinator);
    }

    #[test]
    fn test_agent_role_config_researcher_defaults() {
        let rc = AgentRoleConfig::default_researcher();
        assert_eq!(rc.max_iterations, 10);
        assert_eq!(rc.max_pool, 4);
        assert_eq!(rc.max_tokens, 4096);
        assert!((rc.temperature - 0.1).abs() < f32::EPSILON);
    }

    #[test]
    fn test_agent_config_has_researcher() {
        let ac = AgentConfig::default();
        assert_eq!(ac.researcher.max_pool, 4);
    }

    // --- IntegratorConfig extension tests ---

    #[test]
    fn test_integrator_config_new_fields() {
        let ic = IntegratorConfig::default();
        assert_eq!(ic.interval_secs, 15);
        assert!(!ic.enabled);
    }

    #[test]
    fn test_strategy_config_deserialize_from_yaml() {
        let yaml = r#"
stale_policy: reject_if_stale
conflict_policy: lock_strict
tick_cadence:
  mode: batched
  min_bundles: 5
  timeout_secs: 600
bundle_size:
  max_files_touched: 10
  max_loc_changed: 500
validator_strictness: suggest_only
promotion:
  min_reinforcements: 5
  max_age_days: 60
  auto_promote: false
max_lock_ttl_minutes: 120
"#;
        let sc: StrategyConfig = serde_yaml::from_str(yaml).expect("should parse strategy config");
        assert_eq!(sc.stale_policy, StalePolicy::RejectIfStale);
        assert_eq!(sc.conflict_policy, ConflictPolicy::LockStrict);
        assert_eq!(
            sc.tick_cadence,
            TickCadence::Batched {
                min_bundles: 5,
                timeout_secs: 600,
            }
        );
        assert_eq!(sc.bundle_size.max_files_touched, 10);
        assert_eq!(sc.bundle_size.max_loc_changed, 500);
        assert_eq!(sc.validator_strictness, ValidatorStrictness::SuggestOnly);
        assert_eq!(sc.promotion.min_reinforcements, 5);
        assert!(!sc.promotion.auto_promote);
        assert_eq!(sc.max_lock_ttl_minutes, 120);
    }

    #[test]
    fn test_config_log_level_default_none() {
        let config = Config::default();
        assert!(config.log_level.is_none());
    }

    #[test]
    fn test_config_log_level_yaml_roundtrip() {
        let yaml = r#"
name: test
log_level: debug
"#;
        let config: Config = serde_yaml::from_str(yaml).expect("should parse config with log_level");
        assert_eq!(config.log_level.as_deref(), Some("debug"));

        let serialized = serde_yaml::to_string(&config).unwrap();
        let deserialized: Config = serde_yaml::from_str(&serialized).unwrap();
        assert_eq!(deserialized.log_level.as_deref(), Some("debug"));
    }

    #[test]
    fn test_config_log_level_yaml_without() {
        let yaml = r#"
name: test
"#;
        let config: Config = serde_yaml::from_str(yaml).expect("should parse config without log_level");
        assert!(config.log_level.is_none());
    }

    // --- Coverage & Interview config tests ---

    #[test]
    fn test_coverage_strictness_default() {
        assert_eq!(CoverageStrictness::default(), CoverageStrictness::RequireComplete);
    }

    #[test]
    fn test_coverage_strictness_serde_roundtrip() {
        for strictness in [
            CoverageStrictness::RequireComplete,
            CoverageStrictness::AllowMinorGaps,
            CoverageStrictness::SuggestOnly,
        ] {
            let json = serde_json::to_string(&strictness).unwrap();
            let deserialized: CoverageStrictness = serde_json::from_str(&json).unwrap();
            assert_eq!(strictness, deserialized);
        }
    }

    #[test]
    fn test_strategy_config_coverage_defaults() {
        let sc = StrategyConfig::default();
        assert!(sc.coverage_enabled);
        assert_eq!(sc.coverage_strictness, CoverageStrictness::RequireComplete);
        assert_eq!(sc.max_decomposition_attempts, 3);
        assert_eq!(sc.max_bubble_up_depth, 2);
        assert!(sc.plan_interview_enabled);
        assert!(sc.plan_approval_required);
    }

    #[test]
    fn test_strategy_config_coverage_yaml() {
        let yaml = r#"
coverage_enabled: false
coverage_strictness: allow_minor_gaps
max_decomposition_attempts: 5
max_bubble_up_depth: 3
plan_interview_enabled: false
plan_approval_required: false
"#;
        let sc: StrategyConfig = serde_yaml::from_str(yaml).expect("should parse coverage config");
        assert!(!sc.coverage_enabled);
        assert_eq!(sc.coverage_strictness, CoverageStrictness::AllowMinorGaps);
        assert_eq!(sc.max_decomposition_attempts, 5);
        assert_eq!(sc.max_bubble_up_depth, 3);
        assert!(!sc.plan_interview_enabled);
        assert!(!sc.plan_approval_required);
    }

    #[test]
    fn test_evaluator_config_default() {
        let ec = EvaluatorConfig::default();
        assert!(!ec.enabled);
        assert_eq!(ec.provider, "anthropic");
        assert_eq!(ec.model, "claude-sonnet-4-6");
    }

    #[test]
    fn test_config_has_evaluator() {
        let config = Config::default();
        assert!(!config.evaluator.enabled);
    }

    // --- InterviewMode tests ---

    #[test]
    fn test_interview_mode_default() {
        assert_eq!(InterviewMode::default(), InterviewMode::Interactive);
    }

    #[test]
    fn test_interview_mode_serde_roundtrip() {
        for mode in [InterviewMode::Interactive, InterviewMode::Auto, InterviewMode::Skip] {
            let json = serde_json::to_string(&mode).unwrap();
            let deserialized: InterviewMode = serde_json::from_str(&json).unwrap();
            assert_eq!(mode, deserialized);
        }
    }

    #[test]
    fn test_coordinator_config_interview_mode_default() {
        let cc = CoordinatorConfig::default();
        assert_eq!(cc.interview_mode, InterviewMode::Interactive);
    }

    #[test]
    fn test_coordinator_config_interview_mode_yaml() {
        let yaml = r#"
interview_mode: skip
"#;
        let cc: CoordinatorConfig = serde_yaml::from_str(yaml).expect("should parse coordinator config");
        assert_eq!(cc.interview_mode, InterviewMode::Skip);
    }

    #[test]
    fn test_coordinator_config_interview_mode_yaml_auto() {
        let yaml = r#"
interview_mode: auto
"#;
        let cc: CoordinatorConfig = serde_yaml::from_str(yaml).expect("should parse coordinator config");
        assert_eq!(cc.interview_mode, InterviewMode::Auto);
    }

    // --- ChatConfig tests ---

    #[test]
    fn test_chat_config_default() {
        let cc = ChatConfig::default();
        assert_eq!(cc.model, "claude-sonnet-4-6");
        assert_eq!(cc.delegate_model, "claude-haiku-4-5-20251001");
        assert_eq!(cc.max_tokens, 8192);
        assert_eq!(cc.max_iterations, 3);
        assert!((cc.temperature - 0.3).abs() < f32::EPSILON);
    }

    #[test]
    fn test_chat_config_to_role_config() {
        let cc = ChatConfig::default();
        let role = cc.to_role_config();
        assert_eq!(role.model, "claude-sonnet-4-6");
        assert_eq!(role.max_tokens, 8192);
        assert_eq!(role.max_iterations, 3);
    }

    #[test]
    fn test_chat_config_to_delegate_role_config() {
        let cc = ChatConfig::default();
        let role = cc.to_delegate_role_config();
        assert_eq!(role.model, "claude-haiku-4-5-20251001");
        assert_eq!(role.max_iterations, 20);
    }

    #[test]
    fn test_chat_config_yaml_roundtrip() {
        let yaml = r#"
model: "claude-opus-4-6"
delegate_model: "claude-sonnet-4-6"
max_tokens: 4096
temperature: 0.5
max_iterations: 20
"#;
        let cc: ChatConfig = serde_yaml::from_str(yaml).expect("should parse chat config");
        assert_eq!(cc.model, "claude-opus-4-6");
        assert_eq!(cc.delegate_model, "claude-sonnet-4-6");
        assert_eq!(cc.max_tokens, 4096);
        assert_eq!(cc.max_iterations, 20);
    }

    #[test]
    fn test_config_has_chat() {
        let config = Config::default();
        assert_eq!(config.chat.model, "claude-sonnet-4-6");
        assert_eq!(config.chat.delegate_model, "claude-haiku-4-5-20251001");
    }

    #[test]
    fn test_coordinator_config_interview_mode_yaml_omitted() {
        let yaml = r#"
active_interval_secs: 10
"#;
        let cc: CoordinatorConfig = serde_yaml::from_str(yaml).expect("should parse without interview_mode");
        assert_eq!(cc.interview_mode, InterviewMode::Interactive);
    }
}
