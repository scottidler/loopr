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

/// Shared base configuration for any LLM-calling subsystem.
///
/// Every section that makes an LLM call embeds this via `#[serde(flatten)]`.
/// This gives AR a uniform surface: `model`, `max-tokens`, `temperature`, and
/// `api-key-env` appear at the same nesting depth in every section.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct LlmConfig {
    pub model: String,
    pub api_key_env: String,
    pub max_tokens: u32,
    pub temperature: f32,
}

impl Default for LlmConfig {
    fn default() -> Self {
        Self {
            model: "claude-sonnet-4-6".to_string(),
            api_key_env: "ANTHROPIC_API_KEY".to_string(),
            max_tokens: 4096,
            temperature: 0.0,
        }
    }
}

/// Goal clarity gate configuration - LLM pre-validation for `loopr run`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct ClarityGateConfig {
    /// Enable/disable the clarity gate (default: true).
    pub enabled: bool,
    /// Model for clarity evaluation.
    pub model: String,
    /// API key env var.
    pub api_key_env: String,
    /// Max tokens for the clarity evaluation call.
    pub max_tokens: u32,
    /// Temperature for the clarity evaluation call.
    pub temperature: f32,
    /// Minimum score per dimension to pass (default: 3).
    pub min_score: u8,
}

impl Default for ClarityGateConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            model: "claude-sonnet-4-6".to_string(),
            api_key_env: "ANTHROPIC_API_KEY".to_string(),
            max_tokens: 1024,
            temperature: 0.0,
            min_score: 3,
        }
    }
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
    /// Maximum decomposition attempts per parent before bubble-up.
    pub max_decomposition_attempts: u32,
    /// Maximum bubble-up depth to prevent infinite recursion.
    pub max_bubble_up_depth: u32,
    /// Maximum consecutive agent session failures before Work transitions to
    /// Blocked. Catches crash-before-bundle loops independently of
    /// max_bundle_rejections.
    #[serde(default = "default_max_session_failures")]
    pub max_session_failures: u32,
}

fn default_max_session_failures() -> u32 {
    3
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
            max_decomposition_attempts: 3,
            max_bubble_up_depth: 2,
            max_session_failures: default_max_session_failures(),
        }
    }
}

/// How the Director handles the interview phase.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InterviewMode {
    /// Default: Director asks questions, human answers via TUI.
    #[default]
    Interactive,
    /// Director generates questions then self-answers from goal + repo context.
    /// Auto-approves the resulting Plan.
    Auto,
    /// Skip Interviewing entirely. Start in Planning state.
    /// Auto-creates a Plan from the goal text.
    Skip,
}

fn default_max_abandon_ratio() -> f64 {
    0.4
}

/// Daemon-specific configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct DaemonConfig {
    pub socket_path: PathBuf,
    pub pid_path: PathBuf,
}

/// Periodic reconciliation sweep configuration.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct ReconcilerConfig {
    /// Seconds between reconciliation sweeps. Default: 60.
    pub interval_secs: u64,
    /// Enable periodic reconciliation. Default: true.
    pub enabled: bool,
}

impl Default for ReconcilerConfig {
    fn default() -> Self {
        Self {
            interval_secs: 60,
            enabled: true,
        }
    }
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
    /// Enable bundle.merged -> implementer rebase propagation (Phase 4).
    #[serde(default = "default_rebase_on_merge")]
    pub rebase_on_merge: bool,
    /// Abandon worktree after N consecutive rebase failures (Phase 4).
    #[serde(default = "default_max_rebase_lag")]
    pub max_rebase_lag: u32,
}

fn default_rebase_on_merge() -> bool {
    true
}

fn default_max_rebase_lag() -> u32 {
    5
}

/// Worker pool size: a fixed count, or "auto"/"nproc" to use available parallelism.
///
/// "auto" and "nproc" both resolve to `std::thread::available_parallelism()` at
/// daemon startup. Falls back to 4 if the OS cannot determine the value.
///
/// YAML examples:
///   worker-pool-size: auto   # use all available cores
///   worker-pool-size: nproc  # alias for auto
///   worker-pool-size: 4      # fixed count
#[derive(Debug, Clone, Serialize)]
#[serde(untagged)]
pub enum WorkerPoolSize {
    Named(String),
    Fixed(u32),
}

impl WorkerPoolSize {
    pub fn resolve(&self) -> u32 {
        match self {
            WorkerPoolSize::Fixed(n) => *n,
            WorkerPoolSize::Named(_) => std::thread::available_parallelism()
                .map(|n| n.get() as u32)
                .unwrap_or(4),
        }
    }
}

impl Default for WorkerPoolSize {
    fn default() -> Self {
        WorkerPoolSize::Named("auto".to_string())
    }
}

impl<'de> serde::Deserialize<'de> for WorkerPoolSize {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        struct Visitor;
        impl<'de> serde::de::Visitor<'de> for Visitor {
            type Value = WorkerPoolSize;
            fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
                write!(f, "a positive integer or \"auto\"/\"nproc\"")
            }
            fn visit_u64<E: serde::de::Error>(self, v: u64) -> Result<WorkerPoolSize, E> {
                Ok(WorkerPoolSize::Fixed(v as u32))
            }
            fn visit_str<E: serde::de::Error>(self, v: &str) -> Result<WorkerPoolSize, E> {
                Ok(WorkerPoolSize::Named(v.to_string()))
            }
        }
        d.deserialize_any(Visitor)
    }
}

/// Agent system configuration - LLM agents running as Tokio tasks.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct AgentConfig {
    pub enabled: bool,
    pub auto_start_implementer: bool,
    pub auto_start_reviewer: bool,
    /// When true, persistent worker pool pulls Ready Works instead of
    /// engine-driven spawn-implementer-for-ready-work strategy. Default false (feature flag).
    pub pull_based_workers: bool,
    /// Number of persistent worker tasks in the pull-based pool.
    /// Accepts a fixed count or "auto"/"nproc" to use available parallelism.
    pub worker_pool_size: WorkerPoolSize,
    pub implementer: AgentRoleConfig,
    pub reviewer: AgentRoleConfig,
    pub researcher: AgentRoleConfig,
    pub tools: Vec<ToolEntry>,
    /// How the Director handles the interview phase.
    #[serde(default)]
    pub interview_mode: InterviewMode,
    /// Maximum fraction of abandoned works (across all phases) before the GoalComplete
    /// quality gate fires need_help instead of done. Default: 0.4 (40%).
    /// A value of 1.0 effectively disables the gate.
    #[serde(default = "default_max_abandon_ratio")]
    pub max_abandon_ratio: f64,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            auto_start_implementer: false,
            auto_start_reviewer: false,
            pull_based_workers: false,
            worker_pool_size: WorkerPoolSize::default(),
            implementer: AgentRoleConfig::default_implementer(),
            reviewer: AgentRoleConfig::default_reviewer(),
            researcher: AgentRoleConfig::default_researcher(),
            tools: Vec::new(),
            interview_mode: InterviewMode::default(),
            max_abandon_ratio: default_max_abandon_ratio(),
        }
    }
}

/// Sentinel value for `max_pool` meaning "no hard cap - bounded by worker_pool_size".
/// When `max_pool` equals this value, the effective cap is resolved from
/// `AgentsConfig::worker_pool_size` at runtime. Explicit values in config still work.
pub const MAX_POOL_UNLIMITED: u32 = u32::MAX;

/// Prompt file paths for the Decomposer system call.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct DecomposerPrompts {
    pub spec: String,
    pub phase: String,
    pub work: String,
    pub validate: String,
    pub ratify: String,
    /// Generation-work prompt (from agents/generation.rs, part of decomposition flow).
    pub generation_work: String,
}

impl Default for DecomposerPrompts {
    fn default() -> Self {
        Self {
            spec: "decompose/spec/prompt".to_string(),
            phase: "decompose/phase/prompt".to_string(),
            work: "decompose/work/prompt".to_string(),
            validate: "decompose/validate".to_string(),
            ratify: "decompose/ratify".to_string(),
            generation_work: "decompose/work/generation".to_string(),
        }
    }
}

/// Prompt file paths for the Doc Validator.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct ValidatorPrompts {
    pub schema: String,
    pub plan: String,
    pub spec: String,
    pub phase: String,
}

impl Default for ValidatorPrompts {
    fn default() -> Self {
        Self {
            schema: "decompose/schema".to_string(),
            plan: "decompose/plan/validator".to_string(),
            spec: "decompose/spec/validator".to_string(),
            phase: "decompose/phase/validator".to_string(),
        }
    }
}

/// Prompt file paths for the Coverage Evaluator.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct EvaluatorPrompts {
    pub schema: String,
    pub plan_specs: String,
    pub spec_phases: String,
    pub phase_works: String,
}

impl Default for EvaluatorPrompts {
    fn default() -> Self {
        Self {
            schema: "decompose/coverage-schema".to_string(),
            plan_specs: "decompose/spec/coverage".to_string(),
            spec_phases: "decompose/phase/coverage".to_string(),
            phase_works: "decompose/work/coverage".to_string(),
        }
    }
}

/// Prompt file paths for the Chat system.
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
#[serde(default, rename_all = "kebab-case")]
pub struct ChatPrompts {
    pub default: String,
    pub interview: String,
    pub draft: String,
    pub refine: String,
    pub executing: String,
}

impl Default for ChatPrompts {
    fn default() -> Self {
        Self {
            default: "chat/default".to_string(),
            interview: "chat/interview".to_string(),
            draft: "chat/draft".to_string(),
            refine: "chat/refine".to_string(),
            executing: "chat/executing".to_string(),
        }
    }
}

/// Per-role agent configuration (Implementer, Reviewer, Researcher, Coordinator).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct AgentRoleConfig {
    #[serde(flatten)]
    pub llm: LlmConfig,
    pub max_iterations: u32,
    pub max_pool: u32,
    pub session_timeout_secs: Option<u64>,
    /// Max re-prompts per iteration for self-correction (parse/tool errors). 0 = disabled.
    pub max_requeries: u32,
    /// Path to the system prompt file for this role (relative to prompts dir, or absolute).
    pub prompt: String,
}

impl Default for AgentRoleConfig {
    fn default() -> Self {
        Self::default_implementer()
    }
}

impl AgentRoleConfig {
    pub fn default_implementer() -> Self {
        Self {
            llm: LlmConfig {
                model: "claude-sonnet-4-6".to_string(),
                api_key_env: "ANTHROPIC_API_KEY".to_string(),
                max_tokens: 8192,
                temperature: 0.3,
            },
            max_iterations: 20,
            max_pool: MAX_POOL_UNLIMITED,
            session_timeout_secs: Some(1800), // 30 min
            max_requeries: 3,
            prompt: "agents/implementer".to_string(),
        }
    }

    pub fn default_reviewer() -> Self {
        Self {
            llm: LlmConfig {
                model: "claude-sonnet-4-6".to_string(),
                api_key_env: "ANTHROPIC_API_KEY".to_string(),
                max_tokens: 4096,
                temperature: 0.1,
            },
            max_iterations: 5,
            max_pool: MAX_POOL_UNLIMITED,
            session_timeout_secs: Some(600), // 10 min
            max_requeries: 3,
            prompt: "agents/reviewer".to_string(),
        }
    }

    pub fn default_researcher() -> Self {
        Self {
            llm: LlmConfig {
                model: "claude-sonnet-4-6".to_string(),
                api_key_env: "ANTHROPIC_API_KEY".to_string(),
                max_tokens: 4096,
                temperature: 0.1,
            },
            max_iterations: 10,
            max_pool: 4,
            session_timeout_secs: Some(600), // 10 min
            max_requeries: 3,
            prompt: "agents/researcher".to_string(),
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
    /// Prompt file paths for chat sessions.
    pub prompts: ChatPrompts,
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
            prompts: ChatPrompts::default(),
        }
    }
}

impl ChatConfig {
    /// Build an `AgentRoleConfig` for the parent chat LLM.
    pub fn to_role_config(&self) -> AgentRoleConfig {
        AgentRoleConfig {
            llm: LlmConfig {
                model: self.model.clone(),
                api_key_env: self.api_key_env.clone(),
                max_tokens: self.max_tokens,
                temperature: self.temperature,
            },
            max_iterations: self.max_iterations,
            ..AgentRoleConfig::default_implementer()
        }
    }

    /// Build an `AgentRoleConfig` for delegate subagents.
    pub fn to_delegate_role_config(&self) -> AgentRoleConfig {
        AgentRoleConfig {
            llm: LlmConfig {
                model: self.delegate_model.clone(),
                api_key_env: self.api_key_env.clone(),
                max_tokens: self.max_tokens,
                temperature: self.temperature,
            },
            max_iterations: 20,
            ..AgentRoleConfig::default_implementer()
        }
    }
}

/// Doc Validator configuration — LLM-powered document validation for quality gates.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct ValidatorConfig {
    #[serde(flatten)]
    pub llm: LlmConfig,
    pub enabled: bool,
    pub provider: String,
    /// Prompt file paths for document validation.
    pub prompts: ValidatorPrompts,
}

impl Default for ValidatorConfig {
    fn default() -> Self {
        Self {
            llm: LlmConfig {
                model: "claude-sonnet-4-6".to_string(),
                api_key_env: "ANTHROPIC_API_KEY".to_string(),
                max_tokens: 4096,
                temperature: 0.0,
            },
            enabled: false,
            provider: "anthropic".to_string(),
            prompts: ValidatorPrompts::default(),
        }
    }
}

/// Tier gate configuration - LLM-powered classification of Plan as Full or Brief.
///
/// Uses a lightweight model (Haiku by default) to read the Plan and determine
/// whether it introduces contracts (Full) or is contract-neutral (Brief).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct TierGateConfig {
    #[serde(flatten)]
    pub llm: LlmConfig,
    pub enabled: bool,
    pub provider: String,
    /// Path to the tier-gate prompt file (relative to prompts dir, or absolute).
    pub prompt: String,
}

impl Default for TierGateConfig {
    fn default() -> Self {
        Self {
            llm: LlmConfig {
                model: "claude-haiku-4-5-20251001".to_string(),
                api_key_env: "ANTHROPIC_API_KEY".to_string(),
                max_tokens: 16,
                temperature: 0.0,
            },
            enabled: true,
            provider: "anthropic".to_string(),
            prompt: "agents/tier-gate".to_string(),
        }
    }
}

/// Coverage Evaluator configuration - LLM-powered coverage evaluation at decomposition boundaries.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct EvaluatorConfig {
    #[serde(flatten)]
    pub llm: LlmConfig,
    pub enabled: bool,
    pub provider: String,
    /// Prompt file paths for coverage evaluation.
    pub prompts: EvaluatorPrompts,
}

impl Default for EvaluatorConfig {
    fn default() -> Self {
        Self {
            llm: LlmConfig {
                model: "claude-sonnet-4-6".to_string(),
                api_key_env: "ANTHROPIC_API_KEY".to_string(),
                max_tokens: 4096,
                temperature: 0.0,
            },
            enabled: false,
            provider: "anthropic".to_string(),
            prompts: EvaluatorPrompts::default(),
        }
    }
}

/// Decomposer configuration - LLM-powered plan decomposition.
///
/// The Decomposer is a system call (not an agent) that takes a document at
/// any hierarchy level and produces child documents. It uses one model for
/// generation and a lighter model for validation (template adherence checks).
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(default)]
pub struct DecomposerConfig {
    #[serde(flatten)]
    pub llm: LlmConfig,
    pub provider: String,
    /// Lightweight model for template validation checks.
    pub validation_model: String,
    /// Prompt file paths for decomposition.
    pub prompts: DecomposerPrompts,
}

impl Default for DecomposerConfig {
    fn default() -> Self {
        Self {
            llm: LlmConfig {
                model: "claude-sonnet-4-6".to_string(),
                api_key_env: "ANTHROPIC_API_KEY".to_string(),
                max_tokens: 4096,
                temperature: 0.3,
            },
            provider: "anthropic".to_string(),
            validation_model: "claude-haiku-4-5-20251001".to_string(),
            prompts: DecomposerPrompts::default(),
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
            rebase_on_merge: true,
            max_rebase_lag: 5,
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
    pub log_level: Option<String>,
    pub daemon: DaemonConfig,
    pub project: ProjectConfig,
    pub chat: ChatConfig,
    pub integrator: IntegratorConfig,
    pub validator: ValidatorConfig,
    pub tier_gate: TierGateConfig,
    pub evaluator: EvaluatorConfig,
    pub agents: AgentConfig,
    pub strategy: StrategyConfig,
    pub reconciler: ReconcilerConfig,
    pub decomposer: DecomposerConfig,
    /// Goal clarity gate - promoted from strategy.clarity-gate to top-level.
    pub clarity_gate: ClarityGateConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            name: "loopr".to_string(),
            log_level: None,
            daemon: DaemonConfig::default(),
            project: ProjectConfig::default(),
            chat: ChatConfig::default(),
            integrator: IntegratorConfig::default(),
            validator: ValidatorConfig::default(),
            tier_gate: TierGateConfig::default(),
            evaluator: EvaluatorConfig::default(),
            agents: AgentConfig::default(),
            strategy: StrategyConfig::default(),
            reconciler: ReconcilerConfig::default(),
            decomposer: DecomposerConfig::default(),
            clarity_gate: ClarityGateConfig::default(),
        }
    }
}

impl Config {
    /// Load configuration with fallback chain
    pub fn load(config_path: Option<&PathBuf>) -> Result<Self> {
        tracing::debug!("Config::load(config_path={:?})", config_path);
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
                        tracing::warn!("Failed to load config from {}: {}", primary_config.display(), e);
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
                    tracing::warn!("Failed to load config from {}: {}", fallback_config.display(), e);
                }
            }
        }

        // No config file found, use defaults
        tracing::info!("No config file found, using defaults");
        Ok(Self::default())
    }

    fn load_from_file<P: AsRef<Path>>(path: P) -> Result<Self> {
        tracing::debug!("Config::load_from_file(path={})", path.as_ref().display());
        let content = fs::read_to_string(&path).context("Failed to read config file")?;

        let mut config: Self = serde_yaml::from_str(&content).context("Failed to parse config file")?;

        // Backward compat: if strategy.clarity-gate (legacy location) exists in YAML,
        // promote it to the top-level clarity_gate field. New configs use top-level only.
        let raw: serde_yaml::Value =
            serde_yaml::from_str(&content).context("Failed to parse config file (raw pass)")?;
        if let Some(legacy) = raw
            .get("strategy")
            .and_then(|s| s.get("clarity-gate").or_else(|| s.get("clarity_gate")))
            .cloned()
            && let Ok(legacy_config) = serde_yaml::from_value::<ClarityGateConfig>(legacy)
        {
            config.clarity_gate = legacy_config;
            tracing::warn!("Config uses legacy strategy.clarity-gate; move it to top-level clarity-gate");
        }

        tracing::info!("Loaded config from: {}", path.as_ref().display());
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
    }

    #[test]
    fn test_agent_config_default_tools_empty() {
        let ac = AgentConfig::default();
        assert!(
            ac.tools.is_empty(),
            "Default tools should be empty - configure in loopr.yml or use detection"
        );
    }

    #[test]
    fn test_agent_role_config_implementer_defaults() {
        let rc = AgentRoleConfig::default_implementer();
        assert_eq!(rc.max_iterations, 20);
        assert_eq!(rc.max_pool, MAX_POOL_UNLIMITED);
        assert_eq!(rc.llm.max_tokens, 8192);
        assert!((rc.llm.temperature - 0.3).abs() < f32::EPSILON);
        assert_eq!(rc.max_requeries, 3);
    }

    #[test]
    fn test_agent_role_config_reviewer_defaults() {
        let rc = AgentRoleConfig::default_reviewer();
        assert_eq!(rc.max_iterations, 5);
        assert_eq!(rc.max_pool, MAX_POOL_UNLIMITED);
        assert_eq!(rc.llm.max_tokens, 4096);
        assert!((rc.llm.temperature - 0.1).abs() < f32::EPSILON);
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
        assert!(config.agents.tools.is_empty(), "Default tools should be empty");
    }

    #[test]
    fn test_agent_config_deserialize_from_yaml() {
        let yaml = r#"
enabled: true
auto_start_implementer: true
auto_start_reviewer: false
implementer:
  model: "claude-sonnet-4-6"
  api-key-env: "MY_KEY"
  max-tokens: 4096
  max_iterations: 10
  min_pool: 2
  max_pool: 3
  temperature: 0.5
reviewer:
  model: "claude-sonnet-4-6"
  api-key-env: "MY_KEY"
  max-tokens: 2048
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
        assert_eq!(ac.implementer.llm.max_tokens, 4096);
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

    #[test]
    fn test_agent_role_config_researcher_defaults() {
        let rc = AgentRoleConfig::default_researcher();
        assert_eq!(rc.max_iterations, 10);
        assert_eq!(rc.max_pool, 4);
        assert_eq!(rc.llm.max_tokens, 4096);
        assert!((rc.llm.temperature - 0.1).abs() < f32::EPSILON);
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

    // --- Coverage config tests ---

    #[test]
    fn test_strategy_config_coverage_defaults() {
        let sc = StrategyConfig::default();
        assert!(sc.coverage_enabled);
        assert_eq!(sc.max_decomposition_attempts, 3);
        assert_eq!(sc.max_bubble_up_depth, 2);
    }

    #[test]
    fn test_strategy_config_coverage_yaml() {
        let yaml = r#"
coverage_enabled: false
max_decomposition_attempts: 5
max_bubble_up_depth: 3
"#;
        let sc: StrategyConfig = serde_yaml::from_str(yaml).expect("should parse coverage config");
        assert!(!sc.coverage_enabled);
        assert_eq!(sc.max_decomposition_attempts, 5);
        assert_eq!(sc.max_bubble_up_depth, 3);
    }

    #[test]
    fn test_strategy_config_ignores_removed_fields() {
        // Serde should silently ignore fields that no longer exist on the struct
        let yaml = r#"
coverage_strictness: allow_minor_gaps
plan_interview_enabled: false
plan_approval_required: false
"#;
        let sc: StrategyConfig = serde_yaml::from_str(yaml).expect("removed fields should be ignored");
        // Should get defaults for everything
        assert!(sc.coverage_enabled);
    }

    #[test]
    fn test_evaluator_config_default() {
        let ec = EvaluatorConfig::default();
        assert!(!ec.enabled);
        assert_eq!(ec.provider, "anthropic");
        assert_eq!(ec.llm.model, "claude-sonnet-4-6");
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
    fn test_agent_config_interview_mode_default() {
        let ac = AgentConfig::default();
        assert_eq!(ac.interview_mode, InterviewMode::Interactive);
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
        assert_eq!(role.llm.model, "claude-sonnet-4-6");
        assert_eq!(role.llm.max_tokens, 8192);
        assert_eq!(role.max_iterations, 3);
    }

    #[test]
    fn test_chat_config_to_delegate_role_config() {
        let cc = ChatConfig::default();
        let role = cc.to_delegate_role_config();
        assert_eq!(role.llm.model, "claude-haiku-4-5-20251001");
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
    fn test_reconciler_config_default() {
        let rc = ReconcilerConfig::default();
        assert_eq!(rc.interval_secs, 60);
        assert!(rc.enabled);
    }

    #[test]
    fn test_reconciler_config_serde() {
        let yaml = "interval-secs: 120\nenabled: false\n";
        let rc: ReconcilerConfig = serde_yaml::from_str(yaml).expect("should parse");
        assert_eq!(rc.interval_secs, 120);
        assert!(!rc.enabled);
    }

    #[test]
    fn test_config_default_has_reconciler() {
        let config = Config::default();
        assert_eq!(config.reconciler.interval_secs, 60);
        assert!(config.reconciler.enabled);
    }

    #[test]
    fn test_worker_pool_size_fixed_resolve() {
        let size = WorkerPoolSize::Fixed(4);
        assert_eq!(size.resolve(), 4);
    }

    #[test]
    fn test_worker_pool_size_auto_resolve() {
        let size = WorkerPoolSize::Named("auto".to_string());
        // Should resolve to at least 1
        assert!(size.resolve() >= 1);
    }

    #[test]
    fn test_worker_pool_size_nproc_resolve() {
        let size = WorkerPoolSize::Named("nproc".to_string());
        assert!(size.resolve() >= 1);
    }

    #[test]
    fn test_worker_pool_size_default_is_auto() {
        let size = WorkerPoolSize::default();
        match size {
            WorkerPoolSize::Named(ref s) => assert_eq!(s, "auto"),
            WorkerPoolSize::Fixed(_) => panic!("expected Named variant"),
        }
    }

    #[test]
    fn test_worker_pool_size_deserialize_fixed() {
        let yaml = "4";
        let size: WorkerPoolSize = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(size.resolve(), 4);
    }

    #[test]
    fn test_worker_pool_size_deserialize_auto() {
        let yaml = "\"auto\"";
        let size: WorkerPoolSize = serde_yaml::from_str(yaml).unwrap();
        assert!(size.resolve() >= 1);
    }

    #[test]
    fn test_worker_pool_size_deserialize_nproc() {
        let yaml = "\"nproc\"";
        let size: WorkerPoolSize = serde_yaml::from_str(yaml).unwrap();
        assert!(size.resolve() >= 1);
    }

    #[test]
    fn test_agent_config_default_worker_pool_size() {
        let config = AgentConfig::default();
        // Default is auto, resolves to >= 1
        assert!(config.worker_pool_size.resolve() >= 1);
    }

    // --- LlmConfig tests (Phase 1) ---

    #[test]
    fn test_llm_config_default() {
        let llm = LlmConfig::default();
        assert_eq!(llm.model, "claude-sonnet-4-6");
        assert_eq!(llm.api_key_env, "ANTHROPIC_API_KEY");
        assert_eq!(llm.max_tokens, 4096);
        assert!((llm.temperature - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn test_llm_config_serde_roundtrip() {
        let llm = LlmConfig {
            model: "claude-opus-4-6".to_string(),
            api_key_env: "MY_API_KEY".to_string(),
            max_tokens: 8192,
            temperature: 0.3,
        };
        let yaml = serde_yaml::to_string(&llm).unwrap();
        let deserialized: LlmConfig = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(deserialized.model, llm.model);
        assert_eq!(deserialized.api_key_env, llm.api_key_env);
        assert_eq!(deserialized.max_tokens, llm.max_tokens);
        assert!((deserialized.temperature - llm.temperature).abs() < f32::EPSILON);
    }

    #[test]
    fn test_llm_config_kebab_case_yaml() {
        let yaml = r#"
model: claude-opus-4-6
api-key-env: MY_KEY
max-tokens: 8192
temperature: 0.5
"#;
        let llm: LlmConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(llm.model, "claude-opus-4-6");
        assert_eq!(llm.api_key_env, "MY_KEY");
        assert_eq!(llm.max_tokens, 8192);
        assert!((llm.temperature - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn test_agent_role_config_flatten_preserves_llm_fields() {
        let yaml = r#"
model: claude-haiku-4-5-20251001
api-key-env: CUSTOM_KEY
max-tokens: 512
temperature: 0.0
max_iterations: 5
max_pool: 2
"#;
        let role: AgentRoleConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(role.llm.model, "claude-haiku-4-5-20251001");
        assert_eq!(role.llm.api_key_env, "CUSTOM_KEY");
        assert_eq!(role.llm.max_tokens, 512);
        assert_eq!(role.max_iterations, 5);
        assert_eq!(role.max_pool, 2);
    }

    #[test]
    fn test_validator_config_flatten_roundtrip() {
        let vc = ValidatorConfig::default();
        let yaml = serde_yaml::to_string(&vc).unwrap();
        let deserialized: ValidatorConfig = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(deserialized.llm.model, vc.llm.model);
        assert_eq!(deserialized.llm.max_tokens, vc.llm.max_tokens);
        assert_eq!(deserialized.enabled, vc.enabled);
        assert_eq!(deserialized.provider, vc.provider);
    }

    #[test]
    fn test_tier_gate_config_flatten_roundtrip() {
        let tg = TierGateConfig::default();
        let yaml = serde_yaml::to_string(&tg).unwrap();
        let deserialized: TierGateConfig = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(deserialized.llm.model, "claude-haiku-4-5-20251001");
        assert_eq!(deserialized.llm.max_tokens, 16);
        assert!(deserialized.enabled);
    }

    #[test]
    fn test_evaluator_config_flatten_roundtrip() {
        let ec = EvaluatorConfig::default();
        let yaml = serde_yaml::to_string(&ec).unwrap();
        let deserialized: EvaluatorConfig = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(deserialized.llm.model, ec.llm.model);
        assert!(!deserialized.enabled);
    }

    #[test]
    fn test_decomposer_config_flatten_roundtrip() {
        let dc = DecomposerConfig::default();
        let yaml = serde_yaml::to_string(&dc).unwrap();
        let deserialized: DecomposerConfig = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(deserialized.llm.model, "claude-sonnet-4-6");
        assert_eq!(deserialized.llm.max_tokens, 4096);
        assert!((deserialized.llm.temperature - 0.3).abs() < f32::EPSILON);
        assert_eq!(deserialized.validation_model, "claude-haiku-4-5-20251001");
    }

    #[test]
    fn test_clarity_gate_config_has_max_tokens_and_temperature() {
        let cgc = ClarityGateConfig::default();
        assert_eq!(cgc.max_tokens, 1024);
        assert!((cgc.temperature - 0.0).abs() < f32::EPSILON);
        assert_eq!(cgc.min_score, 3);
        assert!(cgc.enabled);
    }

    #[test]
    fn test_config_clarity_gate_at_top_level() {
        let config = Config::default();
        assert!(config.clarity_gate.enabled);
        assert_eq!(config.clarity_gate.model, "claude-sonnet-4-6");
        assert_eq!(config.clarity_gate.max_tokens, 1024);
    }

    #[test]
    fn test_config_strategy_no_longer_has_clarity_gate() {
        // StrategyConfig serialization must not include clarity-gate
        let sc = StrategyConfig::default();
        let yaml = serde_yaml::to_string(&sc).unwrap();
        assert!(
            !yaml.contains("clarity-gate") && !yaml.contains("clarity_gate"),
            "StrategyConfig must not serialize clarity_gate: {}",
            yaml
        );
    }

    #[test]
    fn test_backward_compat_strategy_clarity_gate_yaml() {
        // Old-style config with strategy.clarity-gate must load into config.clarity_gate
        let yaml = r#"
name: test
strategy:
  clarity-gate:
    enabled: true
    model: claude-haiku-4-5-20251001
    api-key-env: CUSTOM_KEY
    max-tokens: 512
    temperature: 0.1
    min-score: 4
"#;
        // Write to temp file and load with Config::load_from_file
        let tmp = std::env::temp_dir().join("loopr_compat_test.yml");
        std::fs::write(&tmp, yaml).unwrap();
        let config = Config::load(Some(&tmp)).unwrap();
        std::fs::remove_file(&tmp).unwrap();
        assert_eq!(config.clarity_gate.model, "claude-haiku-4-5-20251001");
        assert_eq!(config.clarity_gate.max_tokens, 512);
        assert_eq!(config.clarity_gate.min_score, 4);
    }

    // --- Prompt config field tests (Phase 2) ---

    #[test]
    fn test_agent_role_config_has_prompt_field() {
        assert_eq!(AgentRoleConfig::default_implementer().prompt, "agents/implementer");
        assert_eq!(AgentRoleConfig::default_reviewer().prompt, "agents/reviewer");
        assert_eq!(AgentRoleConfig::default_researcher().prompt, "agents/researcher");
    }

    #[test]
    fn test_tier_gate_config_has_prompt_field() {
        let tg = TierGateConfig::default();
        assert_eq!(tg.prompt, "agents/tier-gate");
    }

    #[test]
    fn test_decomposer_prompts_defaults() {
        let dp = DecomposerPrompts::default();
        assert_eq!(dp.spec, "decompose/spec/prompt");
        assert_eq!(dp.phase, "decompose/phase/prompt");
        assert_eq!(dp.work, "decompose/work/prompt");
        assert_eq!(dp.validate, "decompose/validate");
        assert_eq!(dp.ratify, "decompose/ratify");
        assert_eq!(dp.generation_work, "decompose/work/generation");
    }

    #[test]
    fn test_validator_prompts_defaults() {
        let vp = ValidatorPrompts::default();
        assert_eq!(vp.schema, "decompose/schema");
        assert_eq!(vp.plan, "decompose/plan/validator");
        assert_eq!(vp.spec, "decompose/spec/validator");
        assert_eq!(vp.phase, "decompose/phase/validator");
    }

    #[test]
    fn test_evaluator_prompts_defaults() {
        let ep = EvaluatorPrompts::default();
        assert_eq!(ep.schema, "decompose/coverage-schema");
        assert_eq!(ep.plan_specs, "decompose/spec/coverage");
        assert_eq!(ep.spec_phases, "decompose/phase/coverage");
        assert_eq!(ep.phase_works, "decompose/work/coverage");
    }

    #[test]
    fn test_chat_prompts_defaults() {
        let cp = ChatPrompts::default();
        assert_eq!(cp.default, "chat/default");
        assert_eq!(cp.interview, "chat/interview");
        assert_eq!(cp.draft, "chat/draft");
        assert_eq!(cp.refine, "chat/refine");
        assert_eq!(cp.executing, "chat/executing");
    }

    #[test]
    fn test_decomposer_config_has_prompts() {
        let dc = DecomposerConfig::default();
        assert_eq!(dc.prompts.work, "decompose/work/prompt");
        assert_eq!(dc.prompts.generation_work, "decompose/work/generation");
    }

    #[test]
    fn test_validator_config_has_prompts() {
        let vc = ValidatorConfig::default();
        assert_eq!(vc.prompts.schema, "decompose/schema");
    }

    #[test]
    fn test_evaluator_config_has_prompts() {
        let ec = EvaluatorConfig::default();
        assert_eq!(ec.prompts.schema, "decompose/coverage-schema");
    }

    #[test]
    fn test_chat_config_has_prompts() {
        let cc = ChatConfig::default();
        assert_eq!(cc.prompts.default, "chat/default");
        assert_eq!(cc.prompts.interview, "chat/interview");
    }

    #[test]
    fn test_prompt_field_yaml_roundtrip() {
        let yaml = r#"
prompt: custom-implementer
model: claude-sonnet-4-6
api-key-env: ANTHROPIC_API_KEY
max-tokens: 8192
temperature: 0.3
max_iterations: 20
max_pool: 10
"#;
        let role: AgentRoleConfig = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(role.prompt, "custom-implementer");
        assert_eq!(role.llm.model, "claude-sonnet-4-6");
        assert_eq!(role.max_iterations, 20);
    }
}
