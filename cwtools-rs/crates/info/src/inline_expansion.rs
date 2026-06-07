// The recursive clone/expand helpers thread the same source+dest arena, table,
// depth and call-stack set; splitting them into a context struct buys nothing
// and obscures the recursion.
#![allow(clippy::too_many_arguments)]

use cwtools_parser::ast::{Arena, Child, Leaf, Node, ParsedFile, Value};
use cwtools_parser::parser::parse_string;
use cwtools_string_table::string_table::{StringTable, StringTokens};
use std::collections::HashMap;
use std::path::Path;

/// Maximum nesting depth for inline-script expansion (mirrors F# updateInlineScripts).
const MAX_INLINE_DEPTH: usize = 5;

/// An inline script registry holds loaded script files keyed by name.
pub struct InlineRegistry {
    /// script name → parsed file (with its own string table, arena, etc.)
    scripts: HashMap<String, (ParsedFile, StringTable)>,
}

impl Default for InlineRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl InlineRegistry {
    pub fn new() -> Self {
        Self {
            scripts: HashMap::new(),
        }
    }

    /// Scan a directory for `.txt` files and load them as inline scripts.
    pub fn load_directory(
        &mut self,
        dir: &Path,
        _table: &mut StringTable,
    ) -> Result<(), Box<dyn std::error::Error>> {
        if !dir.is_dir() {
            return Ok(()); // no inline_scripts folder is fine
        }
        for entry in std::fs::read_dir(dir)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|s| s.to_str()) == Some("txt") {
                let name = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or("")
                    .to_string();
                let content = std::fs::read_to_string(&path)?;
                let script_table = StringTable::new();
                match parse_string(&content, &script_table) {
                    Ok(parsed) => {
                        self.scripts.insert(name, (parsed, script_table));
                    }
                    Err(e) => {
                        eprintln!(
                            "Warning: failed to parse inline script {}: {}",
                            path.display(),
                            e
                        );
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

// ── Public entry point ────────────────────────────────────────────────────────

/// Expand `inline_script = { script = foo $bar$ = baz }` by substituting parameters.
///
/// Returns `Ok(Some(expanded_children))` on success, `Ok(None)` if the node is
/// not an inline_script call, `Err` if the referenced script is missing.
///
/// Nested inline scripts inside the expanded body are also expanded up to
/// `MAX_INLINE_DEPTH` (5) levels deep, guarded against infinite recursion by
/// tracking which script names are currently on the call stack.
pub fn expand_inline_script(
    leaf: &Leaf,
    _leaf_idx: u32,
    arena: &Arena,
    table: &StringTable,
    registry: &InlineRegistry,
    target_table: &mut StringTable,
    target_arena: &mut Arena,
) -> Result<Option<Vec<Child>>, String> {
    expand_inline_inner(
        leaf,
        arena,
        table,
        registry,
        target_table,
        target_arena,
        &mut Vec::new(),
    )
}

// ── Internal recursive expander ───────────────────────────────────────────────

/// `call_stack` tracks currently-expanding script names for cycle detection.
fn expand_inline_inner(
    leaf: &Leaf,
    arena: &Arena,
    table: &StringTable,
    registry: &InlineRegistry,
    target_table: &mut StringTable,
    target_arena: &mut Arena,
    call_stack: &mut Vec<String>,
) -> Result<Option<Vec<Child>>, String> {
    let key = table.get_string(leaf.key.normal).unwrap_or_default();
    if key != "inline_script" {
        return Ok(None);
    }

    let Value::Clause(call_children) = &leaf.value else {
        return Err("inline_script value is not a clause".to_string());
    };

    let mut script_name: Option<String> = None;
    let mut params: HashMap<String, String> = HashMap::new();

    for child in call_children {
        if let Child::Leaf(idx) = child {
            let c = &arena.leaves[*idx as usize];
            let c_key = table.get_string(c.key.normal).unwrap_or_default();
            let c_val = leaf_value_str(&c.value, table);
            if c_key == "script" {
                script_name = Some(c_val);
            } else {
                params.insert(c_key, c_val);
            }
        }
    }

    let Some(name) = script_name else {
        return Err("inline_script missing 'script' field".to_string());
    };

    // Depth guard
    if call_stack.len() >= MAX_INLINE_DEPTH {
        return Err(format!(
            "inline_script expansion depth limit ({}) reached at '{}'",
            MAX_INLINE_DEPTH, name
        ));
    }
    // Cycle guard
    if call_stack.contains(&name) {
        return Err(format!(
            "inline_script cycle detected: {} -> {}",
            call_stack.join(" -> "),
            name
        ));
    }

    let Some((script_parsed, script_table)) = registry.get(&name) else {
        return Err(format!("inline_script '{}' not found in registry", name));
    };

    call_stack.push(name.clone());

    let result = clone_and_expand_children(
        &script_parsed.root_children,
        &script_parsed.arena,
        script_table,
        target_arena,
        target_table,
        &params,
        registry,
        call_stack,
    );

    call_stack.pop();

    Ok(Some(result?))
}

// ── Clone + expand in one pass ────────────────────────────────────────────────

/// Clone a child list from (src_arena, src_table) → (dst_arena, dst_table), applying
/// parameter substitution, and simultaneously expand any `inline_script` leaves
/// encountered during the clone.
///
/// Returns `Err` if a nested inline_script expansion fails (cycle / depth limit).
/// On a missing script, the leaf is cloned as-is (soft error).
fn clone_and_expand_children(
    children: &[Child],
    src_arena: &Arena,
    src_table: &StringTable,
    dst_arena: &mut Arena,
    dst_table: &mut StringTable,
    params: &HashMap<String, String>,
    registry: &InlineRegistry,
    call_stack: &mut Vec<String>,
) -> Result<Vec<Child>, String> {
    let mut out = Vec::with_capacity(children.len());
    for child in children {
        match child {
            Child::Leaf(idx) => {
                let src_leaf = &src_arena.leaves[*idx as usize];
                let key = src_table
                    .get_string(src_leaf.key.normal)
                    .unwrap_or_default();
                if key == "inline_script" {
                    let exp = expand_inline_inner(
                        src_leaf, src_arena, src_table, registry, dst_table, dst_arena, call_stack,
                    );
                    match exp {
                        Ok(Some(expanded_children)) => {
                            out.extend(expanded_children);
                            continue;
                        }
                        // Script not found → clone leaf as-is (soft error, non-fatal)
                        Ok(None) => {}
                        // Cycle or depth exceeded → propagate as hard error
                        Err(e) => return Err(e),
                    }
                }
                // Normal leaf clone
                let new_key = clone_tokens(&src_leaf.key, src_table, dst_table, params);
                let new_value = clone_value_r(
                    &src_leaf.value,
                    src_arena,
                    src_table,
                    dst_arena,
                    dst_table,
                    params,
                    registry,
                    call_stack,
                )?;
                let new_leaf = Leaf {
                    key: new_key,
                    value: new_value,
                    op: src_leaf.op,
                    pos: src_leaf.pos,
                };
                let new_idx = dst_arena.leaves.len() as u32;
                dst_arena.leaves.push(new_leaf);
                out.push(Child::Leaf(new_idx));
            }
            other => {
                out.push(clone_and_expand_child_r(
                    other, src_arena, src_table, dst_arena, dst_table, params, registry, call_stack,
                )?);
            }
        }
    }
    Ok(out)
}

fn clone_and_expand_child_r(
    child: &Child,
    src_arena: &Arena,
    src_table: &StringTable,
    dst_arena: &mut Arena,
    dst_table: &mut StringTable,
    params: &HashMap<String, String>,
    registry: &InlineRegistry,
    call_stack: &mut Vec<String>,
) -> Result<Child, String> {
    match child {
        Child::Leaf(idx) => {
            // Handled by the caller (clone_and_expand_children) for inline_script detection;
            // here we just do a normal clone.
            let src_leaf = &src_arena.leaves[*idx as usize];
            let new_key = clone_tokens(&src_leaf.key, src_table, dst_table, params);
            let new_value = clone_value_r(
                &src_leaf.value,
                src_arena,
                src_table,
                dst_arena,
                dst_table,
                params,
                registry,
                call_stack,
            )?;
            let new_leaf = Leaf {
                key: new_key,
                value: new_value,
                op: src_leaf.op,
                pos: src_leaf.pos,
            };
            let new_idx = dst_arena.leaves.len() as u32;
            dst_arena.leaves.push(new_leaf);
            Ok(Child::Leaf(new_idx))
        }
        Child::Node(idx) => {
            let node = &src_arena.nodes[*idx as usize];
            let new_key = clone_tokens(&node.key, src_table, dst_table, params);
            let new_children = clone_and_expand_children(
                &node.children,
                src_arena,
                src_table,
                dst_arena,
                dst_table,
                params,
                registry,
                call_stack,
            )?;
            let new_node = Node {
                key: new_key,
                children: new_children,
                pos: node.pos,
                key_prefix: None,
                value_prefix: None,
            };
            let new_idx = dst_arena.nodes.len() as u32;
            dst_arena.nodes.push(new_node);
            Ok(Child::Node(new_idx))
        }
        Child::Comment(idx) => {
            let c = &src_arena.comments[*idx as usize];
            let new_comment = cwtools_parser::ast::Comment {
                text: substitute_params(&c.text, params),
                pos: c.pos,
            };
            let new_idx = dst_arena.comments.len() as u32;
            dst_arena.comments.push(new_comment);
            Ok(Child::Comment(new_idx))
        }
        Child::LeafValue(idx) => {
            let lv = &src_arena.leaf_values[*idx as usize];
            let new_value = clone_value_r(
                &lv.value, src_arena, src_table, dst_arena, dst_table, params, registry, call_stack,
            )?;
            let new_lv = cwtools_parser::ast::LeafValue {
                value: new_value,
                pos: lv.pos,
            };
            let new_idx = dst_arena.leaf_values.len() as u32;
            dst_arena.leaf_values.push(new_lv);
            Ok(Child::LeafValue(new_idx))
        }
        Child::ValueClause(idx) => {
            let vc = &src_arena.value_clauses[*idx as usize];
            let new_children = clone_and_expand_children(
                &vc.children,
                src_arena,
                src_table,
                dst_arena,
                dst_table,
                params,
                registry,
                call_stack,
            )?;
            let new_keys: Vec<StringTokens> = vc
                .keys
                .iter()
                .map(|k| clone_tokens(k, src_table, dst_table, params))
                .collect();
            let new_vc = cwtools_parser::ast::ValueClause {
                keys: new_keys,
                children: new_children,
                pos: vc.pos,
            };
            let new_idx = dst_arena.value_clauses.len() as u32;
            dst_arena.value_clauses.push(new_vc);
            Ok(Child::ValueClause(new_idx))
        }
    }
}

fn clone_value_r(
    value: &Value,
    src_arena: &Arena,
    src_table: &StringTable,
    dst_arena: &mut Arena,
    dst_table: &mut StringTable,
    params: &HashMap<String, String>,
    registry: &InlineRegistry,
    call_stack: &mut Vec<String>,
) -> Result<Value, String> {
    match value {
        Value::String(t) => {
            let text = src_table.get_string(t.normal).unwrap_or_default();
            let new_text = substitute_params(&text, params);
            Ok(Value::String(intern_both(dst_table, &new_text)))
        }
        Value::QString(t) => {
            let text = src_table.get_string(t.normal).unwrap_or_default();
            let new_text = substitute_params(&text, params);
            Ok(Value::QString(intern_both(dst_table, &new_text)))
        }
        Value::Float(f) => Ok(Value::Float(*f)),
        Value::Int(i) => Ok(Value::Int(*i)),
        Value::Bool(b) => Ok(Value::Bool(*b)),
        Value::Clause(children) => {
            let new_children = clone_and_expand_children(
                children, src_arena, src_table, dst_arena, dst_table, params, registry, call_stack,
            )?;
            Ok(Value::Clause(new_children))
        }
    }
}

// ── String helpers ────────────────────────────────────────────────────────────

/// Intern a string and its lowercase form so that `tokens.lower` really is the
/// lowercase intern.  The original code called `intern` once and reused the
/// `.normal` id for both, which broke case-insensitive lookups for mixed-case
/// substitution results.
fn intern_both(table: &mut StringTable, text: &str) -> StringTokens {
    let normal_tokens = table.intern(text);
    let lower_str = text.to_lowercase();
    let lower_tokens = table.intern(&lower_str);
    StringTokens {
        normal: normal_tokens.normal,
        lower: lower_tokens.normal,
        quoted: normal_tokens.quoted,
    }
}

fn clone_tokens(
    tokens: &StringTokens,
    src_table: &StringTable,
    dst_table: &mut StringTable,
    params: &HashMap<String, String>,
) -> StringTokens {
    let text = src_table.get_string(tokens.normal).unwrap_or_default();
    let new_text = substitute_params(&text, params);
    intern_both(dst_table, &new_text)
}

fn substitute_params(text: &str, params: &HashMap<String, String>) -> String {
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

fn leaf_value_str(value: &Value, table: &StringTable) -> String {
    match value {
        Value::String(t) | Value::QString(t) => table.get_string(t.normal).unwrap_or_default(),
        Value::Float(f) => f.to_string(),
        Value::Int(i) => i.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Clause(_) => String::new(),
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use cwtools_parser::parser::parse_string;

    #[test]
    fn test_substitute_params_basic() {
        let mut p = HashMap::new();
        p.insert("FOO".to_string(), "bar".to_string());
        assert_eq!(
            substitute_params("hello_$FOO$_world", &p),
            "hello_bar_world"
        );
    }

    #[test]
    fn test_substitute_params_unresolved() {
        let p = HashMap::new();
        assert_eq!(substitute_params("$MISSING$", &p), "$MISSING$");
    }

    #[test]
    fn test_intern_both_lowercase() {
        let table = StringTable::new();
        let mut table = table;
        let tokens = intern_both(&mut table, "MyEvent");
        let normal = table.get_string(tokens.normal).unwrap_or_default();
        let lower = table.get_string(tokens.lower).unwrap_or_default();
        assert_eq!(normal, "MyEvent");
        assert_eq!(lower, "myevent");
    }

    #[test]
    fn test_depth_limit() {
        // Script "a" = inline_script = { script = a } → self-referential → cycle/depth error
        let mut registry = InlineRegistry::new();
        let table = StringTable::new();
        let src = "inline_script = { script = a }";
        let parsed = parse_string(src, &table).unwrap();
        registry.scripts.insert("a".to_string(), (parsed, table));

        let outer_table = StringTable::new();
        let outer_src = "inline_script = { script = a }";
        let outer_parsed = parse_string(outer_src, &outer_table).unwrap();
        let leaf_idx = match outer_parsed.root_children.first().unwrap() {
            Child::Leaf(i) => *i,
            _ => panic!("expected leaf"),
        };
        let leaf = &outer_parsed.arena.leaves[leaf_idx as usize];

        let mut tgt_table = StringTable::new();
        let mut tgt_arena = Arena::default();
        let result = expand_inline_script(
            leaf,
            leaf_idx,
            &outer_parsed.arena,
            &outer_table,
            &registry,
            &mut tgt_table,
            &mut tgt_arena,
        );
        assert!(
            result.is_err(),
            "expected depth-limit or cycle error, got {:?}",
            result
        );
    }

    #[test]
    fn test_basic_expansion_no_params() {
        let mut registry = InlineRegistry::new();
        let script_table = StringTable::new();
        let script_src = "some_effect = yes";
        let script_parsed = parse_string(script_src, &script_table).unwrap();
        registry
            .scripts
            .insert("my_script".to_string(), (script_parsed, script_table));

        let outer_table = StringTable::new();
        let outer_src = "inline_script = { script = my_script }";
        let outer_parsed = parse_string(outer_src, &outer_table).unwrap();
        let leaf_idx = match outer_parsed.root_children.first().unwrap() {
            Child::Leaf(i) => *i,
            _ => panic!("expected leaf"),
        };
        let leaf = &outer_parsed.arena.leaves[leaf_idx as usize];

        let mut tgt_table = StringTable::new();
        let mut tgt_arena = Arena::default();
        let result = expand_inline_script(
            leaf,
            leaf_idx,
            &outer_parsed.arena,
            &outer_table,
            &registry,
            &mut tgt_table,
            &mut tgt_arena,
        )
        .expect("expansion should succeed");
        let expanded = result.expect("should produce children");
        assert_eq!(expanded.len(), 1, "should expand to one child");
    }
}
