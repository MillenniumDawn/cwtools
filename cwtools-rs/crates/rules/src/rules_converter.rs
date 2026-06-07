use crate::rules_types::*;
use cwtools_parser::ast::{Child, ParsedFile, Value};
use cwtools_string_table::string_table::StringTable;

// ±1e12 sentinel for unranged float; 1e6 was too narrow (build costs, populations).
const FLOAT_MAX: f64 = 1e12;
const FLOAT_MIN: f64 = -1e12;
const INT_MAX: i32 = 2_147_483_647;
const INT_MIN: i32 = -2_147_483_648;

/// Precompute comment text directly preceding every child in a single O(N) pass.
/// `result[i]` is the list of comments before child `i` (may be empty).
fn precompute_comments(
    children: &[Child],
    ast: &ParsedFile,
    _table: &StringTable,
) -> Vec<Vec<String>> {
    let mut result = vec![Vec::new(); children.len()];
    let mut pending: Vec<String> = Vec::new();
    for (i, child) in children.iter().enumerate() {
        match child {
            Child::Comment(cidx) => {
                let c = &ast.arena.comments[*cidx as usize];
                pending.push(c.text.trim().to_string());
            }
            _ => {
                if !pending.is_empty() {
                    result[i] = std::mem::take(&mut pending);
                }
            }
        }
    }
    result
}

/// Convert a parsed .cwt AST into a RuleSet.
pub fn ast_to_ruleset(ast: &ParsedFile, table: &StringTable) -> RuleSet {
    let mut ruleset = RuleSet::new();

    let precomputed = precompute_comments(&ast.root_children, ast, table);
    for (idx, child) in ast.root_children.iter().enumerate() {
        let comments = &precomputed[idx];

        match child {
            Child::Node(nidx) => {
                let node = &ast.arena.nodes[*nidx as usize];
                let key = table.get_string(node.key.normal).unwrap_or_default();
                match key.as_str() {
                    "types" => {
                        extract_types_from_children(&node.children, ast, table, &mut ruleset)
                    }
                    "enums" => {
                        extract_enums_from_children(&node.children, ast, table, &mut ruleset)
                    }
                    "values" => {
                        extract_values_from_children(&node.children, ast, table, &mut ruleset)
                    }
                    "modifiers" => extract_modifier_names(&node.children, ast, table, &mut ruleset),
                    "links" => extract_links(&node.children, ast, table, &mut ruleset),
                    "scopes" => extract_scope_defs(&node.children, ast, table, &mut ruleset),
                    _ => {
                        process_root_node(key, node, ast, table, comments, &mut ruleset);
                    }
                }
            }
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
                    "values" => {
                        if let Value::Clause(children) = &leaf.value {
                            extract_values_from_children(children, ast, table, &mut ruleset);
                        }
                    }
                    "modifiers" => {
                        if let Value::Clause(children) = &leaf.value {
                            extract_modifier_names(children, ast, table, &mut ruleset);
                        }
                    }
                    "links" => {
                        if let Value::Clause(children) = &leaf.value {
                            extract_links(children, ast, table, &mut ruleset);
                        }
                    }
                    "scopes" => {
                        if let Value::Clause(children) = &leaf.value {
                            extract_scope_defs(children, ast, table, &mut ruleset);
                        }
                    }
                    _ => {
                        process_root_leaf(key, leaf, ast, table, comments, &mut ruleset);
                    }
                }
            }
            _ => {}
        }
    }

    ruleset.reindex();
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
        if let Some((category, _alias_name)) = get_alias_settings(&key, "alias") {
            let full_name = format!("{}:{}", category, _alias_name);
            let rule = node_to_noderule(node, ast, table, ruleset);
            let opts = options_from_comments(comments, false);
            ruleset.aliases.push((
                full_name,
                (
                    RuleType::NodeRule {
                        left: NewField::AliasField(category),
                        rules: rule,
                    },
                    opts,
                ),
            ));
        }
    } else if key.starts_with("single_alias[") {
        if let Some(alias_name) = get_setting_from_string(&key, "single_alias") {
            let rule = node_to_noderule(node, ast, table, ruleset);
            let opts = options_from_comments(comments, false);
            ruleset.single_aliases.push((
                alias_name.clone(),
                (
                    RuleType::NodeRule {
                        left: NewField::SingleAliasField(alias_name),
                        rules: rule,
                    },
                    opts,
                ),
            ));
        }
    } else {
        let rule = build_rule_from_node(node, ast, table, ruleset);
        let opts = options_from_comments(comments, false);
        ruleset
            .root_rules
            .push(RootRule::TypeRule(key, (rule, opts)));
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
        if let Some((category, _alias_name)) = get_alias_settings(&key, "alias") {
            let full_name = format!("{}:{}", category, _alias_name);
            let rule = leaf_to_rule(leaf, ast, table, ruleset);
            let opts = options_from_comments(comments, leaf_is_eqeq(leaf));
            ruleset.aliases.push((full_name, (rule, opts)));
        }
    } else if key.starts_with("single_alias[") {
        if let Some(alias_name) = get_setting_from_string(&key, "single_alias") {
            let rule = leaf_to_rule(leaf, ast, table, ruleset);
            let opts = options_from_comments(comments, leaf_is_eqeq(leaf));
            ruleset.single_aliases.push((alias_name, (rule, opts)));
        }
    } else {
        let rule = leaf_to_rule(leaf, ast, table, ruleset);
        let opts = options_from_comments(comments, leaf_is_eqeq(leaf));
        ruleset
            .root_rules
            .push(RootRule::TypeRule(key, (rule, opts)));
    }
}

fn leaf_is_eqeq(leaf: &cwtools_parser::ast::Leaf) -> bool {
    leaf.op == cwtools_parser::ast::Operator::EqualEqual
}

fn build_rule_from_node(
    node: &cwtools_parser::ast::Node,
    ast: &ParsedFile,
    table: &StringTable,
    ruleset: &mut RuleSet,
) -> RuleType {
    let inner = node_to_noderule(node, ast, table, ruleset);
    RuleType::NodeRule {
        left: NewField::SpecificField(table.get_string(node.key.normal).unwrap_or_default()),
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
            let key_str = table.get_string(leaf.key.normal).unwrap_or_default();
            let left = field_from_string(&key_str);
            let right = field_from_string(&value_to_string(&leaf.value, table));
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
    children_to_rules(&node.children, ast, table, ruleset)
}

/// Shared field parser for both left-hand keys and right-hand values.
/// Matches F# processKey (RulesParser.fs:371-567).
fn field_from_string(s: &str) -> NewField {
    let trimmed = s.trim().trim_matches('"');

    match trimmed {
        "scalar" => return NewField::ScalarField,
        "bool" => return NewField::ValueField(ValueType::Bool),
        "int" => {
            return NewField::ValueField(ValueType::Int {
                min: INT_MIN,
                max: INT_MAX,
            });
        }
        "float" => {
            return NewField::ValueField(ValueType::Float {
                min: FLOAT_MIN,
                max: FLOAT_MAX,
            });
        }
        "percentage_field" => return NewField::ValueField(ValueType::Percent),
        "localisation" => {
            return NewField::LocalisationField {
                synced: false,
                is_inline: false,
            };
        }
        "localisation_synced" => {
            return NewField::LocalisationField {
                synced: true,
                is_inline: false,
            };
        }
        "localisation_inline" => {
            return NewField::LocalisationField {
                synced: false,
                is_inline: true,
            };
        }
        "filepath" => {
            return NewField::FilepathField {
                prefix: None,
                extension: None,
            };
        }
        "date_field" => return NewField::ValueField(ValueType::Date),
        "datetime_field" => return NewField::ValueField(ValueType::DateTime),
        "scope_field" => return NewField::ScopeField(vec!["any".to_string()]),
        "variable_field" => {
            return NewField::VariableField {
                is_int: false,
                is_32bit: false,
                min: FLOAT_MIN,
                max: FLOAT_MAX,
            };
        }
        "int_variable_field" => {
            return NewField::VariableField {
                is_int: true,
                is_32bit: false,
                min: INT_MIN as f64,
                max: INT_MAX as f64,
            };
        }
        "variable_field_32" => {
            return NewField::VariableField {
                is_int: false,
                is_32bit: true,
                min: FLOAT_MIN,
                max: FLOAT_MAX,
            };
        }
        "int_variable_field_32" => {
            return NewField::VariableField {
                is_int: true,
                is_32bit: true,
                min: INT_MIN as f64,
                max: INT_MAX as f64,
            };
        }
        "value_field" => {
            return NewField::ValueScopeMarkerField {
                is_int: false,
                min: FLOAT_MIN,
                max: FLOAT_MAX,
            };
        }
        "int_value_field" => {
            return NewField::ValueScopeMarkerField {
                is_int: true,
                min: INT_MIN as f64,
                max: INT_MAX as f64,
            };
        }
        "portrait_dna_field" => return NewField::ValueField(ValueType::Ck2Dna),
        "portrait_properties_field" => return NewField::ValueField(ValueType::Ck2DnaProperty),
        // Legacy aliases from earlier implementation
        "ck2_dna_field" => return NewField::ValueField(ValueType::Ck2Dna),
        "ck2_dna_property_field" => return NewField::ValueField(ValueType::Ck2DnaProperty),
        "ir_family_name_field" => return NewField::ValueField(ValueType::IrFamilyName),
        "ir_country_tag_field" => return NewField::MarkerField(Marker::IrCountryTag),
        "colour_field" | "color_field" => return NewField::MarkerField(Marker::ColourField),
        "ignore_field" => return NewField::IgnoreMarkerField,
        "stellaris_name_format" => {
            return NewField::ValueField(ValueType::StlNameFormat(String::new()));
        }
        "percent_field" => return NewField::ValueField(ValueType::Percent),
        _ => {}
    }

    // filepath[folder] or filepath[folder,ext]
    if trimmed.starts_with("filepath[") && trimmed.ends_with(']') {
        let inner = &trimmed[9..trimmed.len() - 1];
        return if inner.contains(',') {
            let mut parts = inner.splitn(2, ',');
            let folder = parts.next().unwrap_or("").to_string();
            let ext = parts.next().unwrap_or("").to_string();
            NewField::FilepathField {
                prefix: Some(folder),
                extension: Some(ext),
            }
        } else {
            NewField::FilepathField {
                prefix: Some(inner.to_string()),
                extension: None,
            }
        };
    }

    // int[min..max] with inf/-inf sentinels
    if trimmed.starts_with("int[") && trimmed.ends_with(']') {
        let inner = &trimmed[4..trimmed.len() - 1];
        if let Some((min_s, max_s)) = inner.split_once("..") {
            let min = parse_int_sentinel(min_s.trim());
            let max = parse_int_sentinel(max_s.trim());
            if let (Some(mn), Some(mx)) = (min, max) {
                return NewField::ValueField(ValueType::Int { min: mn, max: mx });
            }
        }
        return NewField::ValueField(ValueType::Int {
            min: INT_MIN,
            max: INT_MAX,
        });
    }

    // float[min..max] with inf/-inf sentinels
    if trimmed.starts_with("float[") && trimmed.ends_with(']') {
        let inner = &trimmed[6..trimmed.len() - 1];
        if let Some((min_s, max_s)) = inner.split_once("..") {
            let min = parse_float_sentinel(min_s.trim());
            let max = parse_float_sentinel(max_s.trim());
            if let (Some(mn), Some(mx)) = (min, max) {
                return NewField::ValueField(ValueType::Float { min: mn, max: mx });
            }
        }
        return NewField::ValueField(ValueType::Float {
            min: FLOAT_MIN,
            max: FLOAT_MAX,
        });
    }

    // enum[x]
    if trimmed.starts_with("enum[") && trimmed.ends_with(']') {
        let name = &trimmed[5..trimmed.len() - 1];
        return NewField::ValueField(ValueType::Enum(name.to_string()));
    }

    // complex_enum[x] — referenced on right side
    if trimmed.starts_with("complex_enum[") && trimmed.ends_with(']') {
        let name = &trimmed[13..trimmed.len() - 1];
        return NewField::ValueField(ValueType::Enum(name.to_string()));
    }

    // value[x] -> VariableGetField
    if trimmed.starts_with("value[") && trimmed.ends_with(']') {
        let var = &trimmed[6..trimmed.len() - 1];
        return NewField::VariableGetField(var.to_string());
    }

    // value_set[x] -> VariableSetField
    if trimmed.starts_with("value_set[") && trimmed.ends_with(']') {
        let var = &trimmed[10..trimmed.len() - 1];
        return NewField::VariableSetField(var.to_string());
    }

    // scope[x]
    if trimmed.starts_with("scope[") && trimmed.ends_with(']') {
        let scope = &trimmed[6..trimmed.len() - 1];
        return NewField::ScopeField(vec![scope.to_string()]);
    }

    // event_target[x] — same as scope[x]
    if trimmed.starts_with("event_target[") && trimmed.ends_with(']') {
        let scope = &trimmed[13..trimmed.len() - 1];
        return NewField::ScopeField(vec![scope.to_string()]);
    }

    // scope_group[x] — resolve to ScopeField with known group or scalar fallback
    if trimmed.starts_with("scope_group[") && trimmed.ends_with(']') {
        let _group = &trimmed[12..trimmed.len() - 1];
        // Without a scope-group map we fall back to any-scope
        return NewField::ScopeField(vec!["any".to_string()]);
    }

    // alias_keys_field[x] -> AliasValueKeysField
    if trimmed.starts_with("alias_keys_field[") && trimmed.ends_with(']') {
        let alias = &trimmed[17..trimmed.len() - 1];
        return NewField::AliasValueKeysField(alias.to_string());
    }

    // alias_name[x] / alias_match_left[x] -> AliasField
    if (trimmed.starts_with("alias_name[") || trimmed.starts_with("alias_match_left["))
        && trimmed.ends_with(']')
    {
        let inner_start = trimmed.find('[').unwrap() + 1;
        let alias = &trimmed[inner_start..trimmed.len() - 1];
        return NewField::AliasField(alias.to_string());
    }

    // single_alias_right[x] -> SingleAliasField
    if trimmed.starts_with("single_alias_right[") && trimmed.ends_with(']') {
        let alias = &trimmed[19..trimmed.len() - 1];
        return NewField::SingleAliasField(alias.to_string());
    }

    // icon[folder] -> IconField
    if trimmed.starts_with("icon[") && trimmed.ends_with(']') {
        let folder = &trimmed[5..trimmed.len() - 1];
        return NewField::IconField(folder.to_string());
    }

    // variable_field[min..max]
    if trimmed.starts_with("variable_field[") && trimmed.ends_with(']') {
        let inner = &trimmed[15..trimmed.len() - 1];
        let (mn, mx) = parse_float_range(inner, FLOAT_MIN, FLOAT_MAX);
        return NewField::VariableField {
            is_int: false,
            is_32bit: false,
            min: mn,
            max: mx,
        };
    }

    // int_variable_field[min..max]
    if trimmed.starts_with("int_variable_field[") && trimmed.ends_with(']') {
        let inner = &trimmed[19..trimmed.len() - 1];
        let (mn, mx) = parse_int_range_as_float(inner, INT_MIN as f64, INT_MAX as f64);
        return NewField::VariableField {
            is_int: true,
            is_32bit: false,
            min: mn,
            max: mx,
        };
    }

    // variable_field_32[min..max]
    if trimmed.starts_with("variable_field_32[") && trimmed.ends_with(']') {
        let inner = &trimmed[18..trimmed.len() - 1];
        let (mn, mx) = parse_float_range(inner, FLOAT_MIN, FLOAT_MAX);
        return NewField::VariableField {
            is_int: false,
            is_32bit: true,
            min: mn,
            max: mx,
        };
    }

    // int_variable_field_32[min..max]
    if trimmed.starts_with("int_variable_field_32[") && trimmed.ends_with(']') {
        let inner = &trimmed[22..trimmed.len() - 1];
        let (mn, mx) = parse_int_range_as_float(inner, INT_MIN as f64, INT_MAX as f64);
        return NewField::VariableField {
            is_int: true,
            is_32bit: true,
            min: mn,
            max: mx,
        };
    }

    // value_field[min..max]
    if trimmed.starts_with("value_field[") && trimmed.ends_with(']') {
        let inner = &trimmed[12..trimmed.len() - 1];
        let (mn, mx) = parse_float_range(inner, FLOAT_MIN, FLOAT_MAX);
        return NewField::ValueScopeMarkerField {
            is_int: false,
            min: mn,
            max: mx,
        };
    }

    // int_value_field[min..max]
    if trimmed.starts_with("int_value_field[") && trimmed.ends_with(']') {
        let inner = &trimmed[16..trimmed.len() - 1];
        let (mn, mx) = parse_int_range_as_float(inner, INT_MIN as f64, INT_MAX as f64);
        return NewField::ValueScopeMarkerField {
            is_int: true,
            min: mn,
            max: mx,
        };
    }

    // stellaris_name_format[x]
    if trimmed.starts_with("stellaris_name_format[") && trimmed.ends_with(']') {
        let var = &trimmed[22..trimmed.len() - 1];
        return NewField::ValueField(ValueType::StlNameFormat(var.to_string()));
    }

    // colour[rgb] / colour[hsv] handled in children_to_rules at leaf level
    // Here we just emit the marker so the post-processor can expand it
    if trimmed.starts_with("colour[") && trimmed.ends_with(']') {
        return NewField::MarkerField(Marker::ColourField);
    }

    // <type> simple
    if trimmed.starts_with('<') && trimmed.ends_with('>') && !trimmed.contains(' ') {
        let type_ref = &trimmed[1..trimmed.len() - 1];
        return NewField::TypeField(TypeType::Simple(type_ref.to_string()));
    }

    // prefix<type>suffix complex
    if trimmed.contains('<') && trimmed.contains('>') {
        let s = trimmed.trim_matches('"');
        if let (Some(pi), Some(si)) = (s.find('<'), s.find('>'))
            && pi < si {
                let prefix = s[..pi].to_string();
                let name = s[pi + 1..si].to_string();
                let suffix = s[si + 1..].to_string();
                return NewField::TypeField(TypeType::Complex {
                    prefix,
                    name,
                    suffix,
                });
            }
    }

    // Default: specific string value
    NewField::SpecificField(trimmed.to_string())
}

fn parse_int_sentinel(s: &str) -> Option<i32> {
    match s {
        "inf" => Some(INT_MAX),
        "-inf" => Some(INT_MIN),
        other => other.parse::<i32>().ok(),
    }
}

fn parse_float_sentinel(s: &str) -> Option<f64> {
    match s {
        "inf" => Some(FLOAT_MAX),
        "-inf" => Some(FLOAT_MIN),
        other => other.parse::<f64>().ok(),
    }
}

fn parse_float_range(inner: &str, default_min: f64, default_max: f64) -> (f64, f64) {
    if let Some((min_s, max_s)) = inner.split_once("..") {
        let mn = parse_float_sentinel(min_s.trim()).unwrap_or(default_min);
        let mx = parse_float_sentinel(max_s.trim()).unwrap_or(default_max);
        (mn, mx)
    } else {
        (default_min, default_max)
    }
}

fn parse_int_range_as_float(inner: &str, default_min: f64, default_max: f64) -> (f64, f64) {
    if let Some((min_s, max_s)) = inner.split_once("..") {
        let mn = parse_int_sentinel(min_s.trim())
            .map(|v| v as f64)
            .unwrap_or(default_min);
        let mx = parse_int_sentinel(max_s.trim())
            .map(|v| v as f64)
            .unwrap_or(default_max);
        (mn, mx)
    } else {
        (default_min, default_max)
    }
}

// `ruleset` is threaded so nested rules can register types/enums as the engine
// grows; today only the recursive descent forwards it.
#[allow(clippy::only_used_in_recursion)]
fn children_to_rules(
    children: &[Child],
    ast: &ParsedFile,
    table: &StringTable,
    ruleset: &mut RuleSet,
) -> Vec<NewRule> {
    let mut rules = Vec::new();
    let precomputed = precompute_comments(children, ast, table);
    for (idx, child) in children.iter().enumerate() {
        let comments = &precomputed[idx];
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
                            Value::Clause(ch) => children_to_rules(ch, ast, table, ruleset),
                            _ => Vec::new(),
                        };
                        rules.push((
                            RuleType::SubtypeRule {
                                name,
                                positive,
                                rules: inner,
                            },
                            options_from_comments(comments, false),
                        ));
                    }
                    continue;
                }

                let is_eqeq = leaf.op == cwtools_parser::ast::Operator::EqualEqual;
                let opts = options_from_comments(comments, is_eqeq);
                let rule = match &leaf.value {
                    Value::Clause(ch) => {
                        let inner = children_to_rules(ch, ast, table, ruleset);
                        RuleType::NodeRule {
                            left: field_from_string(&key),
                            rules: inner,
                        }
                    }
                    _ => {
                        let right_str = value_to_string(&leaf.value, table);
                        // colour[rgb]/colour[hsv] special: expand inline to NodeRule
                        if right_str.starts_with("colour[") && right_str.ends_with(']') {
                            let colour_rules = build_colour_rules(&right_str);
                            RuleType::NodeRule {
                                left: field_from_string(&key),
                                rules: colour_rules,
                            }
                        } else {
                            let left = field_from_string(&key);
                            let right = field_from_string(&right_str);
                            RuleType::LeafRule { left, right }
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
                        let inner = children_to_rules(&node.children, ast, table, ruleset);
                        rules.push((
                            RuleType::SubtypeRule {
                                name,
                                positive,
                                rules: inner,
                            },
                            options_from_comments(comments, false),
                        ));
                    }
                    continue;
                }

                let opts = options_from_comments(comments, false);
                let inner = children_to_rules(&node.children, ast, table, ruleset);
                rules.push((
                    RuleType::NodeRule {
                        left: field_from_string(&key),
                        rules: inner,
                    },
                    opts,
                ));
            }
            Child::LeafValue(lvidx) => {
                let lv = &ast.arena.leaf_values[*lvidx as usize];
                if let Value::Clause(clause_ch) = &lv.value {
                    // Anonymous {…} block in a rule definition — same as F# ValueClauseC.
                    let opts = options_from_comments(comments, false);
                    let inner = children_to_rules(clause_ch, ast, table, ruleset);
                    rules.push((RuleType::ValueClauseRule { rules: inner }, opts));
                } else {
                    let val_str = value_to_string(&lv.value, table);
                    let field = field_from_string(&val_str);
                    let mut opts = options_from_comments(comments, false);
                    opts.leafvalue = true;
                    rules.push((RuleType::LeafValueRule { right: field }, opts));
                }
            }
            Child::ValueClause(vcidx) => {
                // Anonymous {…} parsed as a true ValueClause node (some parser versions).
                let vc = &ast.arena.value_clauses[*vcidx as usize];
                let opts = options_from_comments(comments, false);
                let inner = children_to_rules(&vc.children, ast, table, ruleset);
                rules.push((RuleType::ValueClauseRule { rules: inner }, opts));
            }
            _ => {}
        }
    }
    rules
}

/// Build colour sub-rules for colour[rgb] / colour[hsv] inline expansion.
fn build_colour_rules(colour_spec: &str) -> Vec<NewRule> {
    let inner = if colour_spec.starts_with("colour[") && colour_spec.ends_with(']') {
        &colour_spec[7..colour_spec.len() - 1]
    } else {
        ""
    };
    match inner {
        "rgb" => vec![(
            RuleType::LeafValueRule {
                right: NewField::ValueField(ValueType::Int { min: 0, max: 255 }),
            },
            Options {
                min: 3,
                max: 4,
                strict_min: true,
                leafvalue: true,
                ..Options::default()
            },
        )],
        "hsv" => vec![(
            RuleType::LeafValueRule {
                right: NewField::ValueField(ValueType::Float { min: 0.0, max: 2.0 }),
            },
            Options {
                min: 3,
                max: 4,
                strict_min: true,
                leafvalue: true,
                ..Options::default()
            },
        )],
        _ => {
            // Unknown colour format — emit both
            vec![
                (
                    RuleType::LeafValueRule {
                        right: NewField::ValueField(ValueType::Int { min: 0, max: 255 }),
                    },
                    Options {
                        min: 3,
                        max: 4,
                        strict_min: true,
                        leafvalue: true,
                        ..Options::default()
                    },
                ),
                (
                    RuleType::LeafValueRule {
                        right: NewField::ValueField(ValueType::Float { min: 0.0, max: 2.0 }),
                    },
                    Options {
                        min: 3,
                        max: 4,
                        strict_min: true,
                        leafvalue: true,
                        ..Options::default()
                    },
                ),
            ]
        }
    }
}

fn extract_types_from_children(
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
            && let Some(typename) = extract_bracket_content(&key, "type") {
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

fn extract_enums_from_children(
    children: &[Child],
    ast: &ParsedFile,
    table: &StringTable,
    ruleset: &mut RuleSet,
) {
    let precomputed = precompute_comments(children, ast, table);
    for (idx, echild) in children.iter().enumerate() {
        let comments = &precomputed[idx];
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
                        process_enum_node(
                            enum_name,
                            &ast.arena.leaves[*lidx as usize],
                            ast,
                            table,
                            comments,
                        )
                    } else {
                        continue;
                    }
                } else {
                    if let Child::Node(nidx) = echild {
                        process_enum_node_from_node(
                            enum_name,
                            &ast.arena.nodes[*nidx as usize],
                            ast,
                            table,
                            comments,
                        )
                    } else {
                        continue;
                    }
                };
                ruleset.enums.push(def);
            }
        } else if key.starts_with("complex_enum[")
            && let Some(enum_name) = extract_bracket_content(&key, "complex_enum") {
                if !is_leaf {
                    if let Child::Node(nidx) = echild {
                        let node = &ast.arena.nodes[*nidx as usize];
                        let def = process_complex_enum_node(enum_name, node, ast, table, comments);
                        ruleset.complex_enums.push(def);
                    }
                } else if let Child::Leaf(lidx) = echild {
                    let leaf = &ast.arena.leaves[*lidx as usize];
                    if let Value::Clause(ch) = &leaf.value {
                        // Synthesize a node-like view from the clause children
                        let def =
                            process_complex_enum_from_children(enum_name, ch, ast, table, comments);
                        ruleset.complex_enums.push(def);
                    }
                }
            }
    }
}

/// Parse `values = { value[name] = { ... } }` top-level block (F# RulesParser.fs:1298-1321).
/// Collect modifier names from a top-level `modifiers = { name = category ... }`
/// block. Each entry's key is a valid modifier name (the value is its category).
fn extract_modifier_names(
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
fn extract_scope_defs(
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
fn extract_links(children: &[Child], ast: &ParsedFile, table: &StringTable, ruleset: &mut RuleSet) {
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

fn extract_values_from_children(
    children: &Vec<Child>,
    ast: &ParsedFile,
    table: &StringTable,
    ruleset: &mut RuleSet,
) {
    for vchild in children {
        let (key, is_leaf) = match vchild {
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
        if key.starts_with("value[")
            && let Some(value_name) = extract_bracket_content(&key, "value") {
                let vals = if is_leaf {
                    if let Child::Leaf(lidx) = vchild {
                        collect_leaf_values_from_clause(
                            &ast.arena.leaves[*lidx as usize].value,
                            ast,
                            table,
                        )
                    } else {
                        Vec::new()
                    }
                } else {
                    if let Child::Node(nidx) = vchild {
                        let node = &ast.arena.nodes[*nidx as usize];
                        collect_leaf_values_from_children(&node.children, ast, table)
                    } else {
                        Vec::new()
                    }
                };
                ruleset.values.push((value_name, vals));
            }
    }
}

fn collect_leaf_values_from_clause(
    value: &Value,
    ast: &ParsedFile,
    table: &StringTable,
) -> Vec<String> {
    if let Value::Clause(ch) = value {
        collect_leaf_values_from_children(ch, ast, table)
    } else {
        Vec::new()
    }
}

fn collect_leaf_values_from_children(
    children: &[Child],
    ast: &ParsedFile,
    table: &StringTable,
) -> Vec<String> {
    let mut out = Vec::new();
    for child in children {
        match child {
            Child::LeafValue(lvidx) => {
                let lv = &ast.arena.leaf_values[*lvidx as usize];
                let v = value_to_string(&lv.value, table);
                if !v.is_empty() {
                    out.push(v);
                }
            }
            Child::Leaf(lidx) => {
                let l = &ast.arena.leaves[*lidx as usize];
                let v = table.get_string(l.key.normal).unwrap_or_default();
                if !v.is_empty() {
                    out.push(v);
                }
            }
            _ => {}
        }
    }
    out
}

fn process_type_node_from_node(
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

fn process_enum_node_from_node(
    name: String,
    node: &cwtools_parser::ast::Node,
    ast: &ParsedFile,
    table: &StringTable,
    comments: &[String],
) -> EnumDefinition {
    let synthetic_leaf = cwtools_parser::ast::Leaf {
        key: node.key,
        value: Value::Clause(node.children.clone()),
        op: cwtools_parser::ast::Operator::Equals,
        pos: node.pos,
    };
    process_enum_node(name, &synthetic_leaf, ast, table, comments)
}

fn extract_bracket_content(full: &str, prefix: &str) -> Option<String> {
    if let Some(body) = full.strip_prefix(prefix)
        && let Some(inner) = body.strip_prefix('[').and_then(|s| s.strip_suffix(']')) {
            return Some(inner.to_string());
        }
    None
}

fn process_type_node(
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
                            "path_strict"
                                if leaf_value_string(l, table) == "yes" => {
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
                            "type_per_file"
                                if leaf_value_string(l, table) == "yes" => {
                                    def.type_per_file = true;
                                }
                            "starts_with" => {
                                def.starts_with = Some(leaf_value_string(l, table));
                            }
                            "type_key_prefix" => {
                                def.key_prefix = Some(leaf_value_string(l, table));
                            }
                            "severity"
                                if leaf_value_string(l, table) == "warning" => {
                                    def.warning_only = true;
                                }
                            "unique"
                                if leaf_value_string(l, table) == "yes" => {
                                    def.unique = true;
                                }
                            "should_be_used"
                                if leaf_value_string(l, table) == "yes" => {
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

fn parse_type_key_filter_from_comments(comments: &[String]) -> Option<(Vec<String>, bool)> {
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
        && let Some(idx) = c.find('=') {
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

fn parse_localisation_block(
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

fn parse_subtype_localisation(
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
                && let Some(st_name) = extract_bracket_content(&nk, "subtype") {
                    let locs = parse_localisation_block(&n.children, ast, table);
                    out.push((st_name, locs));
                }
        }
    }
    out
}

fn parse_modifiers_block(
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

fn parse_subtype_modifiers(
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
                && let Some(st_name) = extract_bracket_content(&nk, "subtype") {
                    let mods = parse_modifiers_block(&n.children, ast, table);
                    out.push((st_name, mods));
                }
        }
    }
    out
}

fn process_subtype_node(
    name: String,
    node: &cwtools_parser::ast::Node,
    ast: &ParsedFile,
    table: &StringTable,
    ruleset: &mut RuleSet,
    comments: &[String],
) -> SubTypeDefinition {
    build_subtype(name, &node.children, ast, table, ruleset, comments)
}

fn process_subtype_node_from_leaf(
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
        && let Some(idx) = c.find('=') {
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

fn process_enum_node(
    name: String,
    leaf: &cwtools_parser::ast::Leaf,
    ast: &ParsedFile,
    table: &StringTable,
    comments: &[String],
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

    // Description from ### or ## comments
    let description = extract_description_from_comments(comments).unwrap_or_else(|| name.clone());

    EnumDefinition {
        key: name,
        description,
        values,
    }
}

/// Extract description from ### comments (## are options).
fn extract_description_from_comments(comments: &[String]) -> Option<String> {
    // Only `###` lines are documentation. `##` lines are rule options
    // (cardinality, scope, severity, ...) and must NOT leak into the hover
    // tooltip. This intentionally diverges from F# (RulesParser.fs collects
    // every `##` line), which polluted every tooltip with option text.
    let desc_lines: Vec<String> = comments
        .iter()
        .filter(|s| s.starts_with("###"))
        .map(|s| s.trim_matches('#').trim().to_string())
        .collect();
    match desc_lines.len() {
        0 => None,
        1 => Some(desc_lines[0].clone()),
        _ => Some(desc_lines.join("\n")),
    }
}

fn process_complex_enum_node(
    name: String,
    node: &cwtools_parser::ast::Node,
    ast: &ParsedFile,
    table: &StringTable,
    comments: &[String],
) -> ComplexEnumDef {
    process_complex_enum_from_children(name, &node.children, ast, table, comments)
}

fn process_complex_enum_from_children(
    name: String,
    children: &[Child],
    ast: &ParsedFile,
    table: &StringTable,
    comments: &[String],
) -> ComplexEnumDef {
    let mut paths: Vec<String> = Vec::new();
    let mut path_strict = false;
    let mut path_file = None;
    let mut path_extension = None;
    let mut start_from_root = false;
    let mut name_tree: Option<ComplexEnumNameTree> = None;

    for child in children {
        match child {
            Child::Leaf(lidx) => {
                let l = &ast.arena.leaves[*lidx as usize];
                let k = table.get_string(l.key.normal).unwrap_or_default();
                // Handle `name = { ... }` as a Leaf with Clause value
                if k == "name"
                    && let Value::Clause(name_ch) = &l.value {
                        name_tree = Some(build_name_tree(name_ch, ast, table));
                        continue;
                    }
                let v = leaf_value_string(l, table);
                match k.as_str() {
                    "path" => paths.push(clean_path(&v)),
                    "path_strict"
                        if v == "yes" => {
                            path_strict = true;
                        }
                    "path_file" => {
                        path_file = Some(v);
                    }
                    "path_extension" => {
                        path_extension = Some(v);
                    }
                    "start_from_root"
                        if v == "yes" => {
                            start_from_root = true;
                        }
                    _ => {}
                }
            }
            Child::Node(nidx) => {
                let n = &ast.arena.nodes[*nidx as usize];
                let nk = table.get_string(n.key.normal).unwrap_or_default();
                if nk == "name" {
                    name_tree = Some(build_name_tree(&n.children, ast, table));
                }
            }
            _ => {}
        }
    }

    let description = extract_description_from_comments(comments).unwrap_or_else(|| name.clone());

    ComplexEnumDef {
        name,
        description,
        path_options: PathOptions {
            paths,
            path_strict,
            path_file,
            path_extension,
            paths_lower: Vec::new(),
        },
        name_tree: name_tree.unwrap_or(ComplexEnumNameTree::Empty),
        start_from_root,
    }
}

/// Build a ComplexEnumNameTree from the `name = { ... }` block children.
fn build_name_tree(
    children: &[Child],
    ast: &ParsedFile,
    table: &StringTable,
) -> ComplexEnumNameTree {
    let mut entries = Vec::new();
    for child in children {
        match child {
            Child::Leaf(lidx) => {
                let l = &ast.arena.leaves[*lidx as usize];
                let k = table.get_string(l.key.normal).unwrap_or_default();
                // Leaf with Clause value = nested node in CWT
                if let Value::Clause(sub_ch) = &l.value {
                    let sub = build_name_tree(sub_ch, ast, table);
                    entries.push(ComplexEnumNameTreeEntry::Node {
                        key: k,
                        children: sub,
                    });
                } else {
                    let v = leaf_value_string(l, table);
                    if v == "enum_name" || v == "this" {
                        entries.push(ComplexEnumNameTreeEntry::Leaf {
                            key: k,
                            is_name: true,
                        });
                    } else {
                        entries.push(ComplexEnumNameTreeEntry::Leaf {
                            key: k,
                            is_name: false,
                        });
                    }
                }
            }
            Child::Node(nidx) => {
                let n = &ast.arena.nodes[*nidx as usize];
                let nk = table.get_string(n.key.normal).unwrap_or_default();
                let sub = build_name_tree(&n.children, ast, table);
                entries.push(ComplexEnumNameTreeEntry::Node {
                    key: nk,
                    children: sub,
                });
            }
            _ => {}
        }
    }
    ComplexEnumNameTree::Entries(entries)
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
        // CW script uses yes/no for booleans, not true/false
        Value::Bool(true) => "yes".to_string(),
        Value::Bool(false) => "no".to_string(),
        Value::Clause(_) => String::new(),
    }
}

fn get_alias_settings(full: &str, prefix: &str) -> Option<(String, String)> {
    let setting = get_setting_from_string(full, prefix)?;
    let parts: Vec<&str> = setting.splitn(2, ':').collect();
    if parts.len() < 2 {
        None
    } else {
        Some((parts[0].to_string(), parts[1].to_string()))
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

/// Parse Options from comment lines preceding a rule.
/// CRITICAL: when NO cardinality comment is present, use min=1, max=1, strict_min=true (F# default).
fn options_from_comments(comments: &[String], is_comparison: bool) -> Options {
    // Cardinality: match by Contains("cardinality") (handles both ## and ##cardinality= forms)
    let (min, max, strict_min) =
        if let Some(c) = comments.iter().find(|s| s.contains("cardinality")) {
            // Extract everything after the '='
            if let Some(eq_idx) = c.find('=') {
                let spec = c[eq_idx + 1..].trim();
                if let Some((min_s, max_s)) = spec.split_once("..") {
                    let min_s = min_s.trim();
                    // Handle ~ prefix for strict_min=false
                    let (min_s, strict) = match min_s.strip_prefix('~') {
                        Some(rest) => (rest, false),
                        None => (min_s, true),
                    };
                    let min = min_s.parse::<i32>().unwrap_or(1);
                    let max = if max_s.trim() == "inf" {
                        i32::MAX
                    } else {
                        max_s.trim().parse::<i32>().unwrap_or(1)
                    };
                    (min, max, strict)
                } else {
                    (1, 1, true)
                }
            } else {
                (1, 1, true)
            }
        } else {
            // No cardinality comment -> F# default: 1..1, strict
            (1, 1, true)
        };

    // Description: all ## lines joined
    let description = extract_description_from_comments(comments);

    // push_scope: match by Contains("push_scope")
    let push_scope = comments
        .iter()
        .find(|s| s.contains("push_scope"))
        .and_then(|s| s.find('=').map(|i| s[i + 1..].trim().to_string()))
        .filter(|s| !s.is_empty());

    // replace_scopes / replace_scope: hand-parse key=value pairs
    let replace_scopes = parse_replace_scopes_from_comments(comments);

    // severity
    let severity = comments
        .iter()
        .find(|s| s.contains("severity"))
        .and_then(|s| s.find('=').map(|i| s[i + 1..].trim().to_string()))
        .and_then(|sev| match sev.as_str() {
            "error" => Some(Severity::Error),
            "warning" => Some(Severity::Warning),
            "info" | "information" => Some(Severity::Information),
            "hint" => Some(Severity::Hint),
            _ => None,
        });

    // required_scopes: # scope = X or # scope = { A B }
    let required_scopes = parse_required_scopes(comments);

    // reference_details
    let reference_details = if let Some(c) = comments
        .iter()
        .find(|s| s.contains("outgoingReferenceLabel"))
    {
        c.find('=').map(|i| (true, c[i + 1..].trim().to_string()))
    } else if let Some(c) = comments
        .iter()
        .find(|s| s.contains("incomingReferenceLabel"))
    {
        c.find('=').map(|i| (false, c[i + 1..].trim().to_string()))
    } else {
        None
    };

    // error_if_only_match
    let error_if_only_match = comments
        .iter()
        .find(|s| s.contains("error_if_only_match"))
        .and_then(|s| s.find('=').map(|i| s[i + 1..].trim().to_string()))
        .filter(|s| !s.is_empty());

    Options {
        min,
        max,
        strict_min,
        leafvalue: false,
        description,
        push_scope,
        replace_scopes,
        severity,
        required_scopes,
        comparison: is_comparison,
        reference_details,
        key_required_quotes: false,
        value_required_quotes: false,
        type_hint: None,
        error_if_only_match,
    }
}

fn parse_replace_scopes_from_comments(comments: &[String]) -> Option<ReplaceScopes> {
    let line = comments.iter().find(|s| s.contains("replace_scope"))?;

    // Hand-parse key=value pairs from the comment text
    // Strip leading # chars to get the content
    let content = line.trim_start_matches('#').trim();

    // Find replace_scope(s) = { ... } or replace_scope(s) = bare_value
    let rs_start = content.find("replace_scope").unwrap_or(0);
    let after_rs = &content[rs_start..];

    let eq_idx = after_rs.find('=')?;
    let rhs = after_rs[eq_idx + 1..].trim();

    let pairs_str = if rhs.starts_with('{') {
        let close = rhs.find('}')?;
        &rhs[1..close]
    } else {
        rhs
    };

    let mut root = None;
    let mut this = None;
    let mut froms = Vec::new();
    let mut prevs = Vec::new();

    // Parse space-separated key = value pairs
    let tokens: Vec<&str> = pairs_str.split_whitespace().collect();
    let mut ti = 0;
    while ti + 2 < tokens.len() {
        if tokens[ti + 1] == "=" {
            match tokens[ti] {
                "this" => this = Some(tokens[ti + 2].to_string()),
                "root" => root = Some(tokens[ti + 2].to_string()),
                "from" => {
                    if froms.is_empty() {
                        froms.push(tokens[ti + 2].to_string());
                    } else {
                        froms[0] = tokens[ti + 2].to_string();
                    }
                }
                "fromfrom" => {
                    while froms.len() < 2 {
                        froms.push(String::new());
                    }
                    froms[1] = tokens[ti + 2].to_string();
                }
                "fromfromfrom" => {
                    while froms.len() < 3 {
                        froms.push(String::new());
                    }
                    froms[2] = tokens[ti + 2].to_string();
                }
                "fromfromfromfrom" => {
                    while froms.len() < 4 {
                        froms.push(String::new());
                    }
                    froms[3] = tokens[ti + 2].to_string();
                }
                "prev" => {
                    if prevs.is_empty() {
                        prevs.push(tokens[ti + 2].to_string());
                    } else {
                        prevs[0] = tokens[ti + 2].to_string();
                    }
                }
                "prevprev" => {
                    while prevs.len() < 2 {
                        prevs.push(String::new());
                    }
                    prevs[1] = tokens[ti + 2].to_string();
                }
                "prevprevprev" => {
                    while prevs.len() < 3 {
                        prevs.push(String::new());
                    }
                    prevs[2] = tokens[ti + 2].to_string();
                }
                "prevprevprevprev" => {
                    while prevs.len() < 4 {
                        prevs.push(String::new());
                    }
                    prevs[3] = tokens[ti + 2].to_string();
                }
                _ => {}
            }
            ti += 3;
        } else {
            ti += 1;
        }
    }

    if root.is_none() && this.is_none() && froms.is_empty() && prevs.is_empty() {
        return None;
    }

    Some(ReplaceScopes {
        root,
        this,
        froms,
        prevs,
    })
}

fn parse_required_scopes(comments: &[String]) -> Vec<String> {
    // The parser keeps the leading `#`s in comment text, so a `## scope = X`
    // annotation arrives as "## scope = X". Strip the `#`s + whitespace, then
    // match the bare `scope` directive (NOT `push_scope` / `replace_scope`,
    // which don't start with "scope").
    //
    // Scan from the END: comments accumulate across commented-out rules
    // (`# alias[...]`), so the `## scope` closest to this rule (the last one) is
    // the relevant one, not an earlier orphaned annotation.
    for c in comments.iter().rev() {
        let t = c.trim_start_matches('#').trim();
        let Some(rest) = t.strip_prefix("scope") else {
            continue;
        };
        let Some(rhs) = rest.trim_start().strip_prefix('=') else {
            continue;
        };
        let rhs = rhs.trim();
        if rhs.starts_with('{') && rhs.ends_with('}') {
            return rhs[1..rhs.len() - 1]
                .split_whitespace()
                .map(|s| s.to_string())
                .collect();
        } else if !rhs.is_empty() {
            return vec![rhs.to_string()];
        }
    }
    Vec::new()
}

fn clean_path(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    normalized
        .strip_prefix("game/")
        .unwrap_or(&normalized)
        .to_string()
}

fn leaf_value_string(leaf: &cwtools_parser::ast::Leaf, table: &StringTable) -> String {
    value_to_string(&leaf.value, table)
}

// Keep process_right_field as a thin wrapper for any callers outside this file
#[allow(dead_code)]
fn process_right_field(value: &str, _table: &StringTable) -> NewField {
    field_from_string(value)
}

#[cfg(test)]
mod description_tests {
    use super::extract_description_from_comments;

    #[test]
    fn only_triple_hash_is_documentation() {
        // `## cardinality`/`## scope` are options and must not appear in the
        // hover tooltip; only `###` lines are documentation.
        let comments = vec![
            "### Numeric index of an ai_area (see common/ai_areas), not a name.".to_string(),
            "## cardinality = 0..1".to_string(),
            "## scope = country".to_string(),
        ];
        let desc = extract_description_from_comments(&comments).unwrap();
        assert_eq!(
            desc,
            "Numeric index of an ai_area (see common/ai_areas), not a name."
        );
        assert!(!desc.contains("cardinality"));
        assert!(!desc.contains("scope"));
    }

    #[test]
    fn multiple_doc_lines_join() {
        let comments = vec![
            "### First line.".to_string(),
            "## cardinality = 0..1".to_string(),
            "### Second line.".to_string(),
        ];
        assert_eq!(
            extract_description_from_comments(&comments).unwrap(),
            "First line.\nSecond line."
        );
    }

    #[test]
    fn no_doc_lines_yields_none() {
        let comments = vec![
            "## cardinality = 0..1".to_string(),
            "# plain note".to_string(),
        ];
        assert!(extract_description_from_comments(&comments).is_none());
    }
}
