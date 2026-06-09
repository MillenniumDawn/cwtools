//! Enum extraction: `enums = { enum[x] = { ... } complex_enum[x] = { ... } }`
//! and the related `values = { value[x] = { ... } }` block.

use super::*;

pub(crate) fn extract_enums_from_children(
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
            && let Some(enum_name) = extract_bracket_content(&key, "complex_enum")
        {
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

pub(crate) fn extract_values_from_children(
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
            && let Some(value_name) = extract_bracket_content(&key, "value")
        {
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
            ruleset.values.entry(value_name).or_default().extend(vals);
        }
    }
}

pub(crate) fn process_enum_node(
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

pub(crate) fn process_enum_node_from_node(
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

pub(crate) fn process_complex_enum_node(
    name: String,
    node: &cwtools_parser::ast::Node,
    ast: &ParsedFile,
    table: &StringTable,
    comments: &[String],
) -> ComplexEnumDef {
    process_complex_enum_from_children(name, &node.children, ast, table, comments)
}

pub(crate) fn process_complex_enum_from_children(
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
                    && let Value::Clause(name_ch) = &l.value
                {
                    name_tree = Some(build_name_tree(name_ch, ast, table));
                    continue;
                }
                let v = leaf_value_string(l, table);
                match k.as_str() {
                    "path" => paths.push(clean_path(&v)),
                    "path_strict" if v == "yes" => {
                        path_strict = true;
                    }
                    "path_file" => {
                        path_file = Some(v);
                    }
                    "path_extension" => {
                        path_extension = Some(v);
                    }
                    "start_from_root" if v == "yes" => {
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
            ..Default::default()
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
