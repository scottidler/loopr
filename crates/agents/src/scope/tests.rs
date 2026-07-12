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

// --- directory-prefix scope semantics (Phase 14) ---

#[test]
fn test_dir_prefix_matches_nested_paths() {
    // A trailing-slash entry is a directory prefix: everything under it is
    // in scope, at any depth.
    let dirty = vec![
        "src/foo.rs".to_string(),
        "src/nested/bar.rs".to_string(),
        "other.rs".to_string(),
    ];
    let scope = vec!["src/".to_string()];
    let (in_scope, out) = partition_by_scope(&dirty, &scope);
    assert_eq!(in_scope, vec!["src/foo.rs", "src/nested/bar.rs"]);
    assert_eq!(out, vec!["other.rs"]);
}

#[test]
fn test_dir_prefix_does_not_match_sibling_prefix() {
    // `src/` must NOT match `src-gen/...` — the boundary is the slash, not a
    // raw string prefix.
    let dirty = vec!["src-gen/x.rs".to_string(), "srcfoo.rs".to_string()];
    let scope = vec!["src/".to_string()];
    let (in_scope, out) = partition_by_scope(&dirty, &scope);
    assert!(in_scope.is_empty());
    assert_eq!(out, vec!["src-gen/x.rs", "srcfoo.rs"]);
}

#[test]
fn test_exact_entry_does_not_match_directory_children() {
    // Without a trailing slash the entry is an exact path: `src` matches only
    // a file literally named `src`, not files under a `src/` directory.
    let dirty = vec!["src/foo.rs".to_string()];
    let scope = vec!["src".to_string()];
    let (in_scope, out) = partition_by_scope(&dirty, &scope);
    assert!(in_scope.is_empty());
    assert_eq!(out, vec!["src/foo.rs"]);
}

#[test]
fn test_bare_slash_scope_fails_closed() {
    // A bare `/` entry is not a usable directory scope. Fail closed: it must
    // NOT match everything.
    let dirty = vec!["anything.rs".to_string()];
    let scope = vec!["/".to_string()];
    let (in_scope, out) = partition_by_scope(&dirty, &scope);
    assert!(in_scope.is_empty());
    assert_eq!(out, vec!["anything.rs"]);
}

#[test]
fn test_dir_prefix_still_excludes_loopr_artifacts() {
    // `.loopr/` is always out of scope even under a broad directory prefix.
    let dirty = vec![".loopr/state.jsonl".to_string(), "keep.rs".to_string()];
    let scope = vec!["./".to_string(), "keep.rs".to_string()];
    let (in_scope, out) = partition_by_scope(&dirty, &scope);
    assert_eq!(in_scope, vec!["keep.rs"]);
    assert_eq!(out, vec![".loopr/state.jsonl"]);
}

// --- out_of_scope_paths (propose-time defense-in-depth gate) ---

#[test]
fn test_out_of_scope_paths_flags_only_out_of_scope() {
    let paths = vec![
        "src/foo.rs".to_string(),
        "secret.txt".to_string(),
        "src/nested/bar.rs".to_string(),
    ];
    let scope = vec!["src/".to_string()];
    let out = out_of_scope_paths(&paths, &scope);
    assert_eq!(out, vec!["secret.txt"]);
}

#[test]
fn test_out_of_scope_paths_empty_when_all_in_scope() {
    let paths = vec!["src/foo.rs".to_string(), "README.md".to_string()];
    let scope = vec!["src/foo.rs".to_string(), "README.md".to_string()];
    assert!(out_of_scope_paths(&paths, &scope).is_empty());
}

#[test]
fn test_out_of_scope_paths_flags_loopr_artifacts() {
    let paths = vec!["src/foo.rs".to_string(), ".loopr/leak.txt".to_string()];
    let scope = vec!["src/".to_string()];
    assert_eq!(out_of_scope_paths(&paths, &scope), vec![".loopr/leak.txt"]);
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
