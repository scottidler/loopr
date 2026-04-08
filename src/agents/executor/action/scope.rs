/// Loopr orchestration artifacts that should never be staged.
/// These are also in .git/info/exclude (Layer 0), but we filter
/// them here as defense in depth.
const LOOPR_ARTIFACTS: &[&str] = &[".taskstore/", ".worktrees/", "loopr.yml"];

/// Partition dirty files into in-scope and out-of-scope relative to files.
///
/// Returns (in_scope, out_of_scope). A file is in-scope if:
///   - It is NOT a Loopr artifact (always filtered, regardless of files)
///   - AND it matches at least one resource_tag as an exact path
///   - OR files is empty (backward compat: all non-artifact files are in-scope)
///
/// Both sides are normalized: leading "./" is stripped, paths are compared
/// case-sensitively (Unix convention).
pub fn partition_by_scope(dirty_files: &[String], files: &[String]) -> (Vec<String>, Vec<String>) {
    let mut in_scope = Vec::new();
    let mut out_of_scope = Vec::new();
    for file in dirty_files {
        let normalized = file.strip_prefix("./").unwrap_or(file);
        let is_artifact = LOOPR_ARTIFACTS.iter().any(|a| normalized.starts_with(a));
        if is_artifact {
            out_of_scope.push(normalized.to_string());
            continue;
        }
        // Empty files = legacy mode: all non-artifact files are in-scope
        let matches_tag = files.is_empty()
            || files
                .iter()
                .any(|tag| normalized == tag.strip_prefix("./").unwrap_or(tag));
        if matches_tag {
            in_scope.push(normalized.to_string());
        } else {
            out_of_scope.push(normalized.to_string());
        }
    }
    (in_scope, out_of_scope)
}

/// Parse `git status --porcelain` output into a list of file paths.
/// Handles status prefixes (M, A, D, ??, R, C) and quoted paths.
pub fn parse_porcelain_status(output: &str) -> Vec<String> {
    output
        .lines()
        .filter_map(|line| {
            // Format: "XY filename" or "XY orig -> renamed"
            let rest = line.get(3..)?;
            // For renames/copies, take the destination (after " -> ")
            let path = rest.split(" -> ").last().unwrap_or(rest);
            Some(path.trim_matches('"').to_string())
        })
        .collect()
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    // --- partition_by_scope tests ---

    #[test]
    fn test_exact_match_in_scope() {
        let dirty = vec!["todo.lua".to_string(), "cli.lua".to_string()];
        let tags = vec!["todo.lua".to_string(), "cli.lua".to_string()];
        let (in_scope, out) = partition_by_scope(&dirty, &tags);
        assert_eq!(in_scope, vec!["todo.lua", "cli.lua"]);
        assert!(out.is_empty());
    }

    #[test]
    fn test_no_match_out_of_scope() {
        let dirty = vec!["helpers.lua".to_string()];
        let tags = vec!["todo.lua".to_string()];
        let (in_scope, out) = partition_by_scope(&dirty, &tags);
        assert!(in_scope.is_empty());
        assert_eq!(out, vec!["helpers.lua"]);
    }

    #[test]
    fn test_empty_tags_all_non_artifacts_in_scope() {
        let dirty = vec!["todo.lua".to_string(), "helpers.lua".to_string()];
        let tags: Vec<String> = vec![];
        let (in_scope, out) = partition_by_scope(&dirty, &tags);
        assert_eq!(in_scope, vec!["todo.lua", "helpers.lua"]);
        assert!(out.is_empty());
    }

    #[test]
    fn test_empty_files_returns_empty() {
        let dirty: Vec<String> = vec![];
        let tags = vec!["todo.lua".to_string()];
        let (in_scope, out) = partition_by_scope(&dirty, &tags);
        assert!(in_scope.is_empty());
        assert!(out.is_empty());
    }

    #[test]
    fn test_mixed_in_and_out_of_scope() {
        let dirty = vec!["todo.lua".to_string(), "cli.lua".to_string(), "helpers.lua".to_string()];
        let tags = vec!["todo.lua".to_string(), "cli.lua".to_string()];
        let (in_scope, out) = partition_by_scope(&dirty, &tags);
        assert_eq!(in_scope, vec!["todo.lua", "cli.lua"]);
        assert_eq!(out, vec!["helpers.lua"]);
    }

    #[test]
    fn test_leading_dot_slash_normalization() {
        let dirty = vec!["./todo.lua".to_string(), "cli.lua".to_string()];
        let tags = vec!["todo.lua".to_string(), "./cli.lua".to_string()];
        let (in_scope, out) = partition_by_scope(&dirty, &tags);
        assert_eq!(in_scope, vec!["todo.lua", "cli.lua"]);
        assert!(out.is_empty());
    }

    #[test]
    fn test_loopr_artifacts_always_filtered() {
        let dirty = vec![
            "todo.lua".to_string(),
            ".taskstore/db.sqlite".to_string(),
            ".worktrees/wk-1/file.txt".to_string(),
            "loopr.yml".to_string(),
        ];
        let tags = vec!["todo.lua".to_string()];
        let (in_scope, out) = partition_by_scope(&dirty, &tags);
        assert_eq!(in_scope, vec!["todo.lua"]);
        assert_eq!(
            out,
            vec![".taskstore/db.sqlite", ".worktrees/wk-1/file.txt", "loopr.yml"]
        );
    }

    #[test]
    fn test_artifacts_filtered_even_with_empty_tags() {
        let dirty = vec![
            "todo.lua".to_string(),
            ".taskstore/data.jsonl".to_string(),
            "loopr.yml".to_string(),
        ];
        let tags: Vec<String> = vec![];
        let (in_scope, out) = partition_by_scope(&dirty, &tags);
        assert_eq!(in_scope, vec!["todo.lua"]);
        assert_eq!(out, vec![".taskstore/data.jsonl", "loopr.yml"]);
    }

    // --- parse_porcelain_status tests ---

    #[test]
    fn test_parse_modified_files() {
        let output = " M todo.lua\n M cli.lua\n";
        let files = parse_porcelain_status(output);
        assert_eq!(files, vec!["todo.lua", "cli.lua"]);
    }

    #[test]
    fn test_parse_added_files() {
        let output = "A  new_file.lua\n";
        let files = parse_porcelain_status(output);
        assert_eq!(files, vec!["new_file.lua"]);
    }

    #[test]
    fn test_parse_deleted_files() {
        let output = " D old_file.lua\n";
        let files = parse_porcelain_status(output);
        assert_eq!(files, vec!["old_file.lua"]);
    }

    #[test]
    fn test_parse_untracked_files() {
        let output = "?? untracked.lua\n";
        let files = parse_porcelain_status(output);
        assert_eq!(files, vec!["untracked.lua"]);
    }

    #[test]
    fn test_parse_renamed_files() {
        let output = "R  old.lua -> new.lua\n";
        let files = parse_porcelain_status(output);
        assert_eq!(files, vec!["new.lua"]);
    }

    #[test]
    fn test_parse_quoted_paths_with_spaces() {
        let output = " M \"path with spaces/file.lua\"\n";
        let files = parse_porcelain_status(output);
        assert_eq!(files, vec!["path with spaces/file.lua"]);
    }

    #[test]
    fn test_parse_empty_output() {
        let output = "";
        let files = parse_porcelain_status(output);
        assert!(files.is_empty());
    }

    #[test]
    fn test_parse_mixed_statuses() {
        let output = " M modified.lua\nA  added.lua\n D deleted.lua\n?? untracked.lua\nR  old.lua -> renamed.lua\n";
        let files = parse_porcelain_status(output);
        assert_eq!(
            files,
            vec![
                "modified.lua",
                "added.lua",
                "deleted.lua",
                "untracked.lua",
                "renamed.lua"
            ]
        );
    }
}
