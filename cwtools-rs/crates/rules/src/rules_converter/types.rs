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
        let (key, is_leaf) = match tchild {
            Child::Leaf(lidx) => {
                let leaf = &ast.arena.leaves[*lidx as usize];
                (table.get_string(leaf.key.normal).unwrap_or_default(), true)
            }
            Child::Node(nidx) => {
                let node = &ast.arena.nodes[*nidx as usize];
                (table.get_string(node.key.normal).unwrap_or_default(), false)
            }
            _ => continue,
        };
        if key.starts_with("type[")
            && let Some(typename) = extract_bracket_content(&key, "type")
        {
            let typedef = if is_leaf {
                if let Child::Leaf(lidx) = tchild {
                    process_type_node(
                        typename,
                        &ast.arena.leaves[*lidx as usize],
                        ast,
                        table,
                        ruleset,
                        comments,
                    )
                } else {
                    continue;
                }
            } else {
                if let Child::Node(nidx) = tchild {
                    let node = &ast.arena.nodes[*nidx as usize];
                    process_type_node_from_node(typename, node, ast, table, ruleset, comments)
                } else {
                    continue;
                }
            };
            ruleset.types.push(typedef);
        }
    }
}

pub(crate) fn process_type_node_from_node(
    name: String,
    node: &cwtools_parser::ast::Node,
    ast: &ParsedFile,
    table: &StringTable,
    ruleset: &mut RuleSet,
    comments: &[String],
) -> TypeDefinition {
    let synthetic_leaf = cwtools_parser::ast::Leaf {
        key: node.key,
        value: Value::Clause(node.children.clone()),
        op: cwtools_parser::ast::Operator::Equals,
        pos: node.pos,
    };
    process_type_node(name, &synthetic_leaf, ast, table, ruleset, comments)
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
            match child {
                Child::Leaf(lidx) => {
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
                            "should_be_used" if leaf_value_string(l, table) == "yes" => {
                                def.should_be_referenced = true;
                            }
                            "skip_root_key" => {
                                let op = l.op;
                                let v = leaf_value_string(l, table);
                                if v == "any" {
                                    def.skip_root_key.push(SkipRootKey::AnyKey);
                                } else {
                                    let should_match = op == cwtools_parser::ast::Operator::Equals;
                                    if def.skip_root_key.is_empty() {
                                        def.skip_root_key.push(SkipRootKey::SpecificKey(v));
                                    } else {
                                        // Multiple leaves: promote to MultipleKeys
                                        let mut all_keys: Vec<String> = Vec::new();
                                        for existing in def.skip_root_key.drain(..) {
                                            match existing {
                                                SkipRootKey::SpecificKey(k) => all_keys.push(k),
                                                SkipRootKey::MultipleKeys(mut ks, _) => {
                                                    all_keys.append(&mut ks)
                                                }
                                                SkipRootKey::AnyKey => {}
                                            }
                                        }
                                        all_keys.push(v);
                                        def.skip_root_key.push(SkipRootKey::MultipleKeys(
                                            all_keys,
                                            should_match,
                                        ));
                                    }
                                }
                            }
                            _ => {}
                        }
                    }
                }
                Child::Node(nidx) => {
                    let n = &ast.arena.nodes[*nidx as usize];
                    let nk = table.get_string(n.key.normal).unwrap_or_default();
                    if nk.starts_with("subtype[") {
                        if let Some(st_name) = extract_bracket_content(&nk, "subtype") {
                            let st = process_subtype_node(
                                st_name,
                                n,
                                ast,
                                table,
                                ruleset,
                                child_comments,
                            );
                            def.subtypes.push(st);
                        }
                    } else if nk == "localisation" {
                        localisation_children = Some(n.children.clone());
                    } else if nk == "modifiers" {
                        modifiers_children = Some(n.children.clone());
                    } else if nk == "skip_root_key" {
                        // Block form: skip_root_key = { A B C }
                        let mut block_keys = Vec::new();
                        for block_child in &n.children {
                            if let Child::LeafValue(lvidx) = block_child {
                                let lv = &ast.arena.leaf_values[*lvidx as usize];
                                let v = value_to_string(&lv.value, table);
                                if !v.is_empty() {
                                    // `any` flows through as a literal key here; any
                                    // wildcard semantics live in the matcher.
                                    block_keys.push(v);
                                }
                            }
                        }
                        if !block_keys.is_empty() {
                            def.skip_root_key
                                .push(SkipRootKey::MultipleKeys(block_keys, true));
                        }
                    }
                }
                _ => {}
            }
        }

        // Promote single SkipRootKey::SpecificKey to MultipleKeys if there were multiple skip_root_key leaves
        if def.skip_root_key.len() > 1 {
            let mut all_keys = Vec::new();
            let mut should_match = true;
            for existing in def.skip_root_key.drain(..) {
                match existing {
                    SkipRootKey::SpecificKey(k) => all_keys.push(k),
                    SkipRootKey::MultipleKeys(mut ks, sm) => {
                        should_match = sm;
                        all_keys.append(&mut ks);
                    }
                    SkipRootKey::AnyKey => {}
                }
            }
            def.skip_root_key
                .push(SkipRootKey::MultipleKeys(all_keys, should_match));
        }

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

pub(crate) fn parse_type_key_filter_from_comments(
    comments: &[String],
) -> Option<(Vec<String>, bool)> {
    if let Some(c) = comments.iter().find(|s| s.contains("type_key_filter")) {
        let negative = c.contains("<>");
        let has_eq = c.contains('=');
        if !negative && !has_eq {
            return None;
        }
        let rhs = if negative {
            let idx = c.find("<>").unwrap() + 2;
            c[idx..].trim().to_string()
        } else {
            let idx = c.find('=').unwrap() + 1;
            c[idx..].trim().to_string()
        };
        let values = if rhs.starts_with('{') && rhs.ends_with('}') {
            let inner = rhs.trim_matches(|c| c == '{' || c == '}');
            inner
                .split_whitespace()
                .filter(|s| !s.is_empty())
                .map(|s| s.to_string())
                .collect()
        } else {
            vec![rhs]
        };
        Some((values, negative))
    } else {
        None
    }
}

fn parse_graph_related_types_from_comments(comments: &[String]) -> Vec<String> {
    if let Some(c) = comments.iter().find(|s| s.contains("graph_related_types"))
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
            // Skip subtype[] sub-blocks (they are Node children)
            if key.starts_with("subtype[") {
                continue;
            }
            let value = value_to_string(&l.value, table);
            let required = child_comments.iter().any(|s| s.contains("required"));
            let optional = child_comments.iter().any(|s| s.contains("optional"));
            let primary = child_comments.iter().any(|s| s.contains("primary"));
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
            let explicit = child_comments.iter().any(|s| s.contains("explicit"));
            let documentation = child_comments
                .iter()
                .find(|s| s.starts_with("##"))
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
