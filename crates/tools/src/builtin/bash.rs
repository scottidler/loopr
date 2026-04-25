use std::path::PathBuf;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use tracing::instrument;
use tree_sitter::{Node, Tree};

use crate::error::ToolError;
use crate::lane::Lane;
use crate::shell::sh_command;
use crate::spawn::PersistConfig;
use crate::tool::ToolContext;

pub const DESCRIPTION: &str = "Run a bash command in a sandboxed subprocess. \
Each invocation is a fresh process - working directory changes (`cd x`) within ONE call persist \
for that call only; two consecutive calls do NOT share CWD. `&&`, `||`, `;`, pipelines, heredocs, \
and subshells are all supported. Known-bad commands (rm -rf /, sudo, git push, curl | sh) are \
rejected pre-flight via a denylist.";

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Input {
    pub command: String,
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

#[derive(Debug, Serialize)]
pub struct Output {
    pub stdout: String,
    pub stderr: String,
    pub combined_output: String,
    pub exit_code: i32,
    pub duration_ms: u64,
    pub timed_out: bool,
    pub truncated: bool,
    pub persisted_output_path: Option<PathBuf>,
}

#[instrument(
    name = "tool.bash",
    level = "debug",
    skip_all,
    fields(
        tool_name = "bash",
        lane = tracing::field::Empty,
        command_chars = input.command.len(),
        timeout_secs = ?input.timeout_secs,
        working_dir = %ctx.working_dir.display(),
    ),
    err,
)]
pub async fn execute(input: Input, ctx: &ToolContext) -> Result<Output, ToolError> {
    // Parse the command via tree-sitter-bash ONCE; reuse the CST for both
    // the denylist check (step 1) and the lane-routing decision (step 2).
    // Per the design doc Phase 3 flow: "parse command via tree-sitter-bash
    // ONCE -> BashDenylist::check(&tree) -> lane_for_command(&tree)". This
    // avoids parsing the same string twice on every bash invocation.
    let tree_opt = crate::denylist::parse_bash(&input.command);

    // 1. Denylist pre-flight. Unparseable fragments pass through and the
    //    subprocess will surface the parse error.
    if let Some(ref tree) = tree_opt
        && let Err(pat) = ctx.bash_denylist.check_tree(tree, &input.command)
    {
        return Err(ToolError::BashDenied {
            reason: pat.reason.clone(),
        });
    }

    // 2. Per-invocation lane routing (D8). If any `command` node's resolved
    //    head matches HEAVY_EXECUTABLES or HEAVY_PREFIXES, upgrade to Heavy.
    let lane = match &tree_opt {
        Some(tree) => lane_for_tree(tree, &input.command),
        None => Lane::Net,
    };
    tracing::Span::current().record("lane", lane.as_str());

    // 3. Build the shell command and hand off to the router.
    let cmd = sh_command(&input.command, &ctx.working_dir);
    let persist = PersistConfig {
        base: ctx.persist_base.as_deref(),
        invocation_id: ctx.invocation_id,
    };
    let result = ctx
        .router
        .spawn(cmd, lane, &ctx.working_dir, input.timeout_secs, persist)
        .await?;

    Ok(Output {
        stdout: result.stdout,
        stderr: result.stderr,
        combined_output: result.combined_output,
        exit_code: result.exit_code,
        duration_ms: result.duration_ms,
        timed_out: result.timed_out,
        truncated: result.truncated,
        persisted_output_path: result.persisted_output_path,
    })
}

/// Walk the parsed command; if any simple command's head resolves to an entry
/// in HEAVY_EXECUTABLES or matches HEAVY_PREFIXES, route Heavy. Otherwise Net.
/// `execute()` calls this with the tree it already parsed for the denylist
/// check, so we amortize parsing across both checks.
pub(crate) fn lane_for_tree(tree: &Tree, source: &str) -> Lane {
    if contains_heavy(tree, source) { Lane::Heavy } else { Lane::Net }
}

/// Test convenience: parse the command and classify in one call. Production
/// code calls `crate::denylist::parse_bash` + `lane_for_tree` to reuse the
/// tree across the denylist check and the lane decision.
#[cfg(test)]
pub(crate) fn classify_bash_command(command: &str) -> Lane {
    let Some(tree) = crate::denylist::parse_bash(command) else {
        return Lane::Net;
    };
    lane_for_tree(&tree, command)
}

/// Heavy executables: tools that routinely run for minutes (compilers, test
/// runners, container builders). Anything in this list routes the whole
/// invocation to the Heavy lane (1 slot, 1800s max timeout). See D8 / R3.
const HEAVY_EXECUTABLES: &[&str] = &[
    // Rust
    "cargo",
    "rustup",
    // Node / JS
    "npm",
    "npx",
    "pnpm",
    "yarn",
    "bun",
    "deno",
    "nvm",
    "tsc",
    "jest",
    "vitest",
    // Python
    "pytest",
    "black",
    "flake8",
    "uv",
    "pip",
    "pipx",
    "poetry",
    // Go
    "go",
    // Build systems
    "make",
    "cmake",
    "gradle",
    "mvn",
    "bazel",
    "just",
    "task",
    "otto",
    // Tool runners
    "mise",
    // Containers / infra
    "docker",
    "docker-compose",
    "kubectl",
    "terraform",
    "terragrunt",
    // Package managers
    "apt",
    "apt-get",
    "brew",
    "nix",
    "gem",
    "bundle",
];

/// Prefixes that route Heavy. Covers cargo subcommands installed as
/// `cargo-expand`, `cargo-nextest`, etc.
const HEAVY_PREFIXES: &[&str] = &["cargo-"];

fn contains_heavy(tree: &Tree, source: &str) -> bool {
    let root = tree.root_node();
    let mut stack = vec![root];
    while let Some(node) = stack.pop() {
        if node.kind() == "command"
            && let Some(head) = resolved_head(node, source)
        {
            let h = head.as_str();
            if HEAVY_EXECUTABLES.contains(&h) || HEAVY_PREFIXES.iter().any(|p| h.starts_with(p)) {
                return true;
            }
        }
        let mut cursor = node.walk();
        for child in node.children(&mut cursor) {
            stack.push(child);
        }
    }
    false
}

/// Extract the resolved command head from a `command` node. Strips
/// env-var-assignment prefixes (`RUST_LOG=debug cargo build` -> `cargo`),
/// strips `./` relative-path prefix, strips the leading directory component
/// (`./path/to/cargo` -> `cargo`), strips surrounding quotes.
fn resolved_head(node: Node<'_>, source: &str) -> Option<String> {
    let mut cursor = node.walk();
    for child in node.children(&mut cursor) {
        match child.kind() {
            "variable_assignment" => continue,
            "command_name" => {
                // command_name has one child (word/string/expansion); read its text.
                let mut inner = child.walk();
                for grand in child.children(&mut inner) {
                    if let Ok(text) = grand.utf8_text(source.as_bytes()) {
                        return Some(normalize_head(text));
                    }
                }
                // Fall back to reading the command_name node's own text.
                if let Ok(text) = child.utf8_text(source.as_bytes()) {
                    return Some(normalize_head(text));
                }
                return None;
            }
            _ => {
                // Could be raw word before command_name in unusual grammars; ignore.
            }
        }
    }
    None
}

fn normalize_head(s: &str) -> String {
    let mut t = s.trim();
    if (t.starts_with('"') && t.ends_with('"') && t.len() >= 2)
        || (t.starts_with('\'') && t.ends_with('\'') && t.len() >= 2)
    {
        t = &t[1..t.len() - 1];
    }
    let t = t.strip_prefix("./").unwrap_or(t);
    match t.rfind('/') {
        Some(idx) => t[idx + 1..].to_string(),
        None => t.to_string(),
    }
}

#[cfg(test)]
mod tests;
