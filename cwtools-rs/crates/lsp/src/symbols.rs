use cwtools_parser::ast::{Arena, Child, ParsedFile, Value};
use cwtools_string_table::string_table::StringTable;
use std::collections::HashMap;

/// Index of symbols across all loaded documents.
/// For each symbol name, stores (uri, line) pairs where it's defined or referenced.
pub struct SymbolIndex {
    /// Symbol name → list of definition locations (uri, line)
    definitions: HashMap<String, Vec<SymbolLocation>>,
    /// Symbol name → list of reference locations (uri, line)
    references: HashMap<String, Vec<SymbolLocation>>,
}

#[derive(Debug, Clone)]
pub struct SymbolLocation {
    pub uri: String,
    pub line: u32,
    pub col: u16,
}

impl SymbolIndex {
    pub fn new() -> Self {
        Self {
            definitions: HashMap::new(),
            references: HashMap::new(),
        }
    }

    /// Index a parsed document.
    pub fn index_document(&mut self, uri: &str, ast: &ParsedFile, table: &StringTable) {
        for child in &ast.root_children {
            self.index_child(uri, child, &ast.arena, table);
        }
    }

    fn index_child(&mut self, uri: &str, child: &Child, arena: &Arena, table: &StringTable) {
        match child {
            Child::Node(idx) => {
                let node = &arena.nodes[*idx as usize];
                let key = table.get_string(node.key.normal).unwrap_or_default();
                // Heuristic: top-level nodes in game files are often definitions
                if self.looks_like_definition(&key) {
                    self.definitions
                        .entry(key.clone())
                        .or_default()
                        .push(SymbolLocation {
                            uri: uri.to_string(),
                            line: node.pos.start.line,
                            col: node.pos.start.col,
                        });
                }
                for c in &node.children {
                    self.index_child(uri, c, arena, table);
                }
            }
            Child::Leaf(idx) => {
                let leaf = &arena.leaves[*idx as usize];
                let key = table.get_string(leaf.key.normal).unwrap_or_default();
                // Check if key references a known type: <type> syntax
                if key.starts_with('<') && key.ends_with('>') {
                    let inner = &key[1..key.len() - 1];
                    self.references
                        .entry(inner.to_string())
                        .or_default()
                        .push(SymbolLocation {
                            uri: uri.to_string(),
                            line: leaf.pos.start.line,
                            col: leaf.pos.start.col,
                        });
                }
                // Check value for type references
                if let Value::String(t) | Value::QString(t) = &leaf.value {
                    let value = table.get_string(t.normal).unwrap_or_default();
                    if value.starts_with('<') && value.ends_with('>') {
                        let inner = &value[1..value.len() - 1];
                        self.references.entry(inner.to_string()).or_default().push(
                            SymbolLocation {
                                uri: uri.to_string(),
                                line: leaf.pos.start.line,
                                col: leaf.pos.start.col,
                            },
                        );
                    }
                }
            }
            _ => {}
        }
    }

    /// Naive heuristic: does this key look like a definition?
    fn looks_like_definition(&self, key: &str) -> bool {
        // Top-level keys that aren't special syntax are likely definitions
        !key.starts_with("alias[")
            && !key.starts_with("types")
            && !key.starts_with("enums")
            && !key.starts_with("#")
            && !key.is_empty()
    }

    #[allow(dead_code)]
    pub fn find_definitions(&self, name: &str) -> Option<&Vec<SymbolLocation>> {
        self.definitions.get(name)
    }

    pub fn find_references(&self, name: &str) -> Option<&Vec<SymbolLocation>> {
        self.references.get(name)
    }

    pub fn clear_document(&mut self, uri: &str) {
        for locs in self.definitions.values_mut() {
            locs.retain(|l| l.uri != uri);
        }
        for locs in self.references.values_mut() {
            locs.retain(|l| l.uri != uri);
        }
        // Clean up empty entries
        self.definitions.retain(|_, v| !v.is_empty());
        self.references.retain(|_, v| !v.is_empty());
    }
}
