use super::Resources;

#[test]
fn test_load_embedded_prompt() {
    let content = Resources::load("coordinator.pmt", None).unwrap();
    assert!(!content.is_empty(), "coordinator.pmt should not be empty");
}

#[test]
fn test_load_embedded_strategy() {
    let content = Resources::load("fsm/work.yml", None).unwrap();
    assert!(!content.is_empty(), "fsm/work.yml should not be empty");
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
    assert!(Resources::exists("coordinator.pmt", None));
}

#[test]
fn test_exists_for_embedded_strategy() {
    assert!(Resources::exists("fsm/work.yml", None));
}

#[test]
fn test_exists_false_for_missing() {
    assert!(!Resources::exists("nonexistent.pmt", None));
}

#[test]
fn test_load_dir_fsm_returns_all_definitions() {
    let files = Resources::load_dir("fsm/", None).unwrap();
    assert!(!files.is_empty());
    let paths: Vec<&str> = files.iter().map(|(p, _)| p.as_str()).collect();
    assert!(paths.contains(&"fsm/work.yml"), "should contain work.yml");
    assert!(paths.contains(&"fsm/bundle.yml"), "should contain bundle.yml");
    assert!(paths.contains(&"fsm/hierarchy.yml"), "should contain hierarchy.yml");
    assert!(paths.contains(&"fsm/tick.yml"), "should contain tick.yml");
    assert!(paths.contains(&"fsm/agent.yml"), "should contain agent.yml");
}

#[test]
fn test_load_dir_empty_prefix_returns_error() {
    let result = Resources::load_dir("nonexistent-prefix/", None);
    assert!(result.is_err());
    let msg = result.unwrap_err().to_string();
    assert!(msg.contains("no embedded resources found"), "unexpected error: {}", msg);
}

#[test]
fn test_load_dir_results_sorted() {
    let files = Resources::load_dir("fsm/", None).unwrap();
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
    let files = Resources::load_dir("triggers/", None).unwrap();
    assert!(!files.is_empty(), "should find embedded trigger files");
    for (path, content) in &files {
        assert!(
            path.starts_with("triggers/"),
            "path should start with triggers/: {}",
            path
        );
        assert!(path.ends_with(".yml"), "trigger files should be .yml: {}", path);
        assert!(!content.is_empty(), "trigger file should not be empty: {}", path);
    }
}

#[test]
fn test_load_dir_excluding_skips_fsm_triggers_roles() {
    let files = Resources::load_dir_excluding(&["fsm/", "triggers/", "roles/"], None).unwrap();
    assert!(!files.is_empty(), "should find strategy files after exclusions");
    for (path, _) in &files {
        assert!(!path.starts_with("fsm/"), "should not contain fsm/ entries: {}", path);
        assert!(
            !path.starts_with("triggers/"),
            "should not contain triggers/ entries: {}",
            path
        );
        assert!(
            !path.starts_with("roles/"),
            "should not contain roles/ entries: {}",
            path
        );
    }
}

#[test]
fn test_load_dir_excluding_returns_strategy_subdirs() {
    let files = Resources::load_dir_excluding(&["fsm/", "triggers/", "roles/"], None).unwrap();
    let paths: Vec<&str> = files.iter().map(|(p, _)| p.as_str()).collect();
    assert!(
        paths.iter().any(|p| p.starts_with("decomposition/")),
        "should include decomposition/ strategies"
    );
    assert!(
        paths.iter().any(|p| p.starts_with("recovery/")),
        "should include recovery/ strategies"
    );
}

#[test]
fn test_load_embedded_role_config() {
    let content = Resources::load("roles/decomposer.yml", None).unwrap();
    assert!(!content.is_empty(), "decomposer.yml should not be empty");
    let content_brief = Resources::load("roles/decomposer-brief.yml", None).unwrap();
    assert!(!content_brief.is_empty(), "decomposer-brief.yml should not be empty");
}
