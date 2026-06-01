//! The child-dependency DAG of a single Plan's Works.
//!
//! `WorkGraph` is the single typed owner of "the edges": forward edges
//! (a Work -> the Works it depends on) and reverse edges (a Work -> the
//! Works that depend on it). It replaces four hand-rolled scan sites
//! (see `docs/design/2026-05-31-workgraph-consolidation.md`).
//!
//! Status is intentionally NOT stored on the graph; it is supplied at
//! query time (`ready_set`'s `done` set) so the graph stays a pure
//! topology object and can be rebuilt cheaply from a freshly listed
//! sibling slice.
//!
//! Fallibility is split on purpose: `from_works` is infallible because
//! cycle-freedom is a decompose-time invariant (the decomposer rejects
//! cycles before persisting, and `Work.dependencies` is never mutated
//! afterward), so runtime construction trusts it. `from_edges` is the
//! one place cycles are rejected, once, before the Works are persisted.

use std::collections::{HashMap, HashSet};

use crate::{Work, WorkId};

/// Construction error for `WorkGraph::from_edges`.
///
/// Hand-rolled `Display` + `std::error::Error` to match `domain`'s
/// existing `FsmError` style (`crate::fsm`); `domain` has no `thiserror`
/// dependency and this type does not add one.
#[derive(Debug)]
pub enum GraphError {
    /// The dependency edges form a cycle. Carries the typed node ids
    /// participating in the cycle (Kahn's leftover, nonzero-in-degree
    /// set) so callers can map them programmatically (e.g. id -> title)
    /// without re-parsing a string. `Display` renders the comma-joined
    /// ids for log / wire use.
    Cycle(Vec<WorkId>),
}

impl std::fmt::Display for GraphError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            GraphError::Cycle(ids) => {
                let joined = ids.iter().map(|id| id.to_string()).collect::<Vec<_>>().join(", ");
                write!(f, "dependency cycle among: {joined}")
            }
        }
    }
}

impl std::error::Error for GraphError {}

/// Topological view of a Plan's child Works. See module docs.
#[derive(Debug)]
pub struct WorkGraph {
    /// node -> its dependency ids (forward edges).
    deps: HashMap<WorkId, Vec<WorkId>>,
    /// node -> ids that depend on it (reverse edges).
    dependents: HashMap<WorkId, Vec<WorkId>>,
}

impl WorkGraph {
    /// Build from persisted Works via each Work's `dependencies` field.
    /// Infallible (see module docs): does not re-check cycles. Dep ids
    /// referencing Works absent from `works` are retained as forward
    /// edges; they simply never become satisfiable because they can't
    /// appear in `done`.
    pub fn from_works(works: &[Work]) -> Self {
        let edges = works.iter().map(|w| (w.id.clone(), w.dependencies.clone()));
        Self::build(edges)
    }

    /// Build from explicit `(node, deps)` edges, for decompose-time use
    /// where Works are not yet persisted but WorkIds are pre-minted.
    /// Rejects cycles via the ported Kahn's algorithm, returning the
    /// offending node ids in `GraphError::Cycle`.
    pub fn from_edges(edges: impl IntoIterator<Item = (WorkId, Vec<WorkId>)>) -> Result<Self, GraphError> {
        let graph = Self::build(edges);
        graph.detect_cycle()?;
        Ok(graph)
    }

    /// Shared topology builder: populates both the forward (`deps`) and
    /// reverse (`dependents`) adjacency maps from one pass over `edges`.
    fn build(edges: impl IntoIterator<Item = (WorkId, Vec<WorkId>)>) -> Self {
        let mut deps: HashMap<WorkId, Vec<WorkId>> = HashMap::new();
        let mut dependents: HashMap<WorkId, Vec<WorkId>> = HashMap::new();
        for (node, node_deps) in edges {
            for dep in &node_deps {
                dependents.entry(dep.clone()).or_default().push(node.clone());
            }
            deps.insert(node, node_deps);
        }
        Self { deps, dependents }
    }

    /// Every node whose dependency ids are all in `done` and which is
    /// not itself in `done`. A no-dependency node is ready (empty subset
    /// of `done`); a node already in `done` is finished, not ready, so it
    /// is excluded. Returns nodes regardless of their non-done status;
    /// callers intersect with the status they care about (e.g.
    /// `Pending`). Excluding `done` keeps the result to live candidates
    /// rather than re-returning historical works the caller would filter.
    pub fn ready_set(&self, done: &HashSet<WorkId>) -> Vec<WorkId> {
        self.deps
            .iter()
            .filter(|(node, _)| !done.contains(*node))
            .filter(|(_, node_deps)| node_deps.iter().all(|dep| done.contains(dep)))
            .map(|(node, _)| node.clone())
            .collect()
    }

    /// Direct reverse edges: the nodes that list `node` in their deps.
    /// Returns `&[]` for a node with no dependents. A node absent from
    /// the constructing `&[Work]` but named as a dependency by a present
    /// node still has reverse edges tracked, so this may return
    /// dependents for such a "phantom" node.
    pub fn dependents_of(&self, node: &WorkId) -> &[WorkId] {
        self.dependents.get(node).map(Vec::as_slice).unwrap_or(&[])
    }

    /// Kahn's-algorithm cycle detection over the forward edges, ported
    /// from the decomposer's former `cycles::detect_cycles` and re-keyed
    /// from `&str` titles to `WorkId`. On a cycle, returns the set of
    /// nodes with nonzero residual in-degree (those that never reach the
    /// queue). Self-loops are caught: a node depending on itself starts
    /// with in-degree >= 1 and never drains.
    #[tracing::instrument(level = "debug", skip_all, fields(node_count = self.deps.len()), err)]
    fn detect_cycle(&self) -> Result<(), GraphError> {
        let mut in_degree: HashMap<&WorkId, usize> = HashMap::new();
        for node in self.deps.keys() {
            in_degree.entry(node).or_insert(0);
        }
        for node_deps in self.deps.values() {
            for dep in node_deps {
                if let Some(deg) = in_degree.get_mut(dep) {
                    *deg += 1;
                }
            }
        }

        let mut queue: Vec<&WorkId> = in_degree
            .iter()
            .filter(|(_, deg)| **deg == 0)
            .map(|(node, _)| *node)
            .collect();
        let mut visited = 0usize;

        while let Some(node) = queue.pop() {
            visited += 1;
            if let Some(node_deps) = self.deps.get(node) {
                for dep in node_deps {
                    if let Some(deg) = in_degree.get_mut(dep) {
                        *deg -= 1;
                        if *deg == 0 {
                            queue.push(dep);
                        }
                    }
                }
            }
        }

        if visited < self.deps.len() {
            let cycled: Vec<WorkId> = in_degree
                .iter()
                .filter(|(_, deg)| **deg > 0)
                .map(|(node, _)| (*node).clone())
                .collect();
            return Err(GraphError::Cycle(cycled));
        }
        tracing::debug!(node_count = self.deps.len(), "domain: WorkGraph detect_cycle ok");
        Ok(())
    }
}
