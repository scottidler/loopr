use crate::tools::ToolResult;

/// Result of executing a single agent action.
#[derive(Debug)]
pub enum ActionResult {
    ToolRun(ToolResult),
    FileWritten(String),
    FileEdited(String),
    FileRead(String),
    Committed(String),
    BundleProposed(String),
    Transitioned(String),
    LearningCreated(String),
    /// Tool registered via tools.register IPC - contains tool name.
    ToolRegistered(String),
    /// Lock acquired - contains lock_id.
    LockAcquired(String),
    /// Lock released - contains lock_id.
    LockReleased(String),
    Done(String),
    NeedHelp(String),
    /// Non-fatal error - fed back to the LLM so it can self-correct.
    ActionError(String),
    /// Record created via bridge - contains (collection, id).
    RecordCreated {
        collection: String,
        id: String,
    },
    /// Agent session spawned - contains (session_id, agent_type).
    AgentSpawned {
        session_id: String,
        agent_type: String,
    },
    // AssignAgent, AcceptBundle, ValidateDocument, EvaluateCoverage removed -
    // replaced by engine strategies. DependencyNotMet, DocumentValidated,
    // CoverageEvaluated result variants removed with them.
}
