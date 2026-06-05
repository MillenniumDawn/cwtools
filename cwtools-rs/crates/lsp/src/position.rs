use cwtools_parser::ast::{Arena, Child, ParsedFile, SourcePos, Value};
use cwtools_string_table::string_table::StringTable;

/// Find the AST node or leaf at a given position.
pub fn find_at_position(
    ast: &ParsedFile,
    pos: &SourcePos,
    table: &StringTable,
) -> Option<AstElement> {
    for child in &ast.root_children {
        if let Some(found) = find_in_child(child, &ast.arena, pos, table) {
            return Some(found);
        }
    }
    None
}

#[derive(Debug, Clone)]
pub enum AstElement {
    Node {
        key: String,
        #[allow(dead_code)]
        idx: u32,
    },
    Leaf {
        key: String,
        value: String,
        #[allow(dead_code)]
        idx: u32,
    },
    LeafValue {
        value: String,
        #[allow(dead_code)]
        idx: u32,
    },
}

fn find_in_child(
    child: &Child,
    arena: &Arena,
    pos: &SourcePos,
    table: &StringTable,
) -> Option<AstElement> {
    match child {
        Child::Node(idx) => {
            let node = &arena.nodes[*idx as usize];
            if pos_in_range(pos, &node.pos) {
                // Check children first for more specific match
                for c in &node.children {
                    if let Some(found) = find_in_child(c, arena, pos, table) {
                        return Some(found);
                    }
                }
                let key = table.get_string(node.key.normal).unwrap_or_default();
                return Some(AstElement::Node { key, idx: *idx });
            }
            None
        }
        Child::Leaf(idx) => {
            let leaf = &arena.leaves[*idx as usize];
            if pos_in_range(pos, &leaf.pos) {
                let key = table.get_string(leaf.key.normal).unwrap_or_default();
                let value = leaf_value_to_string(&leaf.value, table);
                return Some(AstElement::Leaf {
                    key,
                    value,
                    idx: *idx,
                });
            }
            None
        }
        Child::LeafValue(idx) => {
            let lv = &arena.leaf_values[*idx as usize];
            if pos_in_range(pos, &lv.pos) {
                let value = leaf_value_to_string(&lv.value, table);
                return Some(AstElement::LeafValue { value, idx: *idx });
            }
            None
        }
        _ => None,
    }
}

fn pos_in_range(pos: &SourcePos, range: &cwtools_parser::ast::SourceRange) -> bool {
    let after_start =
        pos.line > range.start.line || (pos.line == range.start.line && pos.col >= range.start.col);
    let before_end =
        pos.line < range.end.line || (pos.line == range.end.line && pos.col <= range.end.col);
    after_start && before_end
}

fn leaf_value_to_string(value: &Value, table: &StringTable) -> String {
    match value {
        Value::String(t) | Value::QString(t) => table.get_string(t.normal).unwrap_or_default(),
        Value::Float(f) => f.to_string(),
        Value::Int(i) => i.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Clause(_) => "{...}".to_string(),
    }
}
