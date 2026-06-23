//! Type extraction: `types = { type[x] = { ... } }` blocks into `TypeDefinition`s,
//! including their localisation/modifier sub-blocks and `## type_key_filter` comments.

use super::*;

pub(crate) fn extract_types_from_children(
    children: &[Child],
    ast: &ParsedFile,
    table: &StringTable,
    ruleset: &mut RuleSet,
) {
    let precomputed = precompute_comments(children, ast, table);
    for (idx, tchild) in children.iter().enumerate() {
        let comments = &precomputed[idx];
        let Child::Leaf(lidx) = tchild else {
            continue;
        };
        let leaf = &ast.arena.leaves[*lidx as usize];
        let key = table.get_string(leaf.key.normal).unwrap_or_default();
        if key.starts_with("type[")
            && let Some(typename) = extract_bracket_content(&key, "type")
        {
            let typedef = process_type_node(typename, leaf, ast, table, ruleset, comments);
            ruleset.types.push(typedef);
        }
    }
}

pub(crate) fn process_type_node(
    name: String,
    leaf: &cwtools_parser::ast::Leaf,
    ast: &ParsedFile,
    table: &StringTable,
    ruleset: &mut RuleSet,
    comments: &[String],
) -> TypeDefinition {
    let mut def = TypeDefinition {
        name,
        name_field: None,
        path_options: PathOptions {
            paths: Vec::new(),
            path_strict: false,
            path_file: None,
            path_extension: None,
            paths_lower: Vec::new(),
            ..Default::default()
        },
        subtypes: Vec::new(),
        type_key_filter: None,
        skip_root_key: Vec::new(),
        starts_with: None,
        type_per_file: false,
        key_prefix: None,
        warning_only: false,
        unique: false,
        should_be_referenced: false,
        localisation: Vec::new(),
        graph_related_types: Vec::new(),
        modifiers: Vec::new(),
    };

    // Parse type_key_filter from comments before this type[] node
    def.type_key_filter = parse_type_key_filter_from_comments(comments);
    def.graph_related_types = parse_graph_related_types_from_comments(comments);

    if let Value::Clause(children) = &leaf.value {
        // First pass: collect subtypes, localisation node, modifiers node
        let mut localisation_children: Option<Vec<Child>> = None;
        let mut modifiers_children: Option<Vec<Child>> = None;

        let precomputed = precompute_comments(children, ast, table);
        for (cidx, child) in children.iter().enumerate() {
            let child_comments = &precomputed[cidx];
            if let Child::Leaf(lidx) = child {
                let l = &ast.arena.leaves[*lidx as usize];
                let k = table.get_string(l.key.normal).unwrap_or_default();
                if k.starts_with("subtype[") {
                    if let Some(st_name) = extract_bracket_content(&k, "subtype") {
                        let st = process_subtype_node_from_leaf(
                            st_name,
                            l,
                            ast,
                            table,
                            ruleset,
                            child_comments,
                        );
                        def.subtypes.push(st);
                    }
                } else if k == "localisation" || k == "modifiers" {
                    if let Value::Clause(clause_ch) = &l.value {
                        if k == "localisation" {
                            localisation_children = Some(clause_ch.clone());
                        } else {
                            modifiers_children = Some(clause_ch.clone());
                        }
                    }
                } else {
                    match k.as_str() {
                        "path" => {
                            let v = clean_path(&leaf_value_string(l, table));
                            def.path_options.paths.push(v);
                        }
                        "path_strict" if leaf_value_string(l, table) == "yes" => {
                            def.path_options.path_strict = true;
                        }
                        "path_file" => {
                            def.path_options.path_file = Some(leaf_value_string(l, table));
                        }
                        "path_extension" => {
                            def.path_options.path_extension = Some(leaf_value_string(l, table));
                        }
                        "name_field" => {
                            def.name_field = Some(leaf_value_string(l, table));
                        }
                        "type_per_file" if leaf_value_string(l, table) == "yes" => {
                            def.type_per_file = true;
                        }
                        "starts_with" => {
                            def.starts_with = Some(leaf_value_string(l, table));
                        }
                        "type_key_prefix" => {
                            def.key_prefix = Some(leaf_value_string(l, table));
                        }
                        "severity" if leaf_value_string(l, table) == "warning" => {
                            def.warning_only = true;
                        }
                        "unique" if leaf_value_string(l, table) == "yes" => {
                            def.unique = true;
                        }
                        // The `should_be_used` directive maps onto the
                        // `should_be_referenced` field (the field is named for
                        // the cross-file "is this type ever referenced?" check
                        // it feeds, but the directive that enables it is spelled
                        // `should_be_used`). Field is shared across crates, so
                        // it is not renamed here (#204).
                        "should_be_used" if leaf_value_string(l, table) == "yes" => {
                            def.should_be_referenced = true;
                        }
                        "skip_root_key" => {
                            if let Value::Clause(block_children) = &l.value {
                                parse_skip_root_key_block(
                                    block_children,
                                    ast,
                                    table,
                                    &mut def.skip_root_key,
                                );
                            } else {
                                parse_skip_root_key_leaf(l, table, &mut def.skip_root_key);
                            }
                        }
                        _ => {}
                    }
                }
            }
        }

        // Multiple leaf skip_root_key directives are already promoted inline
        // (above) to a single MultipleKeys entry.  The block form intentionally
        // produces one entry per element (nested levels), so no further
        // collapsing is needed or correct here.

        // Parse localisation block
        if let Some(loc_children) = localisation_children {
            def.localisation = parse_localisation_block(&loc_children, ast, table);
            // Also look for subtype localisation sub-blocks and attach them
            let subtype_locs = parse_subtype_localisation(&loc_children, ast, table);
            for (st_name, locs) in subtype_locs {
                if let Some(st) = def.subtypes.iter_mut().find(|s| s.name == st_name) {
                    st.localisation.extend(locs);
                }
            }
        }

        // Parse modifiers block
        if let Some(mod_children) = modifiers_children {
            def.modifiers = parse_modifiers_block(&mod_children, ast, table);
            let subtype_mods = parse_subtype_modifiers(&mod_children, ast, table);
            for (st_name, mods) in subtype_mods {
                if let Some(st) = def.subtypes.iter_mut().find(|s| s.name == st_name) {
                    st.modifiers.extend(mods);
                }
            }
        }
    }

    def
}

/// Block form: `skip_root_key = { A B }`.
/// Each element is a separate nested level (F# RulesParser.fs:1031-1035 maps
/// each to its own layer). `any` becomes `AnyKey`; anything else becomes
/// `SpecificKey`. Appends to `out`.
fn parse_skip_root_key_block(
    block_children: &[Child],
    ast: &ParsedFile,
    table: &StringTable,
    out: &mut Vec<SkipRootKey>,
) {
    for block_child in block_children {
        if let Child::LeafValue(lvidx) = block_child {
            let lv = &ast.arena.leaf_values[*lvidx as usize];
            let v = value_to_string(&lv.value, table);
            if v.is_empty() {
                continue;
            }
            if v == "any" {
                out.push(SkipRootKey::AnyKey);
            } else {
                out.push(SkipRootKey::SpecificKey(v));
            }
        }
    }
}

/// Leaf form: `skip_root_key = A`.
/// `any` becomes `AnyKey`. A first named key becomes `SpecificKey`; subsequent
/// named leaves (multiple `skip_root_key = ...` directives) promote the prior
/// entries into a single `MultipleKeys` alternative, using the first entry's
/// operator (F# parity). Appends to / rewrites `out`.
fn parse_skip_root_key_leaf(
    l: &cwtools_parser::ast::Leaf,
    table: &StringTable,
    out: &mut Vec<SkipRootKey>,
) {
    let op = l.op;
    let v = leaf_value_string(l, table);
    if v == "any" {
        out.push(SkipRootKey::AnyKey);
    } else if out.is_empty() {
        out.push(SkipRootKey::SpecificKey(v));
    } else {
        // Multiple leaves: promote to MultipleKeys, using the first entry's
        // operator (F# parity).
        let should_match = op == cwtools_parser::ast::Operator::Equals;
        let first_match_kind = match &out[0] {
            SkipRootKey::MultipleKeys(_, mk) => *mk,
            _ => MatchKind::from_equals(should_match),
        };
        // Flatten the existing entries (SpecificKey / MultipleKeys) plus the
        // new key into one alternative list. AnyKey carries no key text.
        let mut all_keys: Vec<String> = out
            .drain(..)
            .flat_map(|existing| match existing {
                SkipRootKey::SpecificKey(k) => vec![k],
                SkipRootKey::MultipleKeys(ks, _) => ks,
                SkipRootKey::AnyKey => Vec::new(),
            })
            .collect();
        all_keys.push(v);
        out.push(SkipRootKey::MultipleKeys(all_keys, first_match_kind));
    }
}

pub(crate) fn parse_type_key_filter_from_comments(
    comments: &[String],
) -> Option<(Vec<String>, bool)> {
    // Check for negated form first (`type_key_filter <> value`) — only on exactly-## lines.
    for c in comments.iter().rev() {
        let Some(rest) = c.strip_prefix("##") else {
            continue;
        };
        if rest.starts_with('#') {
            continue;
        }
        let rest = rest.trim_start();
        if !rest.starts_with("type_key_filter") {
            continue;
        }
        let after = rest["type_key_filter".len()..].trim_start();
        let (rhs, negative) = if let Some(r) = after.strip_prefix("<>") {
            (r.trim(), true)
        } else if let Some(r) = after.strip_prefix('=') {
            (r.trim(), false)
        } else {
            continue;
        };
        let values = if rhs.starts_with('{') && rhs.ends_with('}') {
            let inner = rhs.trim_matches(|c| c == '{' || c == '}');
            inner
                .split_whitespace()
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .collect()
        } else {
            vec![rhs.to_string()]
        };
        return Some((values, negative));
    }
    None
}

fn parse_graph_related_types_from_comments(comments: &[String]) -> Vec<String> {
    if let Some(rhs) = find_directive(comments, "graph_related_types") {
        if rhs.starts_with('{') && rhs.ends_with('}') {
            let inner = rhs.trim_matches(|c| c == '{' || c == '}');
            return inner
                .split_whitespace()
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .collect();
        } else if !rhs.is_empty() {
            return vec![rhs.to_string()];
        }
    }
    Vec::new()
}

pub(crate) fn parse_localisation_block(
    children: &[Child],
    ast: &ParsedFile,
    table: &StringTable,
) -> Vec<TypeLocalisation> {
    let mut out = Vec::new();
    let precomputed = precompute_comments(children, ast, table);
    for (cidx, child) in children.iter().enumerate() {
        let child_comments = &precomputed[cidx];
        if let Child::Leaf(lidx) = child {
            let l = &ast.arena.leaves[*lidx as usize];
            let key = table.get_string(l.key.normal).unwrap_or_default();
            // Skip subtype[] sub-blocks (Leaf+Clause children, handled by
            // parse_subtype_localisation)
            if key.starts_with("subtype[") {
                continue;
            }
            let value = value_to_string(&l.value, table);
            let required = has_directive(child_comments, "required");
            let optional = has_directive(child_comments, "optional");
            let primary = has_directive(child_comments, "primary");
            let replace_scopes = parse_replace_scopes_from_comments(child_comments);

            let loc = if let Some(dollar_idx) = value.find('$') {
                let prefix = value[..dollar_idx].to_string();
                let suffix = value[dollar_idx + 1..].to_string();
                TypeLocalisation {
                    name: key,
                    prefix,
                    suffix,
                    required,
                    optional,
                    explicit_field: None,
                    replace_scopes,
                    primary,
                }
            } else {
                TypeLocalisation {
                    name: key,
                    prefix: String::new(),
                    suffix: String::new(),
                    required,
                    optional,
                    explicit_field: Some(value),
                    replace_scopes,
                    primary,
                }
            };
            out.push(loc);
        }
    }
    out
}

pub(crate) fn parse_modifiers_block(
    children: &[Child],
    ast: &ParsedFile,
    table: &StringTable,
) -> Vec<TypeModifier> {
    let mut out = Vec::new();
    let precomputed = precompute_comments(children, ast, table);
    for (cidx, child) in children.iter().enumerate() {
        let child_comments = &precomputed[cidx];
        if let Child::Leaf(lidx) = child {
            let l = &ast.arena.leaves[*lidx as usize];
            let key = table.get_string(l.key.normal).unwrap_or_default();
            if key.starts_with("subtype[") {
                continue;
            }
            let value = value_to_string(&l.value, table);
            let explicit = has_directive(child_comments, "explicit");
            // Documentation is the first exactly-### line (not ##, which is directives).
            let documentation = child_comments
                .iter()
                .find(|s| s.starts_with("###"))
                .map(|s| s.trim_start_matches('#').trim().to_string());

            let modifier = if let Some(dollar_idx) = value.find('$') {
                let prefix = value[..dollar_idx].to_string();
                let suffix = value[dollar_idx + 1..].to_string();
                TypeModifier {
                    prefix,
                    suffix,
                    category: key,
                    documentation,
                    explicit,
                }
            } else {
                TypeModifier {
                    prefix: String::new(),
                    suffix: String::new(),
                    category: key,
                    documentation,
                    explicit,
                }
            };
            out.push(modifier);
        }
    }
    out
}

#[cfg(test)]
mod skip_root_key_tests {
    use cwtools_parser::parser::parse_string;
    use cwtools_string_table::string_table::StringTable;

    use crate::{
        rules_converter::ast_to_ruleset,
        rules_types::{MatchKind, SkipRootKey},
    };

    fn parse_type(cwt: &str) -> Vec<SkipRootKey> {
        let table = StringTable::new();
        let ast = parse_string(cwt, &table).unwrap();
        let rs = ast_to_ruleset(&ast, &table);
        rs.types
            .into_iter()
            .next()
            .map(|t| t.skip_root_key)
            .unwrap_or_default()
    }

    // Single leaf: skip_root_key = ideas
    #[test]
    fn single_leaf_produces_specific_key() {
        let srk = parse_type(
            r#"types = { type[idea] = { path = "game/common/ideas" skip_root_key = ideas } }"#,
        );
        assert_eq!(srk, vec![SkipRootKey::SpecificKey("ideas".into())]);
    }

    // Single leaf: skip_root_key = any
    #[test]
    fn single_any_leaf_produces_any_key() {
        let srk = parse_type(
            r#"types = { type[idea] = { path = "game/common/ideas" skip_root_key = any } }"#,
        );
        assert_eq!(srk, vec![SkipRootKey::AnyKey]);
    }

    // Block form: skip_root_key = { ideas any }
    // Must produce TWO nested levels, not one MultipleKeys.
    #[test]
    fn block_form_produces_nested_levels() {
        let srk = parse_type(
            r#"types = { type[idea] = { path = "game/common/ideas" skip_root_key = { ideas any } } }"#,
        );
        assert_eq!(
            srk,
            vec![
                SkipRootKey::SpecificKey("ideas".into()),
                SkipRootKey::AnyKey,
            ],
            "block form must produce one entry per element (nested levels)"
        );
    }

    // Block form with two named keys: skip_root_key = { A B }
    #[test]
    fn block_form_two_named_keys_are_two_levels() {
        let srk = parse_type(
            r#"types = { type[foo] = { path = "game/x" skip_root_key = { wrapper inner } } }"#,
        );
        assert_eq!(
            srk,
            vec![
                SkipRootKey::SpecificKey("wrapper".into()),
                SkipRootKey::SpecificKey("inner".into()),
            ]
        );
    }

    // Multiple leaves: skip_root_key = A  +  skip_root_key = B  (alternatives, F# parity)
    // Must keep MultipleKeys (alternative form, single level with two candidates).
    #[test]
    fn multiple_leaves_produce_multiple_keys() {
        let srk = parse_type(
            r#"types = { type[foo] = { path = "game/x" skip_root_key = a skip_root_key = b } }"#,
        );
        assert_eq!(srk.len(), 1, "multiple leaves must collapse to ONE entry");
        match &srk[0] {
            SkipRootKey::MultipleKeys(keys, MatchKind::Equals) => {
                assert!(keys.contains(&"a".to_string()));
                assert!(keys.contains(&"b".to_string()));
            }
            other => panic!("expected MultipleKeys, got {other:?}"),
        }
    }
}
