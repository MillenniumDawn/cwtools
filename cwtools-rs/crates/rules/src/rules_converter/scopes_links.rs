//! Scope and link extraction from `scopes = { ... }` (scopes.cwt) and
//! `links = { ... }` (links.cwt), plus the top-level `modifiers` name list.

use super::*;

/// Collect modifier names from a top-level `modifiers = { name = category ... }`
/// block. Each entry's key is a valid modifier name (the value is its category).
pub(crate) fn extract_modifier_names(
    children: &Vec<Child>,
    ast: &ParsedFile,
    table: &StringTable,
    ruleset: &mut RuleSet,
) {
    for child in children {
        let name = match child {
            Child::Leaf(lidx) => table
                .get_string(ast.arena.leaves[*lidx as usize].key.normal)
                .unwrap_or_default(),
            Child::Node(nidx) => table
                .get_string(ast.arena.nodes[*nidx as usize].key.normal)
                .unwrap_or_default(),
            _ => continue,
        };
        if !name.is_empty() {
            ruleset.modifiers.push(name);
        }
    }
}

/// The `(key, body-children)` of a `key = { ... }` config entry, stored by this
/// parser as either a `Node` or a `Leaf` with a `Clause` value. Key is unquoted.
fn entry_body<'a>(
    child: &Child,
    ast: &'a ParsedFile,
    table: &StringTable,
) -> Option<(String, &'a [Child])> {
    match child {
        Child::Node(nidx) => {
            let n = &ast.arena.nodes[*nidx as usize];
            Some((
                table.get_string(n.key.normal).unwrap_or_default(),
                n.children.as_slice(),
            ))
        }
        Child::Leaf(lidx) => {
            let l = &ast.arena.leaves[*lidx as usize];
            if let Value::Clause(ch) = &l.value {
                Some((
                    table.get_string(l.key.normal).unwrap_or_default(),
                    ch.as_slice(),
                ))
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Bare values inside a child `key = { a b c }` clause (e.g. `aliases`, `input_scopes`).
fn child_clause_values(
    children: &[Child],
    ast: &ParsedFile,
    table: &StringTable,
    key: &str,
) -> Vec<String> {
    for child in children {
        match child {
            Child::Leaf(lidx) => {
                let l = &ast.arena.leaves[*lidx as usize];
                if table.get_string(l.key.normal).unwrap_or_default() == key {
                    return collect_leaf_values_from_clause(&l.value, ast, table);
                }
            }
            Child::Node(nidx) => {
                let n = &ast.arena.nodes[*nidx as usize];
                if table.get_string(n.key.normal).unwrap_or_default() == key {
                    return collect_leaf_values_from_children(&n.children, ast, table);
                }
            }
            _ => {}
        }
    }
    Vec::new()
}

/// First scalar `key = value` (not a clause) for `key`.
fn child_scalar(
    children: &[Child],
    ast: &ParsedFile,
    table: &StringTable,
    key: &str,
) -> Option<String> {
    children.iter().find_map(|child| {
        if let Child::Leaf(lidx) = child {
            let l = &ast.arena.leaves[*lidx as usize];
            if table.get_string(l.key.normal).unwrap_or_default() == key
                && !matches!(l.value, Value::Clause(_))
            {
                return Some(value_to_string(&l.value, table));
            }
        }
        None
    })
}

/// All scalar values for a possibly-repeated key (`data_source = <a>` repeated).
fn child_scalars(
    children: &[Child],
    ast: &ParsedFile,
    table: &StringTable,
    key: &str,
) -> Vec<String> {
    children
        .iter()
        .filter_map(|child| {
            if let Child::Leaf(lidx) = child {
                let l = &ast.arena.leaves[*lidx as usize];
                if table.get_string(l.key.normal).unwrap_or_default() == key
                    && !matches!(l.value, Value::Clause(_))
                {
                    return Some(value_to_string(&l.value, table));
                }
            }
            None
        })
        .collect()
}

/// A scope list that may be written as `key = scope` (scalar) or `key = { a b }` (clause).
fn child_scope_list(
    children: &[Child],
    ast: &ParsedFile,
    table: &StringTable,
    key: &str,
) -> Vec<String> {
    let clause = child_clause_values(children, ast, table, key);
    if !clause.is_empty() {
        return clause;
    }
    child_scalar(children, ast, table, key)
        .into_iter()
        .collect()
}

/// Parse a top-level `scopes = { Name = { aliases = {..} is_subscope_of = {..} } }`
/// block (scopes.cwt) into `ScopeInput`s for the runtime scope registry.
pub(crate) fn extract_scope_defs(
    children: &[Child],
    ast: &ParsedFile,
    table: &StringTable,
    ruleset: &mut RuleSet,
) {
    for child in children {
        let Some((name, body)) = entry_body(child, ast, table) else {
            continue;
        };
        let name = name.trim_matches('"').to_string();
        if name.is_empty() {
            continue;
        }
        ruleset.scope_inputs.push(ScopeInput {
            aliases: child_clause_values(body, ast, table, "aliases"),
            is_subscope_of: child_clause_values(body, ast, table, "is_subscope_of"),
            name,
        });
    }
}

/// Parse a top-level `links = { name = { output_scope=.. input_scopes=.. ... } }`
/// block (links.cwt) into full `LinkInput`s, and record link/prefix names in
/// `scope_links` (the valid-key set used by `scope_field` matching).
pub(crate) fn extract_links(
    children: &[Child],
    ast: &ParsedFile,
    table: &StringTable,
    ruleset: &mut RuleSet,
) {
    for child in children {
        let Some((name, body)) = entry_body(child, ast, table) else {
            // A `name = value` shorthand link still contributes its name.
            if let Child::Leaf(lidx) = child {
                let n = table
                    .get_string(ast.arena.leaves[*lidx as usize].key.normal)
                    .unwrap_or_default();
                if !n.is_empty() {
                    ruleset.scope_links.insert(n);
                }
            }
            continue;
        };
        let name = name.trim_matches('"').to_string();
        if name.is_empty() {
            continue;
        }
        let prefix = child_scalar(body, ast, table, "prefix");
        ruleset.scope_links.insert(name.clone());
        ruleset.link_inputs.push(LinkInput {
            output_scope: child_scalar(body, ast, table, "output_scope"),
            input_scopes: child_scope_list(body, ast, table, "input_scopes"),
            from_data: child_scalar(body, ast, table, "from_data")
                .is_some_and(|v| v.eq_ignore_ascii_case("yes")),
            data_source: child_scalars(body, ast, table, "data_source"),
            prefix,
            name,
        });
    }
}
