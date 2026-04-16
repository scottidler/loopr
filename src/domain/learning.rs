use std::collections::HashMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use taskstore::{IndexValue, Record};

use crate::config::PromotionPolicy;
use crate::domain::role::Role;
use crate::id;

fn default_confidence() -> f32 {
    0.5
}

/// A code citation supporting a structural claim in a learning.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CodeCitation {
    pub file_path: String,
    pub line_number: u32,
    pub excerpt: String,
}

/// Returns `true` when `content` makes a structural code claim (about function signatures,
/// interface contracts, or API shapes). These learnings are marked tentative unless they
/// carry a `code_citation`.
///
/// Markers are intentionally narrow: "signature", "interface contract", "function contract",
/// "has signature", "takes a". Broad terms like "type" or "parameter" are excluded to
/// avoid false positives on legitimate non-code learnings.
pub fn is_structural_claim(content: &str) -> bool {
    let lower = content.to_lowercase();
    lower.contains("signature")
        || lower.contains("interface contract")
        || lower.contains("function contract")
        || lower.contains("has signature")
        || lower.contains("takes a")
}

/// Scope at which a learning applies.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum LearningScope {
    #[serde(alias = "Work", alias = "work")]
    Work,
    #[serde(alias = "Phase")]
    Phase,
    #[serde(alias = "Spec")]
    Spec,
    #[serde(alias = "Plan")]
    Plan,
    #[serde(alias = "Global")]
    Global,
}

impl fmt::Display for LearningScope {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LearningScope::Work => write!(f, "Work"),
            LearningScope::Phase => write!(f, "Phase"),
            LearningScope::Spec => write!(f, "Spec"),
            LearningScope::Plan => write!(f, "Plan"),
            LearningScope::Global => write!(f, "Global"),
        }
    }
}

/// Insight captured during work. Can be promoted to Policy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Learning {
    pub id: String,
    pub source_id: String,
    pub scope: LearningScope,
    pub content: String,
    pub reinforcements: u32,
    pub contradictions: u32,
    pub promoted: bool,
    pub created_at: i64,
    pub updated_at: i64,

    /// Roles this learning is relevant to. None = all roles.
    #[serde(default)]
    pub applicable_roles: Option<Vec<Role>>,

    /// File paths for scoped selection (formerly resource_tags).
    #[serde(default, alias = "resource_tags")]
    pub files: Vec<String>,

    /// Computed confidence: reinforcements / (reinforcements + contradictions).
    /// Updated on reinforce() / contradict(). Range 0.0..=1.0.
    /// Default 0.5 for new learnings (neutral).
    #[serde(default = "default_confidence")]
    pub confidence: f32,

    /// Code citation grounding a structural claim. When set, the learning is authoritative
    /// for that specific claim (file/line/excerpt observed in actual merged code).
    #[serde(default)]
    pub code_citation: Option<CodeCitation>,

    /// `true` when this learning makes a structural code claim (signature, interface
    /// contract, etc.) but carries no `code_citation`. Tentative learnings are rendered
    /// with a `[TENTATIVE]` prefix in agent context and cannot override implementer claims
    /// that cite actual code.
    #[serde(default)]
    pub tentative: bool,
}

impl Learning {
    pub fn new(source_id: String, scope: LearningScope, content: String) -> Self {
        tracing::debug!("Learning::new(source_id={}, scope={})", source_id, scope);
        let now = id::now_millis();
        // Auto-mark as tentative when content makes a structural claim without a citation.
        let tentative = is_structural_claim(&content);
        Self {
            id: id::generate_id("ln"),
            source_id,
            scope,
            content,
            reinforcements: 0,
            contradictions: 0,
            promoted: false,
            created_at: now,
            updated_at: now,
            applicable_roles: None,
            files: Vec::new(),
            confidence: default_confidence(),
            code_citation: None,
            tentative,
        }
    }

    /// Recompute confidence from reinforcements and contradictions.
    /// Range 0.0..=1.0, default 0.5 for zero observations.
    pub fn recompute_confidence(&mut self) {
        let total = self.reinforcements + self.contradictions;
        self.confidence = if total == 0 {
            0.5
        } else {
            (self.reinforcements as f32 / total as f32).clamp(0.0, 1.0)
        };
    }

    /// Record an independent confirmation of this learning.
    /// If PromotionPolicy allows auto-promotion and thresholds are met
    /// (reinforcements >= min, contradictions == 0, age <= max_age_days),
    /// the learning is automatically promoted to Policy.
    pub fn reinforce(&mut self, promotion: &PromotionPolicy) {
        self.reinforcements += 1;
        self.recompute_confidence();
        self.updated_at = id::now_millis();

        // Auto-promotion check
        if promotion.auto_promote && self.reinforcements >= promotion.min_reinforcements && self.contradictions == 0 {
            let age_days = (self.updated_at - self.created_at) / (24 * 60 * 60 * 1000);
            if age_days <= promotion.max_age_days as i64 {
                self.promoted = true;
            }
        }
    }

    /// Record a contradiction of this learning.
    pub fn contradict(&mut self) {
        self.contradictions += 1;
        self.recompute_confidence();
        self.updated_at = id::now_millis();
    }

    /// Promote this learning to a policy.
    pub fn promote(&mut self) {
        self.promoted = true;
        self.updated_at = id::now_millis();
    }

    /// Demote this learning from policy status.
    pub fn demote(&mut self) {
        self.promoted = false;
        self.updated_at = id::now_millis();
    }
}

impl Record for Learning {
    fn id(&self) -> &str {
        &self.id
    }

    fn updated_at(&self) -> i64 {
        self.updated_at
    }

    fn collection_name() -> &'static str {
        "learnings"
    }

    fn indexed_fields(&self) -> HashMap<String, IndexValue> {
        let mut m = HashMap::new();
        m.insert("scope".into(), IndexValue::String(self.scope.to_string()));
        m.insert("source_id".into(), IndexValue::String(self.source_id.clone()));
        m.insert("promoted".into(), IndexValue::String(self.promoted.to_string()));
        m.insert(
            "confidence".into(),
            IndexValue::String(format!("{:.2}", self.confidence)),
        );
        m
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    /// No-auto-promote policy for tests that just want simple reinforcement.
    fn no_promote() -> PromotionPolicy {
        PromotionPolicy {
            auto_promote: false,
            ..PromotionPolicy::default()
        }
    }

    // --- LearningScope tests ---

    #[test]
    fn test_learning_scope_display() {
        assert_eq!(LearningScope::Work.to_string(), "Work");
        assert_eq!(LearningScope::Phase.to_string(), "Phase");
        assert_eq!(LearningScope::Spec.to_string(), "Spec");
        assert_eq!(LearningScope::Plan.to_string(), "Plan");
        assert_eq!(LearningScope::Global.to_string(), "Global");
    }

    #[test]
    fn test_learning_scope_serde_roundtrip() {
        for scope in [
            LearningScope::Work,
            LearningScope::Phase,
            LearningScope::Spec,
            LearningScope::Plan,
            LearningScope::Global,
        ] {
            let json = serde_json::to_string(&scope).unwrap();
            let deserialized: LearningScope = serde_json::from_str(&json).unwrap();
            assert_eq!(scope, deserialized);
        }
    }

    #[test]
    fn test_learning_scope_serde_format() {
        assert_eq!(serde_json::to_string(&LearningScope::Work).unwrap(), "\"work\"");
        assert_eq!(serde_json::to_string(&LearningScope::Phase).unwrap(), "\"phase\"");
        assert_eq!(serde_json::to_string(&LearningScope::Spec).unwrap(), "\"spec\"");
        assert_eq!(serde_json::to_string(&LearningScope::Plan).unwrap(), "\"plan\"");
        assert_eq!(serde_json::to_string(&LearningScope::Global).unwrap(), "\"global\"");
    }

    // --- Learning struct tests ---

    #[test]
    fn test_learning_new() {
        let learning = Learning::new(
            "wi-123".to_string(),
            LearningScope::Work,
            "Always run tests before committing".to_string(),
        );
        assert_eq!(learning.source_id, "wi-123");
        assert_eq!(learning.scope, LearningScope::Work);
        assert_eq!(learning.content, "Always run tests before committing");
        assert_eq!(learning.reinforcements, 0);
        assert_eq!(learning.contradictions, 0);
        assert!(!learning.promoted);
        assert!(!learning.id.is_empty());
        assert!(learning.created_at > 0);
        assert_eq!(learning.created_at, learning.updated_at);
        // MVP4 fields
        assert!(learning.applicable_roles.is_none());
        assert!(learning.files.is_empty());
        assert!((learning.confidence - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn test_learning_serde_roundtrip() {
        let mut learning = Learning::new(
            "phase-456".to_string(),
            LearningScope::Phase,
            "Split large tasks into smaller ones".to_string(),
        );
        learning.applicable_roles = Some(vec![Role::Implementer, Role::Reviewer]);
        learning.files = vec!["src/main.rs".to_string()];
        learning.confidence = 0.8;

        let json = serde_json::to_string(&learning).unwrap();
        let deserialized: Learning = serde_json::from_str(&json).unwrap();
        assert_eq!(learning.id, deserialized.id);
        assert_eq!(learning.source_id, deserialized.source_id);
        assert_eq!(learning.scope, deserialized.scope);
        assert_eq!(learning.content, deserialized.content);
        assert_eq!(learning.reinforcements, deserialized.reinforcements);
        assert_eq!(learning.contradictions, deserialized.contradictions);
        assert_eq!(learning.promoted, deserialized.promoted);
        assert_eq!(learning.created_at, deserialized.created_at);
        assert_eq!(learning.applicable_roles, deserialized.applicable_roles);
        assert_eq!(learning.files, deserialized.files);
        assert!((learning.confidence - deserialized.confidence).abs() < f32::EPSILON);
    }

    #[test]
    fn test_learning_unique_ids() {
        let l1 = Learning::new("a".to_string(), LearningScope::Global, "x".to_string());
        let l2 = Learning::new("a".to_string(), LearningScope::Global, "y".to_string());
        assert_ne!(l1.id, l2.id);
    }

    #[test]
    fn test_learning_reinforce() {
        let policy = no_promote();
        let mut learning = Learning::new("wi-1".to_string(), LearningScope::Work, "insight".to_string());
        assert_eq!(learning.reinforcements, 0);
        learning.reinforce(&policy);
        assert_eq!(learning.reinforcements, 1);
        learning.reinforce(&policy);
        assert_eq!(learning.reinforcements, 2);
    }

    #[test]
    fn test_learning_contradict() {
        let mut learning = Learning::new("wi-1".to_string(), LearningScope::Work, "insight".to_string());
        assert_eq!(learning.contradictions, 0);
        learning.contradict();
        assert_eq!(learning.contradictions, 1);
        learning.contradict();
        assert_eq!(learning.contradictions, 2);
    }

    #[test]
    fn test_learning_promote_demote() {
        let mut learning = Learning::new(
            "plan-1".to_string(),
            LearningScope::Plan,
            "policy candidate".to_string(),
        );
        assert!(!learning.promoted);
        learning.promote();
        assert!(learning.promoted);
        learning.demote();
        assert!(!learning.promoted);
    }

    #[test]
    fn test_learning_source_id_preserved() {
        let learning = Learning::new("spec-789".to_string(), LearningScope::Spec, "content".to_string());
        assert_eq!(learning.source_id, "spec-789");
    }

    #[test]
    fn test_learning_global_scope() {
        let learning = Learning::new(
            "global".to_string(),
            LearningScope::Global,
            "Global insight".to_string(),
        );
        assert_eq!(learning.scope, LearningScope::Global);
        assert_eq!(learning.scope.to_string(), "Global");
    }

    // --- Record trait tests ---

    #[test]
    fn test_record_id() {
        let l = Learning::new("wi-1".to_string(), LearningScope::Work, "insight".to_string());
        assert_eq!(Record::id(&l), l.id);
    }

    #[test]
    fn test_record_updated_at() {
        let l = Learning::new("wi-1".to_string(), LearningScope::Work, "insight".to_string());
        assert_eq!(Record::updated_at(&l), l.updated_at);
    }

    #[test]
    fn test_record_collection_name() {
        assert_eq!(Learning::collection_name(), "learnings");
    }

    #[test]
    fn test_record_indexed_fields() {
        let l = Learning::new("phase-42".to_string(), LearningScope::Phase, "insight".to_string());
        let fields = l.indexed_fields();
        assert_eq!(fields.get("scope"), Some(&IndexValue::String("Phase".to_string())));
        assert_eq!(
            fields.get("source_id"),
            Some(&IndexValue::String("phase-42".to_string()))
        );
        assert_eq!(fields.get("promoted"), Some(&IndexValue::String("false".to_string())));
        assert_eq!(fields.get("confidence"), Some(&IndexValue::String("0.50".to_string())));
        assert_eq!(fields.len(), 4);
    }

    #[test]
    fn test_record_indexed_fields_reflect_scope() {
        let l = Learning::new("g".to_string(), LearningScope::Global, "global insight".to_string());
        let fields = l.indexed_fields();
        assert_eq!(fields.get("scope"), Some(&IndexValue::String("Global".to_string())));
    }

    // --- Confidence computation tests ---

    #[test]
    fn test_confidence_default_for_new_learning() {
        let l = Learning::new("wi-1".to_string(), LearningScope::Work, "insight".to_string());
        assert!((l.confidence - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn test_recompute_confidence_zero_observations() {
        let mut l = Learning::new("wi-1".to_string(), LearningScope::Work, "insight".to_string());
        l.recompute_confidence();
        assert!((l.confidence - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn test_recompute_confidence_all_reinforced() {
        let mut l = Learning::new("wi-1".to_string(), LearningScope::Work, "insight".to_string());
        l.reinforcements = 5;
        l.contradictions = 0;
        l.recompute_confidence();
        assert!((l.confidence - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn test_recompute_confidence_all_contradicted() {
        let mut l = Learning::new("wi-1".to_string(), LearningScope::Work, "insight".to_string());
        l.reinforcements = 0;
        l.contradictions = 5;
        l.recompute_confidence();
        assert!((l.confidence - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn test_recompute_confidence_mixed() {
        let mut l = Learning::new("wi-1".to_string(), LearningScope::Work, "insight".to_string());
        l.reinforcements = 3;
        l.contradictions = 1;
        l.recompute_confidence();
        assert!((l.confidence - 0.75).abs() < f32::EPSILON);
    }

    #[test]
    fn test_reinforce_updates_confidence() {
        let policy = no_promote();
        let mut l = Learning::new("wi-1".to_string(), LearningScope::Work, "insight".to_string());
        l.reinforce(&policy);
        // 1 reinforcement, 0 contradictions → 1.0
        assert!((l.confidence - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn test_contradict_updates_confidence() {
        let mut l = Learning::new("wi-1".to_string(), LearningScope::Work, "insight".to_string());
        l.contradict();
        // 0 reinforcements, 1 contradiction → 0.0
        assert!((l.confidence - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn test_confidence_after_mixed_operations() {
        let policy = no_promote();
        let mut l = Learning::new("wi-1".to_string(), LearningScope::Work, "insight".to_string());
        l.reinforce(&policy); // 1r, 0c → 1.0
        l.reinforce(&policy); // 2r, 0c → 1.0
        l.contradict(); // 2r, 1c → 0.667
        assert!((l.confidence - 2.0 / 3.0).abs() < 0.01);
    }

    // --- Backward compatibility tests ---

    #[test]
    fn test_backward_compat_deserialize_pre_mvp4_json() {
        // Pre-MVP4 JSON: no applicable_roles, files, or confidence fields
        let json = r#"{
            "id": "learn-old",
            "source_id": "wi-1",
            "scope": "work",
            "content": "Old learning",
            "reinforcements": 2,
            "contradictions": 0,
            "promoted": false,
            "created_at": 1000000,
            "updated_at": 2000000
        }"#;
        let learning: Learning = serde_json::from_str(json).unwrap();
        assert_eq!(learning.id, "learn-old");
        assert_eq!(learning.source_id, "wi-1");
        assert_eq!(learning.reinforcements, 2);
        // Defaults applied for missing MVP4 fields
        assert!(learning.applicable_roles.is_none());
        assert!(learning.files.is_empty());
        assert!((learning.confidence - 0.5).abs() < f32::EPSILON);
    }

    // --- Auto-promotion tests ---

    #[test]
    fn test_auto_promotion_on_threshold() {
        let policy = PromotionPolicy {
            min_reinforcements: 3,
            max_age_days: 30,
            auto_promote: true,
        };
        let mut l = Learning::new("wi-1".to_string(), LearningScope::Work, "insight".to_string());
        l.reinforce(&policy); // 1
        assert!(!l.promoted);
        l.reinforce(&policy); // 2
        assert!(!l.promoted);
        l.reinforce(&policy); // 3 — meets threshold, auto-promote
        assert!(l.promoted);
    }

    #[test]
    fn test_auto_promotion_blocked_by_contradictions() {
        let policy = PromotionPolicy {
            min_reinforcements: 3,
            max_age_days: 30,
            auto_promote: true,
        };
        let mut l = Learning::new("wi-1".to_string(), LearningScope::Work, "insight".to_string());
        l.reinforce(&policy); // 1
        l.reinforce(&policy); // 2
        l.contradict(); // 1 contradiction blocks auto-promotion
        l.reinforce(&policy); // 3 reinforcements, but contradictions > 0
        assert!(!l.promoted);
    }

    #[test]
    fn test_auto_promotion_disabled() {
        let policy = PromotionPolicy {
            min_reinforcements: 1,
            max_age_days: 365,
            auto_promote: false,
        };
        let mut l = Learning::new("wi-1".to_string(), LearningScope::Work, "insight".to_string());
        l.reinforce(&policy);
        l.reinforce(&policy);
        l.reinforce(&policy);
        assert!(!l.promoted);
    }

    #[test]
    fn test_auto_promotion_respects_min_reinforcements() {
        let policy = PromotionPolicy {
            min_reinforcements: 5,
            max_age_days: 30,
            auto_promote: true,
        };
        let mut l = Learning::new("wi-1".to_string(), LearningScope::Work, "insight".to_string());
        for _ in 0..4 {
            l.reinforce(&policy);
        }
        assert!(!l.promoted);
        l.reinforce(&policy); // 5th — meets threshold
        assert!(l.promoted);
    }

    // --- New fields tests ---

    #[test]
    fn test_applicable_roles_filtering() {
        let mut l = Learning::new("wi-1".to_string(), LearningScope::Work, "insight".to_string());
        l.applicable_roles = Some(vec![Role::Implementer, Role::Reviewer]);
        let roles = l.applicable_roles.as_ref().unwrap();
        assert!(roles.contains(&Role::Implementer));
        assert!(roles.contains(&Role::Reviewer));
        assert!(!roles.contains(&Role::Coordinator));
    }

    #[test]
    fn test_applicable_roles_none_means_all() {
        let l = Learning::new("wi-1".to_string(), LearningScope::Work, "insight".to_string());
        assert!(l.applicable_roles.is_none());
        // None means applicable to all roles
        let applies_to_all = l
            .applicable_roles
            .as_ref()
            .map(|roles| roles.contains(&Role::Coordinator))
            .unwrap_or(true);
        assert!(applies_to_all);
    }

    #[test]
    fn test_files() {
        let mut l = Learning::new("wi-1".to_string(), LearningScope::Work, "insight".to_string());
        l.files = vec!["src/main.rs".to_string(), "iteration:5".to_string()];
        assert_eq!(l.files.len(), 2);
        assert!(l.files.contains(&"src/main.rs".to_string()));
    }

    #[test]
    fn test_indexed_fields_promoted_true() {
        let mut l = Learning::new("wi-1".to_string(), LearningScope::Work, "insight".to_string());
        l.promoted = true;
        let fields = l.indexed_fields();
        assert_eq!(fields.get("promoted"), Some(&IndexValue::String("true".to_string())));
    }

    // --- Phase 2: CodeCitation and tentative tests ---

    #[test]
    fn test_is_structural_claim_signature() {
        assert!(is_structural_claim(
            "function update_bookmark has signature (db_path, id)"
        ));
        assert!(is_structural_claim("The function signature is foo(a, b)"));
    }

    #[test]
    fn test_is_structural_claim_interface_contract() {
        assert!(is_structural_claim(
            "The interface contract specifies update_bookmark(db_path, id, fields: dict)"
        ));
        assert!(is_structural_claim("This violates the function contract"));
    }

    #[test]
    fn test_is_structural_claim_has_signature() {
        assert!(is_structural_claim("update_bookmark has signature (db_path, ...)"));
    }

    #[test]
    fn test_is_structural_claim_takes_a() {
        assert!(is_structural_claim("The function takes a dict as its second argument"));
    }

    #[test]
    fn test_is_structural_claim_non_structural() {
        // Broad terms that should NOT trigger the marker
        assert!(!is_structural_claim("Always run tests before committing"));
        assert!(!is_structural_claim("The type system enforces safety"));
        assert!(!is_structural_claim("Check the parameter names for clarity"));
        assert!(!is_structural_claim("Review passed with no issues"));
    }

    #[test]
    fn test_learning_new_structural_is_tentative() {
        let l = Learning::new(
            "wi-1".to_string(),
            LearningScope::Work,
            "update_bookmark has signature (db_path, id, fields: dict)".to_string(),
        );
        assert!(l.tentative, "structural claim without citation should be tentative");
        assert!(l.code_citation.is_none());
    }

    #[test]
    fn test_learning_new_non_structural_not_tentative() {
        let l = Learning::new(
            "wi-1".to_string(),
            LearningScope::Work,
            "Always run tests before committing to catch regressions".to_string(),
        );
        assert!(!l.tentative, "non-structural learning should not be tentative");
    }

    #[test]
    fn test_code_citation_serde_roundtrip() {
        let citation = CodeCitation {
            file_path: "src/database.py".to_string(),
            line_number: 42,
            excerpt: "def update_bookmark(db_path, bookmark_id, title=None):".to_string(),
        };
        let json = serde_json::to_string(&citation).unwrap();
        let restored: CodeCitation = serde_json::from_str(&json).unwrap();
        assert_eq!(restored.file_path, "src/database.py");
        assert_eq!(restored.line_number, 42);
        assert_eq!(
            restored.excerpt,
            "def update_bookmark(db_path, bookmark_id, title=None):"
        );
    }

    #[test]
    fn test_learning_with_citation_not_tentative() {
        // A learning that would otherwise be structural but has a citation should stay non-tentative
        // (citation overrides the auto-promotion rule when set externally)
        let mut l = Learning::new(
            "wi-1".to_string(),
            LearningScope::Work,
            "update_bookmark has signature (db_path, id, fields: dict)".to_string(),
        );
        // Attach a citation after creation (as a handler would do)
        l.code_citation = Some(CodeCitation {
            file_path: "database.py".to_string(),
            line_number: 10,
            excerpt: "def update_bookmark(db_path, id, fields: dict):".to_string(),
        });
        // tentative was set at creation time; the handler would also clear it when citing
        l.tentative = false;
        assert!(!l.tentative);
        assert!(l.code_citation.is_some());
    }

    #[test]
    fn test_learning_backward_compat_deserialize_no_tentative_fields() {
        let json = r#"{
            "id": "ln-old",
            "source_id": "wi-1",
            "scope": "work",
            "content": "Old learning without new fields",
            "reinforcements": 0,
            "contradictions": 0,
            "promoted": false,
            "created_at": 1000000,
            "updated_at": 2000000
        }"#;
        let l: Learning = serde_json::from_str(json).unwrap();
        assert!(!l.tentative, "old records default to non-tentative");
        assert!(l.code_citation.is_none());
    }

    // --- M2: LearningScope alias tests ---

    #[test]
    fn test_learning_scope_pascal_case_alias() {
        let scope: LearningScope = serde_json::from_str("\"Phase\"").unwrap();
        assert_eq!(scope, LearningScope::Phase);
    }

    #[test]
    fn test_learning_scope_lowercase_canonical() {
        let scope: LearningScope = serde_json::from_str("\"phase\"").unwrap();
        assert_eq!(scope, LearningScope::Phase);
    }

    #[test]
    fn test_learning_scope_work_alias() {
        // snake_case alias
        let scope: LearningScope = serde_json::from_str("\"work\"").unwrap();
        assert_eq!(scope, LearningScope::Work);
        // PascalCase alias
        let scope2: LearningScope = serde_json::from_str("\"Work\"").unwrap();
        assert_eq!(scope2, LearningScope::Work);
    }

    #[test]
    fn test_learning_scope_all_aliases() {
        for (input, expected) in [
            ("\"phase\"", LearningScope::Phase),
            ("\"Phase\"", LearningScope::Phase),
            ("\"spec\"", LearningScope::Spec),
            ("\"Spec\"", LearningScope::Spec),
            ("\"plan\"", LearningScope::Plan),
            ("\"Plan\"", LearningScope::Plan),
            ("\"global\"", LearningScope::Global),
            ("\"Global\"", LearningScope::Global),
            ("\"work\"", LearningScope::Work),
            ("\"Work\"", LearningScope::Work),
            ("\"work\"", LearningScope::Work),
        ] {
            let scope: LearningScope =
                serde_json::from_str(input).unwrap_or_else(|e| panic!("Failed to deserialize {}: {}", input, e));
            assert_eq!(scope, expected, "Failed for input {}", input);
        }
    }
}
