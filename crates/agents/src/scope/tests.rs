#![allow(clippy::unwrap_used)]

use super::*;

// --- partition_by_scope tests ---

#[test]
fn test_exact_match_in_scope() {
    let dirty = vec!["main.py".to_string(), "test_api.py".to_string()];
    let scope = vec!["main.py".to_string(), "test_api.py".to_string()];
    let (in_scope, out) = partition_by_scope(&dirty, &scope);
    assert_eq!(in_scope, vec!["main.py", "test_api.py"]);
    assert!(out.is_empty());
}

#[test]
fn test_no_match_out_of_scope() {
    let dirty = vec!["database.py".to_string()];
    let scope = vec!["main.py".to_string()];
    let (in_scope, out) = partition_by_scope(&dirty, &scope);
    assert!(in_scope.is_empty());
    assert_eq!(out, vec!["database.py"]);
}

#[test]
fn test_empty_scope_all_non_artifacts_in_scope() {
    let dirty = vec!["main.py".to_string(), "database.py".to_string()];
    let scope: Vec<String> = vec![];
    let (in_scope, out) = partition_by_scope(&dirty, &scope);
    assert_eq!(in_scope, vec!["main.py", "database.py"]);
    assert!(out.is_empty());
}

#[test]
fn test_empty_dirty_returns_empty() {
    let dirty: Vec<String> = vec![];
    let scope = vec!["main.py".to_string()];
    let (in_scope, out) = partition_by_scope(&dirty, &scope);
    assert!(in_scope.is_empty());
    assert!(out.is_empty());
}

#[test]
fn test_mixed_in_and_out_of_scope() {
    let dirty = vec![
        "main.py".to_string(),
        "test_api.py".to_string(),
        "database.py".to_string(),
    ];
    let scope = vec!["main.py".to_string(), "test_api.py".to_string()];
    let (in_scope, out) = partition_by_scope(&dirty, &scope);
    assert_eq!(in_scope, vec!["main.py", "test_api.py"]);
    assert_eq!(out, vec!["database.py"]);
}

#[test]
fn test_leading_dot_slash_normalization() {
    let dirty = vec!["./main.py".to_string(), "test_api.py".to_string()];
    let scope = vec!["main.py".to_string(), "./test_api.py".to_string()];
    let (in_scope, out) = partition_by_scope(&dirty, &scope);
    assert_eq!(in_scope, vec!["main.py", "test_api.py"]);
    assert!(out.is_empty());
}

#[test]
fn test_loopr_artifacts_always_filtered() {
    let dirty = vec![
        "main.py".to_string(),
        ".loopr/taskstore/db.sqlite".to_string(),
        ".loopr/worktrees/wk-1/file.txt".to_string(),
    ];
    let scope = vec!["main.py".to_string()];
    let (in_scope, out) = partition_by_scope(&dirty, &scope);
    assert_eq!(in_scope, vec!["main.py"]);
    assert_eq!(
        out,
        vec![".loopr/taskstore/db.sqlite", ".loopr/worktrees/wk-1/file.txt"]
    );
}

#[test]
fn test_artifacts_filtered_even_with_empty_scope() {
    let dirty = vec![
        "main.py".to_string(),
        ".loopr/taskstore/data.jsonl".to_string(),
        ".loopr/runs/r-1/log".to_string(),
    ];
    let scope: Vec<String> = vec![];
    let (in_scope, out) = partition_by_scope(&dirty, &scope);
    assert_eq!(in_scope, vec!["main.py"]);
    assert_eq!(out, vec![".loopr/taskstore/data.jsonl", ".loopr/runs/r-1/log"]);
}

// --- parse_porcelain_status tests ---

#[test]
fn test_parse_modified_files() {
    let output = " M main.py\n M test_api.py\n";
    let files = parse_porcelain_status(output);
    assert_eq!(files, vec!["main.py", "test_api.py"]);
}

#[test]
fn test_parse_added_files() {
    let output = "A  new_file.py\n";
    let files = parse_porcelain_status(output);
    assert_eq!(files, vec!["new_file.py"]);
}

#[test]
fn test_parse_deleted_files() {
    let output = " D old_file.py\n";
    let files = parse_porcelain_status(output);
    assert_eq!(files, vec!["old_file.py"]);
}

#[test]
fn test_parse_untracked_files() {
    let output = "?? untracked.py\n";
    let files = parse_porcelain_status(output);
    assert_eq!(files, vec!["untracked.py"]);
}

#[test]
fn test_parse_renamed_emits_both_paths() {
    // The v4 bug fix: a rename line must emit BOTH old and new so
    // `partition_by_scope` can evaluate scope membership for each side.
    // Without this, `git commit --only -- <new>` drops the source-side
    // deletion and the rename lands as a copy on disk.
    let output = "R  old.py -> new.py\n";
    let files = parse_porcelain_status(output);
    assert_eq!(files, vec!["old.py", "new.py"]);
}

#[test]
fn test_parse_copied_emits_both_paths() {
    let output = "C  src.py -> dest.py\n";
    let files = parse_porcelain_status(output);
    assert_eq!(files, vec!["src.py", "dest.py"]);
}

#[test]
fn test_parse_quoted_paths_with_spaces() {
    let output = " M \"path with spaces/file.py\"\n";
    let files = parse_porcelain_status(output);
    assert_eq!(files, vec!["path with spaces/file.py"]);
}

#[test]
fn test_parse_empty_output() {
    let output = "";
    let files = parse_porcelain_status(output);
    assert!(files.is_empty());
}

#[test]
fn test_parse_mixed_statuses() {
    let output = " M modified.py\nA  added.py\n D deleted.py\n?? untracked.py\nR  old.py -> renamed.py\n";
    let files = parse_porcelain_status(output);
    assert_eq!(
        files,
        vec![
            "modified.py",
            "added.py",
            "deleted.py",
            "untracked.py",
            "old.py",
            "renamed.py",
        ]
    );
}

#[test]
fn test_partition_handles_renamed_pair() {
    // When parse_porcelain_status emits both sides of a rename, partition
    // can decide each side independently. If only one side is in scope,
    // the other lands in `out_of_scope` so the agent's `dropped` feedback
    // surfaces the half-rename concern.
    let dirty = vec!["old.py".to_string(), "new.py".to_string()];
    let scope = vec!["new.py".to_string()];
    let (in_scope, out) = partition_by_scope(&dirty, &scope);
    assert_eq!(in_scope, vec!["new.py"]);
    assert_eq!(out, vec!["old.py"]);
}
