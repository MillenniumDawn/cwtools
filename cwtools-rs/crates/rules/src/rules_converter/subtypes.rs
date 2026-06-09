//! Subtype extraction: `subtype[x] = { ... }` bodies plus the subtype-scoped
//! localisation/modifier sub-blocks nested inside a type's `localisation`/`modifiers`.

use super::*;

pub(crate) fn parse_subtype_localisation(
    children: &[Child],
    ast: &ParsedFile,
    table: &StringTable,
) -> Vec<(String, Vec<TypeLocalisation>)> {
    let mut out = Vec::new();
    for child in children {
        if let Child::Node(nidx) = child {
            let n = &ast.arena.nodes[*nidx as usize];
            let nk = table.get_string(n.key.normal).unwrap_or_default();
            if nk.starts_with("subtype[")
                && let Some(st_name) = extract_bracket_content(&nk, "subtype")
            {
                let locs = parse_localisation_block(&n.children, ast, table);
                out.push((st_name, locs));
            }
        }
    }
    out
}

pub(crate) fn parse_subtype_modifiers(
    children: &[Child],
    ast: &ParsedFile,
    table: &StringTable,
) -> Vec<(String, Vec<TypeModifier>)> {
    let mut out = Vec::new();
    for child in children {
        if let Child::Node(nidx) = child {
            let n = &ast.arena.nodes[*nidx as usize];
            let nk = table.get_string(n.key.normal).unwrap_or_default();
            if nk.starts_with("subtype[")
                && let Some(st_name) = extract_bracket_content(&nk, "subtype")
            {
                let mods = parse_modifiers_block(&n.children, ast, table);
                out.push((st_name, mods));
            }
        }
    }
    out
}

pub(crate) fn process_subtype_node(
    name: String,
    node: &cwtools_parser::ast::Node,
    ast: &ParsedFile,
    table: &StringTable,
    ruleset: &mut RuleSet,
    comments: &[String],
) -> SubTypeDefinition {
    build_subtype(name, &node.children, ast, table, ruleset, comments)
}

pub(crate) fn process_subtype_node_from_leaf(
    name: String,
    leaf: &cwtools_parser::ast::Leaf,
    ast: &ParsedFile,
    table: &StringTable,
    ruleset: &mut RuleSet,
    comments: &[String],
) -> SubTypeDefinition {
    let children = if let Value::Clause(ch) = &leaf.value {
        ch.clone()
    } else {
        Vec::new()
    };
    build_subtype(name, &children, ast, table, ruleset, comments)
}

fn build_subtype(
    name: String,
    children: &[Child],
    ast: &ParsedFile,
    table: &StringTable,
    ruleset: &mut RuleSet,
    comments: &[String],
) -> SubTypeDefinition {
    // Parse metadata from comments preceding the subtype[] declaration
    let display_name = extract_comment_value(comments, "display_name");
    let abbreviation = extract_comment_value(comments, "abbreviation");
    let push_scope = extract_comment_value(comments, "push_scope");
    let starts_with = extract_comment_value(comments, "starts_with");
    // `## type_key_filter = X` discriminates on the instance's OWN node key — a
    // different mechanism from `type_key_field` (which checks for a child field).
    let type_key_filter = parse_type_key_filter_from_comments(comments)
        .map(|(vals, _)| vals)
        .unwrap_or_default();
    let mut type_key_field: Option<String> = None;
    let only_if_not = parse_only_if_not_from_comments(comments);

    // Also recognise `type_key_field = <value>` placed as a direct leaf inside the
    // subtype body (the inline alternative to a ## type_key_filter = ... comment).
    // Strip it out of the children before building rules so it doesn't become a
    // spurious required field.
    let filtered_children: Vec<Child> = children
        .iter()
        .filter(|child| {
            if let Child::Leaf(lidx) = child {
                let leaf = &ast.arena.leaves[*lidx as usize];
                let k = table.get_string(leaf.key.normal).unwrap_or_default();
                if k == "type_key_field" {
                    // Extract its value as the type_key_field discriminator and skip it.
                    if type_key_field.is_none() {
                        type_key_field = Some(value_to_string(&leaf.value, table));
                    }
                    return false;
                }
            }
            true
        })
        .cloned()
        .collect();

    // Convert children using full children_to_rules for proper typing
    let rules = children_to_rules(&filtered_children, ast, table, ruleset);

    SubTypeDefinition {
        name,
        display_name,
        abbreviation,
        rules,
        type_key_field,
        starts_with,
        push_scope,
        localisation: Vec::new(),
        only_if_not,
        modifiers: Vec::new(),
        type_key_filter,
    }
}

fn extract_comment_value(comments: &[String], key: &str) -> Option<String> {
    comments
        .iter()
        .find(|s| s.contains(key) && s.contains('='))
        .and_then(|s| s.find('=').map(|i| s[i + 1..].trim().to_string()))
        .filter(|s| !s.is_empty())
}

fn parse_only_if_not_from_comments(comments: &[String]) -> Vec<String> {
    if let Some(c) = comments.iter().find(|s| s.contains("only_if_not"))
        && let Some(idx) = c.find('=')
    {
        let rhs = c[idx + 1..].trim().to_string();
        if rhs.starts_with('{') && rhs.ends_with('}') {
            let inner = rhs.trim_matches(|c| c == '{' || c == '}');
            return inner
                .split_whitespace()
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .collect();
        } else {
            return vec![rhs];
        }
    }
    Vec::new()
}
