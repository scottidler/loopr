use super::*;

use crate::shell::sh_command;
use std::path::Path;

#[test]
fn default_is_required() {
    assert_eq!(SandboxMode::default(), SandboxMode::Required);
}

#[test]
fn serde_lowercase_required() {
    let j: SandboxMode = serde_json::from_str(r#""required""#).unwrap();
    assert_eq!(j, SandboxMode::Required);
}

#[test]
fn serde_lowercase_preferred() {
    let j: SandboxMode = serde_json::from_str(r#""preferred""#).unwrap();
    assert_eq!(j, SandboxMode::Preferred);
}

#[test]
fn serde_lowercase_off() {
    let j: SandboxMode = serde_json::from_str(r#""off""#).unwrap();
    assert_eq!(j, SandboxMode::Off);
}

#[test]
fn serde_rejects_capitalized() {
    let err = serde_json::from_str::<SandboxMode>(r#""Required""#);
    assert!(err.is_err(), "capitalized variant must not deserialize");
}

#[test]
fn detect_bwrap_functional_does_not_panic() {
    let _ = detect_bwrap_functional();
}

#[test]
fn bwrap_command_wraps_sh_command() {
    // network=false (Local-lane shape): --unshare-net present.
    let inner = sh_command("echo hi", Path::new("/tmp"));
    let wrapped = bwrap_command(inner, Path::new("/tmp"), false);
    let std_cmd = wrapped.as_std();
    assert_eq!(std_cmd.get_program(), "bwrap");

    let args: Vec<String> = std_cmd.get_args().map(|a| a.to_string_lossy().into_owned()).collect();
    assert!(args.contains(&"--unshare-net".into()), "args: {args:?}");
    assert!(args.contains(&"--die-with-parent".into()), "args: {args:?}");
    assert!(args.contains(&"--ro-bind".into()), "args: {args:?}");
    assert!(args.contains(&"/tmp".into()), "args: {args:?}");
    // The original `sh`, `-c`, and `echo hi` must survive the wrap verbatim.
    let sep_idx = args
        .iter()
        .position(|a| a == "--")
        .expect("-- must separate bwrap args from inner program");
    assert_eq!(args[sep_idx + 1], "sh");
    assert_eq!(args[sep_idx + 2], "-c");
    assert_eq!(args[sep_idx + 3], "echo hi");
}

#[test]
fn bwrap_command_preserves_custom_program() {
    let mut inner = tokio::process::Command::new("grep");
    inner.arg("-rn").arg("pattern").arg("/tmp");
    inner.current_dir("/tmp");
    let wrapped = bwrap_command(inner, Path::new("/tmp"), false);
    let args: Vec<String> = wrapped
        .as_std()
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let sep_idx = args.iter().position(|a| a == "--").unwrap();
    assert_eq!(args[sep_idx + 1], "grep");
    assert_eq!(args[sep_idx + 2], "-rn");
    assert_eq!(args[sep_idx + 3], "pattern");
    assert_eq!(args[sep_idx + 4], "/tmp");
}

#[test]
fn bwrap_command_uses_inner_cwd_when_set() {
    let mut inner = tokio::process::Command::new("pwd");
    inner.current_dir("/tmp/foo");
    let wrapped = bwrap_command(inner, Path::new("/other"), false);
    let args: Vec<String> = wrapped
        .as_std()
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let chdir_idx = args.iter().position(|a| a == "--chdir").unwrap();
    assert_eq!(args[chdir_idx + 1], "/tmp/foo");
}

#[test]
fn bwrap_command_network_omits_unshare_net() {
    // network=true (Net/Bash-lane shape): --unshare-net absent, but the
    // filesystem-containment flags still present.
    let inner = sh_command("curl https://example.com", Path::new("/tmp"));
    let wrapped = bwrap_command(inner, Path::new("/tmp"), true);
    let args: Vec<String> = wrapped
        .as_std()
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    assert!(
        !args.contains(&"--unshare-net".into()),
        "network lane must keep net: {args:?}"
    );
    assert!(args.contains(&"--die-with-parent".into()), "args: {args:?}");
    assert!(
        args.contains(&"--ro-bind".into()),
        "filesystem containment retained: {args:?}"
    );
}

// --- Link 6: the sandbox scopes writes to the worktree ---------------------

fn arg_strings(wrapped: &tokio::process::Command) -> Vec<String> {
    wrapped
        .as_std()
        .get_args()
        .map(|a| a.to_string_lossy().into_owned())
        .collect()
}

/// The worktree stays rw-bound and gets an ephemeral scratch `tmpfs`; the
/// blanket `--bind /tmp /tmp` (the Link 6 escape hole) is gone, so a cwd
/// outside `/tmp` never rw-binds `/tmp` at all.
#[test]
fn bwrap_command_binds_worktree_and_scratch_not_all_of_tmp() {
    let mut inner = tokio::process::Command::new("pwd");
    inner.current_dir("/home/user/wt/wk-x");
    let wrapped = bwrap_command(inner, Path::new("/home/user/wt/wk-x"), false);
    let args = arg_strings(&wrapped);

    // worktree cwd is rw-bound...
    let bind_idx = args.iter().position(|a| a == "--bind").expect("--bind present");
    assert_eq!(args[bind_idx + 1], "/home/user/wt/wk-x");
    assert_eq!(args[bind_idx + 2], "/home/user/wt/wk-x");

    // ...and the scratch tmpfs sits under it (so bwrap can mkdir the mount point).
    let tmpfs_idx = args.iter().position(|a| a == "--tmpfs").expect("--tmpfs present");
    assert_eq!(args[tmpfs_idx + 1], "/home/user/wt/wk-x/.loopr-sandbox-tmp");

    // The old all-/tmp rw hole is closed: with a non-/tmp cwd, nothing rw-binds /tmp.
    assert!(
        !args
            .windows(3)
            .any(|w| w[0] == "--bind" && w[1] == "/tmp" && w[2] == "/tmp"),
        "blanket --bind /tmp /tmp must be gone: {args:?}"
    );
}

/// `$TMPDIR` is redirected to the scratch tmpfs so temp-writing tools do not
/// try (and fail) to write the now-read-only `/tmp`.
#[test]
fn bwrap_command_sets_tmpdir_to_scratch() {
    let mut inner = tokio::process::Command::new("pwd");
    inner.current_dir("/home/user/wt/wk-x");
    let wrapped = bwrap_command(inner, Path::new("/home/user/wt/wk-x"), false);
    let tmpdir = wrapped
        .as_std()
        .get_envs()
        .find(|(k, _)| *k == std::ffi::OsStr::new("TMPDIR"))
        .and_then(|(_, v)| v)
        .map(|v| v.to_string_lossy().into_owned());
    assert_eq!(tmpdir.as_deref(), Some("/home/user/wt/wk-x/.loopr-sandbox-tmp"));
}

/// Rebuild an equivalent synchronous std::Command from a bwrap-wrapped tokio
/// Command so the integration tests can run it without a nested runtime clash.
/// Inherits the parent env (PATH etc.), then applies only the explicit
/// overrides the wrapper set (TMPDIR).
fn run_sync(wrapped: &tokio::process::Command) -> std::process::Output {
    let std_cmd = wrapped.as_std();
    let mut c = std::process::Command::new(std_cmd.get_program());
    c.args(std_cmd.get_args());
    for (k, v) in std_cmd.get_envs() {
        match v {
            Some(v) => {
                c.env(k, v);
            }
            None => {
                c.env_remove(k);
            }
        }
    }
    c.output().expect("spawn bwrap")
}

/// A worktree write DOES land on the host, an out-of-worktree write does NOT,
/// and temp-file tools ($TMPDIR) work against the scratch tmpfs. Break-to-
/// proven inline: the same escape write against the OLD all-`/tmp`-rw bind
/// DOES land, so the assertion actually discriminates the fix.
///
/// Gated on a functional bwrap (skips cleanly where the kernel forbids user
/// namespaces / mounts), matching every other bwrap-requiring test here.
#[test]
fn sandbox_scopes_writes_to_worktree() {
    if !detect_bwrap_functional() {
        eprintln!("skip sandbox_scopes_writes_to_worktree: bwrap not functional on this host");
        return;
    }

    let worktree = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let escape = outside.path().join("escape-marker.txt");
    let inwt = worktree.path().join("inwt-marker.txt");

    // 1. Out-of-worktree write is DENIED by the fix (nothing lands on host).
    let mut inner = tokio::process::Command::new("sh");
    inner.arg("-c").arg(format!("echo ESCAPED > {}", escape.display()));
    inner.current_dir(worktree.path());
    let out = run_sync(&bwrap_command(inner, worktree.path(), false));
    assert!(
        !escape.exists(),
        "escape write must NOT land on host (stderr: {})",
        String::from_utf8_lossy(&out.stderr)
    );

    // 1b. Break-to-proven: the OLD all-/tmp-rw bind lets the SAME write land.
    //     (Built inline so the test needs no second production code path.)
    if escape.starts_with(std::env::temp_dir()) {
        let mut old = std::process::Command::new("bwrap");
        old.arg("--unshare-net")
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
            .arg(worktree.path())
            .arg(worktree.path())
            .arg("--chdir")
            .arg(worktree.path())
            .arg("--")
            .arg("sh")
            .arg("-c")
            .arg(format!("echo ESCAPED > {}", escape.display()));
        let _ = old.output().expect("spawn old-style bwrap");
        assert!(
            escape.exists(),
            "break-to-proven guard: the pre-fix all-/tmp-rw bind should let the escape land"
        );
        std::fs::remove_file(&escape).ok();
    }

    // 2. In-worktree write DOES land on the host worktree.
    let mut inner = tokio::process::Command::new("sh");
    inner.arg("-c").arg(format!("echo INWT > {}", inwt.display()));
    inner.current_dir(worktree.path());
    let out = run_sync(&bwrap_command(inner, worktree.path(), false));
    assert!(
        inwt.exists(),
        "in-worktree write must land (stderr: {})",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(std::fs::read_to_string(&inwt).unwrap().trim(), "INWT");

    // 3. Temp-file tools work against the scratch tmpfs ($TMPDIR), the thing a
    //    fully read-only /tmp would break (rustdoc doctests, mktemp, cargo temp).
    let mut inner = tokio::process::Command::new("sh");
    inner.arg("-c").arg("mktemp && echo TMPOK");
    inner.current_dir(worktree.path());
    let out = run_sync(&bwrap_command(inner, worktree.path(), false));
    assert!(
        out.status.success() && String::from_utf8_lossy(&out.stdout).contains("TMPOK"),
        "writing to $TMPDIR scratch must succeed (stdout: {}, stderr: {})",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    );
    // The scratch is ephemeral: nothing leaks to the host worktree.
    assert!(
        !worktree.path().join(".loopr-sandbox-tmp").join("").exists()
            || std::fs::read_dir(worktree.path().join(".loopr-sandbox-tmp"))
                .map(|mut d| d.next().is_none())
                .unwrap_or(true),
        "scratch tmpfs contents must not persist on the host worktree"
    );
}

/// Positive build-style check: a real compiler (`rustc`) invoked inside the
/// worktree compiles a program whose output lands in the worktree, proving
/// legitimate in-worktree tool execution still works under the scoped sandbox.
/// Gated on both bwrap and rustc being present.
#[test]
fn sandbox_allows_in_worktree_build() {
    if !detect_bwrap_functional() {
        eprintln!("skip sandbox_allows_in_worktree_build: bwrap not functional");
        return;
    }
    if std::process::Command::new("rustc")
        .arg("--version")
        .output()
        .map(|o| !o.status.success())
        .unwrap_or(true)
    {
        eprintln!("skip sandbox_allows_in_worktree_build: rustc not available");
        return;
    }

    let worktree = tempfile::tempdir().unwrap();
    std::fs::write(worktree.path().join("hello.rs"), "fn main() { println!(\"hi\"); }").unwrap();

    let mut inner = tokio::process::Command::new("sh");
    inner.arg("-c").arg("rustc hello.rs -o hello && ./hello");
    inner.current_dir(worktree.path());
    let out = run_sync(&bwrap_command(inner, worktree.path(), false));
    assert!(
        out.status.success(),
        "in-worktree rustc build must succeed (stderr: {})",
        String::from_utf8_lossy(&out.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), "hi");
    assert!(
        worktree.path().join("hello").exists(),
        "compiled binary must land in worktree"
    );
}
