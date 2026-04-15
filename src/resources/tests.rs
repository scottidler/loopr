use super::Resources;

#[test]
fn test_load_embedded_prompt() {
    let content = Resources::load("agents/coordinator.pmt", None).unwrap();
    assert!(!content.is_empty(), "agents/coordinator.pmt should not be empty");
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
    assert!(Resources::exists("agents/coordinator.pmt", None));
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
