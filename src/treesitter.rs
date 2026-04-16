//! AST-aware file chunking using tree-sitter.
//!
//! Two modes:
//! - `structural_summary`: extracts function/class/struct signatures without bodies.
//!   Compact representation for decomposer context.
//! - `truncate_at_boundary`: returns complete top-level nodes up to a line limit.
//!   For agents that need actual code without mid-function cuts.

use std::path::Path;

use tree_sitter::{Node, Parser};

// ─── Language detection ─────────────────────────────────────────────────────

/// Supported languages for AST-aware chunking.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SourceLang {
    Python,
    Rust,
    JavaScript,
    TypeScript,
    Tsx,
    Unknown,
}

/// Detect language from file extension.
pub fn detect_language(path: &Path) -> SourceLang {
    match path.extension().and_then(|e| e.to_str()) {
        Some("py") => SourceLang::Python,
        Some("rs") => SourceLang::Rust,
        Some("js" | "jsx") => SourceLang::JavaScript,
        Some("ts") => SourceLang::TypeScript,
        Some("tsx") => SourceLang::Tsx,
        _ => SourceLang::Unknown,
    }
}

fn get_ts_language(lang: SourceLang) -> Option<tree_sitter::Language> {
    match lang {
        SourceLang::Python => Some(tree_sitter_python::LANGUAGE.into()),
        SourceLang::Rust => Some(tree_sitter_rust::LANGUAGE.into()),
        SourceLang::JavaScript => Some(tree_sitter_javascript::LANGUAGE.into()),
        SourceLang::TypeScript => Some(tree_sitter_typescript::LANGUAGE_TYPESCRIPT.into()),
        SourceLang::Tsx => Some(tree_sitter_typescript::LANGUAGE_TSX.into()),
        SourceLang::Unknown => None,
    }
}

fn parse_source(content: &str, lang: SourceLang) -> Option<tree_sitter::Tree> {
    let ts_lang = get_ts_language(lang)?;
    let mut parser = Parser::new();
    parser.set_language(&ts_lang).ok()?;
    parser.parse(content, None)
}

// ─── Node classification ────────────────────────────────────────────────────

fn is_definition(kind: &str, lang: SourceLang) -> bool {
    match lang {
        SourceLang::Python => matches!(
            kind,
            "function_definition" | "class_definition" | "decorated_definition"
        ),
        SourceLang::Rust => matches!(
            kind,
            "function_item"
                | "struct_item"
                | "enum_item"
                | "impl_item"
                | "trait_item"
                | "const_item"
                | "static_item"
                | "type_item"
                | "macro_definition"
        ),
        SourceLang::JavaScript | SourceLang::TypeScript | SourceLang::Tsx => matches!(
            kind,
            "function_declaration"
                | "class_declaration"
                | "export_statement"
                | "lexical_declaration"
                | "interface_declaration"
                | "type_alias_declaration"
                | "enum_declaration"
        ),
        SourceLang::Unknown => false,
    }
}

fn is_container(kind: &str, lang: SourceLang) -> bool {
    match lang {
        SourceLang::Python => matches!(kind, "class_definition" | "decorated_definition"),
        SourceLang::Rust => matches!(kind, "impl_item" | "trait_item"),
        SourceLang::JavaScript | SourceLang::TypeScript | SourceLang::Tsx => {
            matches!(kind, "class_declaration" | "interface_declaration")
        }
        SourceLang::Unknown => false,
    }
}

fn is_nested_definition(kind: &str, lang: SourceLang) -> bool {
    match lang {
        SourceLang::Python => matches!(kind, "function_definition" | "decorated_definition"),
        SourceLang::Rust => matches!(kind, "function_item" | "const_item" | "type_item"),
        SourceLang::JavaScript | SourceLang::TypeScript | SourceLang::Tsx => matches!(
            kind,
            "method_definition" | "public_field_definition" | "property_signature" | "method_signature"
        ),
        SourceLang::Unknown => false,
    }
}

// ─── Signature extraction ───────────────────────────────────────────────────

/// Extract the signature portion of a definition node.
/// Uses the tree-sitter "body" field to find where the body starts,
/// then returns everything before it. Falls back to text heuristics.
fn node_signature(node: &Node, source: &[u8]) -> String {
    // Handle export_statement by unwrapping to inner declaration
    if node.kind() == "export_statement" {
        if let Some(decl) = node.child_by_field_name("declaration") {
            let inner = node_signature(&decl, source);
            return format!("export {}", inner);
        }
        return first_line(node, source);
    }

    // Handle decorated_definition by extracting decorators + inner signature
    if node.kind() == "decorated_definition" {
        return decorated_signature(node, source);
    }

    // Try the "body" field - works for functions, classes, impls, traits
    if let Some(body) = node.child_by_field_name("body") {
        let text = &source[node.start_byte()..body.start_byte()];
        return String::from_utf8_lossy(text).trim_end().to_string();
    }

    // No body field: take text up to first `{` or `;` (structs, enums, type aliases)
    let text = node.utf8_text(source).unwrap_or("");
    signature_until_delimiter(text)
}

fn decorated_signature(node: &Node, source: &[u8]) -> String {
    let mut parts = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        if child.kind() == "decorator"
            && let Ok(text) = child.utf8_text(source)
        {
            parts.push(text.to_string());
        }
    }
    if let Some(definition) = node.child_by_field_name("definition") {
        parts.push(node_signature(&definition, source));
    }
    parts.join("\n")
}

fn first_line(node: &Node, source: &[u8]) -> String {
    node.utf8_text(source)
        .unwrap_or("")
        .lines()
        .next()
        .unwrap_or("")
        .to_string()
}

fn signature_until_delimiter(text: &str) -> String {
    for (i, ch) in text.char_indices() {
        if ch == '{' || ch == ';' {
            return text[..=i].trim_end().to_string();
        }
    }
    text.lines().next().unwrap_or("").to_string()
}

/// Extract method/field signatures from inside a container node.
fn nested_signatures(node: &Node, source: &[u8], lang: SourceLang) -> Vec<String> {
    let body = if node.kind() == "decorated_definition" {
        node.child_by_field_name("definition")
            .and_then(|def| def.child_by_field_name("body"))
    } else {
        node.child_by_field_name("body")
    };

    let Some(body) = body else {
        return Vec::new();
    };

    let mut sigs = Vec::new();
    let mut cursor = body.walk();
    for child in body.children(&mut cursor) {
        if is_nested_definition(child.kind(), lang) {
            let sig = node_signature(&child, source);
            sigs.push(format!("    {}", sig.trim()));
        }
    }
    sigs
}

// ─── Public API ─────────────────────────────────────────────────────────────

fn line_truncation(content: &str, max_lines: usize) -> String {
    content.lines().take(max_lines).collect::<Vec<_>>().join("\n")
}

/// Extract top-level definitions as a structural summary suitable for prompt injection.
/// For each definition: signature without body. For containers (class, impl, trait):
/// also includes indented method signatures.
/// Falls back to line-based truncation for unsupported languages or parse failures.
pub fn structural_summary(content: &str, lang: SourceLang, max_lines: usize) -> String {
    if content.is_empty() {
        return String::new();
    }

    let Some(tree) = parse_source(content, lang) else {
        return line_truncation(content, max_lines);
    };

    let root = tree.root_node();
    let source = content.as_bytes();
    let mut output: Vec<String> = Vec::new();
    let mut line_count = 0;

    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        if line_count >= max_lines {
            break;
        }

        if !is_definition(child.kind(), lang) {
            continue;
        }

        let sig = node_signature(&child, source);
        let sig_lines = sig.lines().count().max(1);
        if line_count + sig_lines > max_lines {
            break;
        }
        output.push(sig);
        line_count += sig_lines;

        if is_container(child.kind(), lang) {
            for nested in nested_signatures(&child, source, lang) {
                if line_count >= max_lines {
                    break;
                }
                let n_lines = nested.lines().count().max(1);
                if line_count + n_lines > max_lines {
                    break;
                }
                output.push(nested);
                line_count += n_lines;
            }
        }
    }

    if output.is_empty() {
        return line_truncation(content, max_lines);
    }

    output.join("\n")
}

/// Truncate file content at the nearest complete top-level node boundary.
/// Never cuts mid-function or mid-class.
/// Falls back to line-based truncation for unsupported languages or parse failures.
pub fn truncate_at_boundary(content: &str, lang: SourceLang, max_lines: usize) -> String {
    if content.is_empty() {
        return String::new();
    }

    let Some(tree) = parse_source(content, lang) else {
        return line_truncation(content, max_lines);
    };

    let root = tree.root_node();
    let mut last_end = 0usize;

    let mut cursor = root.walk();
    for child in root.children(&mut cursor) {
        // end_position().row is 0-based; +1 converts to line count
        let end_line = child.end_position().row + 1;
        if end_line > max_lines {
            break;
        }
        last_end = child.end_byte();
    }

    if last_end == 0 {
        return line_truncation(content, max_lines);
    }

    content[..last_end].to_string()
}

#[allow(clippy::unwrap_used)]
#[cfg(test)]
mod tests;
