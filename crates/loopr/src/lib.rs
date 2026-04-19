pub mod cli;
pub mod error;

pub use cli::{Cli, Command, DaemonCmd, LogsCmd};
pub use error::LooprError;

pub fn run(cli: Cli) -> Result<(), LooprError> {
    match cli.command {
        Command::Init => Err(LooprError::StageUnimplemented {
            stage: 5,
            subcommand: "init",
        }),
        Command::Plan { .. } => Err(LooprError::StageUnimplemented {
            stage: 5,
            subcommand: "plan",
        }),
        Command::Decompose { .. } => Err(LooprError::StageUnimplemented {
            stage: 6,
            subcommand: "decompose",
        }),
        Command::Execute { .. } => Err(LooprError::StageUnimplemented {
            stage: 7,
            subcommand: "execute",
        }),
        Command::Integrate => Err(LooprError::StageUnimplemented {
            stage: 8,
            subcommand: "integrate",
        }),
        Command::Daemon { cmd } => match cmd {
            DaemonCmd::Start { .. } => Err(LooprError::StageUnimplemented {
                stage: 4,
                subcommand: "daemon-start",
            }),
            DaemonCmd::Stop => Err(LooprError::StageUnimplemented {
                stage: 4,
                subcommand: "daemon-stop",
            }),
            DaemonCmd::Status => Err(LooprError::StageUnimplemented {
                stage: 4,
                subcommand: "daemon-status",
            }),
        },
        Command::Score { .. } => Err(LooprError::StageUnimplemented {
            stage: 9,
            subcommand: "score",
        }),
        Command::Logs { cmd } => match cmd {
            LogsCmd::Tail { .. } => Err(LooprError::StageUnimplemented {
                stage: 2,
                subcommand: "logs-tail",
            }),
            LogsCmd::Runs => Err(LooprError::StageUnimplemented {
                stage: 2,
                subcommand: "logs-runs",
            }),
        },
        Command::List { .. } => Err(LooprError::StageUnimplemented {
            stage: 5,
            subcommand: "list",
        }),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use clap::Parser;

    fn parse(args: &[&str]) -> Cli {
        Cli::parse_from(args)
    }

    #[test]
    fn run_init_returns_stage_5() {
        let err = run(parse(&["loopr", "init"])).unwrap_err();
        match err {
            LooprError::StageUnimplemented { stage, subcommand } => {
                assert_eq!(stage, 5);
                assert_eq!(subcommand, "init");
            }
            other => panic!("expected StageUnimplemented, got {other:?}"),
        }
    }

    #[test]
    fn run_plan_returns_stage_5() {
        let err = run(parse(&["loopr", "plan", "goal"])).unwrap_err();
        assert!(matches!(
            err,
            LooprError::StageUnimplemented {
                stage: 5,
                subcommand: "plan"
            }
        ));
    }

    #[test]
    fn run_decompose_returns_stage_6() {
        let err = run(parse(&["loopr", "decompose", "plan-1"])).unwrap_err();
        assert!(matches!(
            err,
            LooprError::StageUnimplemented {
                stage: 6,
                subcommand: "decompose"
            }
        ));
    }

    #[test]
    fn run_execute_returns_stage_7() {
        let err = run(parse(&["loopr", "execute"])).unwrap_err();
        assert!(matches!(
            err,
            LooprError::StageUnimplemented {
                stage: 7,
                subcommand: "execute"
            }
        ));
    }

    #[test]
    fn run_integrate_returns_stage_8() {
        let err = run(parse(&["loopr", "integrate"])).unwrap_err();
        assert!(matches!(
            err,
            LooprError::StageUnimplemented {
                stage: 8,
                subcommand: "integrate"
            }
        ));
    }

    #[test]
    fn run_daemon_start_returns_stage_4() {
        let err = run(parse(&["loopr", "daemon", "start"])).unwrap_err();
        assert!(matches!(
            err,
            LooprError::StageUnimplemented {
                stage: 4,
                subcommand: "daemon-start"
            }
        ));
    }

    #[test]
    fn run_daemon_stop_returns_stage_4() {
        let err = run(parse(&["loopr", "daemon", "stop"])).unwrap_err();
        assert!(matches!(
            err,
            LooprError::StageUnimplemented {
                stage: 4,
                subcommand: "daemon-stop"
            }
        ));
    }

    #[test]
    fn run_daemon_status_returns_stage_4() {
        let err = run(parse(&["loopr", "daemon", "status"])).unwrap_err();
        assert!(matches!(
            err,
            LooprError::StageUnimplemented {
                stage: 4,
                subcommand: "daemon-status"
            }
        ));
    }

    #[test]
    fn run_score_returns_stage_9() {
        let err = run(parse(&["loopr", "score", "--dir", "/tmp/run"])).unwrap_err();
        assert!(matches!(
            err,
            LooprError::StageUnimplemented {
                stage: 9,
                subcommand: "score"
            }
        ));
    }

    #[test]
    fn run_logs_tail_returns_stage_2() {
        let err = run(parse(&["loopr", "logs", "tail"])).unwrap_err();
        assert!(matches!(
            err,
            LooprError::StageUnimplemented {
                stage: 2,
                subcommand: "logs-tail"
            }
        ));
    }

    #[test]
    fn run_logs_runs_returns_stage_2() {
        let err = run(parse(&["loopr", "logs", "runs"])).unwrap_err();
        assert!(matches!(
            err,
            LooprError::StageUnimplemented {
                stage: 2,
                subcommand: "logs-runs"
            }
        ));
    }

    #[test]
    fn run_list_returns_stage_5() {
        let err = run(parse(&["loopr", "list", "plans"])).unwrap_err();
        assert!(matches!(
            err,
            LooprError::StageUnimplemented {
                stage: 5,
                subcommand: "list"
            }
        ));
    }
}
