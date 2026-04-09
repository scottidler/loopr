use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap, HashSet};

/// Sort `items` in topological order by their dependencies, with `get_created_at` as
/// tie-breaker when multiple items have in-degree 0.
///
/// Uses Kahn's algorithm (BFS topological sort). Disconnected DAGs are handled
/// natively - no special case is needed. Deps pointing to IDs not present in `items`
/// (already-completed siblings outside the active slice) are ignored.
///
/// Falls back to `created_at` ordering ONLY on cycle detection (when
/// `visited.len() != items.len()`). Logs a warning when fallback triggers.
///
/// # Type Parameters
/// - `T`: item type
/// - `F`: `fn(&T) -> &str` - returns the item's ID
/// - `G`: `fn(&T) -> &[String]` - returns the item's dep IDs
/// - `C`: `fn(&T) -> i64` - returns the item's `created_at` timestamp
pub fn topo_sort_by_deps<T, F, G, C>(items: &[T], get_id: F, get_deps: G, get_created_at: C) -> Vec<&T>
where
    F: Fn(&T) -> &str,
    G: Fn(&T) -> &[String],
    C: Fn(&T) -> i64,
{
    if items.is_empty() {
        return vec![];
    }

    // Build index: id -> (item_index, in_degree)
    let id_to_idx: HashMap<&str, usize> = items.iter().enumerate().map(|(i, t)| (get_id(t), i)).collect();

    // Compute in-degrees: only count edges whose source is in the active set.
    let mut in_degree: Vec<usize> = vec![0; items.len()];
    let mut reverse_adj: Vec<Vec<usize>> = vec![vec![]; items.len()];

    for (idx, item) in items.iter().enumerate() {
        for dep_id in get_deps(item) {
            if let Some(&dep_idx) = id_to_idx.get(dep_id.as_str()) {
                in_degree[idx] += 1;
                reverse_adj[dep_idx].push(idx);
            }
            // deps pointing outside the active set are ignored
        }
    }

    // Min-heap keyed by (created_at, index) for deterministic tie-breaking.
    // Items with lower created_at are processed first among equal in-degree-0 nodes.
    let mut queue: BinaryHeap<Reverse<(i64, usize)>> = in_degree
        .iter()
        .enumerate()
        .filter(|&(_, &d)| d == 0)
        .map(|(i, _)| Reverse((get_created_at(&items[i]), i)))
        .collect();

    let mut result: Vec<&T> = Vec::with_capacity(items.len());
    let mut visited: HashSet<usize> = HashSet::new();

    while let Some(Reverse((_, idx))) = queue.pop() {
        if !visited.insert(idx) {
            continue;
        }
        result.push(&items[idx]);
        // Decrement in-degree of downstream nodes.
        for &next_idx in &reverse_adj[idx] {
            if in_degree[next_idx] > 0 {
                in_degree[next_idx] -= 1;
            }
            if in_degree[next_idx] == 0 && !visited.contains(&next_idx) {
                queue.push(Reverse((get_created_at(&items[next_idx]), next_idx)));
            }
        }
    }

    if visited.len() != items.len() {
        // Cycle detected - fall back to created_at ordering.
        tracing::warn!(
            "topo_sort_by_deps: cycle detected among {} items, falling back to created_at order",
            items.len()
        );
        let mut fallback: Vec<&T> = items.iter().collect();
        fallback.sort_by_key(|t| get_created_at(t));
        return fallback;
    }

    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug)]
    struct Node {
        id: String,
        deps: Vec<String>,
        created_at: i64,
    }

    impl Node {
        fn new(id: &str, deps: Vec<&str>, ts: i64) -> Self {
            Self {
                id: id.to_string(),
                deps: deps.into_iter().map(String::from).collect(),
                created_at: ts,
            }
        }
    }

    fn sort(nodes: &[Node]) -> Vec<&Node> {
        topo_sort_by_deps(nodes, |n| &n.id, |n| &n.deps, |n| n.created_at)
    }

    #[test]
    fn test_empty() {
        let nodes: Vec<Node> = vec![];
        assert!(sort(&nodes).is_empty());
    }

    #[test]
    fn test_single_item() {
        let nodes = vec![Node::new("a", vec![], 1)];
        let sorted = sort(&nodes);
        assert_eq!(sorted.len(), 1);
        assert_eq!(sorted[0].id, "a");
    }

    #[test]
    fn test_linked_list_a_b_c() {
        // A has no deps, B depends on A, C depends on B -> order A, B, C
        let nodes = vec![
            Node::new("c", vec!["b"], 3),
            Node::new("b", vec!["a"], 2),
            Node::new("a", vec![], 1),
        ];
        let sorted = sort(&nodes);
        let ids: Vec<&str> = sorted.iter().map(|n| n.id.as_str()).collect();
        // A must come before B, B before C
        let pos: HashMap<&str, usize> = ids.iter().enumerate().map(|(i, &s)| (s, i)).collect();
        assert!(pos["a"] < pos["b"], "A must be before B");
        assert!(pos["b"] < pos["c"], "B must be before C");
    }

    #[test]
    fn test_general_dag() {
        // A, B have no deps; C depends on A and B; D depends on C
        let nodes = vec![
            Node::new("d", vec!["c"], 4),
            Node::new("c", vec!["a", "b"], 3),
            Node::new("b", vec![], 2),
            Node::new("a", vec![], 1),
        ];
        let sorted = sort(&nodes);
        let ids: Vec<&str> = sorted.iter().map(|n| n.id.as_str()).collect();
        let pos: HashMap<&str, usize> = ids.iter().enumerate().map(|(i, &s)| (s, i)).collect();
        assert!(pos["a"] < pos["c"]);
        assert!(pos["b"] < pos["c"]);
        assert!(pos["c"] < pos["d"]);
    }

    #[test]
    fn test_disconnected_components() {
        // Two independent chains: A->B and X->Y
        let nodes = vec![
            Node::new("b", vec!["a"], 2),
            Node::new("a", vec![], 1),
            Node::new("y", vec!["x"], 4),
            Node::new("x", vec![], 3),
        ];
        let sorted = sort(&nodes);
        assert_eq!(sorted.len(), 4);
        let ids: Vec<&str> = sorted.iter().map(|n| n.id.as_str()).collect();
        let pos: HashMap<&str, usize> = ids.iter().enumerate().map(|(i, &s)| (s, i)).collect();
        // Within each component, ordering is preserved
        assert!(pos["a"] < pos["b"]);
        assert!(pos["x"] < pos["y"]);
        // All 4 nodes present
        assert!(pos.contains_key("a") && pos.contains_key("b"));
        assert!(pos.contains_key("x") && pos.contains_key("y"));
    }

    #[test]
    fn test_cycle_falls_back_to_created_at() {
        // A depends on B, B depends on A - cycle
        let nodes = vec![Node::new("a", vec!["b"], 1), Node::new("b", vec!["a"], 2)];
        let sorted = sort(&nodes);
        // Fallback: sorted by created_at ascending
        assert_eq!(sorted.len(), 2);
        assert_eq!(sorted[0].id, "a"); // created_at=1
        assert_eq!(sorted[1].id, "b"); // created_at=2
    }

    #[test]
    fn test_tie_breaking_by_created_at() {
        // A, B, C all have no deps - tie-break by created_at
        let nodes = vec![
            Node::new("c", vec![], 3),
            Node::new("a", vec![], 1),
            Node::new("b", vec![], 2),
        ];
        let sorted = sort(&nodes);
        let ids: Vec<&str> = sorted.iter().map(|n| n.id.as_str()).collect();
        assert_eq!(ids, vec!["a", "b", "c"]);
    }

    #[test]
    fn test_external_dep_ignored() {
        // A depends on "z" which is not in the active set - dep should be ignored
        let nodes = vec![Node::new("a", vec!["z"], 1)];
        let sorted = sort(&nodes);
        assert_eq!(sorted.len(), 1);
        assert_eq!(sorted[0].id, "a");
    }
}
