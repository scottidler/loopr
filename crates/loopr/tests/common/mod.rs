//! Shared helpers for integration tests.
//!
//! Not every test file uses every helper, so we annotate with
//! `#[allow(dead_code)]` to silence the per-test-file unused warnings.

#![allow(dead_code)]
#![allow(clippy::unwrap_used)]

use std::path::Path;
use std::process::Command;

/// Initialize a git repo at `path` with a single empty commit so HEAD
/// exists. Stage 8 wiring's `handle_plan_create` calls
/// `ensure_integration_branch`, which requires a valid HEAD to branch
/// from; a bare tempdir has neither. `commit.gpgsign` and `tag.gpgsign`
/// are explicitly disabled because the test host may inherit a
/// user-level git config with signing required.
pub fn init_git_repo(path: &Path) {
    let run = |args: &[&str]| {
        let out = Command::new("git")
            .arg("-C")
            .arg(path)
            .args(args)
            .output()
            .expect("git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    run(&["init", "-q", "-b", "main"]);
    run(&["config", "user.email", "test@example.com"]);
    run(&["config", "user.name", "test"]);
    run(&["config", "commit.gpgsign", "false"]);
    run(&["config", "tag.gpgsign", "false"]);
    run(&["commit", "--allow-empty", "-q", "-m", "initial"]);
}
