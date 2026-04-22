use std::path::PathBuf;

use super::*;

#[test]
fn branch_happy_path_single_segment_work_id() {
    let (wk, seq) = branch("loopr/wk-abc-3").expect("parses");
    assert_eq!(wk.as_ref(), "abc");
    assert_eq!(seq, 3);
}

#[test]
fn branch_happy_path_compound_work_id_splits_on_last_dash() {
    // Real WorkIds carry a `wk-` prefix (`wk-abc12`), so the branch name
    // looks like `loopr/wk-wk-abc12-3`. The parser must strip the loopr
    // prefix once, then split on the LAST `-` to separate work_id from seq.
    let (wk, seq) = branch("loopr/wk-wk-abc12-3").expect("parses");
    assert_eq!(wk.as_ref(), "wk-abc12");
    assert_eq!(seq, 3);
}

#[test]
fn branch_rejects_missing_prefix() {
    assert!(branch("main").is_none());
    assert!(branch("feature/wk-abc-1").is_none());
    assert!(branch("loopr/plan-pl-xxx").is_none());
}

#[test]
fn branch_rejects_missing_seq() {
    assert!(branch("loopr/wk-abc").is_none());
}

#[test]
fn branch_rejects_non_numeric_seq() {
    assert!(branch("loopr/wk-abc-xyz").is_none());
}

#[test]
fn branch_rejects_zero_seq() {
    assert!(branch("loopr/wk-abc-0").is_none());
}

#[test]
fn branch_rejects_empty_work_id() {
    // `loopr/wk--5` → rest="-5", rsplit_once('-') → ("", "5") → rejected
    assert!(branch("loopr/wk--5").is_none());
}

#[test]
fn porcelain_parses_single_entry_under_root() {
    let root = PathBuf::from("/repo/.loopr/worktrees");
    let out = "\
worktree /repo/.loopr/worktrees/wk-abc12-1
HEAD 0123456789abcdef0123456789abcdef01234567
branch refs/heads/loopr/wk-wk-abc12-1
";
    let infos = porcelain(out, &root);
    assert_eq!(infos.len(), 1);
    assert_eq!(infos[0].path, PathBuf::from("/repo/.loopr/worktrees/wk-abc12-1"));
    assert_eq!(infos[0].branch, "loopr/wk-wk-abc12-1");
    assert_eq!(infos[0].head, "0123456789abcdef0123456789abcdef01234567");
}

#[test]
fn porcelain_filters_entries_outside_root() {
    let root = PathBuf::from("/repo/.loopr/worktrees");
    let out = "\
worktree /repo
HEAD 0000000000000000000000000000000000000000
branch refs/heads/main

worktree /home/alice/other-worktree
HEAD 1111111111111111111111111111111111111111
branch refs/heads/feature

worktree /repo/.loopr/worktrees/wk-abc12-1
HEAD 2222222222222222222222222222222222222222
branch refs/heads/loopr/wk-wk-abc12-1
";
    let infos = porcelain(out, &root);
    assert_eq!(infos.len(), 1);
    assert_eq!(infos[0].branch, "loopr/wk-wk-abc12-1");
}

#[test]
fn porcelain_skips_detached_entries() {
    let root = PathBuf::from("/repo/.loopr/worktrees");
    let out = "\
worktree /repo/.loopr/worktrees/detached
HEAD 3333333333333333333333333333333333333333
detached
";
    let infos = porcelain(out, &root);
    assert!(infos.is_empty(), "detached entries must be dropped");
}

#[test]
fn porcelain_handles_multiple_entries() {
    let root = PathBuf::from("/repo/.loopr/worktrees");
    let out = "\
worktree /repo/.loopr/worktrees/wk-abc12-1
HEAD aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa
branch refs/heads/loopr/wk-wk-abc12-1

worktree /repo/.loopr/worktrees/wk-abc12-2
HEAD bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb
branch refs/heads/loopr/wk-wk-abc12-2
";
    let infos = porcelain(out, &root);
    assert_eq!(infos.len(), 2);
    assert_eq!(infos[0].branch, "loopr/wk-wk-abc12-1");
    assert_eq!(infos[1].branch, "loopr/wk-wk-abc12-2");
}
