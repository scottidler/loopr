use std::collections::HashSet;

use syn::{Error, Ident, Result};

use super::parse::{EdgeIr, FsmIr};

pub(crate) fn validate(ir: &FsmIr) -> Result<()> {
    let variant_set: HashSet<String> = ir.variants.iter().map(|v| v.to_string()).collect();

    for t in &ir.terminal {
        if !variant_set.contains(&t.to_string()) {
            return Err(Error::new(t.span(), format!("`{}` is not a variant of the enum", t)));
        }
    }

    check_no_duplicate_terminal(&ir.terminal)?;
    check_edges(&ir.transitions, &variant_set, "transitions")?;
    check_edges(&ir.overrides, &variant_set, "overrides")?;
    check_terminal_has_no_outgoing(&ir.terminal, &ir.transitions)?;
    check_terminal_has_no_outgoing(&ir.terminal, &ir.overrides)?;

    Ok(())
}

fn check_no_duplicate_terminal(terminal: &[Ident]) -> Result<()> {
    let mut seen: HashSet<String> = HashSet::new();
    for t in terminal {
        if !seen.insert(t.to_string()) {
            return Err(Error::new(t.span(), format!("duplicate terminal state `{}`", t)));
        }
    }
    Ok(())
}

fn check_edges(edges: &[EdgeIr], variant_set: &HashSet<String>, table: &str) -> Result<()> {
    let mut seen: HashSet<(String, String)> = HashSet::new();
    for edge in edges {
        if !variant_set.contains(&edge.from.to_string()) {
            return Err(Error::new(
                edge.from.span(),
                format!(
                    "`{}` (left side of `=>` in {}) is not a variant of the enum",
                    edge.from, table
                ),
            ));
        }
        if !variant_set.contains(&edge.to.to_string()) {
            return Err(Error::new(
                edge.to.span(),
                format!(
                    "`{}` (right side of `=>` in {}) is not a variant of the enum",
                    edge.to, table
                ),
            ));
        }
        if edge.from == edge.to {
            return Err(Error::new(
                edge.from.span(),
                format!(
                    "self-loop `{} => {}` in {}; remove this entry, self-transitions are implicit",
                    edge.from, edge.to, table
                ),
            ));
        }
        let key = (edge.from.to_string(), edge.to.to_string());
        if !seen.insert(key) {
            return Err(Error::new(
                edge.from.span(),
                format!(
                    "duplicate edge `{} => {}` in {}; merge role lists into a single entry",
                    edge.from, edge.to, table
                ),
            ));
        }
    }
    Ok(())
}

fn check_terminal_has_no_outgoing(terminal: &[Ident], edges: &[EdgeIr]) -> Result<()> {
    let terminal_set: HashSet<String> = terminal.iter().map(|t| t.to_string()).collect();
    for edge in edges {
        if terminal_set.contains(&edge.from.to_string()) {
            return Err(Error::new(
                edge.from.span(),
                format!("terminal state `{}` cannot have outgoing transitions", edge.from),
            ));
        }
    }
    Ok(())
}
