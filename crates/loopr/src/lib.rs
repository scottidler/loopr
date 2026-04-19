pub mod cli;
pub mod error;
pub mod guard;
pub mod target;

pub use cli::{Cli, Command, DaemonCmd, LogsCmd};
pub use error::LooprError;

pub fn run(cli: Cli) -> Result<(), LooprError> {
    let cwd = std::env::current_dir().map_err(|_| LooprError::TargetInvalid {
        path: std::path::PathBuf::from("."),
    })?;
    let env_target = std::env::var("LOOPR_TARGET").ok();
    let effective = target::resolve(cli.chdir.as_deref(), env_target.as_deref(), &cwd)?;
    guard::check(&effective)?;
    dispatch(cli.command)
}

fn dispatch(command: Command) -> Result<(), LooprError> {
    match command {
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
mod tests;
