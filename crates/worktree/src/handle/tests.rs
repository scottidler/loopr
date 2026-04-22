use std::path::PathBuf;

use domain::WorkId;

use super::*;

fn wk(prefix: &str) -> WorkId {
    use std::str::FromStr;
    WorkId::from_str(prefix).unwrap()
}

#[test]
fn accessors_return_stored_fields() {
    let wt = Worktree::from_parts(
        PathBuf::from("/tmp/target/.loopr/worktrees/wk-abc12-1"),
        "loopr/wk-wk-abc12-1".to_string(),
        wk("wk-abc12"),
        1,
        PathBuf::from("/tmp/target"),
        true, // consumed, so Drop doesn't fire the stub `remove_worktree`
    );
    assert_eq!(
        wt.path(),
        std::path::Path::new("/tmp/target/.loopr/worktrees/wk-abc12-1")
    );
    assert_eq!(wt.branch(), "loopr/wk-wk-abc12-1");
    assert_eq!(wt.work_id().as_ref(), "wk-abc12");
    assert_eq!(wt.seq(), 1);
}

#[test]
fn drop_with_consumed_is_a_noop() {
    // `consumed: true` → Drop returns early without calling `ops::remove_worktree`.
    // We can't directly observe the no-op (the stub ops is a no-op too in Phase 1),
    // but we assert this does not panic and does not touch state. Regression guard
    // for when Phase 2 replaces the stub with a real `git worktree remove`.
    let wt = Worktree::from_parts(
        PathBuf::from("/definitely/does/not/exist"),
        "loopr/wk-wk-abc12-1".to_string(),
        wk("wk-abc12"),
        1,
        PathBuf::from("/also/nonexistent"),
        true,
    );
    drop(wt);
}

#[test]
fn explicit_cleanup_marks_consumed() {
    // Because the Phase 1 stub `remove_worktree` always returns Ok, we can
    // exercise the handle's state machine: `cleanup` should consume the handle
    // (moves `self`), leaving no observable side effect afterward.
    let wt = Worktree::from_parts(
        PathBuf::from("/tmp/target/.loopr/worktrees/wk-abc12-1"),
        "loopr/wk-wk-abc12-1".to_string(),
        wk("wk-abc12"),
        1,
        PathBuf::from("/tmp/target"),
        false,
    );
    // `cleanup` takes `self` by value; the post-cleanup handle cannot be observed.
    // Success is type-level: compiles and returns Ok under the Phase 1 stub.
    wt.cleanup().expect("Phase 1 stub always returns Ok");
}
