pub mod dispatch;

use clap::Parser;
use std::path::PathBuf;

use crate::domain::role::Role;

/// CRUD subcommands for Plan, Spec, Phase, Work.
#[derive(Debug, Clone, clap::Subcommand)]
pub enum CrudCmd {
    /// Create a new record
    Create {
        /// Title
        title: String,
        /// Description
        #[arg(short, long, default_value = "")]
        description: String,
        /// Parent ID (required for spec, phase, work)
        #[arg(short, long)]
        parent: Option<String>,
        /// Order (for Phase)
        #[arg(long)]
        order: Option<u32>,
        /// Resource tags (for Work, repeatable)
        #[arg(long = "resource-tag")]
        resource_tags: Vec<String>,
        /// Acceptance criteria (for Work, repeatable)
        #[arg(long = "acceptance-criteria")]
        acceptance_criteria: Vec<String>,
        /// Dependencies (for Work, repeatable)
        #[arg(long = "dependency")]
        dependencies: Vec<String>,
    },
    /// Get a record by ID
    Get {
        /// Record ID
        id: String,
    },
    /// List records (optional parent filter)
    List {
        /// Filter by parent ID
        #[arg(short, long)]
        parent: Option<String>,
    },
    /// Transition a record's status
    Transition {
        /// Record ID
        id: String,
        /// Target status
        status: String,
        /// Skip validation gate (Coordinator only)
        #[arg(long)]
        skip_validation: bool,
    },
}

/// Bundle-specific subcommands (CRUD + propose).
#[derive(Debug, Clone, clap::Subcommand)]
pub enum BundleCmd {
    /// Create a new bundle
    Create {
        /// Work ID
        work_id: String,
        /// Branch name
        #[arg(short, long)]
        branch: String,
        /// Description
        #[arg(short, long, default_value = "")]
        description: String,
        /// Base tick ID (for staleness guard)
        #[arg(long)]
        base_tick_id: Option<String>,
        /// Claims (repeatable)
        #[arg(long = "claim")]
        claims: Vec<String>,
        /// Touched paths (repeatable)
        #[arg(long = "touched-path")]
        touched_paths: Vec<String>,
    },
    /// Get a bundle by ID
    Get {
        /// Bundle ID
        id: String,
    },
    /// List bundles (optional work_id filter)
    List {
        /// Filter by work ID
        #[arg(short, long)]
        work_id: Option<String>,
    },
    /// Transition a bundle's status
    Transition {
        /// Bundle ID
        id: String,
        /// Target status
        status: String,
    },
}

/// Tick-specific subcommands.
#[derive(Debug, Clone, clap::Subcommand)]
pub enum TickCmd {
    /// Create a new tick
    Create {
        /// Tick number
        number: u64,
    },
    /// Get a tick by ID
    Get {
        /// Tick ID
        id: String,
    },
    /// List ticks (optional status filter)
    List {
        /// Filter by status
        #[arg(short, long)]
        status: Option<String>,
    },
    /// Transition a tick's status
    Transition {
        /// Tick ID
        id: String,
        /// Target status
        status: String,
    },
    /// Validate a tick (run validation commands)
    Validate {
        /// Tick ID
        id: String,
    },
    /// Publish a tick (convenience: seal → validate → publish)
    Publish {
        /// Tick ID
        id: String,
    },
}

/// Worktree subcommands.
#[derive(Debug, Clone, clap::Subcommand)]
pub enum WorktreeCmd {
    /// Create a worktree for a work
    Create {
        /// Work ID
        work_id: String,
        /// Git ref to base the worktree on
        #[arg(short, long, default_value = "HEAD")]
        git_ref: String,
    },
    /// List all worktrees
    List,
    /// Clean up a worktree
    Cleanup {
        /// Work ID
        work_id: String,
    },
    /// Refresh a worktree to a new ref
    Refresh {
        /// Work ID
        work_id: String,
        /// Git ref to reset to
        #[arg(short, long, default_value = "HEAD")]
        git_ref: String,
    },
}

/// Learning subcommands.
#[derive(Debug, Clone, clap::Subcommand)]
pub enum LearningCmd {
    /// Create a new learning
    Create {
        /// Source record ID
        source_id: String,
        /// Scope (Work, Phase, Spec, Plan, Global)
        scope: String,
        /// Content
        content: String,
    },
    /// Get a learning by ID
    Get {
        /// Learning ID
        id: String,
    },
    /// List learnings
    List,
    /// Reinforce a learning
    Reinforce {
        /// Learning ID
        id: String,
    },
    /// Contradict a learning
    Contradict {
        /// Learning ID
        id: String,
    },
    /// Promote a learning
    Promote {
        /// Learning ID
        id: String,
    },
    /// Demote a learning
    Demote {
        /// Learning ID
        id: String,
    },
}

/// Agent subcommands.
#[derive(Debug, Clone, clap::Subcommand)]
pub enum AgentCmd {
    /// Start an implementer agent for a work
    #[command(name = "start-implementer")]
    StartImplementer {
        /// Work ID
        work_id: String,
    },
    /// Start a reviewer agent for a bundle
    #[command(name = "start-reviewer")]
    StartReviewer {
        /// Bundle ID
        bundle_id: String,
    },
    /// Start the coordinator agent
    #[command(name = "start-coordinator")]
    StartCoordinator,
    /// Start the integrator agent
    #[command(name = "start-integrator")]
    StartIntegrator,
    /// Start a researcher agent for a query
    #[command(name = "start-researcher")]
    StartResearcher {
        /// Research query
        query: String,
        /// Target scope ID (plan/spec/phase/work ID)
        #[arg(short, long)]
        target_id: Option<String>,
    },
    /// Stop a running agent
    Stop {
        /// Agent session ID
        session_id: String,
    },
    /// Pause a running agent
    Pause {
        /// Agent session ID
        session_id: String,
    },
    /// Resume a paused agent
    Resume {
        /// Agent session ID
        session_id: String,
    },
    /// Get status of an agent session
    Status {
        /// Agent session ID
        session_id: String,
    },
    /// List agent sessions
    List {
        /// Filter by status (starting, running, paused, completed, failed, cancelled)
        #[arg(short, long)]
        status: Option<String>,
        /// Filter by agent type (implementer, reviewer, coordinator, researcher)
        #[arg(short = 't', long)]
        agent_type: Option<String>,
    },
    /// Show iteration-level output for an agent session
    Output {
        /// Agent session ID
        session_id: String,
        /// Only show events after this index
        #[arg(short, long, default_value = "0")]
        since: u64,
    },
}

/// Coordinator subcommands.
#[derive(Debug, Clone, clap::Subcommand)]
pub enum CoordinatorCmd {
    /// Set the coordinator goal
    #[command(name = "set-goal")]
    Set {
        /// Goal text
        goal: String,
    },
    /// Clear the coordinator goal
    #[command(name = "clear-goal")]
    Clear,
    /// Get the current coordinator goal
    #[command(name = "goal")]
    Status,
}

/// Lock subcommands.
#[derive(Debug, Clone, clap::Subcommand)]
pub enum LockCmd {
    /// Create a new lock
    Create {
        /// Resource path
        resource: String,
        /// Holder ID (e.g. work ID)
        holder_id: String,
        /// Granted by
        granted_by: String,
    },
    /// Get a lock by ID
    Get {
        /// Lock ID
        id: String,
    },
    /// List locks
    List {
        /// Filter by resource
        #[arg(short, long)]
        resource: Option<String>,
        /// Filter by holder
        #[arg(long)]
        holder_id: Option<String>,
        /// Only show active locks
        #[arg(long)]
        active_only: bool,
    },
    /// Release a lock
    Release {
        /// Lock ID
        id: String,
    },
    /// Expire a lock
    Expire {
        /// Lock ID
        id: String,
    },
}

#[derive(Parser)]
#[command(
    name = "loopr",
    about = "Dev team in a box — TUI-based orchestrator",
    version = env!("GIT_DESCRIBE"),
)]
pub struct Cli {
    /// Path to config file
    #[arg(short, long, global = true)]
    pub config: Option<PathBuf>,

    /// Role to use for this command
    #[arg(long, global = true)]
    pub r#as: Option<Role>,

    /// Log level (trace, debug, info, warn, error)
    #[arg(long, global = true)]
    pub log_level: Option<String>,

    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Clone, clap::Subcommand)]
pub enum Command {
    /// Start the TUI (default when no subcommand)
    Tui,
    /// Run as daemon
    Daemon,
    /// Show daemon status
    Status,
    /// Plan operations
    Plan {
        #[command(subcommand)]
        cmd: CrudCmd,
    },
    /// Spec operations
    Spec {
        #[command(subcommand)]
        cmd: CrudCmd,
    },
    /// Phase operations
    Phase {
        #[command(subcommand)]
        cmd: CrudCmd,
    },
    /// Work operations
    #[command(name = "work")]
    Work {
        #[command(subcommand)]
        cmd: CrudCmd,
    },
    /// Bundle operations
    Bundle {
        #[command(subcommand)]
        cmd: BundleCmd,
    },
    /// Tick operations
    Tick {
        #[command(subcommand)]
        cmd: TickCmd,
    },
    /// Worktree operations
    Worktree {
        #[command(subcommand)]
        cmd: WorktreeCmd,
    },
    /// Learning operations
    Learning {
        #[command(subcommand)]
        cmd: LearningCmd,
    },
    /// Lock operations
    Lock {
        #[command(subcommand)]
        cmd: LockCmd,
    },
    /// Agent operations (start, stop, pause, resume, list, status)
    Agent {
        #[command(subcommand)]
        cmd: AgentCmd,
    },
    /// Coordinator operations (set-goal, clear-goal, goal)
    Coordinator {
        #[command(subcommand)]
        cmd: CoordinatorCmd,
    },
    /// Initialize TaskStore (create collections, install git hooks)
    Init,
    /// Validate a document (plan/spec/phase) via the Doc Validator LLM
    Validate {
        /// Collection type (plan, spec, phase)
        collection: String,
        /// Record ID
        id: String,
    },
    /// Get a validation report by ID
    Report {
        /// Report ID
        id: String,
    },
    /// List validation reports for a collection/record
    Reports {
        /// Collection type (plans, specs, phases)
        collection: String,
        /// Record ID
        target_id: String,
    },
    /// Set the active role (persisted to config)
    #[command(name = "role")]
    SetRole {
        /// The role to set (coordinator, integrator, implementer, reviewer, researcher)
        role: String,
    },
    /// Graceful daemon shutdown
    Shutdown,
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::CommandFactory;

    #[test]
    fn test_cli_parses_no_args() {
        let cli = Cli::parse_from(["loopr"]);
        assert!(cli.command.is_none());
    }

    #[test]
    fn test_cli_parses_tui() {
        let cli = Cli::parse_from(["loopr", "tui"]);
        assert!(matches!(cli.command, Some(Command::Tui)));
    }

    #[test]
    fn test_cli_parses_daemon() {
        let cli = Cli::parse_from(["loopr", "daemon"]);
        assert!(matches!(cli.command, Some(Command::Daemon)));
    }

    #[test]
    fn test_cli_parses_status() {
        let cli = Cli::parse_from(["loopr", "status"]);
        assert!(matches!(cli.command, Some(Command::Status)));
    }

    #[test]
    fn test_cli_parses_shutdown() {
        let cli = Cli::parse_from(["loopr", "shutdown"]);
        assert!(matches!(cli.command, Some(Command::Shutdown)));
    }

    #[test]
    fn test_cli_parses_plan_create() {
        let cli = Cli::parse_from(["loopr", "plan", "create", "My Plan", "-d", "A description"]);
        match cli.command {
            Some(Command::Plan {
                cmd: CrudCmd::Create { title, description, .. },
            }) => {
                assert_eq!(title, "My Plan");
                assert_eq!(description, "A description");
            }
            _ => panic!("expected Plan Create"),
        }
    }

    #[test]
    fn test_cli_parses_plan_get() {
        let cli = Cli::parse_from(["loopr", "plan", "get", "plan-123"]);
        match cli.command {
            Some(Command::Plan {
                cmd: CrudCmd::Get { id },
            }) => {
                assert_eq!(id, "plan-123");
            }
            _ => panic!("expected Plan Get"),
        }
    }

    #[test]
    fn test_cli_parses_plan_list() {
        let cli = Cli::parse_from(["loopr", "plan", "list"]);
        assert!(matches!(
            cli.command,
            Some(Command::Plan {
                cmd: CrudCmd::List { .. }
            })
        ));
    }

    #[test]
    fn test_cli_parses_plan_transition() {
        let cli = Cli::parse_from(["loopr", "plan", "transition", "p1", "Active"]);
        match cli.command {
            Some(Command::Plan {
                cmd: CrudCmd::Transition { id, status, .. },
            }) => {
                assert_eq!(id, "p1");
                assert_eq!(status, "Active");
            }
            _ => panic!("expected Plan Transition"),
        }
    }

    #[test]
    fn test_cli_parses_work_create() {
        let cli = Cli::parse_from(["loopr", "work", "create", "Task 1", "-p", "phase-1"]);
        match cli.command {
            Some(Command::Work {
                cmd: CrudCmd::Create { title, parent, .. },
            }) => {
                assert_eq!(title, "Task 1");
                assert_eq!(parent, Some("phase-1".to_string()));
            }
            _ => panic!("expected Work Create"),
        }
    }

    #[test]
    fn test_cli_parses_bundle_create() {
        let cli = Cli::parse_from(["loopr", "bundle", "create", "wi-1", "-b", "feature/foo"]);
        match cli.command {
            Some(Command::Bundle {
                cmd: BundleCmd::Create { work_id, branch, .. },
            }) => {
                assert_eq!(work_id, "wi-1");
                assert_eq!(branch, "feature/foo");
            }
            _ => panic!("expected Bundle Create"),
        }
    }

    #[test]
    fn test_cli_parses_work_create_with_resource_tags() {
        let cli = Cli::parse_from([
            "loopr",
            "work",
            "create",
            "Task 1",
            "-p",
            "phase-1",
            "--resource-tag",
            "src/auth/",
            "--resource-tag",
            "src/lib.rs",
            "--acceptance-criteria",
            "tests pass",
            "--dependency",
            "wi-0",
        ]);
        match cli.command {
            Some(Command::Work {
                cmd:
                    CrudCmd::Create {
                        title,
                        parent,
                        resource_tags,
                        acceptance_criteria,
                        dependencies,
                        ..
                    },
            }) => {
                assert_eq!(title, "Task 1");
                assert_eq!(parent, Some("phase-1".to_string()));
                assert_eq!(resource_tags, vec!["src/auth/", "src/lib.rs"]);
                assert_eq!(acceptance_criteria, vec!["tests pass"]);
                assert_eq!(dependencies, vec!["wi-0"]);
            }
            _ => panic!("expected Work Create with resource tags"),
        }
    }

    #[test]
    fn test_cli_parses_bundle_create_with_claims() {
        let cli = Cli::parse_from([
            "loopr",
            "bundle",
            "create",
            "wi-1",
            "-b",
            "feature/foo",
            "--claim",
            "Add JWT signing",
            "--touched-path",
            "src/auth.rs",
            "--touched-path",
            "src/lib.rs",
        ]);
        match cli.command {
            Some(Command::Bundle {
                cmd:
                    BundleCmd::Create {
                        work_id,
                        branch,
                        claims,
                        touched_paths,
                        ..
                    },
            }) => {
                assert_eq!(work_id, "wi-1");
                assert_eq!(branch, "feature/foo");
                assert_eq!(claims, vec!["Add JWT signing"]);
                assert_eq!(touched_paths, vec!["src/auth.rs", "src/lib.rs"]);
            }
            _ => panic!("expected Bundle Create with claims"),
        }
    }

    #[test]
    fn test_cli_parses_tick_publish() {
        let cli = Cli::parse_from(["loopr", "tick", "publish", "t-1"]);
        match cli.command {
            Some(Command::Tick {
                cmd: TickCmd::Publish { id },
            }) => {
                assert_eq!(id, "t-1");
            }
            _ => panic!("expected Tick Publish"),
        }
    }

    #[test]
    fn test_cli_parses_worktree_create() {
        let cli = Cli::parse_from(["loopr", "worktree", "create", "wi-1"]);
        match cli.command {
            Some(Command::Worktree {
                cmd: WorktreeCmd::Create { work_id, git_ref },
            }) => {
                assert_eq!(work_id, "wi-1");
                assert_eq!(git_ref, "HEAD");
            }
            _ => panic!("expected Worktree Create"),
        }
    }

    #[test]
    fn test_cli_parses_as_role() {
        let cli = Cli::parse_from(["loopr", "--as", "Integrator", "status"]);
        assert_eq!(cli.r#as, Some(Role::Integrator));
    }

    #[test]
    fn test_cli_parses_config_flag() {
        let cli = Cli::parse_from(["loopr", "--config", "/tmp/test.yml", "status"]);
        assert_eq!(cli.config, Some(PathBuf::from("/tmp/test.yml")));
    }

    #[test]
    fn test_cli_verify() {
        // Clap's built-in verification that the command tree is well-formed
        Cli::command().debug_assert();
    }

    #[test]
    fn test_cli_parses_learning_create() {
        let cli = Cli::parse_from(["loopr", "learning", "create", "wi-1", "Work", "learned something"]);
        match cli.command {
            Some(Command::Learning {
                cmd:
                    LearningCmd::Create {
                        source_id,
                        scope,
                        content,
                    },
            }) => {
                assert_eq!(source_id, "wi-1");
                assert_eq!(scope, "Work");
                assert_eq!(content, "learned something");
            }
            _ => panic!("expected Learning Create"),
        }
    }

    #[test]
    fn test_cli_parses_lock_create() {
        let cli = Cli::parse_from(["loopr", "lock", "create", "src/main.rs", "wi-1", "coordinator"]);
        match cli.command {
            Some(Command::Lock {
                cmd:
                    LockCmd::Create {
                        resource,
                        holder_id,
                        granted_by,
                    },
            }) => {
                assert_eq!(resource, "src/main.rs");
                assert_eq!(holder_id, "wi-1");
                assert_eq!(granted_by, "coordinator");
            }
            _ => panic!("expected Lock Create"),
        }
    }

    #[test]
    fn test_cli_parses_init() {
        let cli = Cli::parse_from(["loopr", "init"]);
        assert!(matches!(cli.command, Some(Command::Init)));
    }

    #[test]
    fn test_cli_parses_validate() {
        let cli = Cli::parse_from(["loopr", "validate", "plan", "plan-123"]);
        match cli.command {
            Some(Command::Validate { collection, id }) => {
                assert_eq!(collection, "plan");
                assert_eq!(id, "plan-123");
            }
            _ => panic!("expected Validate"),
        }
    }

    #[test]
    fn test_cli_parses_report() {
        let cli = Cli::parse_from(["loopr", "report", "vr-123"]);
        match cli.command {
            Some(Command::Report { id }) => {
                assert_eq!(id, "vr-123");
            }
            _ => panic!("expected Report"),
        }
    }

    #[test]
    fn test_cli_parses_reports() {
        let cli = Cli::parse_from(["loopr", "reports", "plans", "plan-1"]);
        match cli.command {
            Some(Command::Reports { collection, target_id }) => {
                assert_eq!(collection, "plans");
                assert_eq!(target_id, "plan-1");
            }
            _ => panic!("expected Reports"),
        }
    }

    #[test]
    fn test_cli_parses_agent_start_implementer() {
        let cli = Cli::parse_from(["loopr", "agent", "start-implementer", "wi-1"]);
        match cli.command {
            Some(Command::Agent {
                cmd: AgentCmd::StartImplementer { work_id },
            }) => {
                assert_eq!(work_id, "wi-1");
            }
            _ => panic!("expected Agent StartImplementer"),
        }
    }

    #[test]
    fn test_cli_parses_agent_start_reviewer() {
        let cli = Cli::parse_from(["loopr", "agent", "start-reviewer", "b-1"]);
        match cli.command {
            Some(Command::Agent {
                cmd: AgentCmd::StartReviewer { bundle_id },
            }) => {
                assert_eq!(bundle_id, "b-1");
            }
            _ => panic!("expected Agent StartReviewer"),
        }
    }

    #[test]
    fn test_cli_parses_agent_stop() {
        let cli = Cli::parse_from(["loopr", "agent", "stop", "sess-1"]);
        match cli.command {
            Some(Command::Agent {
                cmd: AgentCmd::Stop { session_id },
            }) => {
                assert_eq!(session_id, "sess-1");
            }
            _ => panic!("expected Agent Stop"),
        }
    }

    #[test]
    fn test_cli_parses_agent_pause() {
        let cli = Cli::parse_from(["loopr", "agent", "pause", "sess-1"]);
        match cli.command {
            Some(Command::Agent {
                cmd: AgentCmd::Pause { session_id },
            }) => {
                assert_eq!(session_id, "sess-1");
            }
            _ => panic!("expected Agent Pause"),
        }
    }

    #[test]
    fn test_cli_parses_agent_resume() {
        let cli = Cli::parse_from(["loopr", "agent", "resume", "sess-1"]);
        match cli.command {
            Some(Command::Agent {
                cmd: AgentCmd::Resume { session_id },
            }) => {
                assert_eq!(session_id, "sess-1");
            }
            _ => panic!("expected Agent Resume"),
        }
    }

    #[test]
    fn test_cli_parses_agent_status() {
        let cli = Cli::parse_from(["loopr", "agent", "status", "sess-1"]);
        match cli.command {
            Some(Command::Agent {
                cmd: AgentCmd::Status { session_id },
            }) => {
                assert_eq!(session_id, "sess-1");
            }
            _ => panic!("expected Agent Status"),
        }
    }

    #[test]
    fn test_cli_parses_agent_list() {
        let cli = Cli::parse_from(["loopr", "agent", "list"]);
        match cli.command {
            Some(Command::Agent {
                cmd: AgentCmd::List { status, agent_type },
            }) => {
                assert!(status.is_none());
                assert!(agent_type.is_none());
            }
            _ => panic!("expected Agent List"),
        }
    }

    #[test]
    fn test_cli_parses_agent_list_with_filters() {
        let cli = Cli::parse_from(["loopr", "agent", "list", "-s", "running", "-t", "implementer"]);
        match cli.command {
            Some(Command::Agent {
                cmd: AgentCmd::List { status, agent_type },
            }) => {
                assert_eq!(status, Some("running".to_string()));
                assert_eq!(agent_type, Some("implementer".to_string()));
            }
            _ => panic!("expected Agent List with filters"),
        }
    }

    #[test]
    fn test_cli_parses_agent_output() {
        let cli = Cli::parse_from(["loopr", "agent", "output", "sess-1"]);
        match cli.command {
            Some(Command::Agent {
                cmd: AgentCmd::Output { session_id, since },
            }) => {
                assert_eq!(session_id, "sess-1");
                assert_eq!(since, 0);
            }
            _ => panic!("expected Agent Output"),
        }
    }

    #[test]
    fn test_cli_parses_agent_output_with_since() {
        let cli = Cli::parse_from(["loopr", "agent", "output", "sess-1", "--since", "5"]);
        match cli.command {
            Some(Command::Agent {
                cmd: AgentCmd::Output { session_id, since },
            }) => {
                assert_eq!(session_id, "sess-1");
                assert_eq!(since, 5);
            }
            _ => panic!("expected Agent Output with since"),
        }
    }

    #[test]
    fn test_cli_parses_agent_start_coordinator() {
        let cli = Cli::parse_from(["loopr", "agent", "start-coordinator"]);
        assert!(matches!(
            cli.command,
            Some(Command::Agent {
                cmd: AgentCmd::StartCoordinator
            })
        ));
    }

    #[test]
    fn test_cli_parses_agent_start_researcher() {
        let cli = Cli::parse_from(["loopr", "agent", "start-researcher", "How does auth work?"]);
        match cli.command {
            Some(Command::Agent {
                cmd: AgentCmd::StartResearcher { query, target_id },
            }) => {
                assert_eq!(query, "How does auth work?");
                assert!(target_id.is_none());
            }
            _ => panic!("expected Agent StartResearcher"),
        }
    }

    #[test]
    fn test_cli_parses_agent_start_researcher_with_target() {
        let cli = Cli::parse_from(["loopr", "agent", "start-researcher", "Investigate module", "-t", "wi-1"]);
        match cli.command {
            Some(Command::Agent {
                cmd: AgentCmd::StartResearcher { query, target_id },
            }) => {
                assert_eq!(query, "Investigate module");
                assert_eq!(target_id, Some("wi-1".to_string()));
            }
            _ => panic!("expected Agent StartResearcher with target"),
        }
    }

    #[test]
    fn test_cli_parses_coordinator_set_goal() {
        let cli = Cli::parse_from(["loopr", "coordinator", "set-goal", "Build auth module"]);
        match cli.command {
            Some(Command::Coordinator {
                cmd: CoordinatorCmd::Set { goal },
            }) => {
                assert_eq!(goal, "Build auth module");
            }
            _ => panic!("expected Coordinator SetGoal"),
        }
    }

    #[test]
    fn test_cli_parses_coordinator_clear_goal() {
        let cli = Cli::parse_from(["loopr", "coordinator", "clear-goal"]);
        assert!(matches!(
            cli.command,
            Some(Command::Coordinator {
                cmd: CoordinatorCmd::Clear
            })
        ));
    }

    #[test]
    fn test_cli_parses_coordinator_goal() {
        let cli = Cli::parse_from(["loopr", "coordinator", "goal"]);
        assert!(matches!(
            cli.command,
            Some(Command::Coordinator {
                cmd: CoordinatorCmd::Status
            })
        ));
    }

    #[test]
    fn test_cli_parses_log_level() {
        let cli = Cli::parse_from(["loopr", "--log-level", "debug", "status"]);
        assert_eq!(cli.log_level.as_deref(), Some("debug"));
    }

    #[test]
    fn test_cli_log_level_default_none() {
        let cli = Cli::parse_from(["loopr", "status"]);
        assert!(cli.log_level.is_none());
    }
}
