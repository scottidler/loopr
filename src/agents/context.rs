use std::cmp::Ordering;
use std::collections::HashMap;

use crate::domain::learning::{Learning, LearningScope};
use crate::domain::role::Role;

/// Select learnings relevant to the given scope chain and role.
///
/// Filters by:
/// 1. **Scope**: matches any `(source_id, scope)` pair in `scope_ids`, or `LearningScope::Global`
/// 2. **Role**: `applicable_roles` contains `role`, or `None` (applies to all roles)
/// 3. **Confidence**: promoted learnings (policies) are always included; others must meet `min_confidence`
///
/// Results are sorted: promoted first, then by confidence DESC, then by recency DESC.
/// Truncated to `max_count` items.
///
/// MVP4 note: age-based decay is designed but deferred to MVP5.
pub fn select_learnings<'a>(
    learnings: &'a HashMap<String, Learning>,
    scope_ids: &[(&str, LearningScope)],
    role: Role,
    min_confidence: f32,
    max_count: usize,
) -> Vec<&'a Learning> {
    let mut candidates: Vec<&Learning> = learnings
        .values()
        .filter(|l| {
            // Scope match: this item or any ancestor, or Global
            scope_ids
                .iter()
                .any(|(id, scope)| l.source_id == *id && l.scope == *scope)
                || l.scope == LearningScope::Global
        })
        .filter(|l| {
            // Role match: applicable to this role, or applicable to all (None)
            l.applicable_roles
                .as_ref()
                .map(|roles| roles.contains(&role))
                .unwrap_or(true)
        })
        .filter(|l| {
            // Confidence match: promoted (policies) always included, others need min_confidence
            // MVP4: no age decay — effective_confidence == l.confidence
            l.promoted || l.confidence >= min_confidence
        })
        .collect();

    // Sort: promoted first, then confidence DESC, then recency DESC
    candidates.sort_by(|a, b| {
        b.promoted
            .cmp(&a.promoted)
            .then(
                b.confidence
                    .partial_cmp(&a.confidence)
                    .unwrap_or(Ordering::Equal),
            )
            .then(b.updated_at.cmp(&a.updated_at))
    });

    candidates.truncate(max_count);
    candidates
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PromotionPolicy;
    use crate::domain::learning::Learning;

    fn make_learning(source_id: &str, scope: LearningScope, content: &str) -> Learning {
        Learning::new(source_id.to_string(), scope, content.to_string())
    }

    fn make_learning_with_role(
        source_id: &str,
        scope: LearningScope,
        content: &str,
        roles: Vec<Role>,
    ) -> Learning {
        let mut l = make_learning(source_id, scope, content);
        l.applicable_roles = Some(roles);
        l
    }

    fn make_learning_with_confidence(
        source_id: &str,
        scope: LearningScope,
        content: &str,
        confidence: f32,
    ) -> Learning {
        let mut l = make_learning(source_id, scope, content);
        l.confidence = confidence;
        l
    }

    fn to_map(learnings: Vec<Learning>) -> HashMap<String, Learning> {
        learnings.into_iter().map(|l| (l.id.clone(), l)).collect()
    }

    // --- Basic scope filtering ---

    #[test]
    fn test_select_by_scope_workitem() {
        let l = make_learning("wi-1", LearningScope::WorkItem, "insight");
        let map = to_map(vec![l]);
        let scope_ids = [("wi-1", LearningScope::WorkItem)];

        let result = select_learnings(&map, &scope_ids, Role::Implementer, 0.0, 20);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].content, "insight");
    }

    #[test]
    fn test_select_by_scope_chain() {
        let l1 = make_learning("wi-1", LearningScope::WorkItem, "wi insight");
        let l2 = make_learning("phase-1", LearningScope::Phase, "phase insight");
        let l3 = make_learning("spec-1", LearningScope::Spec, "spec insight");
        let l4 = make_learning("plan-1", LearningScope::Plan, "plan insight");
        let map = to_map(vec![l1, l2, l3, l4]);

        let scope_ids = [
            ("wi-1", LearningScope::WorkItem),
            ("phase-1", LearningScope::Phase),
            ("spec-1", LearningScope::Spec),
            ("plan-1", LearningScope::Plan),
        ];

        let result = select_learnings(&map, &scope_ids, Role::Implementer, 0.0, 20);
        assert_eq!(result.len(), 4);
    }

    #[test]
    fn test_select_global_always_included() {
        let l = make_learning("global", LearningScope::Global, "global insight");
        let map = to_map(vec![l]);

        // Empty scope chain — only Global should match
        let result = select_learnings(&map, &[], Role::Implementer, 0.0, 20);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].content, "global insight");
    }

    #[test]
    fn test_select_excludes_unrelated_scope() {
        let l1 = make_learning("wi-1", LearningScope::WorkItem, "relevant");
        let l2 = make_learning("wi-999", LearningScope::WorkItem, "unrelated");
        let map = to_map(vec![l1, l2]);

        let scope_ids = [("wi-1", LearningScope::WorkItem)];
        let result = select_learnings(&map, &scope_ids, Role::Implementer, 0.0, 20);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].content, "relevant");
    }

    // --- Role filtering ---

    #[test]
    fn test_select_by_role_match() {
        let l = make_learning_with_role(
            "wi-1",
            LearningScope::WorkItem,
            "impl insight",
            vec![Role::Implementer],
        );
        let map = to_map(vec![l]);
        let scope_ids = [("wi-1", LearningScope::WorkItem)];

        let result = select_learnings(&map, &scope_ids, Role::Implementer, 0.0, 20);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn test_select_by_role_mismatch() {
        let l = make_learning_with_role(
            "wi-1",
            LearningScope::WorkItem,
            "reviewer only",
            vec![Role::Reviewer],
        );
        let map = to_map(vec![l]);
        let scope_ids = [("wi-1", LearningScope::WorkItem)];

        let result = select_learnings(&map, &scope_ids, Role::Implementer, 0.0, 20);
        assert_eq!(result.len(), 0);
    }

    #[test]
    fn test_select_none_roles_applies_to_all() {
        let l = make_learning("wi-1", LearningScope::WorkItem, "universal");
        // applicable_roles is None by default
        let map = to_map(vec![l]);
        let scope_ids = [("wi-1", LearningScope::WorkItem)];

        for role in [
            Role::Implementer,
            Role::Reviewer,
            Role::Coordinator,
            Role::Researcher,
            Role::Integrator,
        ] {
            let result = select_learnings(&map, &scope_ids, role, 0.0, 20);
            assert_eq!(result.len(), 1, "should apply to role {role}");
        }
    }

    // --- Confidence filtering ---

    #[test]
    fn test_select_above_confidence_threshold() {
        let l = make_learning_with_confidence("wi-1", LearningScope::WorkItem, "high conf", 0.8);
        let map = to_map(vec![l]);
        let scope_ids = [("wi-1", LearningScope::WorkItem)];

        let result = select_learnings(&map, &scope_ids, Role::Implementer, 0.3, 20);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn test_select_below_confidence_threshold() {
        let l = make_learning_with_confidence("wi-1", LearningScope::WorkItem, "low conf", 0.1);
        let map = to_map(vec![l]);
        let scope_ids = [("wi-1", LearningScope::WorkItem)];

        let result = select_learnings(&map, &scope_ids, Role::Implementer, 0.3, 20);
        assert_eq!(result.len(), 0);
    }

    #[test]
    fn test_select_promoted_always_included_regardless_of_confidence() {
        let mut l = make_learning_with_confidence("wi-1", LearningScope::WorkItem, "policy", 0.1);
        l.promoted = true;
        let map = to_map(vec![l]);
        let scope_ids = [("wi-1", LearningScope::WorkItem)];

        let result = select_learnings(&map, &scope_ids, Role::Implementer, 0.9, 20);
        assert_eq!(result.len(), 1);
        assert!(result[0].promoted);
    }

    // --- Sorting ---

    #[test]
    fn test_sort_promoted_first() {
        let mut l1 = make_learning_with_confidence("wi-1", LearningScope::WorkItem, "normal", 0.9);
        l1.updated_at = 1000;
        let mut l2 = make_learning_with_confidence("wi-1", LearningScope::WorkItem, "policy", 0.5);
        l2.promoted = true;
        l2.updated_at = 500;
        let map = to_map(vec![l1, l2]);
        let scope_ids = [("wi-1", LearningScope::WorkItem)];

        let result = select_learnings(&map, &scope_ids, Role::Implementer, 0.0, 20);
        assert_eq!(result.len(), 2);
        assert!(result[0].promoted, "promoted should come first");
        assert!(!result[1].promoted);
    }

    #[test]
    fn test_sort_by_confidence_desc() {
        let mut l1 = make_learning_with_confidence("wi-1", LearningScope::WorkItem, "low", 0.3);
        l1.updated_at = 1000;
        let mut l2 = make_learning_with_confidence("wi-1", LearningScope::WorkItem, "high", 0.9);
        l2.updated_at = 1000;
        let map = to_map(vec![l1, l2]);
        let scope_ids = [("wi-1", LearningScope::WorkItem)];

        let result = select_learnings(&map, &scope_ids, Role::Implementer, 0.0, 20);
        assert_eq!(result.len(), 2);
        assert!(result[0].confidence > result[1].confidence);
    }

    #[test]
    fn test_sort_by_recency_desc() {
        let mut l1 = make_learning_with_confidence("wi-1", LearningScope::WorkItem, "older", 0.5);
        l1.updated_at = 1000;
        let mut l2 = make_learning_with_confidence("wi-1", LearningScope::WorkItem, "newer", 0.5);
        l2.updated_at = 2000;
        let map = to_map(vec![l1, l2]);
        let scope_ids = [("wi-1", LearningScope::WorkItem)];

        let result = select_learnings(&map, &scope_ids, Role::Implementer, 0.0, 20);
        assert_eq!(result.len(), 2);
        assert!(result[0].updated_at > result[1].updated_at);
    }

    // --- Truncation ---

    #[test]
    fn test_max_count_truncation() {
        let learnings: Vec<Learning> = (0..30)
            .map(|i| make_learning("wi-1", LearningScope::WorkItem, &format!("insight {i}")))
            .collect();
        let map = to_map(learnings);
        let scope_ids = [("wi-1", LearningScope::WorkItem)];

        let result = select_learnings(&map, &scope_ids, Role::Implementer, 0.0, 10);
        assert_eq!(result.len(), 10);
    }

    #[test]
    fn test_fewer_than_max_count() {
        let l = make_learning("wi-1", LearningScope::WorkItem, "only one");
        let map = to_map(vec![l]);
        let scope_ids = [("wi-1", LearningScope::WorkItem)];

        let result = select_learnings(&map, &scope_ids, Role::Implementer, 0.0, 20);
        assert_eq!(result.len(), 1);
    }

    // --- Empty inputs ---

    #[test]
    fn test_empty_learnings() {
        let map = HashMap::new();
        let scope_ids = [("wi-1", LearningScope::WorkItem)];

        let result = select_learnings(&map, &scope_ids, Role::Implementer, 0.0, 20);
        assert!(result.is_empty());
    }

    #[test]
    fn test_empty_scope_ids_only_global() {
        let l1 = make_learning("wi-1", LearningScope::WorkItem, "scoped");
        let l2 = make_learning("global", LearningScope::Global, "global");
        let map = to_map(vec![l1, l2]);

        let result = select_learnings(&map, &[], Role::Implementer, 0.0, 20);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].scope, LearningScope::Global);
    }

    // --- Combined filtering ---

    #[test]
    fn test_combined_scope_role_confidence() {
        // Matches scope, role, and confidence
        let l1 = make_learning_with_confidence("wi-1", LearningScope::WorkItem, "good", 0.8);

        // Matches scope and confidence, wrong role
        let mut l2 = make_learning_with_confidence("wi-1", LearningScope::WorkItem, "wrong role", 0.8);
        l2.applicable_roles = Some(vec![Role::Reviewer]);

        // Matches scope and role, low confidence
        let l3 = make_learning_with_confidence("wi-1", LearningScope::WorkItem, "low conf", 0.1);

        // Wrong scope entirely
        let l4 = make_learning_with_confidence("wi-999", LearningScope::WorkItem, "wrong scope", 0.8);

        let map = to_map(vec![l1, l2, l3, l4]);
        let scope_ids = [("wi-1", LearningScope::WorkItem)];

        let result = select_learnings(&map, &scope_ids, Role::Implementer, 0.3, 20);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].content, "good");
    }

    #[test]
    fn test_auto_promoted_learning_sorted_first() {
        let policy = PromotionPolicy {
            min_reinforcements: 2,
            max_age_days: 30,
            auto_promote: true,
        };
        let mut l1 = make_learning_with_confidence("wi-1", LearningScope::WorkItem, "unpromoted", 0.9);
        l1.updated_at = 2000;
        let mut l2 = make_learning_with_confidence("wi-1", LearningScope::WorkItem, "promoted", 0.7);
        l2.reinforce(&policy);
        l2.reinforce(&policy);
        l2.updated_at = 1000;
        assert!(l2.promoted, "should be auto-promoted after 2 reinforcements");

        let map = to_map(vec![l1, l2]);
        let scope_ids = [("wi-1", LearningScope::WorkItem)];

        let result = select_learnings(&map, &scope_ids, Role::Implementer, 0.0, 20);
        assert_eq!(result.len(), 2);
        assert!(result[0].promoted, "promoted should be first");
        assert_eq!(result[0].content, "promoted");
    }

    // --- Default confidence behavior ---

    #[test]
    fn test_default_confidence_passes_standard_threshold() {
        // New learnings have confidence 0.5, which should pass the standard 0.3 threshold
        let l = make_learning("wi-1", LearningScope::WorkItem, "new insight");
        assert!((l.confidence - 0.5).abs() < f32::EPSILON);
        let map = to_map(vec![l]);
        let scope_ids = [("wi-1", LearningScope::WorkItem)];

        let result = select_learnings(&map, &scope_ids, Role::Implementer, 0.3, 20);
        assert_eq!(result.len(), 1);
    }

    #[test]
    fn test_zero_max_count_returns_empty() {
        let l = make_learning("wi-1", LearningScope::WorkItem, "insight");
        let map = to_map(vec![l]);
        let scope_ids = [("wi-1", LearningScope::WorkItem)];

        let result = select_learnings(&map, &scope_ids, Role::Implementer, 0.0, 0);
        assert!(result.is_empty());
    }
}
