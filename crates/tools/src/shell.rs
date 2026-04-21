use std::path::Path;

/// Build a `sh -c <cmd_str>` command in `cwd`, with stdout/stderr piped.
///
/// Used by the Bash built-in. Grep, Glob, and other tools build their
/// `Command` directly (via `Command::new("grep").arg(pattern)...`) so that
/// the arg vector bypasses the shell and closes v4's string-concatenation
/// shell-injection vector.
pub fn sh_command(cmd_str: &str, cwd: &Path) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg("-c").arg(cmd_str);
    cmd.current_dir(cwd);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    cmd
}

#[cfg(test)]
mod tests;
