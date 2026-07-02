use cwtools_parser::ast::{Arena, Child, ParsedFile, Value};
use cwtools_rules::rules_types::RuleSet;
use cwtools_string_table::string_table::StringTable;
use std::collections::{HashMap, HashSet};

pub mod inline_expansion;

// The index half of this crate now lives in `cwtools_index`. Re-export it so
// existing `cwtools_info::TypeIndex` / `cwtools_info::collect_type_instances`
// (and the rest) keep resolving for the LSP/CLI callers.
pub use cwtools_index::vanilla_cache;
pub use cwtools_index::*;

// ══════════════════════════════════════════════════════════════════════════════
// Item 4 — Position query for hover / goto-definition
// ══════════════════════════════════════════════════════════════════════════════

/// A hint about what kind of reference a leaf's value or key represents.
/// Used by the LSP for hover text and goto-definition. Populated from the
/// matched rule's right-hand side (the LSP's `hint_from_rule_right`, fed by
/// `cwtools_validation::position::rules_at_pos`).
#[derive(Debug, Clone)]
pub enum ReferenceHint {
    /// The value is a reference to an instance of `type_name`.
    TypeRef { type_name: String, value: String },
    /// The value is a localisation key.
    LocRef { key: String },
    /// The value is a member of enum `enum_name`.
    EnumRef { enum_name: String, value: String },
    /// The key/value is a file path.
    FileRef { path: String },
    /// The value is a scope name.
    ScopeName { name: String },
    /// The value is a read of a defined script variable (`value[variable]`).
    Variable { name: String, namespace: String },
    /// Classification was not possible with current rule depth.
    Unknown,
}

/// Which kind of AST element is at the cursor.
#[derive(Debug, Clone)]
pub enum PositionElement {
    /// A `key = value` leaf (a `key = { … }` clause reports an empty value).
    Leaf { key: String, value: String },
    /// A bare value inside a clause (no key).
    LeafValue { value: String },
}

/// Find the AST element at `(line, col)` without rule classification.
/// Use this when no ruleset is available or only the key/value is needed.
pub fn element_at_position(
    file: &ParsedFile,
    line: u32,
    col: u16,
    table: &StringTable,
) -> Option<PositionElement> {
    let target = cwtools_parser::ast::SourcePos { line, col };
    find_element_in_children(&file.root_children, &file.arena, &target, table)
}

fn find_element_in_children(
    children: &[Child],
    arena: &Arena,
    target: &cwtools_parser::ast::SourcePos,
    table: &StringTable,
) -> Option<PositionElement> {
    for child in children {
        match child {
            Child::Leaf(idx) => {
                let leaf = &arena.leaves[*idx as usize];
                if pos_in_range(target, &leaf.pos) {
                    let key = table.get_string(leaf.key.normal).unwrap_or_default();
                    let value = leaf_value_string(&leaf.value, table);
                    if let Value::Clause(ch) = &leaf.value
                        && let Some(inner) = find_element_in_children(ch, arena, target, table)
                    {
                        return Some(inner);
                    }
                    return Some(PositionElement::Leaf { key, value });
                }
            }
            Child::LeafValue(idx) => {
                let lv = &arena.leaf_values[*idx as usize];
                if pos_in_range(target, &lv.pos) {
                    let value = leaf_value_string(&lv.value, table);
                    return Some(PositionElement::LeafValue { value });
                }
            }
            _ => {}
        }
    }
    None
}

fn pos_in_range(
    pos: &cwtools_parser::ast::SourcePos,
    range: &cwtools_parser::ast::SourceRange,
) -> bool {
    let start = &range.start;
    let end = &range.end;
    if pos.line < start.line || pos.line > end.line {
        return false;
    }
    if pos.line == start.line && pos.col < start.col {
        return false;
    }
    if pos.line == end.line && pos.col > end.col {
        return false;
    }
    true
}

// ══════════════════════════════════════════════════════════════════════════════
// Existing FileInfo / InfoService (preserved, extended)
// ══════════════════════════════════════════════════════════════════════════════

/// Computed data for a single file.
#[derive(Debug, Clone, Default)]
pub struct FileInfo {
    /// Keys that define types (heuristic, kept for LSP compatibility).
    pub type_definitions: HashMap<String, Vec<SourceLocation>>,
    /// Referenced types (e.g. `<ethos>`).
    pub type_references: HashMap<String, Vec<SourceLocation>>,
    /// Defined variables — rule-driven + @-prefix.
    /// Maps namespace (or "@") → list of variables.
    pub defined_variables_ns: HashMap<String, Vec<DefinedVariable>>,
    /// Classic @-var lookup (kept for LSP compatibility).
    pub defined_variables: HashMap<String, SourceLocation>,
    /// Effect blocks (heuristic).
    pub effect_blocks: Vec<SourceLocation>,
    pub trigger_blocks: Vec<SourceLocation>,
    /// Saved event targets with position.
    pub saved_event_targets_detailed: Vec<SavedEventTarget>,
    /// Saved event targets (heuristic set, kept for LSP compatibility).
    pub saved_event_targets: HashSet<String>,
    /// Inline scripts referenced.
    pub inline_scripts: HashMap<String, SourceLocation>,
    /// All top-level keys.
    pub top_level_keys: Vec<(String, SourceLocation)>,
    /// Order-independent hash of this file's exported type instances
    /// (`(type, name)` pairs), computed at index time from the per-file
    /// instance map so the cross-file "did exports change?" check doesn't have
    /// to scan the global type index. See [`InfoService::export_fingerprint`].
    pub export_instances_hash: u64,
    /// Lowercased names of this file's exported type instances, captured at
    /// index time from the per-file instance map. Combined with the variable /
    /// event-target names (already on this struct) by
    /// [`InfoService::export_names`] to scope the dependent sweep without
    /// scanning the global index.
    pub export_instance_names: HashSet<String>,
}

/// InfoService holds computed data for all files in a workspace.
pub struct InfoService {
    pub files: HashMap<String, FileInfo>,
    /// Union of all type definitions across files (rule-driven + heuristic).
    pub all_type_defs: HashMap<String, Vec<(String, SourceLocation)>>,
    /// Cross-file type-instance index.
    pub type_index: TypeIndex,
    /// Refcount maps over the cross-file symbols. Each map counts how many files
    /// define the symbol; a key exists iff at least one file still defines it, so
    /// the keys double as the membership set (use [`HashMap::contains_key`] /
    /// [`HashMap::keys`]). `clear_file` removes the key when its count hits 0.
    pub event_target_counts: HashMap<String, usize>,
    pub variable_counts: HashMap<String, usize>,
    pub inline_script_counts: HashMap<String, usize>,
    /// Effect/trigger names that DEFINE a `value_set[variable]` (e.g.
    /// `set_variable`). Cached so per-file indexing can scan `set_variable`
    /// blocks for defined names (and their values) without recomputing it from
    /// the ruleset each time. Set by [`InfoService::set_var_effects`] at ruleset
    /// load; empty until then, which leaves the variable index untouched.
    var_effects: HashSet<String>,
}

impl Default for InfoService {
    fn default() -> Self {
        Self::new()
    }
}

impl InfoService {
    pub fn new() -> Self {
        Self {
            files: HashMap::new(),
            all_type_defs: HashMap::new(),
            type_index: TypeIndex::new(),
            event_target_counts: HashMap::new(),
            variable_counts: HashMap::new(),
            inline_script_counts: HashMap::new(),
            var_effects: HashSet::new(),
        }
    }

    /// Cache the set of variable-defining effects (from
    /// [`cwtools_index::variable_defining_effects`]). The LSP calls this once at
    /// ruleset load so per-file indexing can populate the variable index that the
    /// CW246/VariableGetField checks consult.
    pub fn set_var_effects(&mut self, effects: HashSet<String>) {
        self.var_effects = effects;
    }

    /// One-line size summary for profiling (counts only, not bytes).
    pub fn profile_summary(&self) -> String {
        let cross_file: usize = self.type_index.map.values().map(|v| v.len()).sum();
        format!(
            "info: {} files | type_index {} instances / {} types | {} vars | {} targets | {} type_defs",
            self.files.len(),
            cross_file,
            self.type_index.map.len(),
            self.variable_counts.len(),
            self.event_target_counts.len(),
            self.all_type_defs.len(),
        )
    }

    /// Compute info for a single parsed file and merge into global indexes.
    pub fn index_file(
        &mut self,
        uri: &str,
        ast: &ParsedFile,
        table: &StringTable,
        ruleset: &RuleSet,
    ) {
        self.index_file_with_path(uri, ast, table, ruleset, uri);
    }

    /// Like `index_file` but accepts a separate `logical_path` (relative to mod
    /// root) for path-matching type definitions.
    pub fn index_file_with_path(
        &mut self,
        uri: &str,
        ast: &ParsedFile,
        table: &StringTable,
        ruleset: &RuleSet,
        logical_path: &str,
    ) {
        let mut info = FileInfo::default();

        // ── Heuristic type-name set (kept for back-compat) ────────────────────
        // Use the pre-built type_by_name index from reindex() instead of
        // rebuilding a HashSet per file.
        let type_names = &ruleset.type_by_name;
        for child in &ast.root_children {
            Self::index_child_heuristic(child, &ast.arena, table, type_names, &mut info);
        }

        // ── Dynamic value collection (completion-only) ────────────────────────
        self.type_index.complex_enum_values.merge_file(
            uri,
            cwtools_index::dynamic_values::collect_complex_enum_values(
                ruleset,
                ast,
                logical_path,
                table,
            ),
        );
        self.type_index.value_set_values.merge_file(
            uri,
            cwtools_index::dynamic_values::collect_value_set_members(ruleset, ast, table),
        );

        // ── Rule-driven: type-instance index ─────────────────────────────────
        // Move the instances straight into the cross-file index. We don't keep a
        // second per-file copy on `FileInfo` (that doubled ~190K instances on
        // MD); document-symbol derives a file's instances from the index instead.
        let instances = collect_type_instances(ruleset, ast, logical_path, table);
        // Hash this file's exported instances now, while we still hold the local
        // per-type map, so the cross-file export check never has to scan the
        // global index. Order-independent (wrapping_add) and stable for a given
        // set of `(type, name)` pairs.
        info.export_instances_hash = hash_instance_exports(&instances);
        info.export_instance_names = instances
            .values()
            .flat_map(|v| v.iter())
            .map(|inst| inst.name.to_ascii_lowercase())
            .collect();
        self.type_index.merge(uri, instances);

        // ── Rule-driven: defined variables ────────────────────────────────────
        // Convert the @-vars already collected by index_child_heuristic into
        // DefinedVariable form so collect_defined_variables_from_rules can skip
        // re-scanning the AST for them.
        let at_vars: Vec<DefinedVariable> = info
            .defined_variables
            .iter()
            .map(|(name, loc)| DefinedVariable {
                name: name.clone(),
                namespace: None,
                location: *loc,
                value: None,
            })
            .collect();
        info.defined_variables_ns =
            collect_defined_variables_from_rules(ruleset, ast, logical_path, table, Some(at_vars));
        // value_set[variable] definitions made through `set_variable`-family
        // effects live inside `alias[effect]` expansions the rule-tree walk above
        // never reaches, so collect them directly (with values, for hover) and
        // merge into the "variable" namespace.
        if !self.var_effects.is_empty() {
            let mut set_vars: Vec<DefinedVariable> = Vec::new();
            collect_set_variable_defs(ast, table, &self.var_effects, &mut set_vars);
            if !set_vars.is_empty() {
                info.defined_variables_ns
                    .entry("variable".to_string())
                    .or_default()
                    .extend(set_vars);
            }
        }
        // Flatten ALL variable entries (both @-vars and value_set names) into the
        // legacy map so clear_file can remove them and completion stays current.
        for vars in info.defined_variables_ns.values() {
            for v in vars {
                info.defined_variables.insert(v.name.clone(), v.location);
            }
        }

        // saved_event_targets_detailed is populated by index_child_heuristic
        // (it detects save_event_target_as / save_global_event_target_as).
        // index_child_heuristic also inserts event_target:-prefixed keys directly
        // into saved_event_targets; merge rather than overwrite so those survive.
        info.saved_event_targets.extend(
            info.saved_event_targets_detailed
                .iter()
                .map(|e| e.name.clone()),
        );

        // ── Merge into global indexes ─────────────────────────────────────────
        for (type_name, locs) in &info.type_definitions {
            self.all_type_defs
                .entry(type_name.clone())
                .or_default()
                .extend(locs.iter().map(|l| (uri.to_string(), *l)));
        }
        for et in &info.saved_event_targets {
            *self.event_target_counts.entry(et.clone()).or_insert(0) += 1;
        }
        for (ns, vars) in &info.defined_variables_ns {
            for v in vars {
                *self.variable_counts.entry(v.name.clone()).or_insert(0) += 1;
                // Feed value_set[variable] names into the project-wide var index
                // that CW246 / VariableGetField consult. @-vars are excluded:
                // reads bypass them, and they'd pollute the unset-variable check.
                if ns != "@" {
                    self.type_index.var_index.add_name(&v.name);
                }
            }
        }
        for script in info.inline_scripts.keys() {
            *self.inline_script_counts.entry(script.clone()).or_insert(0) += 1;
        }

        self.files.insert(uri.to_string(), info);
    }

    /// Order-independent hash of the cross-file-visible symbols a file exports:
    /// type instances, defined variables, and saved event targets. If this is
    /// unchanged across an edit, no other file's diagnostics can change, so the
    /// dependent sweep can be skipped.
    ///
    /// O(symbols-in-this-file): reads the precomputed instance hash plus the
    /// file's variable/event-target lists, never scanning the global index.
    /// Returns 0 for an unknown file (treated as "no exports").
    pub fn export_fingerprint(&self, uri: &str) -> u64 {
        let Some(fi) = self.files.get(uri) else {
            return 0;
        };
        // wrapping_add combines symbols order-independently while preserving
        // multiplicity (XOR would cancel a duplicated symbol to zero).
        let mut acc: u64 = fi.export_instances_hash;
        for (ns, vars) in &fi.defined_variables_ns {
            for v in vars {
                acc = acc.wrapping_add(mix_export_symbol(&["v", ns, &v.name]));
            }
        }
        for et in &fi.saved_event_targets {
            acc = acc.wrapping_add(mix_export_symbol(&["e", et]));
        }
        acc
    }

    /// The lowercased names of every cross-file-visible symbol a file exports:
    /// type instances, defined variables, and saved event targets. Used to scope
    /// the dependent sweep to the open docs that actually reference a name that
    /// changed. O(symbols-in-file): instance names come from the global index
    /// filtered to this file (cheap relative to a full revalidation), the rest
    /// from the file's own `FileInfo`.
    pub fn export_names(&self, uri: &str) -> HashSet<String> {
        let mut names = HashSet::new();
        if let Some(fi) = self.files.get(uri) {
            names.extend(fi.export_instance_names.iter().cloned());
            for vars in fi.defined_variables_ns.values() {
                for v in vars {
                    names.insert(v.name.to_ascii_lowercase());
                }
            }
            for et in &fi.saved_event_targets {
                names.insert(et.to_ascii_lowercase());
            }
        }
        names
    }

    /// Remove a file from all indexes.
    pub fn clear_file(&mut self, uri: &str) {
        if let Some(info) = self.files.remove(uri) {
            // Type definitions (heuristic)
            for type_name in info.type_definitions.keys() {
                if let Some(locs) = self.all_type_defs.get_mut(type_name) {
                    locs.retain(|(u, _)| u != uri);
                    if locs.is_empty() {
                        self.all_type_defs.remove(type_name);
                    }
                }
            }
            // Rule-driven type instances
            self.type_index.remove_file(uri);
            // Event targets
            for et in &info.saved_event_targets {
                if let Some(count) = self.event_target_counts.get_mut(et) {
                    *count -= 1;
                    if *count == 0 {
                        self.event_target_counts.remove(et);
                    }
                }
            }
            // Variables (refcount via variable_counts, keyed by name)
            for (ns, vars) in &info.defined_variables_ns {
                for v in vars {
                    if let Some(count) = self.variable_counts.get_mut(&v.name) {
                        *count -= 1;
                        if *count == 0 {
                            self.variable_counts.remove(&v.name);
                        }
                    }
                    if ns != "@" {
                        self.type_index.var_index.remove_name(&v.name);
                    }
                }
            }
            // Inline scripts
            for script in info.inline_scripts.keys() {
                if let Some(count) = self.inline_script_counts.get_mut(script) {
                    *count -= 1;
                    if *count == 0 {
                        self.inline_script_counts.remove(script);
                    }
                }
            }
        }
    }

    /// Find all heuristic definitions of a given symbol name.
    pub fn find_definitions(&self, name: &str) -> Option<&Vec<(String, SourceLocation)>> {
        self.all_type_defs.get(name)
    }

    /// Find every definition site of a script variable named `name`
    /// (case-insensitive), across all files. Used by goto-definition on a
    /// `value[variable]` read. Returns `(file_uri, location)` pairs.
    pub fn find_variable_definitions(&self, name: &str) -> Vec<(String, SourceLocation)> {
        let mut out = Vec::new();
        for (uri, fi) in &self.files {
            for vars in fi.defined_variables_ns.values() {
                for v in vars {
                    if v.name.eq_ignore_ascii_case(name) {
                        out.push((uri.clone(), v.location));
                    }
                }
            }
        }
        out
    }

    /// The distinct known values a variable named `name` is assigned across the
    /// workspace, in first-seen order. Used to render variable hover. `limit`
    /// caps the returned set; the returned `bool` is `true` when more existed.
    pub fn variable_values(&self, name: &str, limit: usize) -> (Vec<String>, bool) {
        let mut values: Vec<String> = Vec::new();
        let mut seen: HashSet<&str> = HashSet::new();
        let mut truncated = false;
        for fi in self.files.values() {
            for vars in fi.defined_variables_ns.values() {
                for v in vars {
                    if !v.name.eq_ignore_ascii_case(name) {
                        continue;
                    }
                    if let Some(val) = &v.value
                        && seen.insert(val.as_str())
                    {
                        if values.len() >= limit {
                            truncated = true;
                        } else {
                            values.push(val.clone());
                        }
                    }
                }
            }
        }
        (values, truncated)
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

    // ── Heuristic child walker (unchanged from original) ─────────────────────

    fn index_child_heuristic<S: std::hash::BuildHasher>(
        child: &Child,
        arena: &Arena,
        table: &StringTable,
        type_names: &HashMap<String, usize, S>,
        info: &mut FileInfo,
    ) {
        if let Child::Leaf(idx) = child {
            let leaf = &arena.leaves[*idx as usize];
            let key = table.get_string(leaf.key.normal).unwrap_or_default();

            Self::record_top_level_key(leaf, &key, type_names, info);

            let value_str = leaf_value_string(&leaf.value, table);

            Self::record_type_reference(leaf, &value_str, info);

            if let Value::Clause(children) = &leaf.value {
                for c in children {
                    Self::index_child_heuristic(c, arena, table, type_names, info);
                }
            }

            Self::record_saved_event_target(leaf, &key, &value_str, info);
            Self::record_inline_script(leaf, arena, table, &key, info);
            Self::record_effect_trigger_block(leaf, &key, info);

            // Owned consumer last so `key` moves in rather than clones. An
            // `@`-prefixed key never matches any of the borrow-only checks above,
            // so position is behavior-neutral.
            Self::record_defined_variable(leaf, key, info);
        }
    }

    /// Record a clause leaf as a top-level key, and as a type definition when
    /// its key names a known type.
    fn record_top_level_key<S: std::hash::BuildHasher>(
        leaf: &cwtools_parser::ast::Leaf,
        key: &str,
        type_names: &HashMap<String, usize, S>,
        info: &mut FileInfo,
    ) {
        if let Value::Clause(_) = &leaf.value {
            info.top_level_keys.push((
                key.to_string(),
                SourceLocation {
                    line: leaf.pos.start.line,
                    col: leaf.pos.start.col,
                },
            ));

            if type_names.contains_key(key) {
                info.type_definitions
                    .entry(key.to_string())
                    .or_default()
                    .push(SourceLocation {
                        line: leaf.pos.start.line,
                        col: leaf.pos.start.col,
                    });
            }
        }
    }

    /// Record a `<type>` value as a reference to that type.
    fn record_type_reference(
        leaf: &cwtools_parser::ast::Leaf,
        value_str: &str,
        info: &mut FileInfo,
    ) {
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
    }

    /// Record saved event targets: the `event_target:` prefix form into the
    /// name set, and the `save_(global_)event_target_as` form with full detail.
    fn record_saved_event_target(
        leaf: &cwtools_parser::ast::Leaf,
        key: &str,
        value_str: &str,
        info: &mut FileInfo,
    ) {
        if key.starts_with("event_target:") {
            let target = key.strip_prefix("event_target:").unwrap_or("");
            if !target.is_empty() {
                info.saved_event_targets.insert(target.to_string());
            }
        }

        if (key == "save_event_target_as" || key == "save_global_event_target_as")
            && !value_str.is_empty()
        {
            info.saved_event_targets_detailed.push(SavedEventTarget {
                name: value_str.to_string(),
                location: SourceLocation {
                    line: leaf.pos.start.line,
                    col: leaf.pos.start.col,
                },
                is_global: key == "save_global_event_target_as",
            });
        }
    }

    /// Record the `script` referenced by an `inline_script` clause.
    fn record_inline_script(
        leaf: &cwtools_parser::ast::Leaf,
        arena: &Arena,
        table: &StringTable,
        key: &str,
        info: &mut FileInfo,
    ) {
        if key == "inline_script"
            && let Value::Clause(children) = &leaf.value
        {
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

    /// Record `effect`/`*_effect` and `trigger`/`*_trigger` block positions.
    fn record_effect_trigger_block(
        leaf: &cwtools_parser::ast::Leaf,
        key: &str,
        info: &mut FileInfo,
    ) {
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

    /// Record an `@`-prefixed key as a defined variable.
    fn record_defined_variable(leaf: &cwtools_parser::ast::Leaf, key: String, info: &mut FileInfo) {
        if key.starts_with('@') {
            info.defined_variables.insert(
                key,
                SourceLocation {
                    line: leaf.pos.start.line,
                    col: leaf.pos.start.col,
                },
            );
        }
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// Tests
// ══════════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use cwtools_parser::parser::parse_string;
    use cwtools_rules::rules_types::{PathOptions, SkipRootKey, TypeDefinition};

    // ── helpers ──────────────────────────────────────────────────────────────

    fn empty_type_def(name: &str, paths: Vec<&str>) -> TypeDefinition {
        TypeDefinition {
            name: name.to_string(),
            name_field: None,
            path_options: PathOptions {
                paths: paths.into_iter().map(|s| s.to_string()).collect(),
                path_strict: false,
                path_file: None,
                path_extension: None,
                paths_lower: Vec::new(),
                ..Default::default()
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
        }
    }

    fn make_ruleset_with_type(td: TypeDefinition) -> RuleSet {
        let mut rs = RuleSet::new();
        rs.types.push(td);
        rs
    }

    fn make_info_heuristic(source: &str) -> (FileInfo, StringTable) {
        let table = StringTable::new();
        let parsed = parse_string(source, &table).unwrap();
        let mut info = FileInfo::default();
        let type_names = HashMap::new();
        for child in &parsed.root_children {
            InfoService::index_child_heuristic(
                child,
                &parsed.arena,
                &table,
                &type_names,
                &mut info,
            );
        }
        (info, table)
    }

    // ── original heuristic tests ──────────────────────────────────────────────

    #[test]
    fn test_defined_variables() {
        let source = "@my_var = 5\nfoo = { bar = @my_var }";
        let (info, _) = make_info_heuristic(source);
        assert!(info.defined_variables.contains_key("@my_var"));
    }

    #[test]
    fn test_type_references() {
        let source = "create_country = { ethos = <ethos> }";
        let (info, _) = make_info_heuristic(source);
        assert!(info.type_references.contains_key("ethos"));
    }

    #[test]
    fn test_event_targets() {
        let source = "event_target:my_target = { foo = bar }";
        let (info, _) = make_info_heuristic(source);
        assert!(info.saved_event_targets.contains("my_target"));
    }

    #[test]
    fn test_inline_scripts() {
        let source = "inline_script = { script = my_inline_script }";
        let (info, _) = make_info_heuristic(source);
        assert!(info.inline_scripts.contains_key("my_inline_script"));
    }

    // ── Item 1 — type-instance index ─────────────────────────────────────────

    /// Simple case: top-level key = type instance, no skip_root_key.
    #[test]
    fn test_type_instance_simple() {
        let source = "my_ethos = { tradition = foo }";
        let table = StringTable::new();
        let parsed = parse_string(source, &table).unwrap();

        let td = empty_type_def("ethoses", vec!["common/ethics"]);
        let rs = make_ruleset_with_type(td);

        let result = collect_type_instances(&rs, &parsed, "common/ethics/00_ethics.txt", &table);
        let instances = result.get("ethoses").expect("should find ethoses");
        assert_eq!(instances.len(), 1);
        assert_eq!(instances[0].name, "my_ethos");
    }

    /// Path that does NOT match: no instances returned.
    #[test]
    fn test_type_instance_path_mismatch() {
        let source = "my_ethos = { tradition = foo }";
        let table = StringTable::new();
        let parsed = parse_string(source, &table).unwrap();

        let td = empty_type_def("ethoses", vec!["common/ethics"]);
        let rs = make_ruleset_with_type(td);

        let result = collect_type_instances(&rs, &parsed, "events/my_events.txt", &table);
        assert!(result.get("ethoses").is_none_or(|v| v.is_empty()));
    }

    /// skip_root_key = AnyKey: grandchildren are the instances.
    #[test]
    fn test_type_instance_skip_root_key() {
        let source = "technologies = { my_tech = { } another_tech = { } }";
        let table = StringTable::new();
        let parsed = parse_string(source, &table).unwrap();

        let mut td = empty_type_def("technology", vec!["common/technologies"]);
        td.skip_root_key = vec![SkipRootKey::AnyKey];
        let rs = make_ruleset_with_type(td);

        let result =
            collect_type_instances(&rs, &parsed, "common/technologies/00_techs.txt", &table);
        let instances = result.get("technology").expect("should find technology");
        let names: Vec<&str> = instances.iter().map(|i| i.name.as_str()).collect();
        assert!(
            names.contains(&"my_tech"),
            "expected my_tech in {:?}",
            names
        );
        assert!(
            names.contains(&"another_tech"),
            "expected another_tech in {:?}",
            names
        );
    }

    /// name_field: the instance name comes from child leaf value.
    #[test]
    fn test_type_instance_name_field() {
        let source = "some_event = { id = my_event_001 }";
        let table = StringTable::new();
        let parsed = parse_string(source, &table).unwrap();

        let mut td = empty_type_def("event", vec!["events"]);
        td.name_field = Some("id".to_string());
        let rs = make_ruleset_with_type(td);

        let result = collect_type_instances(&rs, &parsed, "events/my_events.txt", &table);
        let instances = result.get("event").expect("should find event");
        assert_eq!(instances.len(), 1);
        assert_eq!(instances[0].name, "my_event_001");
    }

    /// A quoted name_field value (e.g. spriteType `name = "GFX_x"`) must be
    /// indexed without its quotes so unquoted references (`icon = GFX_x`) resolve.
    #[test]
    fn test_type_instance_name_field_quoted() {
        let source = "spriteTypes = { spriteType = { name = \"GFX_test_icon\" } }";
        let table = StringTable::new();
        let parsed = parse_string(source, &table).unwrap();

        let mut td = empty_type_def("spriteType", vec!["game/interface"]);
        td.name_field = Some("name".to_string());
        td.skip_root_key = vec![SkipRootKey::SpecificKey("spriteTypes".to_string())];
        let rs = make_ruleset_with_type(td);

        let result = collect_type_instances(&rs, &parsed, "game/interface/x.gfx", &table);
        let instances = result.get("spriteType").expect("should find spriteType");
        assert_eq!(instances.len(), 1);
        assert_eq!(instances[0].name, "GFX_test_icon");
    }

    /// type_per_file: the instance name is the file stem. On Windows the LSP can
    /// derive logical paths with backslash separators; the name extraction must
    /// normalise them, else `load_oob = "MY_OOB"` is a false positive because the
    /// indexed name becomes the whole path rather than the stem.
    #[test]
    fn test_type_per_file_backslash_path() {
        let source = "MY_OOB = { y = yes }\n";
        let table = StringTable::new();
        let parsed = parse_string(source, &table).unwrap();

        let mut td = empty_type_def("oob", vec!["history/units"]);
        td.type_per_file = true;
        let rs = make_ruleset_with_type(td);

        // Backslash separators, as produced by logical_path_from_uri on Windows.
        let result = collect_type_instances(&rs, &parsed, "history\\units\\MY_OOB.txt", &table);
        let instances = result.get("oob").expect("should find oob");
        assert_eq!(instances.len(), 1);
        assert_eq!(
            instances[0].name, "MY_OOB",
            "type_per_file name must be the file stem, got {:?}",
            instances[0].name
        );
    }

    /// type_key_filter: only nodes with a matching key qualify.
    #[test]
    fn test_type_instance_key_filter() {
        let source = "country_event = { id = foo }\nsome_other = { id = bar }";
        let table = StringTable::new();
        let parsed = parse_string(source, &table).unwrap();

        let mut td = empty_type_def("event", vec!["events"]);
        // Only accept nodes whose key is "country_event"
        td.type_key_filter = Some((vec!["country_event".to_string()], false));
        td.name_field = Some("id".to_string());
        let rs = make_ruleset_with_type(td);

        let result = collect_type_instances(&rs, &parsed, "events/test.txt", &table);
        let instances = result.get("event").expect("should find event");
        let names: Vec<&str> = instances.iter().map(|i| i.name.as_str()).collect();
        assert!(names.contains(&"foo"), "should have foo: {:?}", names);
        assert!(!names.contains(&"bar"), "should not have bar: {:?}", names);
    }

    /// TypeIndex.contains works after merging.
    #[test]
    fn test_type_index_contains() {
        let mut idx = TypeIndex::new();
        let mut map = HashMap::new();
        map.insert(
            "event".to_string(),
            vec![TypeInstance {
                name: "my_event".to_string(),
                location: SourceLocation { line: 1, col: 0 },
                primary_loc_key: None,
            }],
        );
        idx.merge("file://test.txt", map);

        assert!(idx.contains("event", "my_event"));
        assert!(!idx.contains("event", "nonexistent"));
        assert!(!idx.contains("other_type", "my_event"));
    }

    /// TypeIndex.remove_file cleans up properly.
    #[test]
    fn test_type_index_remove_file() {
        let mut idx = TypeIndex::new();
        let mut map = HashMap::new();
        map.insert(
            "event".to_string(),
            vec![TypeInstance {
                name: "ev1".to_string(),
                location: SourceLocation { line: 1, col: 0 },
                primary_loc_key: None,
            }],
        );
        idx.merge("file://a.txt", map.clone());
        idx.merge("file://b.txt", map);

        idx.remove_file("file://a.txt");
        // ev1 still exists from b.txt
        assert!(idx.contains("event", "ev1"));

        idx.remove_file("file://b.txt");
        assert!(!idx.contains("event", "ev1"));
    }

    #[test]
    fn test_is_any_instance_refcount() {
        // is_any_instance is backed by a refcount so a name survives until its
        // last definition is removed (two files defining the same name).
        let mut idx = TypeIndex::new();
        let mut map = HashMap::new();
        map.insert(
            "character".to_string(),
            vec![TypeInstance {
                name: "GER_some_char".to_string(),
                location: SourceLocation { line: 1, col: 0 },
                primary_loc_key: None,
            }],
        );
        idx.merge("file://a.txt", map.clone());
        idx.merge("file://b.txt", map);
        assert!(idx.is_any_instance("GER_some_char"));
        assert!(!idx.is_any_instance("unknown_name"));

        idx.remove_file("file://a.txt");
        // still present via b.txt
        assert!(idx.is_any_instance("GER_some_char"));

        idx.remove_file("file://b.txt");
        assert!(!idx.is_any_instance("GER_some_char"));
    }

    #[test]
    fn test_contains_case_insensitive() {
        // Paradox identifiers are case-insensitive: a reference in any case must
        // resolve to a definition in any case (both `contains` and the
        // `is_any_instance` refcount index agree on lowercase normalization).
        let mut idx = TypeIndex::new();
        let mut map = HashMap::new();
        map.insert(
            "ai_behavior".to_string(),
            vec![TypeInstance {
                name: "LBA_ai_behavior".to_string(),
                location: SourceLocation { line: 1, col: 0 },
                primary_loc_key: None,
            }],
        );
        idx.merge("file://a.txt", map);
        assert!(idx.contains("ai_behavior", "LBA_AI_BEHAVIOR"));
        assert!(idx.contains("ai_behavior", "lba_ai_behavior"));
        assert!(idx.is_any_instance("LBA_AI_BEHAVIOR"));
        // Removing the only definition clears both indexes regardless of case.
        idx.remove_file("file://a.txt");
        assert!(!idx.contains("ai_behavior", "LBA_ai_behavior"));
        assert!(!idx.is_any_instance("lba_ai_behavior"));
    }

    // ── Item 2 — defined variables ────────────────────────────────────────────

    #[test]
    fn test_at_vars_collected() {
        let source = "@min_manpower = 100\n@max_tech = 5";
        let table = StringTable::new();
        let parsed = parse_string(source, &table).unwrap();

        let rs = RuleSet::new();
        // collect_defined_variables was deleted (no production callers); use the
        // rule-aware entry point which also covers the @-var path.
        let vars = collect_defined_variables_from_rules(&rs, &parsed, "", &table, None);
        let at_vars = vars.get("@").expect("should have @-namespace vars");
        let names: Vec<&str> = at_vars.iter().map(|v| v.name.as_str()).collect();
        assert!(names.contains(&"@min_manpower"));
        assert!(names.contains(&"@max_tech"));
    }

    // ── Item 3 — saved event targets ─────────────────────────────────────────

    #[test]
    fn test_saved_event_targets() {
        let source = "
effect = {
    save_event_target_as = my_target
    save_global_event_target_as = global_target
}";
        let table = StringTable::new();
        let parsed = parse_string(source, &table).unwrap();
        // collect_saved_event_targets was deleted (no production callers); use
        // the InfoService to exercise the same code path that production uses.
        let mut service = InfoService::new();
        let rs = RuleSet::new();
        service.index_file_with_path("test.txt", &parsed, &table, &rs, "");
        let fi = service.files.get("test.txt").expect("file indexed");
        let targets = &fi.saved_event_targets_detailed;

        let names: Vec<&str> = targets.iter().map(|t| t.name.as_str()).collect();
        assert!(
            names.contains(&"my_target"),
            "missing my_target: {:?}",
            names
        );
        assert!(
            names.contains(&"global_target"),
            "missing global_target: {:?}",
            names
        );

        let global = targets.iter().find(|t| t.name == "global_target").unwrap();
        assert!(global.is_global);
        let local = targets.iter().find(|t| t.name == "my_target").unwrap();
        assert!(!local.is_global);
    }

    // ── Item 4 — position query ───────────────────────────────────────────────

    #[test]
    fn test_element_at_position_leaf() {
        let source = "foo = bar\n";
        let table = StringTable::new();
        let parsed = parse_string(source, &table).unwrap();

        let element = element_at_position(&parsed, 1, 6, &table);
        match element {
            Some(PositionElement::Leaf { key, value }) => {
                assert_eq!(key, "foo");
                assert_eq!(value, "bar");
            }
            other => panic!("expected Leaf, got {:?}", other),
        }
    }

    /// `variable_defining_effects` picks out aliases whose body declares a
    /// `value_set[variable]`, and `collect_set_variable_names` then extracts the
    /// defined names from both the explicit (`var = X`) and shorthand
    /// (`X = value`) forms.
    #[test]
    fn test_collect_set_variable_names() {
        const RULES: &str = r#"
types = { type[foo] = { path = "game/common/foo" } }
foo = { alias_name[effect] = alias_match_left[effect] }
alias[effect:set_variable] = {
    var = value_set[variable]
    value = int_variable_field
}
alias[effect:set_temp_variable] = {
    value_set[variable] = int_variable_field
}
"#;
        use cwtools_rules::rules_converter::ast_to_ruleset;
        let table = StringTable::new();
        let parsed_cwt = parse_string(RULES, &table).unwrap();
        let ruleset = ast_to_ruleset(&parsed_cwt, &table);

        let effects = variable_defining_effects(&ruleset);
        assert!(effects.contains("set_variable"), "got: {:?}", effects);
        assert!(effects.contains("set_temp_variable"), "got: {:?}", effects);

        let script = "foo = { set_variable = { var = my_explicit value = 3 } set_temp_variable = { my_shorthand = 5 } }";
        let parsed = parse_string(script, &table).unwrap();
        let mut names = Vec::new();
        collect_set_variable_names(&parsed, &table, &effects, &mut names);
        assert!(
            names.contains(&"my_explicit".to_string()),
            "got: {:?}",
            names
        );
        assert!(
            names.contains(&"my_shorthand".to_string()),
            "got: {:?}",
            names
        );
        // The reserved `value` key must not be collected as a variable name.
        assert!(!names.contains(&"value".to_string()), "got: {:?}", names);
    }

    /// `collect_set_variable_defs` captures the assigned value for both the
    /// explicit (`var = X value = N`) and shorthand (`X = N`) forms.
    #[test]
    fn test_collect_set_variable_defs_values() {
        use cwtools_rules::rules_converter::ast_to_ruleset;
        const RULES: &str = r#"
types = { type[foo] = { path = "game/common/foo" } }
foo = { alias_name[effect] = alias_match_left[effect] }
alias[effect:set_variable] = {
    var = value_set[variable]
    value = int_variable_field
}
alias[effect:set_temp_variable] = {
    value_set[variable] = int_variable_field
}
"#;
        let table = StringTable::new();
        let ruleset = ast_to_ruleset(&parse_string(RULES, &table).unwrap(), &table);
        let effects = variable_defining_effects(&ruleset);

        let script = "foo = { set_variable = { var = my_explicit value = 3 } set_temp_variable = { my_shorthand = 5 } }";
        let parsed = parse_string(script, &table).unwrap();
        let mut defs = Vec::new();
        collect_set_variable_defs(&parsed, &table, &effects, &mut defs);

        let explicit = defs
            .iter()
            .find(|d| d.name == "my_explicit")
            .expect("my_explicit");
        assert_eq!(explicit.value.as_deref(), Some("3"));
        let shorthand = defs
            .iter()
            .find(|d| d.name == "my_shorthand")
            .expect("my_shorthand");
        assert_eq!(shorthand.value.as_deref(), Some("5"));
    }

    // ── Item 2.7 — value_set var removed from variable_counts on clear_file ──

    #[test]
    fn value_set_var_cleared_on_file_clear() {
        // Manually inject a non-@-var into a file's defined_variables and confirm
        // that clear_file removes it from variable_counts (the 2.7 inversion fix).
        let mut svc = InfoService::new();
        let uri = "file://test.txt";

        // Simulate a file with a value_set variable "my_var" (no @ prefix).
        let mut file_info = FileInfo::default();
        file_info.defined_variables.insert(
            "my_var".to_string(),
            cwtools_index::SourceLocation { line: 1, col: 0 },
        );
        // Populate defined_variables_ns so that clear_file's refcount path fires.
        file_info.defined_variables_ns.insert(
            "@".to_string(),
            vec![cwtools_index::DefinedVariable {
                name: "my_var".to_string(),
                namespace: None,
                location: cwtools_index::SourceLocation { line: 1, col: 0 },
                value: None,
            }],
        );
        svc.files.insert(uri.to_string(), file_info);
        // Mirror the refcount that index_file_with_path would have set; the key's
        // presence in variable_counts is the membership signal.
        svc.variable_counts.insert("my_var".to_string(), 1);

        // Before clear: variable must be present.
        assert!(svc.variable_counts.contains_key("my_var"));

        // Clear the file: variable must be removed.
        svc.clear_file(uri);
        assert!(
            !svc.variable_counts.contains_key("my_var"),
            "my_var should be gone after clear_file"
        );
    }
}
