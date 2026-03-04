use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use taskstore::record::{IndexValue, Record};

use crate::id;

/// Verdict from the Coverage Evaluator LLM.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum CoverageVerdict {
    /// Children fully cover the parent's requirements
    Complete,
    /// Gaps and/or out-of-scope items exist
    Incomplete,
}

impl fmt::Display for CoverageVerdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            CoverageVerdict::Complete => write!(f, "complete"),
            CoverageVerdict::Incomplete => write!(f, "incomplete"),
        }
    }
}

/// Severity of a coverage gap.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum GapSeverity {
    Critical,
    Minor,
}

impl fmt::Display for GapSeverity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            GapSeverity::Critical => write!(f, "critical"),
            GapSeverity::Minor => write!(f, "minor"),
        }
    }
}

/// A specific gap in coverage: a parent requirement not addressed by any child.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoverageGap {
    /// The specific parent requirement or criterion that is not covered
    pub parent_criterion: String,
    /// Description of what's missing
    pub description: String,
    /// Whether this gap is critical or minor
    pub severity: GapSeverity,
}

/// A child document that includes work beyond the parent's scope.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutOfScopeItem {
    /// Which child doc contains out-of-scope work
    pub child_id: String,
    /// Description of what's out of scope
    pub description: String,
}

/// Structured output from the Coverage Evaluator LLM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CoverageReport {
    pub id: String,
    pub parent_collection: String,
    pub parent_id: String,
    pub children_collection: String,
    pub children_ids: Vec<String>,
    pub verdict: CoverageVerdict,
    pub gaps: Vec<CoverageGap>,
    pub out_of_scope: Vec<OutOfScopeItem>,
    pub summary: String,
    pub model_used: String,
    pub created_at: i64,
}

/// Parameters for constructing a CoverageReport.
pub struct CoverageReportParams {
    pub parent_collection: String,
    pub parent_id: String,
    pub children_collection: String,
    pub children_ids: Vec<String>,
    pub verdict: CoverageVerdict,
    pub gaps: Vec<CoverageGap>,
    pub out_of_scope: Vec<OutOfScopeItem>,
    pub summary: String,
    pub model_used: String,
}

impl CoverageReport {
    pub fn new(params: CoverageReportParams) -> Self {
        Self {
            id: id::generate_id("cr"),
            parent_collection: params.parent_collection,
            parent_id: params.parent_id,
            children_collection: params.children_collection,
            children_ids: params.children_ids,
            verdict: params.verdict,
            gaps: params.gaps,
            out_of_scope: params.out_of_scope,
            summary: params.summary,
            model_used: params.model_used,
            created_at: id::now_millis(),
        }
    }

    /// Returns true if the verdict is Complete.
    pub fn is_complete(&self) -> bool {
        self.verdict == CoverageVerdict::Complete
    }

    /// Returns only critical gaps.
    pub fn critical_gaps(&self) -> Vec<&CoverageGap> {
        self.gaps
            .iter()
            .filter(|g| g.severity == GapSeverity::Critical)
            .collect()
    }
}

impl Record for CoverageReport {
    fn id(&self) -> &str {
        &self.id
    }

    fn updated_at(&self) -> i64 {
        self.created_at
    }

    fn collection_name() -> &'static str {
        "coverage_reports"
    }

    fn indexed_fields(&self) -> HashMap<String, IndexValue> {
        let mut m = HashMap::new();
        m.insert("parent_id".into(), IndexValue::String(self.parent_id.clone()));
        m.insert(
            "parent_collection".into(),
            IndexValue::String(self.parent_collection.clone()),
        );
        m.insert("verdict".into(), IndexValue::String(self.verdict.to_string()));
        m
    }
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;

    /// Helper to create a simple CoverageReport for tests.
    fn make_report(
        parent_col: &str,
        parent_id: &str,
        children_col: &str,
        verdict: CoverageVerdict,
        gaps: Vec<CoverageGap>,
        out_of_scope: Vec<OutOfScopeItem>,
    ) -> CoverageReport {
        CoverageReport::new(CoverageReportParams {
            parent_collection: parent_col.into(),
            parent_id: parent_id.into(),
            children_collection: children_col.into(),
            children_ids: vec![],
            verdict,
            gaps,
            out_of_scope,
            summary: "test".into(),
            model_used: "model".into(),
        })
    }

    #[test]
    fn test_coverage_verdict_display() {
        assert_eq!(CoverageVerdict::Complete.to_string(), "complete");
        assert_eq!(CoverageVerdict::Incomplete.to_string(), "incomplete");
    }

    #[test]
    fn test_coverage_verdict_serde_roundtrip() {
        for verdict in [CoverageVerdict::Complete, CoverageVerdict::Incomplete] {
            let json = serde_json::to_string(&verdict).unwrap();
            let deserialized: CoverageVerdict = serde_json::from_str(&json).unwrap();
            assert_eq!(verdict, deserialized);
        }
    }

    #[test]
    fn test_coverage_verdict_serde_format() {
        assert_eq!(
            serde_json::to_string(&CoverageVerdict::Complete).unwrap(),
            "\"complete\""
        );
        assert_eq!(
            serde_json::to_string(&CoverageVerdict::Incomplete).unwrap(),
            "\"incomplete\""
        );
    }

    #[test]
    fn test_gap_severity_display() {
        assert_eq!(GapSeverity::Critical.to_string(), "critical");
        assert_eq!(GapSeverity::Minor.to_string(), "minor");
    }

    #[test]
    fn test_gap_severity_serde_roundtrip() {
        for severity in [GapSeverity::Critical, GapSeverity::Minor] {
            let json = serde_json::to_string(&severity).unwrap();
            let deserialized: GapSeverity = serde_json::from_str(&json).unwrap();
            assert_eq!(severity, deserialized);
        }
    }

    #[test]
    fn test_coverage_gap_serde() {
        let gap = CoverageGap {
            parent_criterion: "audit logging".to_string(),
            description: "No spec covers audit logging".to_string(),
            severity: GapSeverity::Critical,
        };
        let json = serde_json::to_string(&gap).unwrap();
        let deserialized: CoverageGap = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.parent_criterion, "audit logging");
        assert_eq!(deserialized.severity, GapSeverity::Critical);
    }

    #[test]
    fn test_out_of_scope_item_serde() {
        let item = OutOfScopeItem {
            child_id: "sp-abc12".to_string(),
            description: "Includes email notifications not in Plan".to_string(),
        };
        let json = serde_json::to_string(&item).unwrap();
        let deserialized: OutOfScopeItem = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.child_id, "sp-abc12");
    }

    #[test]
    fn test_coverage_report_new() {
        let report = CoverageReport::new(CoverageReportParams {
            parent_collection: "plans".into(),
            parent_id: "pl-abc12".into(),
            children_collection: "specs".into(),
            children_ids: vec!["sp-111".into(), "sp-222".into()],
            verdict: CoverageVerdict::Complete,
            gaps: vec![],
            out_of_scope: vec![],
            summary: "Full coverage".into(),
            model_used: "claude-sonnet-4-6".into(),
        });
        assert!(report.id.starts_with("cr-"));
        assert_eq!(report.parent_collection, "plans");
        assert_eq!(report.parent_id, "pl-abc12");
        assert_eq!(report.children_collection, "specs");
        assert_eq!(report.children_ids.len(), 2);
        assert_eq!(report.verdict, CoverageVerdict::Complete);
        assert!(report.gaps.is_empty());
        assert!(report.out_of_scope.is_empty());
        assert!(report.created_at > 0);
    }

    #[test]
    fn test_coverage_report_is_complete() {
        let complete = make_report("plans", "pl-1", "specs", CoverageVerdict::Complete, vec![], vec![]);
        assert!(complete.is_complete());

        let incomplete = make_report(
            "plans",
            "pl-1",
            "specs",
            CoverageVerdict::Incomplete,
            vec![CoverageGap {
                parent_criterion: "auth".into(),
                description: "missing".into(),
                severity: GapSeverity::Critical,
            }],
            vec![],
        );
        assert!(!incomplete.is_complete());
    }

    #[test]
    fn test_coverage_report_critical_gaps() {
        let report = make_report(
            "plans",
            "pl-1",
            "specs",
            CoverageVerdict::Incomplete,
            vec![
                CoverageGap {
                    parent_criterion: "auth".into(),
                    description: "missing auth".into(),
                    severity: GapSeverity::Critical,
                },
                CoverageGap {
                    parent_criterion: "docs".into(),
                    description: "missing docs".into(),
                    severity: GapSeverity::Minor,
                },
                CoverageGap {
                    parent_criterion: "tests".into(),
                    description: "missing tests".into(),
                    severity: GapSeverity::Critical,
                },
            ],
            vec![],
        );
        let critical = report.critical_gaps();
        assert_eq!(critical.len(), 2);
        assert_eq!(critical[0].parent_criterion, "auth");
        assert_eq!(critical[1].parent_criterion, "tests");
    }

    #[test]
    fn test_coverage_report_serde_roundtrip() {
        let report = CoverageReport::new(CoverageReportParams {
            parent_collection: "specs".into(),
            parent_id: "sp-123".into(),
            children_collection: "phases".into(),
            children_ids: vec!["ph-1".into(), "ph-2".into()],
            verdict: CoverageVerdict::Incomplete,
            gaps: vec![CoverageGap {
                parent_criterion: "error handling".into(),
                description: "No phase covers error handling".into(),
                severity: GapSeverity::Critical,
            }],
            out_of_scope: vec![OutOfScopeItem {
                child_id: "ph-2".into(),
                description: "Includes logging not in Spec".into(),
            }],
            summary: "Incomplete coverage".into(),
            model_used: "claude-sonnet-4-6".into(),
        });
        let json = serde_json::to_string(&report).unwrap();
        let deserialized: CoverageReport = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.id, report.id);
        assert_eq!(deserialized.verdict, CoverageVerdict::Incomplete);
        assert_eq!(deserialized.gaps.len(), 1);
        assert_eq!(deserialized.out_of_scope.len(), 1);
        assert_eq!(deserialized.parent_collection, "specs");
    }

    #[test]
    fn test_coverage_report_record_trait() {
        let report = make_report("plans", "pl-1", "specs", CoverageVerdict::Complete, vec![], vec![]);
        assert_eq!(Record::id(&report), report.id.as_str());
        assert_eq!(Record::updated_at(&report), report.created_at);
        assert_eq!(CoverageReport::collection_name(), "coverage_reports");
    }

    #[test]
    fn test_coverage_report_indexed_fields() {
        let report = CoverageReport::new(CoverageReportParams {
            parent_collection: "phases".into(),
            parent_id: "ph-789".into(),
            children_collection: "works".into(),
            children_ids: vec!["wk-1".into()],
            verdict: CoverageVerdict::Complete,
            gaps: vec![],
            out_of_scope: vec![],
            summary: "ok".into(),
            model_used: "model".into(),
        });
        let fields = report.indexed_fields();
        assert_eq!(fields.len(), 3);
        assert_eq!(fields.get("parent_id"), Some(&IndexValue::String("ph-789".to_string())));
        assert_eq!(
            fields.get("parent_collection"),
            Some(&IndexValue::String("phases".to_string()))
        );
        assert_eq!(fields.get("verdict"), Some(&IndexValue::String("complete".to_string())));
    }

    #[test]
    fn test_coverage_verdict_display_matches_serde() {
        for verdict in [CoverageVerdict::Complete, CoverageVerdict::Incomplete] {
            let display = verdict.to_string();
            let quoted = format!("\"{}\"", display);
            let deserialized: CoverageVerdict = serde_json::from_str(&quoted)
                .unwrap_or_else(|e| panic!("Display output '{}' not deserializable: {}", display, e));
            assert_eq!(verdict, deserialized);
        }
    }

    #[test]
    fn test_gap_severity_display_matches_serde() {
        for severity in [GapSeverity::Critical, GapSeverity::Minor] {
            let display = severity.to_string();
            let quoted = format!("\"{}\"", display);
            let deserialized: GapSeverity = serde_json::from_str(&quoted)
                .unwrap_or_else(|e| panic!("Display output '{}' not deserializable: {}", display, e));
            assert_eq!(severity, deserialized);
        }
    }
}
