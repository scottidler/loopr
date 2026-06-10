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
        self.check_inner(&tree, command, 0)
    }

    /// Same as `check`, but reuses a pre-parsed `Tree`. Bash::execute parses
    /// the command once and calls this + `lane_for_tree` both, per the
    /// design doc ("parse command via tree-sitter-bash ONCE, reuse the
    /// tree"). Re-parsing inside `check` would double the tree-sitter cost
    /// for every bash invocation.
    pub fn check_tree(&self, tree: &Tree, source: &str) -> Result<(), &DenyPattern> {
        self.check_inner(tree, source, 0)
    }

    /// Core check. `depth` bounds the recursion into `sh|bash|zsh -c <payload>`
    /// re-parsing (Phase-5 finding 1: `bash -c "<denied>"` previously bypassed
    /// the entire denylist).
    fn check_inner(&self, tree: &Tree, source: &str, depth: usize) -> Result<(), &DenyPattern> {
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
            // `argv_norm` differs from `argv` only at index 0, whose path
            // components and `./` prefix are stripped to a basename (finding
            // 2: `/usr/bin/git push` previously bypassed the `git push`
            // pattern). Matching against BOTH preserves user-extension
            // patterns written as literal paths (`./deploy.sh`) while
            // catching absolute-path invocations of built-in denials.
            let argv_norm = normalized_argv(argv);

            // Structural `rm` matching (finding 3): `rm -fr /`, `rm -r -f /`,
            // `rm -rf /*`, `--recursive --force ~` all map to the
            // root/home-deletion reasons regardless of flag ordering or
            // grouping. The literal `rm -rf /` / `rm -rf ~` patterns it
            // replaces are kept in `base()` only as the reason carriers.
            if let Some(idx) = dangerous_rm(&argv_norm) {
                return Err(&self.patterns[idx]);
            }

            for (i, pat) in self.patterns.iter().enumerate() {
                // Skip the synthetic patterns (pipe-to-shell + the two rm
                // reason carriers); they only fire via the structural checks
                // above.
                if SYNTHETIC_IDXS.contains(&i) {
                    continue;
                }
                if pat.matches(argv) || pat.matches(&argv_norm) {
                    return Err(pat);
                }
            }

            // Recurse into `sh|bash|zsh -c <payload>` (finding 1). The payload
            // is a single argv token (quotes already stripped by `argv_text`);
            // re-parse it and check its commands too.
            if depth < MAX_SHELL_C_DEPTH
                && let Some(pidx) = shell_c_index(&argv_norm)
                && let Some(payload) = argv.get(pidx)
                && let Some(sub) = parse(payload)
            {
                self.check_inner(&sub, payload, depth + 1)?;
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
/// Index of the `rm`-deletes-root reason carrier (matched structurally).
const RM_ROOT_IDX: usize = 1;
/// Index of the `rm`-deletes-home reason carrier (matched structurally).
const RM_HOME_IDX: usize = 2;
/// Pattern indices whose `tokens` are NOT used at match time - they fire
/// via the structural checks (`pipe_to_shell`, `dangerous_rm`) and carry
/// only their `reason`.
const SYNTHETIC_IDXS: [usize; 3] = [PIPE_TO_SHELL_IDX, RM_ROOT_IDX, RM_HOME_IDX];
/// Recursion cap for `sh|bash|zsh -c <payload>` re-parsing. A pathological
/// `bash -c "bash -c \"...\""` nest terminates here rather than spinning.
const MAX_SHELL_C_DEPTH: usize = 8;

/// Copy `argv` with index 0 reduced to its command basename: strip a leading
/// `./`, then everything up to and including the last `/`. Quotes are already
/// stripped upstream by `argv_text`. Other tokens are untouched.
fn normalized_argv(argv: &[String]) -> Vec<String> {
    let mut out = argv.to_vec();
    if let Some(head) = out.first_mut() {
        *head = basename(head);
    }
    out
}

fn basename(s: &str) -> String {
    let s = s.strip_prefix("./").unwrap_or(s);
    match s.rfind('/') {
        Some(idx) => s[idx + 1..].to_string(),
        None => s.to_string(),
    }
}

/// If `argv` (with a normalized head) is `sh`/`bash`/`zsh ... -c <payload>`,
/// return the index of the `<payload>` token so the caller can re-parse it.
fn shell_c_index(argv: &[String]) -> Option<usize> {
    let head = argv.first()?;
    if !matches!(head.as_str(), "sh" | "bash" | "zsh") {
        return None;
    }
    let pos = argv.iter().position(|a| a == "-c")?;
    let payload_idx = pos + 1;
    argv.get(payload_idx).map(|_| payload_idx)
}

/// Structural `rm` danger check. Returns the reason-carrier pattern index when
/// `argv` (normalized head) is an `rm` invocation that combines recursive
/// (`-r`/`-R`/`--recursive`) AND force (`-f`/`--force`) flags - in any order
/// or grouping - against a catastrophic target (`/`, `/*`, `~`, `~/...`,
/// `$HOME`, `$HOME/...`). Plain `rm file.txt` and `rm -rf ./build` do not trip.
fn dangerous_rm(argv: &[String]) -> Option<usize> {
    if argv.first().map(String::as_str) != Some("rm") {
        return None;
    }
    let mut recursive = false;
    let mut force = false;
    let mut danger: Option<usize> = None;
    for tok in &argv[1..] {
        if let Some(long) = tok.strip_prefix("--") {
            match long {
                "recursive" => recursive = true,
                "force" => force = true,
                _ => {}
            }
        } else if tok.len() > 1
            && let Some(short) = tok.strip_prefix('-')
        {
            for c in short.chars() {
                match c {
                    'r' | 'R' => recursive = true,
                    'f' => force = true,
                    _ => {}
                }
            }
        } else if danger.is_none() {
            danger = dangerous_rm_target(tok);
        }
    }
    if recursive && force { danger } else { None }
}

fn dangerous_rm_target(tok: &str) -> Option<usize> {
    if tok == "/" || tok == "/*" {
        Some(RM_ROOT_IDX)
    } else if tok == "~" || tok == "$HOME" || tok.starts_with("~/") || tok.starts_with("$HOME/") {
        Some(RM_HOME_IDX)
    } else {
        None
    }
}

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
        // RM_ROOT_IDX / RM_HOME_IDX: reason carriers only. `tokens` is unused
        // at match time (matched structurally via `dangerous_rm`); the
        // `reason` is what callers surface.
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
    // Normalize the head so `/bin/sh` / `./sh` are caught too.
    let head = argv.first().map(|h| basename(h));
    matches!(head.as_deref(), Some("sh") | Some("bash"))
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
