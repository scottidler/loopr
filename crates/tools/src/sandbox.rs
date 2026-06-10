use std::ffi::OsString;
use std::path::Path;

use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Serialize, Deserialize, Copy, Clone, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum SandboxMode {
    #[default]
    Required,
    Preferred,
    Off,
}

/// Functional bwrap detection (D6): not just `bwrap --version` (which only
/// proves the binary exists). This actually invokes bwrap with the full flag
/// set we depend on against `/bin/true`, so a machine that has the binary but
/// whose kernel has `user.max_user_namespaces=0` or other failure modes
/// surfaces at daemon startup, not on first tool call.
///
/// Phase-5 finding 4: the probe now mirrors the actual `bwrap_command` mount
/// flags (`--dev`/`--proc`/`--bind`/`--chdir`), not just `--unshare-net` +
/// `--ro-bind`. The mount flags can fail independently of the net-unshare
/// (e.g. `--proc` under a restrictive kernel), and the old narrow probe would
/// have reported "functional" while the real wrap failed on first tool call.
/// `--unshare-net` is the strictest (Local-lane) shape; the Net lane uses a
/// strict subset, so a probe pass guarantees both lanes wrap successfully.
pub fn detect_bwrap_functional() -> bool {
    std::process::Command::new("bwrap")
        .args([
            "--unshare-net",
            "--die-with-parent",
            "--ro-bind",
            "/",
            "/",
            "--dev",
            "/dev",
            "--proc",
            "/proc",
            "--bind",
            "/tmp",
            "/tmp",
            "--chdir",
            "/tmp",
            "--",
            "/bin/true",
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Wrap a pre-built `tokio::process::Command` with bwrap.
///
/// Extracts `program`, `args`, and `current_dir` from the inner std::Command
/// and rebuilds as:
///
/// ```text
/// bwrap [--unshare-net] --die-with-parent --ro-bind / / \
///       --dev /dev --proc /proc \
///       --bind /tmp /tmp --bind <cwd> <cwd> --chdir <cwd> \
///       -- <program> <args...>
/// ```
///
/// The `program` + `args` vector survives the bwrap boundary verbatim; no
/// `sh -c` shell substitution, so Grep / Glob / etc. still get their
/// shell-injection-safe Command shape.
///
/// `--die-with-parent` (D16 safety net): if the loopr daemon dies, bwrap exits
/// immediately rather than orphaning into the init process.
///
/// `network` (Phase-5 finding 4): `false` adds `--unshare-net` (Local lane,
/// no network); `true` omits it so the Bash/`Net` lane keeps network access
/// while staying filesystem-contained.
pub fn bwrap_command(cmd: tokio::process::Command, working_dir: &Path, network: bool) -> tokio::process::Command {
    let (program, args) = extract_program_and_args(&cmd);
    let cwd = cmd
        .as_std()
        .get_current_dir()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| working_dir.to_path_buf());

    let mut wrapped = tokio::process::Command::new("bwrap");
    if !network {
        wrapped.arg("--unshare-net");
    }
    wrapped
        .arg("--die-with-parent")
        .arg("--ro-bind")
        .arg("/")
        .arg("/")
        .arg("--dev")
        .arg("/dev")
        .arg("--proc")
        .arg("/proc")
        .arg("--bind")
        .arg("/tmp")
        .arg("/tmp")
        .arg("--bind")
        .arg(&cwd)
        .arg(&cwd)
        .arg("--chdir")
        .arg(&cwd)
        .arg("--")
        .arg(program)
        .args(args);
    wrapped.stdout(std::process::Stdio::piped());
    wrapped.stderr(std::process::Stdio::piped());
    wrapped
}

fn extract_program_and_args(cmd: &tokio::process::Command) -> (OsString, Vec<OsString>) {
    let inner = cmd.as_std();
    let program = inner.get_program().to_os_string();
    let args: Vec<OsString> = inner.get_args().map(|a| a.to_os_string()).collect();
    (program, args)
}

#[cfg(test)]
mod tests;
