use std::collections::HashMap;
use std::path::{Path, PathBuf};

use chrono::Local;
use eyre::Result;
use serde::{Deserialize, Serialize};
use taskstore::record::{IndexValue, Record};

use crate::id;

const RUN_DIR_FORMAT: &str = "%Y%m%d-%H%M%S";

/// The level of the plan hierarchy this document represents.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DocKind {
    Plan,
    Spec,
    Phase,
    Work,
}

impl DocKind {
    /// ID prefix for this kind.
    pub fn id_prefix(self) -> &'static str {
        match self {
            DocKind::Plan => "pl",
            DocKind::Spec => "sp",
            DocKind::Phase => "ph",
            DocKind::Work => "wk",
        }
    }

    /// Filename prefix for .md files in the run directory.
    pub fn file_prefix(self) -> &'static str {
        match self {
            DocKind::Plan => "plan",
            DocKind::Spec => "spec",
            DocKind::Phase => "phase",
            DocKind::Work => "work",
        }
    }
}

impl std::fmt::Display for DocKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DocKind::Plan => write!(f, "plan"),
            DocKind::Spec => write!(f, "spec"),
            DocKind::Phase => write!(f, "phase"),
            DocKind::Work => write!(f, "work"),
        }
    }
}

/// Unified storage struct for all plan hierarchy levels.
///
/// `Doc` is intentionally stateless: lifecycle status belongs on domain wrappers,
/// not here. Content (the prose artifact) lives in the `.md` file on disk;
/// `Doc` stores the path and structured metadata the system needs to query.
///
/// The `markdown` field is a relative filename within the run directory
/// (e.g. `spec-core-implementation.md`), not a full path.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Doc {
    pub id: String,
    pub kind: DocKind,
    pub parent_id: Option<String>,
    /// Human-readable title from the LLM output (e.g. "Core Implementation").
    /// Stored on creation so it is available without re-parsing the .md file.
    #[serde(default)]
    pub title: String,
    /// Filename relative to the run directory (e.g. `plan-auth-refactor.md`).
    pub markdown: String,
    /// IDs of sibling docs that must be complete before this one can proceed.
    pub dependencies: Vec<String>,
    /// Dep titles that could not be resolved during decomposition; cleared by the
    /// post-merge cross-spec resolution pass. Never persisted.
    #[serde(skip, default)]
    pub unresolved_dep_titles: Vec<String>,
    /// Extracted from the `## Acceptance Criteria` section at creation time.
    pub acceptance_criteria: Vec<String>,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Doc {
    pub fn new(kind: DocKind, parent_id: Option<String>, title: String, markdown: String) -> Self {
        let now = id::now_millis();
        Self {
            id: id::generate_id(kind.id_prefix()),
            kind,
            parent_id,
            title,
            markdown,
            dependencies: Vec::new(),
            unresolved_dep_titles: Vec::new(),
            acceptance_criteria: Vec::new(),
            created_at: now,
            updated_at: now,
        }
    }
}

impl Record for Doc {
    fn id(&self) -> &str {
        &self.id
    }

    fn updated_at(&self) -> i64 {
        self.updated_at
    }

    fn collection_name() -> &'static str {
        "docs"
    }

    fn indexed_fields(&self) -> HashMap<String, IndexValue> {
        let mut m = HashMap::new();
        m.insert("kind".into(), IndexValue::String(self.kind.to_string()));
        if let Some(ref pid) = self.parent_id {
            m.insert("parent_id".into(), IndexValue::String(pid.clone()));
        }
        m
    }
}

/// Thin in-memory wrappers for type safety at each hierarchy level.
///
/// Only `Doc` implements `Record` and is persisted. These wrappers exist
/// purely to make function signatures self-documenting and catch wrong-level
/// arguments at compile time. Status lives on the wrapper, not on `Doc`.
pub struct PlanDoc(pub Doc);
pub struct SpecDoc(pub Doc);
pub struct PhaseDoc(pub Doc);
pub struct WorkDoc(pub Doc);

impl PlanDoc {
    pub fn new(title: String, markdown: String) -> Self {
        Self(Doc::new(DocKind::Plan, None, title, markdown))
    }
}

impl SpecDoc {
    pub fn new(parent_id: String, title: String, markdown: String) -> Self {
        Self(Doc::new(DocKind::Spec, Some(parent_id), title, markdown))
    }
}

impl PhaseDoc {
    pub fn new(parent_id: String, title: String, markdown: String) -> Self {
        Self(Doc::new(DocKind::Phase, Some(parent_id), title, markdown))
    }
}

impl WorkDoc {
    pub fn new(parent_id: String, title: String, markdown: String) -> Self {
        Self(Doc::new(DocKind::Work, Some(parent_id), title, markdown))
    }
}

// --- Run directory management ---

/// Create a new run directory under `<project_root>/.loopr/runs/YYYYMMDD-HHMMSS/`.
///
/// Each orchestration run gets an isolated flat directory for its .md artifacts.
pub fn create_run_dir(project_root: &Path) -> Result<PathBuf> {
    let name = Local::now().format(RUN_DIR_FORMAT).to_string();
    let run_dir = project_root.join(".loopr").join("runs").join(name);
    std::fs::create_dir_all(&run_dir)?;
    Ok(run_dir)
}

/// Slugify a title for use in filenames.
///
/// - Lowercase
/// - Alphanumeric characters and spaces kept; everything else stripped
/// - Spaces converted to hyphens
/// - Multiple spaces collapsed before conversion
pub fn slug_from_title(title: &str) -> String {
    title
        .chars()
        .filter(|c| c.is_alphanumeric() || c.is_whitespace())
        .collect::<String>()
        .to_lowercase()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join("-")
}

/// Compute the .md filename for a doc within a run directory.
///
/// Format: `{kind}-{slug}.md`
///
/// Appends `-2`, `-3`, etc. on collision to stay unique within the run directory.
pub fn doc_filename(kind: DocKind, title: &str, taken: &[String]) -> String {
    let slug = slug_from_title(title);
    let base = format!("{}-{}", kind.file_prefix(), slug);
    let candidate = format!("{}.md", base);
    if !taken.contains(&candidate) {
        return candidate;
    }
    let mut n = 2u32;
    loop {
        let numbered = format!("{}-{}.md", base, n);
        if !taken.contains(&numbered) {
            log::warn!(
                "doc filename collision: {} already taken, using {}",
                candidate,
                numbered
            );
            return numbered;
        }
        n += 1;
    }
}

/// Write a doc's markdown content to the run directory.
///
/// Returns the filename (relative to `run_dir`) that should be stored in `Doc.markdown`.
/// The caller is responsible for passing all already-used filenames in `taken` so
/// collision detection works correctly.
pub fn write_doc_file(run_dir: &Path, kind: DocKind, title: &str, content: &str, taken: &[String]) -> Result<String> {
    let filename = doc_filename(kind, title, taken);
    let path = run_dir.join(&filename);
    std::fs::write(&path, content)?;
    Ok(filename)
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::TestDir;

    // --- DocKind ---

    #[test]
    fn test_doc_kind_display() {
        assert_eq!(DocKind::Plan.to_string(), "plan");
        assert_eq!(DocKind::Spec.to_string(), "spec");
        assert_eq!(DocKind::Phase.to_string(), "phase");
        assert_eq!(DocKind::Work.to_string(), "work");
    }

    #[test]
    fn test_doc_kind_id_prefix() {
        assert_eq!(DocKind::Plan.id_prefix(), "pl");
        assert_eq!(DocKind::Spec.id_prefix(), "sp");
        assert_eq!(DocKind::Phase.id_prefix(), "ph");
        assert_eq!(DocKind::Work.id_prefix(), "wk");
    }

    #[test]
    fn test_doc_kind_file_prefix() {
        assert_eq!(DocKind::Plan.file_prefix(), "plan");
        assert_eq!(DocKind::Spec.file_prefix(), "spec");
        assert_eq!(DocKind::Phase.file_prefix(), "phase");
        assert_eq!(DocKind::Work.file_prefix(), "work");
    }

    #[test]
    fn test_doc_kind_serde_roundtrip() {
        for kind in [DocKind::Plan, DocKind::Spec, DocKind::Phase, DocKind::Work] {
            let json = serde_json::to_string(&kind).unwrap();
            let restored: DocKind = serde_json::from_str(&json).unwrap();
            assert_eq!(kind, restored);
        }
    }

    #[test]
    fn test_doc_kind_serde_format() {
        assert_eq!(serde_json::to_string(&DocKind::Plan).unwrap(), "\"plan\"");
        assert_eq!(serde_json::to_string(&DocKind::Work).unwrap(), "\"work\"");
    }

    // --- Doc struct ---

    #[test]
    fn test_doc_new_plan() {
        let doc = Doc::new(DocKind::Plan, None, "Plan Impl".to_string(), "plan-impl.md".to_string());
        assert!(doc.id.starts_with("pl-"));
        assert_eq!(doc.kind, DocKind::Plan);
        assert!(doc.parent_id.is_none());
        assert_eq!(doc.title, "Plan Impl");
        assert_eq!(doc.markdown, "plan-impl.md");
        assert!(doc.dependencies.is_empty());
        assert!(doc.acceptance_criteria.is_empty());
        assert!(doc.created_at > 0);
        assert_eq!(doc.created_at, doc.updated_at);
    }

    #[test]
    fn test_doc_new_spec() {
        let doc = Doc::new(
            DocKind::Spec,
            Some("pl-abc12".to_string()),
            "Core Spec".to_string(),
            "spec-core.md".to_string(),
        );
        assert!(doc.id.starts_with("sp-"));
        assert_eq!(doc.kind, DocKind::Spec);
        assert_eq!(doc.parent_id.as_deref(), Some("pl-abc12"));
    }

    #[test]
    fn test_doc_new_phase() {
        let doc = Doc::new(
            DocKind::Phase,
            Some("sp-abc12".to_string()),
            "Phase Data".to_string(),
            "phase-data.md".to_string(),
        );
        assert!(doc.id.starts_with("ph-"));
    }

    #[test]
    fn test_doc_new_work() {
        let doc = Doc::new(
            DocKind::Work,
            Some("ph-abc12".to_string()),
            "Work Schema".to_string(),
            "work-schema.md".to_string(),
        );
        assert!(doc.id.starts_with("wk-"));
    }

    #[test]
    fn test_doc_unique_ids() {
        let d1 = Doc::new(DocKind::Plan, None, "Plan A".to_string(), "a.md".to_string());
        let d2 = Doc::new(DocKind::Plan, None, "Plan B".to_string(), "b.md".to_string());
        assert_ne!(d1.id, d2.id);
    }

    #[test]
    fn test_doc_serde_roundtrip() {
        let mut doc = Doc::new(
            DocKind::Spec,
            Some("pl-abc12".to_string()),
            "API Spec".to_string(),
            "spec-api.md".to_string(),
        );
        doc.dependencies = vec!["sp-x1y2z".to_string()];
        doc.acceptance_criteria = vec!["Must handle errors".to_string(), "Must log failures".to_string()];

        let json = serde_json::to_string(&doc).unwrap();
        let restored: Doc = serde_json::from_str(&json).unwrap();
        assert_eq!(doc.id, restored.id);
        assert_eq!(doc.kind, restored.kind);
        assert_eq!(doc.parent_id, restored.parent_id);
        assert_eq!(doc.title, restored.title);
        assert_eq!(doc.markdown, restored.markdown);
        assert_eq!(doc.dependencies, restored.dependencies);
        assert_eq!(doc.acceptance_criteria, restored.acceptance_criteria);
        assert_eq!(doc.created_at, restored.created_at);
        assert_eq!(doc.updated_at, restored.updated_at);
    }

    #[test]
    fn test_doc_serde_backward_compat_without_title() {
        // Old records serialized without the title field must deserialize with empty string.
        let json = serde_json::json!({
            "id": "pl-abc123",
            "kind": "plan",
            "parent_id": null,
            "markdown": "plan-foo.md",
            "dependencies": [],
            "acceptance_criteria": [],
            "created_at": 1000,
            "updated_at": 1000
        });
        let doc: Doc = serde_json::from_value(json).unwrap();
        assert_eq!(doc.title, "", "old records without title must default to empty string");
        assert_ne!(doc.title, "Untitled Plan");
    }

    // --- Record trait ---

    #[test]
    fn test_doc_record_collection_name() {
        assert_eq!(Doc::collection_name(), "docs");
    }

    #[test]
    fn test_doc_record_id() {
        let doc = Doc::new(DocKind::Plan, None, "My Plan".to_string(), "p.md".to_string());
        assert_eq!(Record::id(&doc), doc.id.as_str());
    }

    #[test]
    fn test_doc_record_updated_at() {
        let doc = Doc::new(DocKind::Work, None, "My Work".to_string(), "w.md".to_string());
        assert_eq!(Record::updated_at(&doc), doc.updated_at);
    }

    #[test]
    fn test_doc_indexed_fields_with_parent() {
        let doc = Doc::new(
            DocKind::Spec,
            Some("pl-abc12".to_string()),
            "My Spec".to_string(),
            "s.md".to_string(),
        );
        let fields = doc.indexed_fields();
        assert_eq!(fields.get("kind"), Some(&IndexValue::String("spec".to_string())));
        assert_eq!(
            fields.get("parent_id"),
            Some(&IndexValue::String("pl-abc12".to_string()))
        );
    }

    #[test]
    fn test_doc_indexed_fields_no_parent() {
        let doc = Doc::new(DocKind::Plan, None, "My Plan".to_string(), "p.md".to_string());
        let fields = doc.indexed_fields();
        assert_eq!(fields.get("kind"), Some(&IndexValue::String("plan".to_string())));
        assert!(!fields.contains_key("parent_id"));
    }

    // --- Wrappers ---

    #[test]
    fn test_plan_doc_new() {
        let pd = PlanDoc::new("Foo Plan".to_string(), "plan-foo.md".to_string());
        assert_eq!(pd.0.kind, DocKind::Plan);
        assert!(pd.0.parent_id.is_none());
        assert!(pd.0.id.starts_with("pl-"));
        assert_eq!(pd.0.title, "Foo Plan");
    }

    #[test]
    fn test_spec_doc_new() {
        let sd = SpecDoc::new(
            "pl-abc12".to_string(),
            "Foo Spec".to_string(),
            "spec-foo.md".to_string(),
        );
        assert_eq!(sd.0.kind, DocKind::Spec);
        assert_eq!(sd.0.parent_id.as_deref(), Some("pl-abc12"));
        assert_eq!(sd.0.title, "Foo Spec");
    }

    #[test]
    fn test_phase_doc_new() {
        let pd = PhaseDoc::new(
            "sp-abc12".to_string(),
            "Foo Phase".to_string(),
            "phase-foo.md".to_string(),
        );
        assert_eq!(pd.0.kind, DocKind::Phase);
        assert_eq!(pd.0.title, "Foo Phase");
    }

    #[test]
    fn test_work_doc_new() {
        let wd = WorkDoc::new(
            "ph-abc12".to_string(),
            "Foo Work".to_string(),
            "work-foo.md".to_string(),
        );
        assert_eq!(wd.0.kind, DocKind::Work);
        assert_eq!(wd.0.title, "Foo Work");
    }

    // --- Slug generation ---

    #[test]
    fn test_slug_from_title_simple() {
        assert_eq!(slug_from_title("Core Implementation"), "core-implementation");
    }

    #[test]
    fn test_slug_from_title_lowercase() {
        assert_eq!(slug_from_title("UPPER CASE"), "upper-case");
    }

    #[test]
    fn test_slug_from_title_strips_special_chars() {
        assert_eq!(slug_from_title("Auth & API Integration!"), "auth-api-integration");
    }

    #[test]
    fn test_slug_from_title_collapses_whitespace() {
        assert_eq!(slug_from_title("  too   many  spaces  "), "too-many-spaces");
    }

    #[test]
    fn test_slug_from_title_single_word() {
        assert_eq!(slug_from_title("Schema"), "schema");
    }

    #[test]
    fn test_slug_from_title_numbers() {
        assert_eq!(slug_from_title("Phase 1 Setup"), "phase-1-setup");
    }

    // --- Filename generation ---

    #[test]
    fn test_doc_filename_no_collision() {
        let name = doc_filename(DocKind::Spec, "Core Implementation", &[]);
        assert_eq!(name, "spec-core-implementation.md");
    }

    #[test]
    fn test_doc_filename_plan() {
        let name = doc_filename(DocKind::Plan, "Auth Refactor", &[]);
        assert_eq!(name, "plan-auth-refactor.md");
    }

    #[test]
    fn test_doc_filename_collision_appends_2() {
        let taken = vec!["spec-core-implementation.md".to_string()];
        let name = doc_filename(DocKind::Spec, "Core Implementation", &taken);
        assert_eq!(name, "spec-core-implementation-2.md");
    }

    #[test]
    fn test_doc_filename_multiple_collisions() {
        let taken = vec!["work-schema.md".to_string(), "work-schema-2.md".to_string()];
        let name = doc_filename(DocKind::Work, "Schema", &taken);
        assert_eq!(name, "work-schema-3.md");
    }

    // --- File writing ---

    #[test]
    fn test_write_doc_file_creates_file() {
        let dir = TestDir::new("loopr-doc-test");
        let content = "# My Plan\n\nThis is a plan.";
        let filename = write_doc_file(&dir, DocKind::Plan, "My Plan", content, &[]).unwrap();
        assert_eq!(filename, "plan-my-plan.md");
        let written = std::fs::read_to_string(dir.join(&filename)).unwrap();
        assert_eq!(written, content);
    }

    #[test]
    fn test_write_doc_file_returns_relative_filename() {
        let dir = TestDir::new("loopr-doc-test");
        let filename = write_doc_file(&dir, DocKind::Spec, "API Integration", "content", &[]).unwrap();
        assert_eq!(filename, "spec-api-integration.md");
        // Filename is relative - should not contain the temp dir path
        assert!(!filename.contains('/'));
    }

    #[test]
    fn test_write_doc_file_collision_handled() {
        let dir = TestDir::new("loopr-doc-test");
        // Pre-create the first filename so collision detection kicks in
        std::fs::write(dir.join("plan-my-plan.md"), "existing").unwrap();
        let taken = vec!["plan-my-plan.md".to_string()];
        let filename = write_doc_file(&dir, DocKind::Plan, "My Plan", "new content", &taken).unwrap();
        assert_eq!(filename, "plan-my-plan-2.md");
        let written = std::fs::read_to_string(dir.join("plan-my-plan-2.md")).unwrap();
        assert_eq!(written, "new content");
    }

    // --- Run directory ---

    #[test]
    fn test_create_run_dir_creates_directory() {
        let dir = TestDir::new("loopr-run-test");
        let run_dir = create_run_dir(&dir).unwrap();
        assert!(run_dir.exists());
        assert!(run_dir.is_dir());
    }

    #[test]
    fn test_create_run_dir_under_loopr_runs() {
        let dir = TestDir::new("loopr-run-test");
        let run_dir = create_run_dir(&dir).unwrap();
        let expected_prefix = dir.join(".loopr").join("runs");
        assert!(run_dir.starts_with(&expected_prefix));
    }

    #[test]
    fn test_create_run_dir_name_format() {
        let dir = TestDir::new("loopr-run-test");
        let run_dir = create_run_dir(&dir).unwrap();
        let name = run_dir.file_name().unwrap().to_str().unwrap();
        // YYYYMMDD-HHMMSS = 15 chars
        assert_eq!(name.len(), 15, "run dir name should be 15 chars: got {}", name);
        assert!(name.chars().nth(8) == Some('-'), "char 8 should be '-'");
        assert!(name[..8].chars().all(|c| c.is_ascii_digit()));
        assert!(name[9..].chars().all(|c| c.is_ascii_digit()));
    }
}
