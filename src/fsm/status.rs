use super::runtime::FsmInterpreter;

/// Trait mapping Rust status enums to their YAML FSM definitions.
///
/// Each status enum (WorkStatus, BundleStatus, etc.) implements this trait
/// to provide the PascalCase <-> kebab-case mapping needed by FsmInterpreter.
/// The YAML definitions in `resources/engine/fsm/` are the single source of truth;
/// this trait is the bridge between Rust types and the runtime interpreter.
pub trait FsmStatus: Sized + Copy {
    /// The FSM definition name (matches the YAML filename without extension).
    fn fsm_name() -> &'static str;

    /// Convert this variant to its kebab-case YAML name.
    fn to_yaml_name(&self) -> &'static str;

    /// Parse a kebab-case YAML name into this enum.
    fn from_yaml_name(name: &str) -> eyre::Result<Self>;

    /// All variants of this enum, in declaration order.
    fn all_variants() -> &'static [Self];

    /// Check if this state is terminal. Delegates to the runtime interpreter
    /// so the YAML `terminal:` list is the single source of truth.
    fn is_terminal(&self, fsm: &FsmInterpreter) -> bool {
        fsm.is_terminal(Self::fsm_name(), self.to_yaml_name()).unwrap_or(false)
    }
}

// --- WorkStatus ---

use crate::domain::work::WorkStatus;

impl FsmStatus for WorkStatus {
    fn fsm_name() -> &'static str {
        "work"
    }

    fn to_yaml_name(&self) -> &'static str {
        match self {
            WorkStatus::Draft => "draft",
            WorkStatus::Pending => "pending",
            WorkStatus::Ready => "ready",
            WorkStatus::InProgress => "in-progress",
            WorkStatus::Blocked => "blocked",
            WorkStatus::InReview => "in-review",
            WorkStatus::Integrated => "integrated",
            WorkStatus::Done => "done",
            WorkStatus::Superseded => "superseded",
            WorkStatus::Abandoned => "abandoned",
        }
    }

    fn from_yaml_name(name: &str) -> eyre::Result<Self> {
        match name {
            "draft" => Ok(WorkStatus::Draft),
            "pending" => Ok(WorkStatus::Pending),
            "ready" => Ok(WorkStatus::Ready),
            "in-progress" => Ok(WorkStatus::InProgress),
            "blocked" => Ok(WorkStatus::Blocked),
            "in-review" => Ok(WorkStatus::InReview),
            "integrated" => Ok(WorkStatus::Integrated),
            "done" => Ok(WorkStatus::Done),
            "superseded" => Ok(WorkStatus::Superseded),
            "abandoned" => Ok(WorkStatus::Abandoned),
            _ => eyre::bail!("unknown work status: '{}'", name),
        }
    }

    fn all_variants() -> &'static [Self] {
        &[
            WorkStatus::Draft,
            WorkStatus::Pending,
            WorkStatus::Ready,
            WorkStatus::InProgress,
            WorkStatus::Blocked,
            WorkStatus::InReview,
            WorkStatus::Integrated,
            WorkStatus::Done,
            WorkStatus::Superseded,
            WorkStatus::Abandoned,
        ]
    }
}

// --- BundleStatus ---

use crate::domain::bundle::BundleStatus;

impl FsmStatus for BundleStatus {
    fn fsm_name() -> &'static str {
        "bundle"
    }

    fn to_yaml_name(&self) -> &'static str {
        match self {
            BundleStatus::Proposed => "proposed",
            BundleStatus::Triaged => "triaged",
            BundleStatus::Reviewed => "reviewed",
            BundleStatus::Accepted => "accepted",
            BundleStatus::Integrating => "integrating",
            BundleStatus::Merged => "merged",
            BundleStatus::Rejected => "rejected",
            BundleStatus::Superseded => "superseded",
        }
    }

    fn from_yaml_name(name: &str) -> eyre::Result<Self> {
        match name {
            "proposed" => Ok(BundleStatus::Proposed),
            "triaged" => Ok(BundleStatus::Triaged),
            "reviewed" => Ok(BundleStatus::Reviewed),
            "accepted" => Ok(BundleStatus::Accepted),
            "integrating" => Ok(BundleStatus::Integrating),
            "merged" => Ok(BundleStatus::Merged),
            "rejected" => Ok(BundleStatus::Rejected),
            "superseded" => Ok(BundleStatus::Superseded),
            _ => eyre::bail!("unknown bundle status: '{}'", name),
        }
    }

    fn all_variants() -> &'static [Self] {
        &[
            BundleStatus::Proposed,
            BundleStatus::Triaged,
            BundleStatus::Reviewed,
            BundleStatus::Accepted,
            BundleStatus::Integrating,
            BundleStatus::Merged,
            BundleStatus::Rejected,
            BundleStatus::Superseded,
        ]
    }
}

// --- HierarchyStatus ---

use crate::domain::plan::HierarchyStatus;

impl FsmStatus for HierarchyStatus {
    fn fsm_name() -> &'static str {
        "hierarchy"
    }

    fn to_yaml_name(&self) -> &'static str {
        match self {
            HierarchyStatus::Draft => "draft",
            HierarchyStatus::Pending => "pending",
            HierarchyStatus::Active => "active",
            HierarchyStatus::Complete => "complete",
            HierarchyStatus::Superseded => "superseded",
            HierarchyStatus::Abandoned => "abandoned",
        }
    }

    fn from_yaml_name(name: &str) -> eyre::Result<Self> {
        match name {
            "draft" => Ok(HierarchyStatus::Draft),
            "pending" => Ok(HierarchyStatus::Pending),
            "active" => Ok(HierarchyStatus::Active),
            "complete" => Ok(HierarchyStatus::Complete),
            "superseded" => Ok(HierarchyStatus::Superseded),
            "abandoned" => Ok(HierarchyStatus::Abandoned),
            _ => eyre::bail!("unknown hierarchy status: '{}'", name),
        }
    }

    fn all_variants() -> &'static [Self] {
        &[
            HierarchyStatus::Draft,
            HierarchyStatus::Pending,
            HierarchyStatus::Active,
            HierarchyStatus::Complete,
            HierarchyStatus::Superseded,
            HierarchyStatus::Abandoned,
        ]
    }
}

// --- TickStatus ---

use crate::domain::tick::TickStatus;

impl FsmStatus for TickStatus {
    fn fsm_name() -> &'static str {
        "tick"
    }

    fn to_yaml_name(&self) -> &'static str {
        match self {
            TickStatus::Open => "open",
            TickStatus::Sealing => "sealing",
            TickStatus::Validating => "validating",
            TickStatus::Published => "published",
            TickStatus::Failed => "failed",
        }
    }

    fn from_yaml_name(name: &str) -> eyre::Result<Self> {
        match name {
            "open" => Ok(TickStatus::Open),
            "sealing" => Ok(TickStatus::Sealing),
            "validating" => Ok(TickStatus::Validating),
            "published" => Ok(TickStatus::Published),
            "failed" => Ok(TickStatus::Failed),
            _ => eyre::bail!("unknown tick status: '{}'", name),
        }
    }

    fn all_variants() -> &'static [Self] {
        &[
            TickStatus::Open,
            TickStatus::Sealing,
            TickStatus::Validating,
            TickStatus::Published,
            TickStatus::Failed,
        ]
    }
}

// --- AgentStatus ---

use crate::agents::status::AgentStatus;

impl FsmStatus for AgentStatus {
    fn fsm_name() -> &'static str {
        "agent"
    }

    fn to_yaml_name(&self) -> &'static str {
        match self {
            AgentStatus::Starting => "starting",
            AgentStatus::Running => "running",
            AgentStatus::WaitingForLlm => "waiting-for-llm",
            AgentStatus::Paused => "paused",
            AgentStatus::Idle => "idle",
            AgentStatus::Completed => "completed",
            AgentStatus::Failed => "failed",
            AgentStatus::Cancelled => "cancelled",
        }
    }

    fn from_yaml_name(name: &str) -> eyre::Result<Self> {
        match name {
            "starting" => Ok(AgentStatus::Starting),
            "running" => Ok(AgentStatus::Running),
            "waiting-for-llm" => Ok(AgentStatus::WaitingForLlm),
            "paused" => Ok(AgentStatus::Paused),
            "idle" => Ok(AgentStatus::Idle),
            "completed" => Ok(AgentStatus::Completed),
            "failed" => Ok(AgentStatus::Failed),
            "cancelled" => Ok(AgentStatus::Cancelled),
            _ => eyre::bail!("unknown agent status: '{}'", name),
        }
    }

    fn all_variants() -> &'static [Self] {
        &[
            AgentStatus::Starting,
            AgentStatus::Running,
            AgentStatus::WaitingForLlm,
            AgentStatus::Paused,
            AgentStatus::Idle,
            AgentStatus::Completed,
            AgentStatus::Failed,
            AgentStatus::Cancelled,
        ]
    }
}
