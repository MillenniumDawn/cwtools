use std::collections::{HashMap, HashSet};
use cwtools_parser::ast::{Arena, Child, ParsedFile, Value};
use cwtools_string_table::string_table::StringTable;
use cwtools_rules::rules_types::RuleSet;

pub mod inline_expansion;

/// Computed data for a single file — mirrors F# `InfoService` batch folds.
#[derive(Debug, Clone, Default)]
pub struct FileInfo {
    /// Keys that define types (e.g., `ethos = { ... }` → type "ethos").
    /// Maps type name → (line, col) of the definition node.
    pub type_definitions: HashMap<String, Vec<SourceLocation>>,
    /// Referenced types (e.g., `create_country = { ethos = <ethos> }`).
    /// Maps referenced type name → locations where it appears.
    pub type_references: HashMap<String, Vec<SourceLocation>>,
    /// Defined variables (`@var = 5`).
    pub defined_variables: HashMap<String, SourceLocation>,
    /// Effect blocks (keys that are known effect/trigger aliases).
    pub effect_blocks: Vec<SourceLocation>,
    pub trigger_blocks: Vec<SourceLocation>,
    /// Saved event targets (`event_target:foo`).
    pub saved_event_targets: HashSet<String>,
    /// Inline scripts referenced (`inline_script = { script = foo }`).
    pub inline_scripts: HashMap<String, SourceLocation>,
    /// All top-level keys (useful for completion / symbol listing).
    pub top_level_keys: Vec<(String, SourceLocation)>,
}

#[derive(Debug, Clone, Copy)]
pub struct SourceLocation {
    pub line: u32,
    pub col: u16,
}

/// InfoService holds computed data for all files in a workspace.
pub struct InfoService {
    /// Per-file info.
    pub files: HashMap<String, FileInfo>,
    /// Union of all type definitions across files (for lookup by type name).
    pub all_type_defs: HashMap<String, Vec<(String, SourceLocation)>>,
    /// Union of all saved event targets across files.
    pub all_event_targets: HashSet<String>,
    /// Union of all defined variables across files.
    pub all_variables: HashSet<String>,
    /// Union of all inline scripts across files.
    pub all_inline_scripts: HashSet<String>,
}

impl InfoService {
    pub fn new() -> Self {
        Self {
            files: HashMap::new(),
            all_type_defs: HashMap::new(),
            all_event_targets: HashSet::new(),
            all_variables: HashSet::new(),
            all_inline_scripts: HashSet::new(),
        }
    }

    /// Compute info for a single parsed file and merge into global indexes.
    pub fn index_file(&mut self, uri: &str, ast: &ParsedFile, table: &StringTable, ruleset: &RuleSet) {
        let mut info = FileInfo::default();
        let mut type_names: HashSet<String> = HashSet::new();
        for t in &ruleset.types {
            type_names.insert(t.name.clone());
        }

        for child in &ast.root_children {
            Self::index_child(
                child, &ast.arena, table, &type_names, &mut info,
            );
        }

        // Merge into global indexes
        for (type_name, locs) in &info.type_definitions {
            self.all_type_defs
                .entry(type_name.clone())
                .or_default()
                .extend(locs.iter().map(|l| (uri.to_string(), *l)));
        }
        self.all_event_targets.extend(info.saved_event_targets.iter().cloned());
        self.all_variables.extend(info.defined_variables.keys().cloned());
        self.all_inline_scripts.extend(info.inline_scripts.keys().cloned());

        self.files.insert(uri.to_string(), info);
    }

    /// Remove a file from all indexes.
    pub fn clear_file(&mut self, uri: &str) {
        if let Some(info) = self.files.remove(uri) {
            // Remove type definitions
            for type_name in info.type_definitions.keys() {
                if let Some(locs) = self.all_type_defs.get_mut(type_name) {
                    locs.retain(|(u, _)| u != uri);
                    if locs.is_empty() {
                        self.all_type_defs.remove(type_name);
                    }
                }
            }
            // Remove event targets (conservatively: only if no other file has them)
            for et in &info.saved_event_targets {
                let still_exists = self.files.values().any(|f| f.saved_event_targets.contains(et));
                if !still_exists {
                    self.all_event_targets.remove(et);
                }
            }
            // Remove variables
            for var in info.defined_variables.keys() {
                let still_exists = self.files.values().any(|f| f.defined_variables.contains_key(var));
                if !still_exists {
                    self.all_variables.remove(var);
                }
            }
            // Remove inline scripts
            for script in info.inline_scripts.keys() {
                let still_exists = self.files.values().any(|f| f.inline_scripts.contains_key(script));
                if !still_exists {
                    self.all_inline_scripts.remove(script);
                }
            }
        }
    }

    /// Find all definitions of a given symbol name.
    pub fn find_definitions(&self, name: &str) -> Option<&Vec<(String, SourceLocation)>> {
        self.all_type_defs.get(name)
    }

    /// Find all references to a given symbol name across all files.
    pub fn find_references(&self, name: &str) -> Option<Vec<(String, SourceLocation)>> {
        let mut result = Vec::new();
        for (uri, info) in &self.files {
            if let Some(locs) = info.type_references.get(name) {
                for loc in locs {
                    result.push((uri.clone(), *loc));
                }
            }
        }
        if result.is_empty() {
            None
        } else {
            Some(result)
        }
    }

    fn index_child(
        child: &Child,
        arena: &Arena,
        table: &StringTable,
        type_names: &HashSet<String>,
        info: &mut FileInfo,
    ) {
        match child {
            Child::Node(idx) => {
                let node = &arena.nodes[*idx as usize];
                let key = table.get_string(node.key.normal).unwrap_or_default();

                // Top-level node → potential type definition
                info.top_level_keys.push((
                    key.clone(),
                    SourceLocation {
                        line: node.pos.start.line,
                        col: node.pos.start.col,
                    },
                ));

                // If this key matches a known type name, it's a type definition
                if type_names.contains(&key) {
                    info.type_definitions
                        .entry(key.clone())
                        .or_default()
                        .push(SourceLocation {
                            line: node.pos.start.line,
                            col: node.pos.start.col,
                        });
                }

                // Check children for references, variables, inline scripts
                for c in &node.children {
                    Self::index_child(c, arena, table, type_names, info);
                }
            }
            Child::Leaf(idx) => {
                let leaf = &arena.leaves[*idx as usize];
                let key = table.get_string(leaf.key.normal).unwrap_or_default();
                let value_str = leaf_value_string(&leaf.value, table);

                // Defined variable: `@var = 5`
                if key.starts_with('@') {
                    info.defined_variables.insert(
                        key.clone(),
                        SourceLocation {
                            line: leaf.pos.start.line,
                            col: leaf.pos.start.col,
                        },
                    );
                }

                // Type reference in value: `<type>` syntax
                if value_str.starts_with('<') && value_str.ends_with('>') {
                    let inner = &value_str[1..value_str.len() - 1];
                    info.type_references
                        .entry(inner.to_string())
                        .or_default()
                        .push(SourceLocation {
                            line: leaf.pos.start.line,
                            col: leaf.pos.start.col,
                        });
                }

                // Also scan clause values for nested references
                if let Value::Clause(children) = &leaf.value {
                    for c in children {
                        Self::index_child(c, arena, table, type_names, info);
                    }
                }

                // Saved event targets: `event_target:foo`
                if key.starts_with("event_target:") {
                    let target = key.strip_prefix("event_target:").unwrap_or("");
                    if !target.is_empty() {
                        info.saved_event_targets.insert(target.to_string());
                    }
                }

                // Inline scripts: `inline_script = { script = foo }`
                if key == "inline_script" {
                    if let Value::Clause(children) = &leaf.value {
                        for c in children {
                            if let Child::Leaf(script_idx) = c {
                                let script_leaf = &arena.leaves[*script_idx as usize];
                                let script_key = table.get_string(script_leaf.key.normal).unwrap_or_default();
                                if script_key == "script" {
                                    let script_name = leaf_value_string(&script_leaf.value, table);
                                    if !script_name.is_empty() {
                                        info.inline_scripts.insert(
                                            script_name,
                                            SourceLocation {
                                                line: script_leaf.pos.start.line,
                                                col: script_leaf.pos.start.col,
                                            },
                                        );
                                    }
                                }
                            }
                        }
                    }
                }

                // Effect / trigger block heuristics
                if key == "effect" || key.ends_with("_effect") {
                    info.effect_blocks.push(SourceLocation {
                        line: leaf.pos.start.line,
                        col: leaf.pos.start.col,
                    });
                }
                if key == "trigger" || key.ends_with("_trigger") {
                    info.trigger_blocks.push(SourceLocation {
                        line: leaf.pos.start.line,
                        col: leaf.pos.start.col,
                    });
                }
            }
            _ => {}
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use cwtools_parser::parser::parse_string;
    use cwtools_string_table::string_table::StringTable;

    fn make_info(source: &str) -> (FileInfo, StringTable) {
        let table = StringTable::new();
        let parsed = parse_string(source, &table).unwrap();
        let mut info = FileInfo::default();
        let type_names = HashSet::new();
        for child in &parsed.root_children {
            InfoService::index_child(child, &parsed.arena, &table, &type_names, &mut info);
        }
        (info, table)
    }

    #[test]
    fn test_defined_variables() {
        let source = "@my_var = 5\nfoo = { bar = @my_var }";
        let (info, _) = make_info(source);
        assert!(info.defined_variables.contains_key("@my_var"));
    }

    #[test]
    fn test_type_references() {
        let source = "create_country = { ethos = <ethos> }";
        let (info, _) = make_info(source);
        assert!(info.type_references.contains_key("ethos"));
    }

    #[test]
    fn test_event_targets() {
        let source = "event_target:my_target = { foo = bar }";
        let (info, _) = make_info(source);
        assert!(info.saved_event_targets.contains("my_target"));
    }

    #[test]
    fn test_inline_scripts() {
        let source = "inline_script = { script = my_inline_script }";
        let (info, _) = make_info(source);
        assert!(info.inline_scripts.contains_key("my_inline_script"));
    }
}
