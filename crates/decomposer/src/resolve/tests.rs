use std::collections::HashMap;

use domain::WorkId;

use super::{normalize, resolve_deps};
use crate::error::DecomposerError;
use crate::tool::DecomposeChild;

// Cycle-detection tests moved to `crates/domain/tests/graph.rs` when
// `detect_cycles` became `domain::WorkGraph::from_edges` (re-keyed to
// WorkId). What remains here is title resolution + `normalize`.

fn child(title: &str, deps: &[&str]) -> DecomposeChild {
    DecomposeChild {
        title: title.to_string(),
        content: String::new(),
        dependencies: deps.iter().map(|s| s.to_string()).collect(),
        acceptance_criteria: Vec::new(),
        files: Vec::new(),
    }
}

#[test]
fn normalize_lowercases_and_trims() {
    assert_eq!(normalize(" Build CLI "), "build cli");
    assert_eq!(normalize("ALREADY_LOWER"), "already_lower");
    assert_eq!(normalize(""), "");
    assert_eq!(normalize("   "), "");
}

#[test]
fn resolve_deps_empty_children_returns_empty() {
    let title_to_id: HashMap<String, WorkId> = HashMap::new();
    let resolved = resolve_deps(&[], &title_to_id).expect("empty");
    assert!(resolved.is_empty());
}

#[test]
fn resolve_deps_no_deps_returns_empty_vecs() {
    let children = vec![child("a", &[]), child("b", &[])];
    let mut title_to_id: HashMap<String, WorkId> = HashMap::new();
    title_to_id.insert("a".to_string(), WorkId::new());
    title_to_id.insert("b".to_string(), WorkId::new());

    let resolved = resolve_deps(&children, &title_to_id).expect("resolve");
    assert_eq!(resolved.len(), 2);
    assert!(resolved[0].is_empty());
    assert!(resolved[1].is_empty());
}

#[test]
fn resolve_deps_exact_match_resolves() {
    let children = vec![child("A", &["B"]), child("B", &[])];
    let mut title_to_id: HashMap<String, WorkId> = HashMap::new();
    let a_id = WorkId::new();
    let b_id = WorkId::new();
    title_to_id.insert("a".to_string(), a_id.clone());
    title_to_id.insert("b".to_string(), b_id.clone());

    let resolved = resolve_deps(&children, &title_to_id).expect("resolve");
    assert_eq!(resolved.len(), 2);
    assert_eq!(resolved[0], vec![b_id]);
    assert!(resolved[1].is_empty());
}

#[test]
fn resolve_deps_case_insensitive_match_resolves() {
    // child titles use "Build CLI"; title_to_id is keyed by "build cli"
    // (the normalized form). Resolution normalizes each dep before lookup.
    let children = vec![child("Tests", &[" build cli "]), child("Build CLI", &[])];
    let mut title_to_id: HashMap<String, WorkId> = HashMap::new();
    let cli_id = WorkId::new();
    title_to_id.insert("build cli".to_string(), cli_id.clone());
    title_to_id.insert("tests".to_string(), WorkId::new());

    let resolved = resolve_deps(&children, &title_to_id).expect("resolve");
    assert_eq!(resolved.len(), 2);
    assert_eq!(resolved[0], vec![cli_id]);
    assert!(resolved[1].is_empty());
}

#[test]
fn resolve_deps_unresolved_title_errors() {
    let children = vec![child("A", &["NotThere"])];
    let mut title_to_id: HashMap<String, WorkId> = HashMap::new();
    title_to_id.insert("a".to_string(), WorkId::new());

    let err = resolve_deps(&children, &title_to_id).expect_err("unresolved");
    match err {
        DecomposerError::UnresolvedDeps(msg) => {
            assert!(msg.contains("NotThere"), "got: {msg}");
            assert!(msg.contains("'A'"), "got: {msg}");
        }
        other => panic!("expected UnresolvedDeps, got {other:?}"),
    }
}

#[test]
fn resolve_deps_collects_all_unresolved_names() {
    let children = vec![child("A", &["X", "Y"]), child("B", &["Z"])];
    let mut title_to_id: HashMap<String, WorkId> = HashMap::new();
    title_to_id.insert("a".to_string(), WorkId::new());
    title_to_id.insert("b".to_string(), WorkId::new());

    let err = resolve_deps(&children, &title_to_id).expect_err("unresolved");
    match err {
        DecomposerError::UnresolvedDeps(msg) => {
            assert!(msg.contains('X'), "got: {msg}");
            assert!(msg.contains('Y'), "got: {msg}");
            assert!(msg.contains('Z'), "got: {msg}");
        }
        other => panic!("expected UnresolvedDeps, got {other:?}"),
    }
}
