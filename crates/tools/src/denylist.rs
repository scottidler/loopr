use std::borrow::Cow;

use tree_sitter::{Node, Parser, Tree};

use crate::config::{DenyEntryConfig, ToolsConfig};

#[derive(Debug, Clone)]
pub enum TokenMatcher {
    Literal(Cow<'static, str>),
    Prefix(Cow<'static, str>),
    Any,
}

#[derive(Debug, Clone)]
pub struct DenyPattern {
    pub tokens: Vec<TokenMatcher>,
    pub reason: String,
}

impl DenyPattern {
    fn matches(&self, argv: &[String]) -> bool {
        if self.tokens.len() > argv.len() {
            return false;
        }
        // Contiguous subsequence match anywhere in argv.
        for start in 0..=argv.len() - self.tokens.len() {
            if self
                .tokens
                .iter()
                .zip(argv[start..].iter())
                .all(|(m, t)| matcher_matches(m, t))
            {
                return true;
            }
        }
        false
    }

    fn from_config(entry: &DenyEntryConfig) -> Self {
        let tokens = entry
            .tokens
            .iter()
            .map(|t| {
                if t == "*" {
                    TokenMatcher::Any
                } else if let Some(prefix) = t.strip_suffix('*') {
                    TokenMatcher::Prefix(Cow::Owned(prefix.to_string()))
                } else {
                    TokenMatcher::Literal(Cow::Owned(t.clone()))
                }
            })
            .collect();
        Self {
            tokens,
            reason: entry.reason.clone(),
        }
    }
}

fn matcher_matches(m: &TokenMatcher, tok: &str) -> bool {
    match m {
        TokenMatcher::Literal(s) => s == tok,
        TokenMatcher::Prefix(s) => tok.starts_with(s.as_ref()),
        TokenMatcher::Any => true,
    }
}

pub struct BashDenylist {
    patterns: Vec<DenyPattern>,
}

impl BashDenylist {
    /// Base patterns from the vision doc. Structurally tighten-only via config.
    pub fn with_base() -> Self {
        Self { patterns: base() }
    }

    pub fn patterns(&self) -> &[DenyPattern] {
        &self.patterns
    }

    pub fn extend_from(&mut self, cfg: &ToolsConfig) {
        for entry in &cfg.bash_denylist_extend {
            self.patterns.push(DenyPattern::from_config(entry));
        }
    }

    /// Parse the command string via tree-sitter-bash and check every simple
    /// command node's `argv` against every pattern.
    ///
    /// Returns `Err(&DenyPattern)` on first match so the caller can surface
    /// the `reason`. Quoted strings in the source (e.g. `echo "git push"`)
    /// appear as a single `string` node rather than two argv tokens, so
    /// substring false positives that plagued v4 do not trigger.
    pub fn check(&self, command: &str) -> Result<(), &DenyPattern> {
        let tree = match parse(command) {
            Some(t) => t,
            None => {
                // Unparseable fragment: no way to inspect argv. Err on the
                // side of letting the spawn layer handle it - the subprocess
                // will fail with a parse error too.
                return Ok(());
            }
        };
        self.check_tree(&tree, command)
    }

    /// Same as `check`, but reuses a pre-parsed `Tree`. Bash::execute parses
    /// the command once and calls this + `lane_for_tree` both, per the
    /// design doc ("parse command via tree-sitter-bash ONCE, reuse the
    /// tree"). Re-parsing inside `check` would double the tree-sitter cost
    /// for every bash invocation.
    pub fn check_tree(&self, tree: &Tree, source: &str) -> Result<(), &DenyPattern> {
        // Structural check: any `sh` / `bash` as a bare command inside a
        // pipeline is the canonical `curl X | sh` footgun. Because "|" is a
        // pipeline node in the CST rather than an argv token, a plain token
        // pattern like `["|", "sh"]` cannot catch it - this is the
        // CST-aware equivalent, and the reason the design doc chose
        // tree-sitter over substring matching.
        if pipe_to_shell(tree, source) {
            return Err(&self.patterns[PIPE_TO_SHELL_IDX]);
        }

        let commands = collect_commands(tree, source);
        for argv in &commands {
            // Skip the synthetic pipe-to-shell pattern in token matching;
            // it only fires via the structural check above.
            for (i, pat) in self.patterns.iter().enumerate() {
                if i == PIPE_TO_SHELL_IDX {
                    continue;
                }
                if pat.matches(argv) {
                    return Err(pat);
                }
            }
        }
        Ok(())
    }
}

pub fn parse_bash(source: &str) -> Option<Tree> {
    parse(source)
}

/// Index of the synthetic pipe-to-shell pattern inside `base()`.
/// Checked via structural CST walk rather than argv-token matching.
const PIPE_TO_SHELL_IDX: usize = 0;

fn base() -> Vec<DenyPattern> {
    use TokenMatcher::*;
    vec![
        // Position 0 is reserved for the pipe-to-shell synthetic pattern.
        // `tokens` is unused at match time (short-circuited in check()); the
        // `reason` is what callers surface.
        DenyPattern {
            tokens: vec![Literal("|sh".into())],
            reason: "piped shell execution".into(),
        },
        DenyPattern {
            tokens: vec![Literal("rm".into()), Literal("-rf".into()), Literal("/".into())],
            reason: "deletes root filesystem".into(),
        },
        DenyPattern {
            tokens: vec![Literal("rm".into()), Literal("-rf".into()), Literal("~".into())],
            reason: "deletes home directory".into(),
        },
        DenyPattern {
            tokens: vec![Literal("sudo".into()), Any],
            reason: "privilege escalation".into(),
        },
        DenyPattern {
            tokens: vec![Literal("git".into()), Literal("push".into())],
            reason: "push policy is human-only".into(),
        },
        DenyPattern {
            tokens: vec![Literal("gh".into()), Literal("repo".into()), Literal("delete".into())],
            reason: "destructive github op".into(),
        },
    ]
}

/// Detect the `curl X | sh` / `wget X | bash` pattern at the CST level.
///
/// Any `command` node with `command_name == "sh" | "bash"` whose argv is the
/// program alone (i.e. no positional args like `bash script.sh`) and that
/// lives inside a `pipeline` ancestor is flagged. The `-c` form
/// (`| sh -c '...'`) is also flagged - piping content into `sh -c` is the
/// same class of footgun.
fn pipe_to_shell(tree: &Tree, source: &str) -> bool {
    let root = tree.root_node();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.kind() == "pipeline" {
            let mut cursor = node.walk();
            for child in node.children(&mut cursor) {
                if is_shell_sink_command(child, source) {
                    return true;
                }
            }
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            stack.push(child);
        }
    }
    false
}

fn is_shell_sink_command(node: Node<'_>, source: &str) -> bool {
    if node.kind() != "command" {
        return false;
    }
    let argv = match argv_for_command(node, source) {
        Some(a) => a,
        None => return false,
    };
    matches!(argv.first().map(String::as_str), Some("sh") | Some("bash"))
}

fn parser() -> Parser {
    let mut p = Parser::new();
    let lang = tree_sitter_bash::LANGUAGE;
    p.set_language(&lang.into())
        .expect("tree-sitter-bash grammar must load");
    p
}

fn parse(source: &str) -> Option<Tree> {
    parser().parse(source, None)
}

/// Walk the CST; for every node kind `command`, read its children and
/// build an argv `Vec<String>`. A `command` node's structure per
/// tree-sitter-bash grammar:
///
/// - optional `variable_assignment` prefix nodes (e.g. `RUST_LOG=debug`)
/// - one `command_name` child
/// - zero or more `word` / `string` / `concatenation` / `simple_expansion`
///   children (the argv tail)
///
/// We skip variable-assignment prefixes (they are env metadata, not argv),
/// read `command_name` as argv\[0\], and then read every remaining non-keyword
/// sibling as its source-text representation.
///
/// Pipelines (`a | b`), lists (`a && b`, `a; b`), subshells (`(a)`), and
/// heredocs are all handled by collecting every `command` node in the tree
/// regardless of where it sits.
fn collect_commands(tree: &Tree, source: &str) -> Vec<Vec<String>> {
    let mut out: Vec<Vec<String>> = Vec::new();
    walk(tree.root_node(), source, &mut out);
    out
}

fn walk(node: Node<'_>, source: &str, out: &mut Vec<Vec<String>>) {
    if node.kind() == "command"
        && let Some(argv) = argv_for_command(node, source)
    {
        out.push(argv);
    }
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        walk(child, source, out);
    }
}

fn argv_for_command(node: Node<'_>, source: &str) -> Option<Vec<String>> {
    let mut argv = Vec::new();
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "variable_assignment" => {
                // Env-var prefix like FOO=bar; skip per D8.
            }
            "command_name" | "word" | "string" | "raw_string" | "concatenation" | "simple_expansion" | "expansion"
            | "number" => {
                if let Some(text) = argv_text(child, source) {
                    argv.push(text);
                }
            }
            _ => {
                // Unknown child kind: ignore. tree-sitter-bash grammar may
                // produce nodes we don't recognize; conservative is to skip
                // them (we'd rather miss one argv token than false-positive).
            }
        }
    }
    if argv.is_empty() { None } else { Some(argv) }
}

fn argv_text(node: Node<'_>, source: &str) -> Option<String> {
    let text = node.utf8_text(source.as_bytes()).ok()?.to_string();
    // Strip surrounding matching quotes only for direct string literals - the
    // quoted content of `"git push"` should appear as one token `git push`,
    // NOT split into two. Denylist patterns target tokens at the argv level,
    // not the source level.
    Some(strip_quotes(&text))
}

fn strip_quotes(s: &str) -> String {
    let bytes = s.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            return s[1..s.len() - 1].to_string();
        }
    }
    s.to_string()
}

#[cfg(test)]
mod tests;
