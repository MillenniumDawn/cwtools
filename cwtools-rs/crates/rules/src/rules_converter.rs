use crate::rules_types::*;
use cwtools_parser::ast::{Child, ParsedFile, Value};
use cwtools_string_table::string_table::StringTable;

use cwtools_parser::ast::Comment;

/// Extract comment text directly preceding a child in the AST.
fn collect_comments_before_child(
    all_children: &[Child],
    idx: usize,
    ast: &ParsedFile,
    table: &StringTable,
) -> Vec<String> {
    let mut comments = Vec::new();
    // Walk backwards from idx to collect adjacent comments
    let mut i = idx;
    while i > 0 {
        i -= 1;
        match &all_children[i] {
            Child::Comment(cidx) => {
                let c = &ast.arena.comments[*cidx as usize];
                let text = c.text.trim();
                if text.starts_with('#') {
                    comments.push(text.to_string());
                } else if text.starts_with("##") {
                    comments.push(text.to_string());
                } else {
                    comments.push(text.to_string());
                }
            }
            _ => break,
        }
    }
    comments.reverse();
    comments
}

/// Convert a parsed .cwt AST into a RuleSet.
pub fn ast_to_ruleset(ast: &ParsedFile, table: &StringTable) -> RuleSet {
    let mut ruleset = RuleSet::new();

    for (idx, child) in ast.root_children.iter().enumerate() {
        let comments = collect_comments_before_child(&ast.root_children, idx, ast, table,
        );

        match child {
            // Case 1: `types = { ... }` parsed as a Node
            Child::Node(nidx) => {
                let node = &ast.arena.nodes[*nidx as usize];
                let key = table.get_string(node.key.normal).unwrap_or_default();
                match key.as_str() {
                    "types" => extract_types_from_children(&node.children, ast, table, &mut ruleset,
                    ),
                    "enums" => extract_enums_from_children(
                        &node.children, ast, table, &mut ruleset,
                    ),
                    _ => {
                        // Top-level type rule or alias
                        process_root_node(key, node, ast, table, &comments, &mut ruleset);
                    }
                }
            }
            // Case 2: `types = { ... }` parsed as a Leaf with clause value
            Child::Leaf(lidx) => {
                let leaf = &ast.arena.leaves[*lidx as usize];
                let key = table.get_string(leaf.key.normal).unwrap_or_default();
                match key.as_str() {
                    "types" => {
                        if let Value::Clause(children) = &leaf.value {
                            extract_types_from_children(children, ast, table, &mut ruleset);
                        }
                    }
                    "enums" => {
                        if let Value::Clause(children) = &leaf.value {
                            extract_enums_from_children(children, ast, table, &mut ruleset);
                        }
                    }
                    _ => {
                        // Top-level alias or type rule
                        process_root_leaf(key, leaf, ast, table, &comments, &mut ruleset);
                    }
                }
            }
            _ => {}
        }
    }

    ruleset
}

fn process_root_node(
    key: String,
    node: &cwtools_parser::ast::Node,
    ast: &ParsedFile,
    table: &StringTable,
    comments: &[String],
    ruleset: &mut RuleSet,
) {
    if key.starts_with("alias[") {
        if let Some((category, alias_name)) = get_alias_settings(&key, "alias") {
            let full_name = format!("{}:{}", category, alias_name);
            let rule = node_to_noderule(node, ast, table, ruleset);
            let opts = options_from_comments(comments);
            ruleset
                .aliases
                .push((full_name, (RuleType::NodeRule { left: NewField::AliasField(category), rules: rule }, opts)));
        }
    } else if key.starts_with("single_alias[") {
        if let Some(alias_name) = get_setting_from_string(&key, "single_alias") {
            let rule = node_to_noderule(node, ast, table, ruleset);
            let opts = options_from_comments(comments);
            ruleset
                .single_aliases
                .push((alias_name.clone(), (RuleType::NodeRule { left: NewField::SingleAliasField(alias_name), rules: rule }, opts)));
        }
    } else {
        let rule = build_rule_from_node(node, ast, table, ruleset);
        let opts = options_from_comments(comments);
        ruleset.root_rules.push(RootRule::TypeRule(key, (rule, opts)));
    }
}

fn process_root_leaf(
    key: String,
    leaf: &cwtools_parser::ast::Leaf,
    ast: &ParsedFile,
    table: &StringTable,
    comments: &[String],
    ruleset: &mut RuleSet,
) {
    if key.starts_with("alias[") {
        if let Some((category, alias_name)) = get_alias_settings(&key, "alias") {
            let full_name = format!("{}:{}", category, alias_name);
            let rule = leaf_to_rule(leaf, ast, table, ruleset);
            let opts = options_from_comments(comments);
            ruleset
                .aliases
                .push((full_name, (rule, opts)));
        }
    } else if key.starts_with("single_alias[") {
        if let Some(alias_name) = get_setting_from_string(&key, "single_alias") {
            let rule = leaf_to_rule(leaf, ast, table, ruleset);
            let opts = options_from_comments(comments);
            ruleset
                .single_aliases
                .push((alias_name, (rule, opts)));
        }
    } else {
        let rule = leaf_to_rule(leaf, ast, table, ruleset);
        let opts = options_from_comments(comments);
        ruleset.root_rules.push(RootRule::TypeRule(key, (rule, opts)));
    }
}

fn build_rule_from_node(
    node: &cwtools_parser::ast::Node,
    ast: &ParsedFile,
    table: &StringTable,
    ruleset: &mut RuleSet,
) -> RuleType {
    let inner = node_to_noderule(node, ast, table, ruleset);
    RuleType::NodeRule {
        left: NewField::SpecificField(
            table.get_string(node.key.normal).unwrap_or_default(),
        ),
        rules: inner,
    }
}

fn leaf_to_rule(
    leaf: &cwtools_parser::ast::Leaf,
    ast: &ParsedFile,
    table: &StringTable,
    ruleset: &mut RuleSet,
) -> RuleType {
    match &leaf.value {
        Value::Clause(children) => {
            let inner = children_to_rules(children, ast, table, ruleset);
            RuleType::NodeRule {
                left: NewField::SpecificField(
                    table.get_string(leaf.key.normal).unwrap_or_default(),
                ),
                rules: inner,
            }
        }
        _ => {
            // scalar alias like alias[effect:set_name] = scalar
            let left = NewField::SpecificField(
                table.get_string(leaf.key.normal).unwrap_or_default(),
            );
            let right = process_right_field(
                &value_to_string(&leaf.value, table),
                table,
            );
            RuleType::LeafRule { left, right }
        }
    }
}

fn node_to_noderule(
    node: &cwtools_parser::ast::Node,
    ast: &ParsedFile,
    table: &StringTable,
    ruleset: &mut RuleSet,
) -> Vec<NewRule> {
    children_to_rules(&node.children, ast, table, ruleset,
    )
}

/// Convert a left-hand key string into the appropriate NewField.
/// Handles CWT idioms like `alias_name[effect]`, `alias_match_left[effect]`,
/// and `single_alias_right[X]` that need to produce AliasField / SingleAliasField
/// rather than a bare SpecificField (which would never match a real modder key).
fn key_string_to_left_field(key: &str) -> NewField {
    if (key.starts_with("alias_name[") || key.starts_with("alias_match_left[")) && key.ends_with(']') {
        let inner_start = key.find('[').unwrap() + 1;
        let inner = &key[inner_start..key.len() - 1];
        return NewField::AliasField(inner.to_string());
    }
    if key.starts_with("single_alias_right[") && key.ends_with(']') {
        let inner = &key[19..key.len() - 1];
        return NewField::SingleAliasField(inner.to_string());
    }
    NewField::SpecificField(key.to_string())
}

fn children_to_rules(
    children: &Vec<Child>,
    ast: &ParsedFile,
    table: &StringTable,
    ruleset: &mut RuleSet,
) -> Vec<NewRule> {
    let mut rules = Vec::new();
    for (idx, child) in children.iter().enumerate() {
        let comments = collect_comments_before_child(children, idx, ast, table);
        match child {
            Child::Leaf(lidx) => {
                let leaf = &ast.arena.leaves[*lidx as usize];
                let key = table.get_string(leaf.key.normal).unwrap_or_default();

                if key.starts_with("subtype[") {
                    if let Some(st_name) = extract_bracket_content(&key, "subtype") {
                        let positive = !st_name.starts_with('!');
                        let name = if positive {
                            st_name
                        } else {
                            st_name[1..].to_string()
                        };
                        let inner = match &leaf.value {
                            Value::Clause(children) => children_to_rules(children, ast, table, ruleset),
                            _ => Vec::new(),
                        };
                        rules.push((
                            RuleType::SubtypeRule {
                                name,
                                positive,
                                rules: inner,
                            },
                            options_from_comments(&comments),
                        ));
                    }
                    continue;
                }

                let opts = options_from_comments(&comments);
                let rule = match &leaf.value {
                    Value::Clause(_) => {
                        let inner = match &leaf.value {
                            Value::Clause(children) => {
                                children_to_rules(children, ast, table, ruleset)
                            }
                            _ => Vec::new(),
                        };
                        RuleType::NodeRule {
                            left: key_string_to_left_field(&key),
                            rules: inner,
                        }
                    }
                    _ => {
                        let right = process_right_field(
                            &value_to_string(&leaf.value, table),
                            table,
                        );
                        RuleType::LeafRule {
                            left: key_string_to_left_field(&key),
                            right,
                        }
                    }
                };
                rules.push((rule, opts));
            }
            Child::Node(nidx) => {
                let node = &ast.arena.nodes[*nidx as usize];
                let key = table.get_string(node.key.normal).unwrap_or_default();

                if key.starts_with("subtype[") {
                    if let Some(st_name) = extract_bracket_content(&key, "subtype") {
                        let positive = !st_name.starts_with('!');
                        let name = if positive {
                            st_name
                        } else {
                            st_name[1..].to_string()
                        };
                        let inner = children_to_rules(&node.children, ast, table, ruleset,
                        );
                        rules.push((
                            RuleType::SubtypeRule {
                                name,
                                positive,
                                rules: inner,
                            },
                            options_from_comments(&comments),
                        ));
                    }
                    continue;
                }

                let opts = options_from_comments(&comments);
                let inner = children_to_rules(&node.children, ast, table, ruleset,
                );
                rules.push((
                    RuleType::NodeRule {
                        left: key_string_to_left_field(&key),
                        rules: inner,
                    },
                    opts,
                ));
            }
            _ => {}
        }
    }
    rules
}

fn extract_types_from_children(
    children: &Vec<Child>,
    ast: &ParsedFile,
    table: &StringTable,
    ruleset: &mut RuleSet,
) {
    for tchild in children {
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
        if key.starts_with("type[") {
            if let Some(typename) = extract_bracket_content(&key, "type") {
                let typedef = if is_leaf {
                    if let Child::Leaf(lidx) = tchild {
                        process_type_node(typename, &ast.arena.leaves[*lidx as usize], ast, table, ruleset)
                    } else {
                        continue;
                    }
                } else {
                    if let Child::Node(nidx) = tchild {
                        // Node-style type definition (no = before brace)
                        let node = &ast.arena.nodes[*nidx as usize];
                        process_type_node_from_node(typename, node, ast, table, ruleset)
                    } else {
                        continue;
                    }
                };
                ruleset.types.push(typedef);
            }
        }
    }
}

fn extract_enums_from_children(
    children: &Vec<Child>,
    ast: &ParsedFile,
    table: &StringTable,
    ruleset: &mut RuleSet,
) {
    for echild in children {
        let (key, is_leaf) = match echild {
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
        if key.starts_with("enum[") {
            if let Some(enum_name) = extract_bracket_content(&key, "enum") {
                let def = if is_leaf {
                    if let Child::Leaf(lidx) = echild {
                        process_enum_node(enum_name, &ast.arena.leaves[*lidx as usize], ast, table)
                    } else {
                        continue;
                    }
                } else {
                    if let Child::Node(nidx) = echild {
                        process_enum_node_from_node(enum_name, &ast.arena.nodes[*nidx as usize], ast, table)
                    } else {
                        continue;
                    }
                };
                ruleset.enums.push(def);
            }
        }
    }
}

fn process_type_node_from_node(
    name: String,
    node: &cwtools_parser::ast::Node,
    ast: &ParsedFile,
    table: &StringTable,
    ruleset: &mut RuleSet,
) -> TypeDefinition {
    // Node-style type definitions have children inside the node itself.
    // We synthesize a Leaf with Value::Clause(children) and delegate.
    let synthetic_leaf = cwtools_parser::ast::Leaf {
        key: node.key,
        value: Value::Clause(node.children.clone()),
        op: cwtools_parser::ast::Operator::Equals,
        pos: node.pos.clone(),
    };
    process_type_node(name, &synthetic_leaf, ast, table, ruleset)
}

fn process_enum_node_from_node(
    name: String,
    node: &cwtools_parser::ast::Node,
    ast: &ParsedFile,
    table: &StringTable,
) -> EnumDefinition {
    // Delegate to process_enum_node via a synthetic Leaf with Clause value,
    // the same technique used by process_type_node_from_node.
    let synthetic_leaf = cwtools_parser::ast::Leaf {
        key: node.key,
        value: Value::Clause(node.children.clone()),
        op: cwtools_parser::ast::Operator::Equals,
        pos: node.pos.clone(),
    };
    process_enum_node(name, &synthetic_leaf, ast, table)
}

fn extract_bracket_content(full: &str, prefix: &str) -> Option<String> {
    let expected = format!("{}[", prefix);
    if full.starts_with(&expected) && full.ends_with(']') {
        Some(full[expected.len()..full.len() - 1].to_string())
    } else {
        None
    }
}

fn process_type_node(
    name: String,
    leaf: &cwtools_parser::ast::Leaf,
    ast: &ParsedFile,
    table: &StringTable,
    ruleset: &mut RuleSet,
) -> TypeDefinition {
    let mut def = TypeDefinition {
        name,
        name_field: None,
        path_options: PathOptions {
            paths: Vec::new(),
            path_strict: false,
            path_file: None,
            path_extension: None,
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

    // If the leaf has a clause value, walk its children
    if let Value::Clause(children) = &leaf.value {
        for child in children {
            match child {
                Child::Leaf(lidx) => {
                    let l = &ast.arena.leaves[*lidx as usize];
                    let k = table.get_string(l.key.normal).unwrap_or_default();
                    if k.starts_with("subtype[") {
                        if let Some(st_name) = extract_bracket_content(&k, "subtype") {
                            let st = process_subtype_node_from_leaf(st_name, l, ast, table, ruleset);
                            def.subtypes.push(st);
                        }
                    } else {
                        match k.as_str() {
                            "path" => {
                                let v = clean_path(&leaf_value_string(l, table));
                                def.path_options.paths.push(v);
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
                            "unique" => {
                                if leaf_value_string(l, table) == "yes" {
                                    def.unique = true;
                                }
                            }
                            "skip_root_key" => {
                                let v = leaf_value_string(l, table);
                                if v == "any" {
                                    def.skip_root_key.push(SkipRootKey::AnyKey);
                                } else {
                                    def.skip_root_key.push(SkipRootKey::SpecificKey(v));
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
                            let st = process_subtype_node(st_name, n, ast, table, ruleset);
                            def.subtypes.push(st);
                        }
                    }
                }
                _ => {}
            }
        }
    }

    def
}

fn process_subtype_node(
    name: String,
    node: &cwtools_parser::ast::Node,
    ast: &ParsedFile,
    table: &StringTable,
    _ruleset: &mut RuleSet,
) -> SubTypeDefinition {
    let mut st = SubTypeDefinition {
        name,
        display_name: None,
        abbreviation: None,
        rules: Vec::new(),
        type_key_field: None,
        starts_with: None,
        push_scope: None,
        localisation: Vec::new(),
        only_if_not: Vec::new(),
        modifiers: Vec::new(),
    };

    for child in &node.children {
        if let Child::Leaf(lidx) = child {
            let l = &ast.arena.leaves[*lidx as usize];
            let k = table.get_string(l.key.normal).unwrap_or_default();
            let v = leaf_value_string(l, table);
            match k.as_str() {
                "type_key_field" => st.type_key_field = Some(v),
                "starts_with" => st.starts_with = Some(v),
                _ => {
                    // Attempt to build a simple LeafRule
                    let left = NewField::SpecificField(k.to_string());
                    let right = process_right_field(&v, table);
                    let opts = Options::default();
                    st.rules.push((RuleType::LeafRule { left, right }, opts));
                }
            }
        }
    }

    st
}

fn process_subtype_node_from_leaf(
    name: String,
    leaf: &cwtools_parser::ast::Leaf,
    ast: &ParsedFile,
    table: &StringTable,
    _ruleset: &mut RuleSet,
) -> SubTypeDefinition {
    let mut st = SubTypeDefinition {
        name,
        display_name: None,
        abbreviation: None,
        rules: Vec::new(),
        type_key_field: None,
        starts_with: None,
        push_scope: None,
        localisation: Vec::new(),
        only_if_not: Vec::new(),
        modifiers: Vec::new(),
    };

    if let Value::Clause(children) = &leaf.value {
        for child in children {
            if let Child::Leaf(lidx) = child {
                let l = &ast.arena.leaves[*lidx as usize];
                let k = table.get_string(l.key.normal).unwrap_or_default();
                let v = leaf_value_string(l, table);
                match k.as_str() {
                    "type_key_field" => st.type_key_field = Some(v),
                    "starts_with" => st.starts_with = Some(v),
                    _ => {
                        let left = NewField::SpecificField(k.to_string());
                        let right = process_right_field(&v, table);
                        let opts = Options::default();
                        st.rules.push((RuleType::LeafRule { left, right }, opts));
                    }
                }
            }
        }
    }

    st
}

fn process_enum_node(
    name: String,
    leaf: &cwtools_parser::ast::Leaf,
    ast: &ParsedFile,
    table: &StringTable,
) -> EnumDefinition {
    let mut values = Vec::new();

    if let Value::Clause(children) = &leaf.value {
        for child in children {
            match child {
                Child::LeafValue(lvidx) => {
                    let lv = &ast.arena.leaf_values[*lvidx as usize];
                    let v = value_to_string(&lv.value, table);
                    if !v.is_empty() {
                        values.push(v);
                    }
                }
                Child::Leaf(lidx) => {
                    let l = &ast.arena.leaves[*lidx as usize];
                    let v = table.get_string(l.key.normal).unwrap_or_default();
                    if !v.is_empty() {
                        values.push(v);
                    }
                }
                _ => {}
            }
        }
    }

    EnumDefinition {
        key: name,
        description: String::new(),
        values,
    }
}

fn value_to_string(value: &Value, table: &StringTable) -> String {
    match value {
        Value::String(t) | Value::QString(t) => {
            let s = table.get_string(t.normal).unwrap_or_default();
            // Strip surrounding quotes if present
            if s.starts_with('"') && s.ends_with('"') && s.len() >= 2 {
                s[1..s.len() - 1].to_string()
            } else {
                s
            }
        }
        Value::Float(f) => f.to_string(),
        Value::Int(i) => i.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Clause(_) => String::new(),
    }
}

/// Clean a path string: remove `game/` or `game\` prefix (matching F# behavior).
/// Extract settings from bracket notation, e.g. `alias[effect:create_starbase]`.
fn get_alias_settings(full: &str, prefix: &str) -> Option<(String, String)> {
    let setting = get_setting_from_string(full, prefix)?;
    let parts: Vec<&str> = setting.split(':').collect();
    if parts.len() < 2 {
        None
    } else {
        Some((parts[0].to_string(), parts[1..].join(":")))
    }
}

fn get_setting_from_string(full: &str, key: &str) -> Option<String> {
    let expected = format!("{}[", key);
    if full.starts_with(&expected) && full.ends_with(']') {
        Some(full[expected.len()..full.len() - 1].to_string())
    } else {
        None
    }
}

fn options_from_comments(comments: &[String]) -> Options {
    let mut opts = Options::default();
    for c in comments {
        let trimmed = c.trim();
        if trimmed.starts_with("## cardinality = ") {
            if let Some(spec) = trimmed.strip_prefix("## cardinality = ") {
                let spec = spec.trim();
                // Parse min..max
                if let Some((min_s, max_s)) = spec.split_once("..") {
                    let min = min_s.trim().parse::<i32>().unwrap_or(0);
                    let max_s = max_s.trim();
                    let max = if max_s == "inf" {
                        i32::MAX
                    } else {
                        max_s.trim().parse::<i32>().unwrap_or(1000)
                    };
                    opts.min = min;
                    opts.max = max;
                }
            }
        }
        if trimmed.starts_with("## push_scope = ") {
            if let Some(scope) = trimmed.strip_prefix("## push_scope = ") {
                opts.push_scope = Some(scope.trim().to_string());
            }
        }
        if trimmed.starts_with("## replace_scope") {
            // Simplified: just note that replace_scope exists
            // Full implementation would parse the scope clause
        }
        if trimmed.starts_with("## severity = ") {
            if let Some(sev) = trimmed.strip_prefix("## severity = ") {
                opts.severity = match sev.trim() {
                    "error" => Some(Severity::Error),
                    "warning" => Some(Severity::Warning),
                    "info" => Some(Severity::Information),
                    "information" => Some(Severity::Information),
                    "hint" => Some(Severity::Hint),
                    _ => None,
                };
            }
        }
        if trimmed.starts_with("###") {
            // Description comment
            opts.description = Some(trimmed[3..].trim().to_string());
        }
    }
    opts
}

fn clean_path(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    normalized.strip_prefix("game/")
        .unwrap_or(&normalized)
        .to_string()
}

fn leaf_value_string(leaf: &cwtools_parser::ast::Leaf, table: &StringTable) -> String {
    value_to_string(&leaf.value, table)
}

/// Convert a .cwt value string to the appropriate NewField.
fn process_right_field(value: &str, _table: &StringTable) -> NewField {
    let trimmed = value.trim();

    if trimmed == "scalar" {
        return NewField::ScalarField;
    }
    if trimmed == "bool" {
        return NewField::ValueField(ValueType::Bool);
    }
    if trimmed == "int" {
        return NewField::ValueField(ValueType::Int {
            min: i32::MIN,
            max: i32::MAX,
        });
    }
    if trimmed == "float" {
        return NewField::ValueField(ValueType::Float {
            min: f64::NEG_INFINITY,
            max: f64::INFINITY,
        });
    }
    if trimmed.starts_with("int[") && trimmed.ends_with(']') {
        let inner = &trimmed[4..trimmed.len() - 1];
        if let Some((min_str, max_str)) = inner.split_once("..") {
            if let (Ok(min), Ok(max)) = (min_str.parse::<i32>(), max_str.parse::<i32>()) {
                return NewField::ValueField(ValueType::Int { min, max });
            }
        }
    }
    if trimmed.starts_with("float[") && trimmed.ends_with(']') {
        let inner = &trimmed[6..trimmed.len() - 1];
        if let Some((min_str, max_str)) = inner.split_once("..") {
            if let (Ok(min), Ok(max)) = (min_str.parse::<f64>(), max_str.parse::<f64>()) {
                return NewField::ValueField(ValueType::Float { min, max });
            }
        }
    }
    if trimmed.starts_with("enum[") && trimmed.ends_with(']') {
        let enum_name = &trimmed[5..trimmed.len() - 1];
        return NewField::ValueField(ValueType::Enum(enum_name.to_string()));
    }
    if trimmed.starts_with("scope[") && trimmed.ends_with(']') {
        let scope = &trimmed[6..trimmed.len() - 1];
        return NewField::ScopeField(vec![scope.to_string()]);
    }
    if trimmed.starts_with("value_set[") && trimmed.ends_with(']') {
        let var = &trimmed[10..trimmed.len() - 1];
        return NewField::VariableSetField(var.to_string());
    }
    if trimmed.starts_with("value[") && trimmed.ends_with(']') {
        let var = &trimmed[6..trimmed.len() - 1];
        return NewField::VariableGetField(var.to_string());
    }
    if trimmed.starts_with("<") && trimmed.ends_with(">") {
        let type_ref = &trimmed[1..trimmed.len() - 1];
        return NewField::TypeField(TypeType::Simple(type_ref.to_string()));
    }
    if trimmed.starts_with("alias_name[") && trimmed.ends_with(']') {
        let alias = &trimmed[11..trimmed.len() - 1];
        return NewField::AliasField(alias.to_string());
    }
    if trimmed.starts_with("alias_match_left[") && trimmed.ends_with(']') {
        let alias = &trimmed[17..trimmed.len() - 1];
        return NewField::AliasField(alias.to_string());
    }
    if trimmed.starts_with("single_alias_right[") && trimmed.ends_with(']') {
        let alias = &trimmed[19..trimmed.len() - 1];
        return NewField::SingleAliasField(alias.to_string());
    }
    if trimmed == "localisation" {
        return NewField::LocalisationField {
            synced: false,
            is_inline: false,
        };
    }
    if trimmed == "filepath" {
        return NewField::FilepathField {
            prefix: None,
            extension: None,
        };
    }
    if trimmed == "colour_field" || trimmed == "color_field" {
        return NewField::MarkerField(Marker::ColourField);
    }
    if trimmed == "ir_country_tag_field" {
        return NewField::MarkerField(Marker::IrCountryTag);
    }
    if trimmed == "ck2_dna_field" {
        return NewField::ValueField(ValueType::Ck2Dna);
    }
    if trimmed == "ck2_dna_property_field" {
        return NewField::ValueField(ValueType::Ck2DnaProperty);
    }
    if trimmed == "ir_family_name_field" {
        return NewField::ValueField(ValueType::IrFamilyName);
    }
    if trimmed == "date_field" {
        return NewField::ValueField(ValueType::Date);
    }
    if trimmed == "datetime_field" {
        return NewField::ValueField(ValueType::DateTime);
    }
    if trimmed == "percent_field" {
        return NewField::ValueField(ValueType::Percent);
    }
    if trimmed == "stellaris_name_format" {
        return NewField::ValueField(ValueType::StlNameFormat(String::new()));
    }
    if trimmed.starts_with("stellaris_name_format[") && trimmed.ends_with(']') {
        let var = &trimmed[22..trimmed.len() - 1];
        return NewField::ValueField(ValueType::StlNameFormat(var.to_string()));
    }
    if trimmed == "ignore_field" {
        return NewField::IgnoreMarkerField;
    }

    // Default: specific string value
    NewField::SpecificField(trimmed.to_string())
}
