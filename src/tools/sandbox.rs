use std::path::Path;

use tracing::{info, warn};

/// Detect if bubblewrap (bwrap) is available on this system.
pub fn detect_bwrap() -> bool {
    std::process::Command::new("bwrap")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Log bwrap availability at startup.
pub fn log_bwrap_status() {
    if detect_bwrap() {
        info!("bwrap detected: Local lane network sandboxing enabled");
    } else {
        warn!("bwrap not found: Local lane will run without network isolation. Install bubblewrap for sandboxing.");
    }
}

/// Build a bwrap-wrapped Command that blocks network access.
///
/// Binds the entire host filesystem read-only (`--ro-bind / /`), with the
/// worktree and `/tmp` as read-write. This guarantees all installed tools
/// (rg, eza, fd, etc.) are available regardless of install location while
/// enforcing `--unshare-net` network isolation.
pub fn bwrap_command(command: &str, working_dir: &Path) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new("bwrap");
    cmd.arg("--unshare-net")
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
        .arg(working_dir)
        .arg(working_dir)
        .arg("--chdir")
        .arg(working_dir)
        .arg("--")
        .arg("sh")
        .arg("-c")
        .arg(command);
    cmd.stdout(std::process::Stdio::piped());
    cmd.stderr(std::process::Stdio::piped());
    cmd
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_detect_bwrap() {
        // Just verify it doesn't panic; result depends on system
        let _ = detect_bwrap();
    }

    #[test]
    fn test_bwrap_command_structure() {
        let cmd = bwrap_command("echo test", Path::new("/tmp/worktree"));
        let inner = cmd.as_std();
        assert_eq!(inner.get_program(), "bwrap");
        let args: Vec<_> = inner.get_args().map(|a| a.to_string_lossy().to_string()).collect();
        assert!(args.contains(&"--unshare-net".to_string()));
        assert!(args.contains(&"--ro-bind".to_string()));
        assert!(args.contains(&"echo test".to_string()));
        // Working dir should appear as bind target
        assert!(args.contains(&"/tmp/worktree".to_string()));
    }

    #[test]
    fn test_bwrap_command_has_shell_wrapper() {
        let cmd = bwrap_command("cargo build", Path::new("/tmp/w"));
        let args: Vec<_> = cmd
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().to_string())
            .collect();
        // Should end with: -- sh -c <command>
        let len = args.len();
        assert_eq!(args[len - 3], "sh");
        assert_eq!(args[len - 2], "-c");
        assert_eq!(args[len - 1], "cargo build");
    }

    #[tokio::test]
    async fn test_bwrap_blocks_network() {
        if !detect_bwrap() {
            eprintln!("skipping bwrap network test: bwrap not installed");
            return;
        }
        let dir = std::env::temp_dir();
        let mut cmd = bwrap_command("curl -s --max-time 2 http://1.1.1.1 2>&1 || echo BLOCKED", &dir);
        let output = cmd.output().await.unwrap();
        let stdout = String::from_utf8_lossy(&output.stdout);
        // Network should be unreachable inside bwrap --unshare-net
        assert!(
            stdout.contains("BLOCKED")
                || stdout.contains("Could not resolve")
                || stdout.contains("Network is unreachable"),
            "expected network to be blocked, got: {}",
            stdout
        );
    }
}
