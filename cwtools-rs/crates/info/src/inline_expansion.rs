use cwtools_parser::ast::{Arena, Child, Leaf, Node, ParsedFile, Value};
use cwtools_parser::parser::parse_string;
use cwtools_string_table::string_table::{StringTable, StringTokens};
use std::collections::HashMap;
use std::path::Path;

/// An inline script registry holds loaded script files keyed by name.
pub struct InlineRegistry {
    /// script name → parsed file (with its own string table, arena, etc.)
    scripts: HashMap<String, (ParsedFile, StringTable)>,
}

impl InlineRegistry {
    pub fn new() -> Self {
        Self {
            scripts: HashMap::new(),
        }
    }

    /// Scan a directory for `.txt` files and load them as inline scripts.
    pub fn load_directory(&mut self, dir: &Path, table: &mut StringTable,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if !dir.is_dir() {
            return Ok(()); // no inline_scripts folder is fine
        }
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("txt") {
                let name = path.file_stem().and_then(|s| s.to_str()).unwrap_or("").to_string();
                let content = std::fs::read_to_string(&path)?;
                let script_table = StringTable::new();
                match parse_string(&content, &script_table) {
                    Ok(parsed) => {
                        self.scripts.insert(name, (parsed, script_table));
                    }
                    Err(e) => {
                        eprintln!("Warning: failed to parse inline script {}: {}", path.display(), e);
                    }
                }
            }
        }
        Ok(())
    }

    /// Resolve an inline script by name.
    pub fn get(&self, name: &str) -> Option<&(ParsedFile, StringTable)> {
        self.scripts.get(name)
    }
}

/// Expand `inline_script = { script = foo $bar$ = baz }` by substituting parameters.
///
/// Returns `Ok(Some(expanded_children))` on success, `Ok(None)` if the node is
/// not an inline_script call, `Err` if the referenced script is missing.
pub fn expand_inline_script(
    leaf: &Leaf,
    _leaf_idx: u32,
    arena: &Arena,
    table: &StringTable,
    registry: &InlineRegistry,
    target_table: &mut StringTable,
    target_arena: &mut Arena,
) -> Result<Option<Vec<Child>>, String> {
    let key = table.get_string(leaf.key.normal).unwrap_or_default();
    if key != "inline_script" {
        return Ok(None);
    }

    let Value::Clause(children) = &leaf.value else {
        return Err("inline_script value is not a clause".to_string());
    };

    let mut script_name: Option<String> = None;
    let mut params: HashMap<String, String> = HashMap::new();

    for child in children {
        match child {
            Child::Leaf(idx) => {
                let c = &arena.leaves[*idx as usize];
                let c_key = table.get_string(c.key.normal).unwrap_or_default();
                let c_val = leaf_value_string(&c.value, table);
                if c_key == "script" {
                    script_name = Some(c_val);
                } else {
                    // Any other key is a parameter substitution target
                    params.insert(c_key, c_val);
                }
            }
            _ => {}
        }
    }

    let Some(name) = script_name else {
        return Err("inline_script missing 'script' field".to_string());
    };

    let Some((script_parsed, script_table)) = registry.get(&name) else {
        return Err(format!("inline_script '{}' not found in registry", name));
    };

    // Deep-clone the script AST into the target arena, applying substitutions
    let mut cloned = Vec::new();
    for child in &script_parsed.root_children {
        cloned.push(clone_child(
            child, &script_parsed.arena, script_table, target_arena, target_table, &params,
        ));
    }

    Ok(Some(cloned))
}

fn clone_child(
    child: &Child,
    src_arena: &Arena,
    src_table: &StringTable,
    dst_arena: &mut Arena,
    dst_table: &mut StringTable,
    params: &HashMap<String, String>,
) -> Child {
    match child {
        Child::Node(idx) => {
            let node = &src_arena.nodes[*idx as usize];
            let new_key = clone_string_tokens(&node.key, src_table, dst_table, params);
            let new_children: Vec<Child> = node
                .children
                .iter()
                .map(|c| clone_child(c, src_arena, src_table, dst_arena, dst_table, params))
                .collect();
            let new_node = Node {
                key: new_key,
                children: new_children,
                pos: node.pos,
                key_prefix: None,
                value_prefix: None,
            };
            let new_idx = dst_arena.nodes.len() as u32;
            dst_arena.nodes.push(new_node);
            Child::Node(new_idx)
        }
        Child::Leaf(idx) => {
            let leaf = &src_arena.leaves[*idx as usize];
            let new_key = clone_string_tokens(&leaf.key, src_table, dst_table, params);
            let new_value = clone_value(
                &leaf.value, src_arena, src_table, dst_arena, dst_table, params,
            );
            let new_leaf = Leaf {
                key: new_key,
                value: new_value,
                op: leaf.op.clone(),
                pos: leaf.pos,
            };
            let new_idx = dst_arena.leaves.len() as u32;
            dst_arena.leaves.push(new_leaf);
            Child::Leaf(new_idx)
        }
        Child::Comment(idx) => {
            let c = &src_arena.comments[*idx as usize];
            let new_comment = cwtools_parser::ast::Comment {
                text: substitute_params(&c.text, params),
                pos: c.pos,
            };
            let new_idx = dst_arena.comments.len() as u32;
            dst_arena.comments.push(new_comment);
            Child::Comment(new_idx)
        }
        Child::LeafValue(idx) => {
            // LeafValue is a string value without a key, just clone it
            let lv = &src_arena.leaf_values[*idx as usize];
            let new_text = clone_string_tokens(&lv.value, src_table, dst_table, params,
            );
            let new_lv = cwtools_parser::ast::LeafValue {
                value: new_text,
                pos: lv.pos,
            };
            let new_idx = dst_arena.leaf_values.len() as u32;
            dst_arena.leaf_values.push(new_lv);
            Child::LeafValue(new_idx)
        }
        Child::ValueClause(idx) => {
            // ValueClause is a clause inside a value position, clone its children
            let vc = &src_arena.value_clauses[*idx as usize];
            let new_children: Vec<Child> = vc
                .children
                .iter()
                .map(|c| clone_child(c, src_arena, src_table, dst_arena, dst_table, params))
                .collect();
            let new_vc = cwtools_parser::ast::ValueClause {
                children: new_children,
                pos: vc.pos,
            };
            let new_idx = dst_arena.value_clauses.len() as u32;
            dst_arena.value_clauses.push(new_vc);
            Child::ValueClause(new_idx)
        }
    }
}

fn clone_value(
    value: &Value,
    src_arena: &Arena,
    src_table: &StringTable,
    dst_arena: &mut Arena,
    dst_table: &mut StringTable,
    params: &HashMap<String, String>,
) -> Value {
    match value {
        Value::String(t) | Value::QString(t) => {
            let text = src_table.get_string(t.normal).unwrap_or_default();
            let new_text = substitute_params(&text, params);
            let new_tokens = dst_table.intern(&new_text);
            Value::String(new_tokens)
        }
        Value::Float(f) => Value::Float(*f),
        Value::Int(i) => Value::Int(*i),
        Value::Bool(b) => Value::Bool(*b),
        Value::Clause(children) => {
            let new_children: Vec<Child> = children
                .iter()
                .map(|c| clone_child(c, src_arena, src_table, dst_arena, dst_table, params))
                .collect();
            Value::Clause(new_children)
        }
    }
}

fn clone_string_tokens(
    tokens: &StringTokens,
    src_table: &StringTable,
    dst_table: &mut StringTable,
    params: &HashMap<String, String>,
) -> StringTokens {
    let text = src_table.get_string(tokens.normal).unwrap_or_default();
    let new_text = substitute_params(&text, params);
    let new_id = dst_table.intern(&new_text).normal;
    let lower_id = dst_table.intern(&new_text.to_lowercase()).normal;
    StringTokens {
        normal: new_id,
        lower: lower_id,
        quoted: false,
    }
}

fn substitute_params(text: &str, params: &HashMap<String, String>) -> String {
    let mut result = text.to_string();
    for (key, val) in params {
        let pattern = format!("${}${}", key, "");
        // The pattern is literally `$key$` — but the trailing $ is empty in format! above.
        // Fix: the correct pattern is `$key$`.
        let _ = pattern; // suppress warning, will redo below
    }
    // Re-implement correctly
    let mut output = String::new();
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '$' {
            let mut param_name = String::new();
            while let Some(&next_ch) = chars.peek() {
                if next_ch == '$' {
                    chars.next(); // consume closing $
                    break;
                }
                param_name.push(next_ch);
                chars.next();
            }
            if let Some(val) = params.get(&param_name) {
                output.push_str(val);
            } else {
                // Unresolved param: keep original
                output.push('$');
                output.push_str(&param_name);
                output.push('$');
            }
        } else {
            output.push(ch);
        }
    }
    output
}

fn leaf_value_string(value: &Value, table: &StringTable) -> String {
    match value {
        Value::String(t) | Value::QString(t) => table.get_string(t.normal).unwrap_or_default(),
        Value::Float(f) => f.to_string(),
        Value::Int(i) => i.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Clause(_) => String::new(),
    }
}
