use std::fs;

use super::Resources;

// ─── Phase 1: warn on override read failures ─────────────────────────────────

/// Passing a directory path as the "file" argument triggers a non-NotFound IO
/// error from read_to_string (IsADirectory on Linux, PermissionDenied on some
/// platforms). This verifies the ErrorKind guard emits warn and falls through.
#[test]
fn test_load_repo_override_dir_as_file_falls_through_to_embedded() {
    let tmp = tempfile::tempdir().unwrap();
    // Create a directory at the path where a file override would be expected.
    // resources/agents/ already exists as an embedded path; creating a directory
    // named after a known file causes read_to_string to fail with IsADirectory.
    let override_dir = tmp.path().join("resources").join("agents");
    fs::create_dir_all(&override_dir).unwrap();
    // Make a directory *at* the exact file path to force IsADirectory.
    let file_as_dir = tmp.path().join("resources").join("agents").join("implementer.pmt");
    fs::create_dir_all(&file_as_dir).unwrap();

    // Should fall through to the embedded default rather than returning an error.
    let result = Resources::load("agents/implementer.pmt", Some(tmp.path()));
    assert!(
        result.is_ok(),
        "should fall through to embedded when override dir exists as directory"
    );
    let content = result.unwrap();
    assert!(!content.is_empty(), "embedded fallback should return content");
}

#[test]
fn test_load_dir_repo_override_dir_unreadable_falls_through() {
    let tmp = tempfile::tempdir().unwrap();
    // Make the override directory exist but be unreadable (mode 000).
    let override_dir = tmp.path().join("resources").join("engine").join("triggers");
    fs::create_dir_all(&override_dir).unwrap();

    // Only attempt permission manipulation on unix where it works predictably.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&override_dir, fs::Permissions::from_mode(0o000)).unwrap();

        // load_dir should warn and fall through - still returning embedded results.
        let result = Resources::load_dir("engine/triggers/", Some(tmp.path()));

        // Restore permissions so tempdir cleanup doesn't fail.
        fs::set_permissions(&override_dir, fs::Permissions::from_mode(0o755)).unwrap();

        assert!(
            result.is_ok(),
            "should fall through to embedded when override dir is unreadable"
        );
        let files = result.unwrap();
        assert!(!files.is_empty(), "embedded trigger files should still be returned");
    }
}

// ─── Phase 3: load_dir prefix guard in all builds ────────────────────────────

#[test]
fn test_load_dir_prefix_missing_trailing_slash_returns_err() {
    let result = Resources::load_dir("engine/triggers", None);
    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("load_dir prefix must end with '/'"),
        "unexpected error: {}",
        msg
    );
}

#[test]
fn test_load_dir_empty_string_prefix_returns_err() {
    let result = Resources::load_dir("", None);
    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("load_dir prefix must end with '/'"),
        "unexpected error: {}",
        msg
    );
}

// ─── Existing tests ───────────────────────────────────────────────────────────

#[test]
fn test_load_embedded_prompt() {
    let content = Resources::load("agents/implementer.pmt", None).unwrap();
    assert!(!content.is_empty(), "agents/implementer.pmt should not be empty");
}

#[test]
fn test_load_embedded_strategy() {
    let content = Resources::load("engine/fsm/work.yml", None).unwrap();
    assert!(!content.is_empty(), "engine/fsm/work.yml should not be empty");
}

#[test]
fn test_load_missing_returns_error() {
    let result = Resources::load("nonexistent.pmt", None);
    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("resource not found: nonexistent.pmt"),
        "unexpected error: {}",
        msg
    );
}

#[test]
fn test_exists_for_embedded_prompt() {
    assert!(Resources::exists("agents/implementer.pmt", None));
}

#[test]
fn test_exists_for_embedded_strategy() {
    assert!(Resources::exists("engine/fsm/work.yml", None));
}

#[test]
fn test_exists_false_for_missing() {
    assert!(!Resources::exists("nonexistent.pmt", None));
}

#[test]
fn test_load_dir_fsm_returns_all_definitions() {
    let files = Resources::load_dir("engine/fsm/", None).unwrap();
    assert!(!files.is_empty());
    let paths: Vec<&str> = files.iter().map(|(p, _)| p.as_str()).collect();
    assert!(paths.contains(&"engine/fsm/work.yml"), "should contain work.yml");
    assert!(paths.contains(&"engine/fsm/bundle.yml"), "should contain bundle.yml");
    assert!(
        paths.contains(&"engine/fsm/hierarchy.yml"),
        "should contain hierarchy.yml"
    );
    assert!(paths.contains(&"engine/fsm/tick.yml"), "should contain tick.yml");
    assert!(paths.contains(&"engine/fsm/agent.yml"), "should contain agent.yml");
}

#[test]
fn test_load_dir_empty_prefix_returns_error() {
    let result = Resources::load_dir("nonexistent-prefix/", None);
    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(msg.contains("no resources found"), "unexpected error: {}", msg);
}

#[test]
fn test_load_dir_results_sorted() {
    let files = Resources::load_dir("engine/fsm/", None).unwrap();
    let paths: Vec<&str> = files.iter().map(|(p, _)| p.as_str()).collect();
    let mut sorted = paths.clone();
    sorted.sort();
    assert_eq!(paths, sorted, "load_dir results should be sorted by path");
}

#[test]
fn test_load_absolute_path_not_found() {
    let result = Resources::load("/nonexistent/absolute/path.pmt", None);
    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(
        msg.contains("absolute resource path not found"),
        "unexpected error: {}",
        msg
    );
}

#[test]
fn test_load_dir_triggers_returns_all_definitions() {
    let files = Resources::load_dir("engine/triggers/", None).unwrap();
    assert!(!files.is_empty(), "should find embedded trigger files");
    for (path, content) in &files {
        assert!(
            path.starts_with("engine/triggers/"),
            "path should start with engine/triggers/: {}",
            path
        );
        assert!(path.ends_with(".yml"), "trigger files should be .yml: {}", path);
        assert!(!content.is_empty(), "trigger file should not be empty: {}", path);
    }
}

#[test]
fn test_load_dir_engine_strategies_returns_definitions() {
    let files = Resources::load_dir("engine/strategies/", None).unwrap();
    assert!(!files.is_empty(), "should find engine strategy files");
    let paths: Vec<&str> = files.iter().map(|(p, _)| p.as_str()).collect();
    assert!(
        paths.iter().any(|p| p.starts_with("engine/strategies/")),
        "should include engine/strategies/ entries"
    );
}

#[test]
fn test_load_dir_decompose_strategies_returns_definitions() {
    let files = Resources::load_dir("decompose/strategies/", None).unwrap();
    assert!(!files.is_empty(), "should find decompose strategy files");
    let paths: Vec<&str> = files.iter().map(|(p, _)| p.as_str()).collect();
    assert!(
        paths.iter().any(|p| p.starts_with("decompose/strategies/")),
        "should include decompose/strategies/ entries"
    );
}

#[test]
fn test_load_embedded_role_config() {
    let content = Resources::load("decompose/roles/full.yml", None).unwrap();
    assert!(!content.is_empty(), "decompose/roles/full.yml should not be empty");
    let content_brief = Resources::load("decompose/roles/brief.yml", None).unwrap();
    assert!(
        !content_brief.is_empty(),
        "decompose/roles/brief.yml should not be empty"
    );
}
