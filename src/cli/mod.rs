pub mod dispatch;

use clap::Parser;
use std::path::PathBuf;

use crate::domain::role::Role;

/// CRUD subcommands for Plan, Spec, Phase, WorkItem.
#[derive(Debug, Clone, clap::Subcommand)]
pub enum CrudCmd {
    /// Create a new record
    Create {
        /// Title
        title: String,
        /// Description
        #[arg(short, long, default_value = "")]
        description: String,
        /// Parent ID (required for spec, phase, work_item)
        #[arg(short, long)]
        parent: Option<String>,
        /// Order (for Phase)
        #[arg(long)]
        order: Option<u32>,
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
    },
}

/// Bundle-specific subcommands (CRUD + propose).
#[derive(Debug, Clone, clap::Subcommand)]
pub enum BundleCmd {
    /// Create a new bundle
    Create {
        /// Work item ID
        work_item_id: String,
        /// Branch name
        #[arg(short, long)]
        branch: String,
        /// Description
        #[arg(short, long, default_value = "")]
        description: String,
        /// Base tick ID (for staleness guard)
        #[arg(long)]
        base_tick_id: Option<String>,
    },
    /// Get a bundle by ID
    Get {
        /// Bundle ID
        id: String,
    },
    /// List bundles (optional work_item_id filter)
    List {
        /// Filter by work item ID
        #[arg(short, long)]
        work_item_id: Option<String>,
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
    /// Create a worktree for a work item
    Create {
        /// Work item ID
        work_item_id: String,
        /// Git ref to base the worktree on
        #[arg(short, long, default_value = "HEAD")]
        git_ref: String,
    },
    /// List all worktrees
    List,
    /// Clean up a worktree
    Cleanup {
        /// Work item ID
        work_item_id: String,
    },
    /// Refresh a worktree to a new ref
    Refresh {
        /// Work item ID
        work_item_id: String,
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
        /// Scope (WorkItem, Phase, Spec, Plan, Global)
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

/// Lock subcommands.
#[derive(Debug, Clone, clap::Subcommand)]
pub enum LockCmd {
    /// Create a new lock
    Create {
        /// Resource path
        resource: String,
        /// Holder ID (e.g. work item ID)
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
    /// Work item operations
    #[command(name = "work-item")]
    WorkItem {
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
                cmd: CrudCmd::Transition { id, status },
            }) => {
                assert_eq!(id, "p1");
                assert_eq!(status, "Active");
            }
            _ => panic!("expected Plan Transition"),
        }
    }

    #[test]
    fn test_cli_parses_work_item_create() {
        let cli = Cli::parse_from(["loopr", "work-item", "create", "Task 1", "-p", "phase-1"]);
        match cli.command {
            Some(Command::WorkItem {
                cmd: CrudCmd::Create { title, parent, .. },
            }) => {
                assert_eq!(title, "Task 1");
                assert_eq!(parent, Some("phase-1".to_string()));
            }
            _ => panic!("expected WorkItem Create"),
        }
    }

    #[test]
    fn test_cli_parses_bundle_create() {
        let cli = Cli::parse_from(["loopr", "bundle", "create", "wi-1", "-b", "feature/foo"]);
        match cli.command {
            Some(Command::Bundle {
                cmd: BundleCmd::Create {
                    work_item_id, branch, ..
                },
            }) => {
                assert_eq!(work_item_id, "wi-1");
                assert_eq!(branch, "feature/foo");
            }
            _ => panic!("expected Bundle Create"),
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
                cmd: WorktreeCmd::Create { work_item_id, git_ref },
            }) => {
                assert_eq!(work_item_id, "wi-1");
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
        let cli = Cli::parse_from(["loopr", "learning", "create", "wi-1", "WorkItem", "learned something"]);
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
                assert_eq!(scope, "WorkItem");
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
}
