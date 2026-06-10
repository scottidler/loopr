#![allow(clippy::unwrap_used)]

use std::fs;

use super::seed_prompts;

#[test]
fn seed_into_empty_target_writes_all_files() {
    let dest = tempfile::tempdir().unwrap();
    let baked = ::context::baked_prompts();
    let outcome = seed_prompts(dest.path(), baked, false).unwrap();
    assert!(outcome.written > 0, "expected to write at least one file");
    assert_eq!(outcome.preserved, 0);
    // The implementer system prompt is the load-bearing file: assert it landed.
    let implementer = dest.path().join("agents/implementer/system.pmt");
    assert!(implementer.exists(), "expected {implementer:?}");
    let content = fs::read_to_string(&implementer).unwrap();
    assert!(content.contains("You are an Implementer agent"));
}

#[test]
fn seed_skips_gitkeep_files_but_creates_their_parents() {
    let dest = tempfile::tempdir().unwrap();
    let baked = ::context::baked_prompts();
    let _ = seed_prompts(dest.path(), baked, false).unwrap();
    let chat_dir = dest.path().join("chat");
    let plan_dir = dest.path().join("decompose/plan");
    assert!(chat_dir.is_dir(), "chat/ dir should exist");
    assert!(plan_dir.is_dir(), "decompose/plan/ dir should exist");
    assert!(!chat_dir.join(".gitkeep").exists(), ".gitkeep must NOT be written");
    assert!(!plan_dir.join(".gitkeep").exists(), ".gitkeep must NOT be written");
}

#[test]
fn seed_preserves_existing_edits_under_default_merge() {
    let dest = tempfile::tempdir().unwrap();
    let baked = ::context::baked_prompts();
    // First seed populates everything.
    seed_prompts(dest.path(), baked, false).unwrap();
    // User edits one file.
    let target_file = dest.path().join("agents/implementer/system.pmt");
    fs::write(&target_file, "USER EDITED CONTENT").unwrap();
    // Re-seed without force.
    let outcome = seed_prompts(dest.path(), baked, false).unwrap();
    assert_eq!(outcome.written, 0, "merge mode must not overwrite anything");
    assert!(outcome.preserved > 0);
    let content = fs::read_to_string(&target_file).unwrap();
    assert_eq!(content, "USER EDITED CONTENT");
}

#[test]
fn seed_force_overwrites_existing_edits() {
    let dest = tempfile::tempdir().unwrap();
    let baked = ::context::baked_prompts();
    seed_prompts(dest.path(), baked, false).unwrap();
    let target_file = dest.path().join("agents/implementer/system.pmt");
    fs::write(&target_file, "USER EDITED CONTENT").unwrap();
    let outcome = seed_prompts(dest.path(), baked, true).unwrap();
    assert!(
        outcome.written > 0,
        "force mode must overwrite at least the edited file"
    );
    let content = fs::read_to_string(&target_file).unwrap();
    assert!(content.contains("You are an Implementer agent"));
    assert!(!content.contains("USER EDITED CONTENT"));
}

// ---------------------------------------------------------------------------
// Phase 9 (Tier-1 cleanup): six-step init.
// `run` is sync; it spins up a small tokio runtime internally for the
// async steps. We exercise the full sequence end-to-end against a
// synthetic git repo.
// ---------------------------------------------------------------------------

fn init_git_repo(path: &std::path::Path) {
    use std::process::Command;
    let run = |args: &[&str]| {
        let out = Command::new("git").arg("-C").arg(path).args(args).output().unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    };
    run(&["init", "-q", "-b", "main"]);
    run(&["config", "user.email", "test@example.com"]);
    run(&["config", "user.name", "test"]);
    run(&["config", "commit.gpgsign", "false"]);
    run(&["commit", "--allow-empty", "-q", "-m", "initial"]);
}

#[test]
fn run_creates_loopr_dir_taskstore_excludes_and_prompts_on_fresh_target() {
    let td = tempfile::tempdir().unwrap();
    init_git_repo(td.path());

    super::run(td.path(), false).unwrap();

    assert!(td.path().join(".loopr").is_dir());
    assert!(td.path().join(".loopr/taskstore").is_dir());
    assert!(td.path().join(".loopr/prompts").is_dir());
    assert!(td.path().join(".loopr/prompts/agents/implementer/system.pmt").exists());
    assert!(td.path().join(".git/info/exclude").exists());
}

#[test]
fn run_is_idempotent_on_already_initialized_target() {
    let td = tempfile::tempdir().unwrap();
    init_git_repo(td.path());
    super::run(td.path(), false).unwrap();
    // Second run must not error and must preserve everything.
    super::run(td.path(), false).unwrap();
    assert!(td.path().join(".loopr/taskstore").is_dir());
}

#[test]
fn step_create_loopr_dir_reports_preserved_when_dir_exists() {
    let td = tempfile::tempdir().unwrap();
    std::fs::create_dir(td.path().join(".loopr")).unwrap();
    let outcome = super::step_create_loopr_dir(td.path()).unwrap();
    assert!(matches!(outcome, super::StepOutcome::Preserved { .. }));
}

#[test]
fn step_create_loopr_dir_reports_created_when_fresh() {
    let td = tempfile::tempdir().unwrap();
    let outcome = super::step_create_loopr_dir(td.path()).unwrap();
    assert!(matches!(outcome, super::StepOutcome::Created { .. }));
}

// ---------------------------------------------------------------------------
// Phase 8: init + target correctness.
// ---------------------------------------------------------------------------

#[test]
fn excludes_step_skips_on_non_git_target() {
    // Finding 2: a non-git target must not fabricate `.git/info/exclude`
    // (which a later run would then mistake for a real repo).
    let td = tempfile::tempdir().unwrap();
    let outcome = super::step_ensure_git_excludes(td.path()).unwrap();
    assert!(matches!(outcome, super::StepOutcome::Skipped { .. }), "got {outcome:?}");
    assert!(!td.path().join(".git").exists(), ".git must not be fabricated");
}

#[test]
fn run_on_non_git_target_does_not_fabricate_git_dir() {
    let td = tempfile::tempdir().unwrap();
    super::run(td.path(), false).unwrap();
    assert!(td.path().join(".loopr").is_dir());
    assert!(
        !td.path().join(".git").exists(),
        ".git must not be fabricated on a non-git target"
    );
}

#[test]
fn hooks_step_installs_merge_driver_even_with_user_precommit_hook() {
    // Finding 3: a husky/user pre-commit (no taskstore marker) must NOT read
    // as "taskstore installed" — the installer must still run so the merge
    // driver lands, and the step reports Created (not a false Preserved).
    let td = tempfile::tempdir().unwrap();
    init_git_repo(td.path());
    let hooks = td.path().join(".git/hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    std::fs::write(hooks.join("pre-commit"), "#!/bin/sh\necho husky\n").unwrap();

    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let outcome = rt.block_on(super::step_install_taskstore_hooks(td.path())).unwrap();
    assert!(matches!(outcome, super::StepOutcome::Created { .. }), "got {outcome:?}");

    let content = std::fs::read_to_string(hooks.join("pre-commit")).unwrap();
    assert!(
        content.contains(super::TASKSTORE_HOOK_MARKER),
        "marker missing: {content}"
    );
    assert!(content.contains("echo husky"), "user content clobbered: {content}");

    // Second call now sees the marker -> Preserved.
    let outcome2 = rt.block_on(super::step_install_taskstore_hooks(td.path())).unwrap();
    assert!(
        matches!(outcome2, super::StepOutcome::Preserved { .. }),
        "got {outcome2:?}"
    );
}
