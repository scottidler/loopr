//! Integration tests for `domain::WorkGraph`. Domain's convention is
//! one `tests/<module>.rs` per module (see `tests/work.rs`), exercising
//! the public API only.

use std::collections::HashSet;

use domain::{GraphError, PlanId, Work, WorkGraph, WorkId, WorkStatus};

/// Build a Work with a chosen id, deps, and status. `id` is set directly
/// (the field is public) so tests can wire concrete dependency edges.
fn work(id: &WorkId, deps: Vec<WorkId>, status: WorkStatus) -> Work {
    let mut w = Work::new(PlanId::new(), "t".to_string());
    w.id = id.clone();
    w.dependencies = deps;
    w.status = status;
    w
}

fn done(ids: &[&WorkId]) -> HashSet<WorkId> {
    ids.iter().map(|id| (*id).clone()).collect()
}

// from_edges: cycle rejection

#[test]
fn from_edges_acyclic_ok() {
    let (a, b, c) = (WorkId::new(), WorkId::new(), WorkId::new());
    // c -> b -> a (c depends on b, b depends on a)
    let graph = WorkGraph::from_edges([
        (a.clone(), vec![]),
        (b.clone(), vec![a.clone()]),
        (c.clone(), vec![b.clone()]),
    ]);
    assert!(graph.is_ok());
}

#[test]
fn from_edges_self_loop_is_cycle() {
    let a = WorkId::new();
    let err = WorkGraph::from_edges([(a.clone(), vec![a.clone()])]).unwrap_err();
    let GraphError::Cycle(ids) = err;
    assert_eq!(ids, vec![a]);
}

#[test]
fn from_edges_two_node_cycle_names_both() {
    let (a, b) = (WorkId::new(), WorkId::new());
    let err = WorkGraph::from_edges([(a.clone(), vec![b.clone()]), (b.clone(), vec![a.clone()])]).unwrap_err();
    let GraphError::Cycle(ids) = err;
    let got: HashSet<WorkId> = ids.into_iter().collect();
    assert_eq!(got, done(&[&a, &b]));
}

#[test]
fn cycle_display_joins_ids() {
    let (a, b) = (WorkId::new(), WorkId::new());
    let err = WorkGraph::from_edges([(a.clone(), vec![b.clone()]), (b.clone(), vec![a.clone()])]).unwrap_err();
    let msg = err.to_string();
    assert!(msg.starts_with("dependency cycle among: "));
    assert!(msg.contains(a.as_ref()));
    assert!(msg.contains(b.as_ref()));
}

// ready_set

#[test]
fn ready_set_no_deps_always_ready() {
    let (a, b) = (WorkId::new(), WorkId::new());
    let graph = WorkGraph::from_edges([(a.clone(), vec![]), (b.clone(), vec![])]).unwrap();
    let ready: HashSet<WorkId> = graph.ready_set(&HashSet::new()).into_iter().collect();
    assert_eq!(ready, done(&[&a, &b]));
}

#[test]
fn ready_set_empty_done_holds_dependent() {
    let (a, b) = (WorkId::new(), WorkId::new());
    // b depends on a; with nothing done, only a is ready.
    let graph = WorkGraph::from_edges([(a.clone(), vec![]), (b.clone(), vec![a.clone()])]).unwrap();
    let ready: Vec<WorkId> = graph.ready_set(&HashSet::new());
    assert_eq!(ready, vec![a]);
}

#[test]
fn ready_set_partial_done_promotes_dependent() {
    let (a, b) = (WorkId::new(), WorkId::new());
    let graph = WorkGraph::from_edges([(a.clone(), vec![]), (b.clone(), vec![a.clone()])]).unwrap();
    // a is done -> b becomes ready; a itself is excluded (finished).
    let ready: Vec<WorkId> = graph.ready_set(&done(&[&a]));
    assert_eq!(ready, vec![b]);
}

#[test]
fn ready_set_excludes_nodes_already_done() {
    let (a, b) = (WorkId::new(), WorkId::new());
    let graph = WorkGraph::from_edges([(a.clone(), vec![]), (b.clone(), vec![a.clone()])]).unwrap();
    // both done -> nothing ready.
    assert!(graph.ready_set(&done(&[&a, &b])).is_empty());
}

#[test]
fn ready_set_unknown_dep_never_ready() {
    let (a, phantom) = (WorkId::new(), WorkId::new());
    // a depends on a node not present in the graph; it can never be ready
    // because the phantom id can never appear in `done`.
    let graph = WorkGraph::from_works(&[work(&a, vec![phantom], WorkStatus::Pending)]);
    assert!(graph.ready_set(&HashSet::new()).is_empty());
}

// dependents_of

#[test]
fn dependents_of_direct_edges() {
    let (a, b, c) = (WorkId::new(), WorkId::new(), WorkId::new());
    // b and c both depend on a.
    let graph = WorkGraph::from_edges([
        (a.clone(), vec![]),
        (b.clone(), vec![a.clone()]),
        (c.clone(), vec![a.clone()]),
    ])
    .unwrap();
    let deps: HashSet<WorkId> = graph.dependents_of(&a).iter().cloned().collect();
    assert_eq!(deps, done(&[&b, &c]));
}

#[test]
fn dependents_of_no_dependents_is_empty() {
    let (a, b) = (WorkId::new(), WorkId::new());
    let graph = WorkGraph::from_edges([(a.clone(), vec![]), (b.clone(), vec![a.clone()])]).unwrap();
    // b is a leaf; nothing depends on it.
    assert!(graph.dependents_of(&b).is_empty());
}

#[test]
fn duplicate_dep_edge_is_deduped() {
    // Phase 3 F11: a node listing the same dependency twice must not
    // plant a duplicate reverse edge (which would make
    // block_dependent_siblings process the dependent twice) nor wedge
    // cycle detection on a non-cycle.
    let (a, b) = (WorkId::new(), WorkId::new());
    // b depends on a twice.
    let graph = WorkGraph::from_works(&[
        work(&a, vec![], WorkStatus::Done),
        work(&b, vec![a.clone(), a.clone()], WorkStatus::Pending),
    ]);
    assert_eq!(graph.dependents_of(&a), std::slice::from_ref(&b), "reverse edge deduped");
    // The duplicate edge must not be mistaken for a cycle.
    WorkGraph::from_edges([(a.clone(), vec![]), (b.clone(), vec![a.clone(), a.clone()])])
        .expect("duplicate dep edge is not a cycle");
}

#[test]
fn dependents_of_phantom_node_tracked() {
    let (a, phantom) = (WorkId::new(), WorkId::new());
    // a depends on a phantom (absent) node; the reverse edge is still
    // tracked, so dependents_of(phantom) returns a.
    let graph = WorkGraph::from_works(&[work(&a, vec![phantom.clone()], WorkStatus::Pending)]);
    assert_eq!(graph.dependents_of(&phantom), &[a]);
}

// from_works specifically

#[test]
fn from_works_reads_dependencies_field() {
    let (a, b) = (WorkId::new(), WorkId::new());
    let works = vec![
        work(&a, vec![], WorkStatus::Done),
        work(&b, vec![a.clone()], WorkStatus::Pending),
    ];
    let graph = WorkGraph::from_works(&works);
    // a Done -> b ready.
    assert_eq!(graph.ready_set(&done(&[&a])), vec![b.clone()]);
    assert_eq!(graph.dependents_of(&a), &[b]);
}
