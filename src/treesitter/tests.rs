use super::*;
use std::path::Path;

// ─── Language detection ─────────────────────────────────────────────────────

#[test]
fn detect_python() {
    assert_eq!(detect_language(Path::new("foo.py")), SourceLang::Python);
}

#[test]
fn detect_rust() {
    assert_eq!(detect_language(Path::new("lib.rs")), SourceLang::Rust);
}

#[test]
fn detect_javascript() {
    assert_eq!(detect_language(Path::new("index.js")), SourceLang::JavaScript);
    assert_eq!(detect_language(Path::new("app.jsx")), SourceLang::JavaScript);
}

#[test]
fn detect_typescript() {
    assert_eq!(detect_language(Path::new("main.ts")), SourceLang::TypeScript);
}

#[test]
fn detect_tsx() {
    assert_eq!(detect_language(Path::new("component.tsx")), SourceLang::Tsx);
}

#[test]
fn detect_unknown() {
    assert_eq!(detect_language(Path::new("config.yaml")), SourceLang::Unknown);
    assert_eq!(detect_language(Path::new("Makefile")), SourceLang::Unknown);
}

// ─── structural_summary: Python ─────────────────────────────────────────────

#[test]
fn summary_python_extracts_function_signatures() {
    let source = "\
def hello(name: str) -> str:
    return f\"Hello, {name}\"

def process(data: list[int], config: dict) -> int:
    total = sum(data)
    return total * config.get(\"factor\", 1)
";
    let result = structural_summary(source, SourceLang::Python, 200);
    assert!(result.contains("def hello(name: str) -> str:"), "got: {result}");
    assert!(
        result.contains("def process(data: list[int], config: dict) -> int:"),
        "got: {result}"
    );
    assert!(!result.contains("return f\"Hello"), "body should be excluded");
    assert!(!result.contains("total = sum"), "body should be excluded");
}

#[test]
fn summary_python_class_with_methods() {
    let source = "\
class DataProcessor:
    def __init__(self, config):
        self.config = config

    def process(self, item):
        return self.config.get(item)

    def validate(self, item):
        return item is not None
";
    let result = structural_summary(source, SourceLang::Python, 200);
    assert!(result.contains("class DataProcessor:"), "got: {result}");
    assert!(result.contains("def __init__(self, config):"), "got: {result}");
    assert!(result.contains("def process(self, item):"), "got: {result}");
    assert!(result.contains("def validate(self, item):"), "got: {result}");
    assert!(!result.contains("self.config = config"), "body should be excluded");
}

#[test]
fn summary_python_decorated_functions() {
    let source = "\
@cache
def expensive(x: int) -> int:
    return x * x

@app.route(\"/api\")
@login_required
def api_handler():
    return {\"status\": \"ok\"}
";
    let result = structural_summary(source, SourceLang::Python, 200);
    assert!(result.contains("@cache"), "got: {result}");
    assert!(result.contains("def expensive(x: int) -> int:"), "got: {result}");
    assert!(result.contains("@app.route"), "got: {result}");
    assert!(result.contains("@login_required"), "got: {result}");
    assert!(result.contains("def api_handler():"), "got: {result}");
    assert!(!result.contains("return x * x"), "body should be excluded");
}

// ─── structural_summary: Rust ───────────────────────────────────────────────

#[test]
fn summary_rust_extracts_fn_signatures() {
    let source = "\
pub fn process(items: &[Item]) -> Result<()> {
    for item in items {
        validate(item)?;
    }
    Ok(())
}

fn helper(x: i32) -> i32 {
    x + 1
}
";
    let result = structural_summary(source, SourceLang::Rust, 200);
    assert!(
        result.contains("pub fn process(items: &[Item]) -> Result<()>"),
        "got: {result}"
    );
    assert!(result.contains("fn helper(x: i32) -> i32"), "got: {result}");
    assert!(!result.contains("for item in items"), "body should be excluded");
}

#[test]
fn summary_rust_struct_and_enum() {
    let source = "\
pub struct Config {
    pub name: String,
    pub value: i32,
}

pub enum Status {
    Active,
    Inactive,
}
";
    let result = structural_summary(source, SourceLang::Rust, 200);
    assert!(result.contains("pub struct Config"), "got: {result}");
    assert!(result.contains("pub enum Status"), "got: {result}");
}

#[test]
fn summary_rust_impl_with_methods() {
    let source = "\
impl Config {
    pub fn new(name: String) -> Self {
        Config { name, value: 0 }
    }

    pub fn validate(&self) -> bool {
        !self.name.is_empty()
    }
}
";
    let result = structural_summary(source, SourceLang::Rust, 200);
    assert!(result.contains("impl Config"), "got: {result}");
    assert!(result.contains("pub fn new(name: String) -> Self"), "got: {result}");
    assert!(result.contains("pub fn validate(&self) -> bool"), "got: {result}");
    assert!(!result.contains("Config { name, value: 0 }"), "body should be excluded");
}

// ─── truncate_at_boundary ───────────────────────────────────────────────────

#[test]
fn truncate_never_cuts_mid_function() {
    let source = "\
fn short() {
    1
}

fn long() {
    let a = 1;
    let b = 2;
    let c = 3;
    let d = 4;
    let e = 5;
    a + b + c + d + e
}
";
    // short() is 3 lines (1-3), blank line at 4, long() starts at 5
    // With max_lines=5, should include short() + blank but not long()
    let result = truncate_at_boundary(source, SourceLang::Rust, 5);
    assert!(result.contains("fn short()"), "got: {result}");
    assert!(!result.contains("fn long()"), "should not include long()");
}

#[test]
fn truncate_includes_complete_nodes() {
    let source = "\
fn a() {
    1
}

fn b() {
    2
}
";
    // a() ends at line 3, blank at 4, b() ends at line 7
    let result = truncate_at_boundary(source, SourceLang::Rust, 8);
    assert!(result.contains("fn a()"), "got: {result}");
    assert!(result.contains("fn b()"), "got: {result}");
}

// ─── Edge cases ─────────────────────────────────────────────────────────────

#[test]
fn unknown_language_falls_back_to_line_truncation() {
    let source = "line1\nline2\nline3\nline4\nline5\n";
    let result = structural_summary(source, SourceLang::Unknown, 3);
    assert_eq!(result, "line1\nline2\nline3");
}

#[test]
fn empty_file_returns_empty_string() {
    assert_eq!(structural_summary("", SourceLang::Python, 200), "");
    assert_eq!(truncate_at_boundary("", SourceLang::Rust, 200), "");
}

#[test]
fn malformed_source_falls_back() {
    // Severely malformed Python that tree-sitter can still partially parse
    // but with zero recognized definitions - should produce empty or fallback
    let source = "}{}{][not valid anything at all";
    let result = structural_summary(source, SourceLang::Python, 200);
    // Should not panic - either empty (no definitions found) or fallback
    assert!(result.is_empty() || result.len() <= source.len());
}

#[test]
fn summary_respects_max_lines() {
    let source = "\
def a():
    pass

def b():
    pass

def c():
    pass

def d():
    pass

def e():
    pass
";
    let result = structural_summary(source, SourceLang::Python, 3);
    let line_count = result.lines().count();
    assert!(
        line_count <= 3,
        "expected at most 3 lines, got {line_count}: {result:?}"
    );
}

#[test]
fn truncate_boundary_unknown_falls_back() {
    let source = "line1\nline2\nline3\nline4\n";
    let result = truncate_at_boundary(source, SourceLang::Unknown, 2);
    assert_eq!(result, "line1\nline2");
}

// ─── JavaScript/TypeScript ──────────────────────────────────────────────────

#[test]
fn summary_javascript_functions() {
    let source = "\
function greet(name) {
    return `Hello, ${name}`;
}

function process(data, config) {
    return data.map(x => x * config.factor);
}
";
    let result = structural_summary(source, SourceLang::JavaScript, 200);
    assert!(result.contains("function greet(name)"), "got: {result}");
    assert!(result.contains("function process(data, config)"), "got: {result}");
    assert!(!result.contains("return `Hello"), "body should be excluded");
}

#[test]
fn summary_typescript_interface() {
    let source = "\
interface Config {
    name: string;
    value: number;
    process(item: Item): boolean;
}
";
    let result = structural_summary(source, SourceLang::TypeScript, 200);
    assert!(result.contains("interface Config"), "got: {result}");
}
