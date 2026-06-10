use cwtools_parser::ast::{Arena, Child, ParsedFile, Value};
use cwtools_string_table::string_table::StringTable;
use std::collections::{HashMap, HashSet};

/// Index of symbols across all loaded documents.
/// For each symbol name, stores (uri, line) pairs where it's defined or referenced.
pub struct SymbolIndex {
    /// Symbol name → list of definition locations (uri, line)
    definitions: HashMap<String, Vec<SymbolLocation>>,
    /// Symbol name → list of reference locations (uri, line)
    references: HashMap<String, Vec<SymbolLocation>>,
    /// URI → set of symbol names that URI contributes to `definitions` or `references`.
    /// Allows O(symbols_in_file) clearing instead of O(total_workspace_symbols).
    reverse: HashMap<String, HashSet<String>>,
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
            reverse: HashMap::new(),
        }
    }

    /// Index a parsed document.
    pub fn index_document(&mut self, uri: &str, ast: &ParsedFile, table: &StringTable) {
        for child in &ast.root_children {
            self.index_child(uri, child, &ast.arena, table);
        }
    }

    fn index_child(&mut self, uri: &str, child: &Child, arena: &Arena, table: &StringTable) {
        // A keyed clause (`key = { ... }`) is a Leaf whose value is a Clause;
        // record it as a definition and walk its subtree.
        if let Some(kc) = arena.keyed_clause(child) {
            let key = table.get_string(kc.key.normal).unwrap_or_default();
            // Heuristic: nodes keyed like plain identifiers are often definitions
            if self.looks_like_definition(&key) {
                self.reverse
                    .entry(uri.to_string())
                    .or_default()
                    .insert(key.clone());
                self.definitions
                    .entry(key.clone())
                    .or_default()
                    .push(SymbolLocation {
                        uri: uri.to_string(),
                        line: kc.pos.start.line,
                        col: kc.pos.start.col,
                    });
            }
            for c in kc.children {
                self.index_child(uri, c, arena, table);
            }
            return;
        }
        if let Child::Leaf(idx) = child {
            let leaf = &arena.leaves[*idx as usize];
            let key = table.get_string(leaf.key.normal).unwrap_or_default();
            // Check if key references a known type: <type> syntax
            if key.starts_with('<') && key.ends_with('>') {
                let inner = &key[1..key.len() - 1];
                self.reverse
                    .entry(uri.to_string())
                    .or_default()
                    .insert(inner.to_string());
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
                    self.reverse
                        .entry(uri.to_string())
                        .or_default()
                        .insert(inner.to_string());
                    self.references
                        .entry(inner.to_string())
                        .or_default()
                        .push(SymbolLocation {
                            uri: uri.to_string(),
                            line: leaf.pos.start.line,
                            col: leaf.pos.start.col,
                        });
                }
            }
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

    pub fn find_references(&self, name: &str) -> Option<&Vec<SymbolLocation>> {
        self.references.get(name)
    }

    /// Remove all entries contributed by `uri`. O(symbols contributed by that file)
    /// rather than O(total workspace symbols).
    pub fn clear_document(&mut self, uri: &str) {
        let names = match self.reverse.remove(uri) {
            Some(n) => n,
            None => return,
        };
        for name in &names {
            if let Some(locs) = self.definitions.get_mut(name) {
                locs.retain(|l| l.uri != uri);
                if locs.is_empty() {
                    self.definitions.remove(name);
                }
            }
            if let Some(locs) = self.references.get_mut(name) {
                locs.retain(|l| l.uri != uri);
                if locs.is_empty() {
                    self.references.remove(name);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use cwtools_parser::parser::parse_string;

    #[test]
    fn test_clear_document_reverse_index() {
        let table = StringTable::new();
        // A leaf whose value matches `<type>` syntax; that populates `references`
        // and the reverse index (the parser produces Child::Leaf for all blocks).
        let src = "some_field = <my_type>\n";
        let parsed = parse_string(src, &table).expect("parse");

        let mut idx = SymbolIndex::new();
        idx.index_document("file:///a.txt", &parsed, &table);

        // References should contain "my_type" (the inner text of `<my_type>`).
        assert!(
            idx.references.contains_key("my_type"),
            "expected 'my_type' in references, got: {:?}",
            idx.references.keys().collect::<Vec<_>>()
        );
        // Reverse index should map the URI to "my_type".
        assert!(
            idx.reverse
                .get("file:///a.txt")
                .map(|s| s.contains("my_type"))
                .unwrap_or(false),
            "expected reverse index entry for my_type"
        );

        idx.clear_document("file:///a.txt");

        assert!(
            !idx.references.contains_key("my_type"),
            "references should be empty after clear"
        );
        assert!(
            !idx.reverse.contains_key("file:///a.txt"),
            "reverse index should be empty after clear"
        );

        // Clearing a URI that was never indexed is a no-op (not a panic)
        idx.clear_document("file:///never_seen.txt");
    }

    /// The parser stores `key = { ... }` as a Leaf with a Clause value, so
    /// definitions must be recorded (and the subtree walked) for that shape.
    #[test]
    fn test_leaf_clause_definitions_indexed() {
        let table = StringTable::new();
        let src = "my_focus = {\n cost = 10\n nested = { other_field = <my_type> }\n}\n";
        let parsed = parse_string(src, &table).expect("parse");

        let mut idx = SymbolIndex::new();
        idx.index_document("file:///b.txt", &parsed, &table);

        assert!(
            idx.definitions.contains_key("my_focus"),
            "top-level Leaf+Clause should be recorded as a definition, got: {:?}",
            idx.definitions.keys().collect::<Vec<_>>()
        );
        // The walk must descend through nested clauses to find references.
        assert!(
            idx.references.contains_key("my_type"),
            "reference inside a nested clause should be indexed"
        );
    }
}
