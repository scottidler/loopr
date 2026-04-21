use std::collections::HashMap;

use domain::WorkId;

use super::{detect_cycles, normalize, resolve_deps};
use crate::error::DecomposerError;
use crate::tool::DecomposeChild;

fn node(title: &str, deps: &[&str]) -> (String, Vec<String>) {
    (title.to_string(), deps.iter().map(|s| s.to_string()).collect())
}

fn child(title: &str, deps: &[&str]) -> DecomposeChild {
    DecomposeChild {
        title: title.to_string(),
        content: String::new(),
        dependencies: deps.iter().map(|s| s.to_string()).collect(),
        acceptance_criteria: Vec::new(),
    }
}

#[test]
fn detect_cycles_acyclic_three_node_dag_ok() {
    let mut g: HashMap<String, Vec<String>> = HashMap::new();
    g.extend([node("a", &["b"]), node("b", &["c"]), node("c", &[])]);
    detect_cycles(&g).expect("acyclic");
}

#[test]
fn detect_cycles_all_independent_nodes_ok() {
    let mut g: HashMap<String, Vec<String>> = HashMap::new();
    g.extend([node("a", &[]), node("b", &[]), node("c", &[])]);
    detect_cycles(&g).expect("independent");
}

#[test]
fn detect_cycles_trivial_two_cycle_errors() {
    let mut g: HashMap<String, Vec<String>> = HashMap::new();
    g.extend([node("a", &["b"]), node("b", &["a"])]);
    let err = detect_cycles(&g).expect_err("cycle");
    assert!(err.contains("a") && err.contains("b"), "got: {err}");
}

#[test]
fn detect_cycles_self_loop_errors() {
    let mut g: HashMap<String, Vec<String>> = HashMap::new();
    g.extend([node("a", &["a"])]);
    let err = detect_cycles(&g).expect_err("self-loop");
    assert!(err.contains("a"), "got: {err}");
}

#[test]
fn detect_cycles_diamond_ok() {
    // a -> b, a -> c, b -> d, c -> d : DAG, no cycle
    let mut g: HashMap<String, Vec<String>> = HashMap::new();
    g.extend([
        node("a", &["b", "c"]),
        node("b", &["d"]),
        node("c", &["d"]),
        node("d", &[]),
    ]);
    detect_cycles(&g).expect("diamond is acyclic");
}

#[test]
fn detect_cycles_ignores_deps_pointing_outside_graph() {
    // An unresolved dep name should not affect cycle detection; that's
    // `resolve_deps`' job. We pass known-good normalized names here.
    let mut g: HashMap<String, Vec<String>> = HashMap::new();
    g.extend([node("a", &["nonexistent"])]);
    detect_cycles(&g).expect("unknown dep target is not a cycle");
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
