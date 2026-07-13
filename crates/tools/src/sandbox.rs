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

/// Per-sandbox writable scratch directory. Link 6 (2026-07-13): the sandbox no
/// longer rw-binds all of `/tmp`. That blanket bind let a subprocess `cd` to
/// any absolute path under `/tmp` — where targets and worktrees live — and
/// write outside its own worktree, defeating worktree isolation (in the Link 5
/// run an implementer ran `cd <target-main-tree> && cargo test`, polluting the
/// operator's live tree with an untracked `Cargo.lock`).
///
/// Now `/tmp` inherits the read-only `--ro-bind / /`, so a write outside the
/// worktree fails closed with EROFS. Tools that genuinely need a writable temp
/// (rustdoc doctests, `mktemp`, cargo/rustc intermediates that honor `$TMPDIR`)
/// write into this directory instead: an ephemeral `tmpfs` mounted *inside* the
/// worktree namespace (so bwrap can create the mount point under the rw
/// worktree bind) and exported as `$TMPDIR`. It is isolated per invocation and
/// gone when bwrap exits, so it never lands on the host worktree.
const SANDBOX_SCRATCH_SUBDIR: &str = ".loopr-sandbox-tmp";

/// Functional bwrap detection (D6): not just `bwrap --version` (which only
/// proves the binary exists). This actually invokes bwrap with the full flag
/// set we depend on against `/bin/true`, so a machine that has the binary but
/// whose kernel has `user.max_user_namespaces=0` or other failure modes
/// surfaces at daemon startup, not on first tool call.
///
/// Phase-5 finding 4: the probe mirrors the actual `bwrap_command` mount flags
/// (`--dev`/`--proc`/`--bind`/`--chdir`), not just `--unshare-net` +
/// `--ro-bind`. The mount flags can fail independently of the net-unshare
/// (e.g. `--proc` under a restrictive kernel), and the old narrow probe would
/// have reported "functional" while the real wrap failed on first tool call.
/// `--unshare-net` is the strictest (Local-lane) shape; the Net lane uses a
/// strict subset, so a probe pass guarantees both lanes wrap successfully.
///
/// Link 6 (2026-07-13): `--tmpfs <cwd>/<scratch>` replaces the old blanket
/// `--bind /tmp /tmp`. The probe uses `/tmp` as a stand-in cwd — rw-binding it
/// then mounting the scratch `tmpfs` on a subpath — so a kernel that rejects
/// the `tmpfs` mount (the one new mount type we depend on) is caught here, not
/// on first tool call.
pub fn detect_bwrap_functional() -> bool {
    let scratch = format!("/tmp/{SANDBOX_SCRATCH_SUBDIR}");
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
            "--tmpfs",
            &scratch,
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
///       --bind <cwd> <cwd> --tmpfs <cwd>/.loopr-sandbox-tmp --chdir <cwd> \
///       -- <program> <args...>
/// ```
///
/// with `$TMPDIR` set to `<cwd>/.loopr-sandbox-tmp` on the wrapped Command.
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
///
/// Link 6 (2026-07-13): the only writable paths are the worktree (`--bind
/// <cwd> <cwd>`) and the ephemeral scratch `tmpfs` (`SANDBOX_SCRATCH_SUBDIR`,
/// exported as `$TMPDIR`). Everything else — including the rest of `/tmp`,
/// where sibling worktrees and the target's main tree live — inherits the
/// read-only `--ro-bind / /`, so a `cd <abs-path-outside-worktree> && write`
/// fails closed with EROFS. The `--tmpfs` mount point sits *under* the rw
/// worktree bind and is ordered *after* it, so bwrap can create the mount
/// point; putting the scratch under `/tmp` at large would either re-open the
/// escape or shadow the worktree's own git dir (see the implementation notes).
pub fn bwrap_command(cmd: tokio::process::Command, working_dir: &Path, network: bool) -> tokio::process::Command {
    let (program, args) = extract_program_and_args(&cmd);
    let cwd = cmd
        .as_std()
        .get_current_dir()
        .map(Path::to_path_buf)
        .unwrap_or_else(|| working_dir.to_path_buf());
    let scratch = cwd.join(SANDBOX_SCRATCH_SUBDIR);

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
        .arg(&cwd)
        .arg(&cwd)
        .arg("--tmpfs")
        .arg(&scratch)
        .arg("--chdir")
        .arg(&cwd)
        .arg("--")
        .arg(program)
        .args(args);
    // Point temp-file-writing tools (rustdoc doctests, `mktemp`, cargo/rustc
    // intermediates) at the writable scratch tmpfs instead of the now-read-only
    // `/tmp`. bwrap passes this through its `execve` of the inner shell (no
    // `--clearenv`); `scrub_command` (D12) only strips secret-suffixed vars.
    wrapped.env("TMPDIR", &scratch);
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
