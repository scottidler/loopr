use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fmt;
use taskstore::record::{IndexValue, Record};

use crate::id;

/// Verdict from the Doc Validator LLM.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ValidationVerdict {
    Pass,
    Fail,
    Warn,
}

impl fmt::Display for ValidationVerdict {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ValidationVerdict::Pass => write!(f, "pass"),
            ValidationVerdict::Fail => write!(f, "fail"),
            ValidationVerdict::Warn => write!(f, "warn"),
        }
    }
}

/// Severity of an individual validation issue.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IssueSeverity {
    Error,
    Warning,
    Info,
}

impl fmt::Display for IssueSeverity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            IssueSeverity::Error => write!(f, "error"),
            IssueSeverity::Warning => write!(f, "warning"),
            IssueSeverity::Info => write!(f, "info"),
        }
    }
}

/// A single issue found during validation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationIssue {
    pub severity: IssueSeverity,
    pub category: String,
    pub message: String,
    pub suggestion: Option<String>,
}

/// Structured output from the Doc Validator LLM.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ValidationReport {
    pub id: String,
    pub target_collection: String,
    pub target_id: String,
    pub verdict: ValidationVerdict,
    pub issues: Vec<ValidationIssue>,
    pub summary: String,
    pub model_used: String,
    pub created_at: i64,
}

impl ValidationReport {
    pub fn new(
        target_collection: String,
        target_id: String,
        verdict: ValidationVerdict,
        issues: Vec<ValidationIssue>,
        summary: String,
        model_used: String,
    ) -> Self {
        Self {
            id: id::generate_id(),
            target_collection,
            target_id,
            verdict,
            issues,
            summary,
            model_used,
            created_at: id::now_millis(),
        }
    }
}

impl Record for ValidationReport {
    fn id(&self) -> &str {
        &self.id
    }

    fn updated_at(&self) -> i64 {
        self.created_at
    }

    fn collection_name() -> &'static str {
        "validation_reports"
    }

    fn indexed_fields(&self) -> HashMap<String, IndexValue> {
        let mut m = HashMap::new();
        m.insert(
            "target_id".into(),
            IndexValue::String(self.target_id.clone()),
        );
        m.insert(
            "target_collection".into(),
            IndexValue::String(self.target_collection.clone()),
        );
        m.insert(
            "verdict".into(),
            IndexValue::String(self.verdict.to_string()),
        );
        m
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validation_verdict_display() {
        assert_eq!(ValidationVerdict::Pass.to_string(), "pass");
        assert_eq!(ValidationVerdict::Fail.to_string(), "fail");
        assert_eq!(ValidationVerdict::Warn.to_string(), "warn");
    }

    #[test]
    fn test_validation_verdict_serde_roundtrip() {
        for verdict in [
            ValidationVerdict::Pass,
            ValidationVerdict::Fail,
            ValidationVerdict::Warn,
        ] {
            let json = serde_json::to_string(&verdict).unwrap();
            let deserialized: ValidationVerdict = serde_json::from_str(&json).unwrap();
            assert_eq!(verdict, deserialized);
        }
    }

    #[test]
    fn test_validation_verdict_serde_format() {
        assert_eq!(
            serde_json::to_string(&ValidationVerdict::Pass).unwrap(),
            "\"pass\""
        );
        assert_eq!(
            serde_json::to_string(&ValidationVerdict::Fail).unwrap(),
            "\"fail\""
        );
        assert_eq!(
            serde_json::to_string(&ValidationVerdict::Warn).unwrap(),
            "\"warn\""
        );
    }

    #[test]
    fn test_issue_severity_display() {
        assert_eq!(IssueSeverity::Error.to_string(), "error");
        assert_eq!(IssueSeverity::Warning.to_string(), "warning");
        assert_eq!(IssueSeverity::Info.to_string(), "info");
    }

    #[test]
    fn test_issue_severity_serde_roundtrip() {
        for severity in [
            IssueSeverity::Error,
            IssueSeverity::Warning,
            IssueSeverity::Info,
        ] {
            let json = serde_json::to_string(&severity).unwrap();
            let deserialized: IssueSeverity = serde_json::from_str(&json).unwrap();
            assert_eq!(severity, deserialized);
        }
    }

    #[test]
    fn test_validation_issue_serde() {
        let issue = ValidationIssue {
            severity: IssueSeverity::Error,
            category: "completeness".to_string(),
            message: "Missing acceptance criteria".to_string(),
            suggestion: Some("Add measurable criteria".to_string()),
        };
        let json = serde_json::to_string(&issue).unwrap();
        let deserialized: ValidationIssue = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.severity, IssueSeverity::Error);
        assert_eq!(deserialized.category, "completeness");
        assert_eq!(deserialized.suggestion, Some("Add measurable criteria".to_string()));
    }

    #[test]
    fn test_validation_issue_no_suggestion() {
        let issue = ValidationIssue {
            severity: IssueSeverity::Info,
            category: "clarity".to_string(),
            message: "Consider adding examples".to_string(),
            suggestion: None,
        };
        let json = serde_json::to_string(&issue).unwrap();
        let deserialized: ValidationIssue = serde_json::from_str(&json).unwrap();
        assert!(deserialized.suggestion.is_none());
    }

    #[test]
    fn test_validation_report_new() {
        let report = ValidationReport::new(
            "plans".to_string(),
            "plan-123".to_string(),
            ValidationVerdict::Pass,
            vec![],
            "All criteria met".to_string(),
            "claude-sonnet-4-6".to_string(),
        );
        assert!(!report.id.is_empty());
        assert_eq!(report.target_collection, "plans");
        assert_eq!(report.target_id, "plan-123");
        assert_eq!(report.verdict, ValidationVerdict::Pass);
        assert!(report.issues.is_empty());
        assert_eq!(report.summary, "All criteria met");
        assert_eq!(report.model_used, "claude-sonnet-4-6");
        assert!(report.created_at > 0);
    }

    #[test]
    fn test_validation_report_serde_roundtrip() {
        let report = ValidationReport::new(
            "specs".to_string(),
            "spec-456".to_string(),
            ValidationVerdict::Warn,
            vec![ValidationIssue {
                severity: IssueSeverity::Warning,
                category: "testability".to_string(),
                message: "Consider adding test plan".to_string(),
                suggestion: None,
            }],
            "Passes with warnings".to_string(),
            "claude-sonnet-4-6".to_string(),
        );
        let json = serde_json::to_string(&report).unwrap();
        let deserialized: ValidationReport = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.id, report.id);
        assert_eq!(deserialized.verdict, ValidationVerdict::Warn);
        assert_eq!(deserialized.issues.len(), 1);
        assert_eq!(deserialized.target_collection, "specs");
    }

    #[test]
    fn test_validation_report_record_id() {
        let report = ValidationReport::new(
            "plans".to_string(),
            "p1".to_string(),
            ValidationVerdict::Pass,
            vec![],
            "ok".to_string(),
            "model".to_string(),
        );
        assert_eq!(Record::id(&report), report.id.as_str());
    }

    #[test]
    fn test_validation_report_record_updated_at() {
        let report = ValidationReport::new(
            "plans".to_string(),
            "p1".to_string(),
            ValidationVerdict::Pass,
            vec![],
            "ok".to_string(),
            "model".to_string(),
        );
        assert_eq!(Record::updated_at(&report), report.created_at);
    }

    #[test]
    fn test_validation_report_collection_name() {
        assert_eq!(ValidationReport::collection_name(), "validation_reports");
    }

    #[test]
    fn test_validation_report_indexed_fields() {
        let report = ValidationReport::new(
            "phases".to_string(),
            "phase-789".to_string(),
            ValidationVerdict::Fail,
            vec![],
            "failed".to_string(),
            "model".to_string(),
        );
        let fields = report.indexed_fields();
        assert_eq!(fields.len(), 3);
        assert_eq!(
            fields.get("target_id"),
            Some(&IndexValue::String("phase-789".to_string()))
        );
        assert_eq!(
            fields.get("target_collection"),
            Some(&IndexValue::String("phases".to_string()))
        );
        assert_eq!(
            fields.get("verdict"),
            Some(&IndexValue::String("fail".to_string()))
        );
    }

    #[test]
    fn test_validation_report_record_roundtrip_json() {
        let report = ValidationReport::new(
            "plans".to_string(),
            "p1".to_string(),
            ValidationVerdict::Pass,
            vec![],
            "ok".to_string(),
            "model".to_string(),
        );
        let json = serde_json::to_string(&report).unwrap();
        let restored: ValidationReport = serde_json::from_str(&json).unwrap();
        assert_eq!(Record::id(&restored), Record::id(&report));
        assert_eq!(Record::updated_at(&restored), Record::updated_at(&report));
        assert_eq!(ValidationReport::collection_name(), "validation_reports");
    }

    #[test]
    fn test_validation_report_with_multiple_issues() {
        let report = ValidationReport::new(
            "plans".to_string(),
            "p1".to_string(),
            ValidationVerdict::Fail,
            vec![
                ValidationIssue {
                    severity: IssueSeverity::Error,
                    category: "completeness".to_string(),
                    message: "Missing objective".to_string(),
                    suggestion: Some("Add a clear objective".to_string()),
                },
                ValidationIssue {
                    severity: IssueSeverity::Warning,
                    category: "scope".to_string(),
                    message: "Scope may be too broad".to_string(),
                    suggestion: None,
                },
                ValidationIssue {
                    severity: IssueSeverity::Info,
                    category: "clarity".to_string(),
                    message: "Consider adding diagrams".to_string(),
                    suggestion: None,
                },
            ],
            "Multiple issues found".to_string(),
            "claude-sonnet-4-6".to_string(),
        );
        assert_eq!(report.issues.len(), 3);
        assert_eq!(report.issues[0].severity, IssueSeverity::Error);
        assert_eq!(report.issues[1].severity, IssueSeverity::Warning);
        assert_eq!(report.issues[2].severity, IssueSeverity::Info);
    }

    #[test]
    fn test_validation_verdict_display_matches_serde() {
        for verdict in [
            ValidationVerdict::Pass,
            ValidationVerdict::Fail,
            ValidationVerdict::Warn,
        ] {
            let display = verdict.to_string();
            let quoted = format!("\"{}\"", display);
            let deserialized: ValidationVerdict = serde_json::from_str(&quoted)
                .unwrap_or_else(|e| {
                    panic!("Display output '{}' not deserializable: {}", display, e)
                });
            assert_eq!(verdict, deserialized);
        }
    }

    #[test]
    fn test_issue_severity_display_matches_serde() {
        for severity in [
            IssueSeverity::Error,
            IssueSeverity::Warning,
            IssueSeverity::Info,
        ] {
            let display = severity.to_string();
            let quoted = format!("\"{}\"", display);
            let deserialized: IssueSeverity = serde_json::from_str(&quoted)
                .unwrap_or_else(|e| {
                    panic!("Display output '{}' not deserializable: {}", display, e)
                });
            assert_eq!(severity, deserialized);
        }
    }
}
