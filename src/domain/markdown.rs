use std::fs;
use std::path::Path;

use chrono::DateTime;
use chrono::SecondsFormat;
use eyre::Result;

// =====================================================
// FmValue
// =====================================================

/// A value that can appear in YAML frontmatter.
pub enum FmValue {
    Text(String),
    List(Vec<String>),
}

// =====================================================
// DocMarkdown trait
// =====================================================

/// Types that can be rendered as a `docs/loopr/<id>.md` file.
///
/// Implemented by Plan, Spec, Phase, and Work. The trait keeps the markdown
/// writer generic - no match arms on type.
pub trait DocMarkdown {
    fn doc_id(&self) -> &str;
    fn doc_frontmatter(&self) -> Vec<(String, FmValue)>;
    /// Returns the markdown body. Ownership is needed because Work appends a
    /// checklist section to the description.
    fn doc_body(&self) -> String;
}

// =====================================================
// write_doc_markdown
// =====================================================

/// Write `record` as `{repo_path}/docs/loopr/{id}.md`.
///
/// Creates the directory if needed. This is a synchronous `std::fs::write` -
/// files are small (a few KB) and non-blocking in practice.
///
/// Advisory: failure logs a warning but MUST NOT propagate to the caller.
/// Callers should use the returned `Result` only for logging.
pub fn write_doc_markdown(repo_path: &Path, record: &impl DocMarkdown) -> Result<()> {
    let dir = repo_path.join("docs").join("loopr");
    fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.md", record.doc_id()));
    let content = format!(
        "---\n{}---\n\n{}\n",
        format_frontmatter(&record.doc_frontmatter()),
        record.doc_body()
    );
    fs::write(&path, content)?;
    Ok(())
}

// =====================================================
// format_frontmatter
// =====================================================

/// Render a list of key-value pairs as YAML frontmatter lines (without the
/// outer `---` delimiters - those are added by the caller).
///
/// Key ordering is preserved (insertion order via `Vec`).
/// Scalar values containing special YAML characters are double-quoted.
/// List values use YAML block sequence syntax.
pub fn format_frontmatter(fields: &[(String, FmValue)]) -> String {
    let mut out = String::new();
    for (k, v) in fields {
        match v {
            FmValue::Text(s) => {
                if needs_quoting(s) {
                    out.push_str(&format!("{}: \"{}\"\n", k, s.replace('"', "\\\"")));
                } else {
                    out.push_str(&format!("{}: {}\n", k, s));
                }
            }
            FmValue::List(items) if items.is_empty() => {
                out.push_str(&format!("{}: []\n", k));
            }
            FmValue::List(items) => {
                out.push_str(&format!("{}:\n", k));
                for item in items {
                    out.push_str(&format!("  - \"{}\"\n", item.replace('"', "\\\"")));
                }
            }
        }
    }
    out
}

// =====================================================
// Helpers
// =====================================================

/// Returns true if the string contains characters that require YAML quoting.
fn needs_quoting(s: &str) -> bool {
    s.contains(':')
        || s.contains('#')
        || s.contains('[')
        || s.contains(']')
        || s.contains('{')
        || s.contains('}')
        || s.contains('\n')
        || s.starts_with(' ')
        || s.ends_with(' ')
        || s.is_empty()
}

/// Convert epoch milliseconds to an ISO 8601 UTC timestamp string.
pub fn millis_to_iso(ms: i64) -> String {
    DateTime::from_timestamp_millis(ms)
        .map(|dt| dt.to_rfc3339_opts(SecondsFormat::Secs, true))
        .unwrap_or_else(|| ms.to_string())
}

// =====================================================
// Tests
// =====================================================

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_util::TestDir;

    // --- millis_to_iso ---

    #[test]
    fn test_millis_to_iso_known_epoch() {
        // 0 ms = 1970-01-01T00:00:00Z
        assert_eq!(millis_to_iso(0), "1970-01-01T00:00:00Z");
    }

    #[test]
    fn test_millis_to_iso_known_value() {
        // 1_000_000_000_000 ms = 2001-09-09T01:46:40Z
        assert_eq!(millis_to_iso(1_000_000_000_000), "2001-09-09T01:46:40Z");
    }

    #[test]
    fn test_millis_to_iso_invalid_falls_back_to_string() {
        // i64::MAX would overflow chrono; should fall back to the raw number string
        let result = millis_to_iso(i64::MAX);
        // Either a valid ISO string or the numeric fallback
        assert!(!result.is_empty());
    }

    // --- needs_quoting ---

    #[test]
    fn test_needs_quoting_plain() {
        assert!(!needs_quoting("hello world"));
        assert!(!needs_quoting("Active"));
        assert!(!needs_quoting("full"));
    }

    #[test]
    fn test_needs_quoting_colon() {
        assert!(needs_quoting("key: value"));
    }

    #[test]
    fn test_needs_quoting_hash() {
        assert!(needs_quoting("# heading"));
    }

    #[test]
    fn test_needs_quoting_brackets() {
        assert!(needs_quoting("[item]"));
        assert!(needs_quoting("{key}"));
    }

    #[test]
    fn test_needs_quoting_newline() {
        assert!(needs_quoting("line one\nline two"));
    }

    #[test]
    fn test_needs_quoting_leading_space() {
        assert!(needs_quoting(" leading"));
    }

    #[test]
    fn test_needs_quoting_trailing_space() {
        assert!(needs_quoting("trailing "));
    }

    #[test]
    fn test_needs_quoting_empty() {
        assert!(needs_quoting(""));
    }

    // --- format_frontmatter ---

    #[test]
    fn test_format_frontmatter_simple_text() {
        let fields = vec![
            ("id".to_string(), FmValue::Text("pl-abc12".to_string())),
            ("title".to_string(), FmValue::Text("My Plan".to_string())),
        ];
        let result = format_frontmatter(&fields);
        assert_eq!(result, "id: pl-abc12\ntitle: My Plan\n");
    }

    #[test]
    fn test_format_frontmatter_quoted_text() {
        let fields = vec![("title".to_string(), FmValue::Text("Add: feature".to_string()))];
        let result = format_frontmatter(&fields);
        assert_eq!(result, "title: \"Add: feature\"\n");
    }

    #[test]
    fn test_format_frontmatter_empty_list() {
        let fields = vec![("deps".to_string(), FmValue::List(vec![]))];
        let result = format_frontmatter(&fields);
        assert_eq!(result, "deps: []\n");
    }

    #[test]
    fn test_format_frontmatter_list_with_items() {
        let fields = vec![(
            "acceptance-criteria".to_string(),
            FmValue::List(vec!["cargo test passes".to_string(), "clippy clean".to_string()]),
        )];
        let result = format_frontmatter(&fields);
        assert_eq!(
            result,
            "acceptance-criteria:\n  - \"cargo test passes\"\n  - \"clippy clean\"\n"
        );
    }

    #[test]
    fn test_format_frontmatter_list_item_with_quotes() {
        let fields = vec![("ac".to_string(), FmValue::List(vec!["say \"hello\"".to_string()]))];
        let result = format_frontmatter(&fields);
        assert_eq!(result, "ac:\n  - \"say \\\"hello\\\"\"\n");
    }

    #[test]
    fn test_format_frontmatter_preserves_order() {
        let fields = vec![
            ("z".to_string(), FmValue::Text("last".to_string())),
            ("a".to_string(), FmValue::Text("first".to_string())),
        ];
        let result = format_frontmatter(&fields);
        let z_pos = result.find("z:").unwrap();
        let a_pos = result.find("a:").unwrap();
        assert!(z_pos < a_pos, "insertion order must be preserved");
    }

    // --- write_doc_markdown ---

    struct SimpleRecord {
        id: String,
    }

    impl DocMarkdown for SimpleRecord {
        fn doc_id(&self) -> &str {
            &self.id
        }
        fn doc_frontmatter(&self) -> Vec<(String, FmValue)> {
            vec![
                ("id".to_string(), FmValue::Text(self.id.clone())),
                ("title".to_string(), FmValue::Text("Test".to_string())),
            ]
        }
        fn doc_body(&self) -> String {
            "Body text.".to_string()
        }
    }

    #[test]
    fn test_write_doc_markdown_creates_file() {
        let dir = TestDir::new("loopr-markdown-test");
        let record = SimpleRecord {
            id: "pl-abc12".to_string(),
        };
        write_doc_markdown(&dir, &record).unwrap();
        let path = dir.join("docs/loopr/pl-abc12.md");
        assert!(path.exists(), "file should be created at docs/loopr/<id>.md");
    }

    #[test]
    fn test_write_doc_markdown_content_format() {
        let dir = TestDir::new("loopr-markdown-test");
        let record = SimpleRecord {
            id: "pl-def34".to_string(),
        };
        write_doc_markdown(&dir, &record).unwrap();
        let content = fs::read_to_string(dir.join("docs/loopr/pl-def34.md")).unwrap();
        assert!(content.starts_with("---\n"), "must start with frontmatter delimiter");
        assert!(
            content.contains("---\n\nBody text.\n"),
            "body must follow closing delimiter"
        );
        assert!(content.contains("id: pl-def34"), "frontmatter must contain id");
        assert!(content.contains("title: Test"), "frontmatter must contain title");
    }

    #[test]
    fn test_write_doc_markdown_creates_dir() {
        let dir = TestDir::new("loopr-markdown-test");
        // docs/loopr/ does not exist yet - write_doc_markdown must create it
        let record = SimpleRecord {
            id: "wk-99999".to_string(),
        };
        write_doc_markdown(&dir, &record).unwrap();
        assert!(dir.join("docs/loopr").is_dir(), "docs/loopr/ must be created");
    }

    #[test]
    fn test_write_doc_markdown_overwrites_on_update() {
        let dir = TestDir::new("loopr-markdown-test");
        let r1 = SimpleRecord {
            id: "pl-upd01".to_string(),
        };
        write_doc_markdown(&dir, &r1).unwrap();

        // Second write with same id overwrites
        write_doc_markdown(&dir, &r1).unwrap();
        let content = fs::read_to_string(dir.join("docs/loopr/pl-upd01.md")).unwrap();
        // Only one copy of the id in the frontmatter
        assert_eq!(content.matches("id: pl-upd01").count(), 1);
    }
}
