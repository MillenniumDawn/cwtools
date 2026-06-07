use cwtools_parser::ast::{Arena, Child, ParsedFile, Value};
use cwtools_rules::rules_types::{
    NewField, PathOptions, RuleSet, RuleType, SkipRootKey, TypeDefinition,
};
use cwtools_string_table::string_table::StringTable;
use std::collections::{HashMap, HashSet};

pub mod inline_expansion;
pub mod vanilla_cache;

// ── Shared helpers ────────────────────────────────────────────────────────────

/// Strip one layer of surrounding double-quotes, if present.
fn unquote(s: &str) -> &str {
    s.strip_prefix('"')
        .and_then(|t| t.strip_suffix('"'))
        .unwrap_or(s)
}

/// Extract a plain string from a leaf value.
fn leaf_value_string(value: &Value, table: &StringTable) -> String {
    match value {
        Value::String(t) | Value::QString(t) => table.get_string(t.normal).unwrap_or_default(),
        Value::Float(f) => f.to_string(),
        Value::Int(i) => i.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Clause(_) => String::new(),
    }
}

// ── Source location ───────────────────────────────────────────────────────────

#[derive(Debug, Clone, Copy)]
pub struct SourceLocation {
    pub line: u32,
    pub col: u16,
}

// ══════════════════════════════════════════════════════════════════════════════
// Item 1 — Cross-file type-instance index
// ══════════════════════════════════════════════════════════════════════════════

/// A single defined instance of a CW type (e.g. one event, one technology …).
#[derive(Debug, Clone)]
pub struct TypeInstance {
    /// The instance name (node key, or the value of `name_field` child).
    pub name: String,
    /// Where the definition starts in the source file.
    pub location: SourceLocation,
}

/// Holds all known instances for every type, aggregated across files.
/// An index of every file path under the game roots (mod + vanilla), used to
/// check that `filepath` references resolve (CW113). Paths are stored lowercased
/// and forward-slashed, relative to their root, so lookups are case-insensitive.
#[derive(Debug, Default)]
pub struct FileIndex {
    files: HashSet<String>,
}

impl FileIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// Walk `root` recursively and add every file's path relative to `root`.
    pub fn add_root(&mut self, root: &std::path::Path) {
        Self::walk(root, root, &mut self.files);
    }

    fn walk(root: &std::path::Path, dir: &std::path::Path, out: &mut HashSet<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                Self::walk(root, &path, out);
            } else if let Ok(rel) = path.strip_prefix(root)
                && let Some(s) = rel.to_str() {
                    out.insert(s.replace('\\', "/").to_ascii_lowercase());
                }
        }
    }

    pub fn is_empty(&self) -> bool {
        self.files.is_empty()
    }

    pub fn len(&self) -> usize {
        self.files.len()
    }

    /// Whether a game-relative path exists (case-insensitive). The argument is
    /// normalised (lowercased, forward slashes, leading slash stripped).
    pub fn contains(&self, path: &str) -> bool {
        let norm = path
            .trim()
            .trim_start_matches('/')
            .replace('\\', "/")
            .to_ascii_lowercase();
        self.files.contains(&norm)
    }
}

/// Project-wide set of defined script-variable names (every `value_set[...]`
/// definition collected across the mod + base game), used to check that a
/// `variable_field` reference resolves (CW246). Names are normalised to a
/// canonical key so a definition like `morale@ROOT` and a read like
/// `morale@GER` both resolve to `morale`. Empty unless the CLI populated it.
#[derive(Debug, Default)]
pub struct VarIndex {
    names: HashSet<String>,
}

impl VarIndex {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_empty(&self) -> bool {
        self.names.is_empty()
    }

    pub fn len(&self) -> usize {
        self.names.len()
    }

    /// Canonical lookup key for a raw variable token: lowercased, unquoted, the
    /// base before any `@`-concatenation, the last `.`-segment of that base, and
    /// before any `?`/`^` selector. Mirrors F# `getVariableFromString` plus the
    /// read-side dot-split in `changeScope`.
    pub fn normalize(raw: &str) -> String {
        let s = raw.trim().trim_matches('"');
        let before_amp = s.split('@').next().unwrap_or(s);
        let last_seg = before_amp.rsplit('.').next().unwrap_or(before_amp);
        let core = last_seg.split(['?', '^']).next().unwrap_or(last_seg);
        core.trim().to_ascii_lowercase()
    }

    pub fn add_name(&mut self, raw: &str) {
        let n = Self::normalize(raw);
        if !n.is_empty() {
            self.names.insert(n);
        }
    }

    /// Whether a raw reference resolves to a known defined variable.
    pub fn contains(&self, raw: &str) -> bool {
        self.names.contains(&Self::normalize(raw))
    }

    /// Fold another index's names into this one (e.g. base-game variables into
    /// the mod's index).
    pub fn merge(&mut self, other: &VarIndex) {
        self.names.extend(other.names.iter().cloned());
    }
}

#[derive(Debug, Default)]
pub struct TypeIndex {
    /// type_name → Vec<(file_uri, instance)>
    pub map: HashMap<String, Vec<(String, TypeInstance)>>,
    /// instance name → how many definitions carry that name (across all types and
    /// files). Lets `is_any_instance` be O(1) instead of scanning every instance.
    /// A refcount so `remove_file` can drop a name only when its last definition goes.
    name_counts: HashMap<String, usize>,
    /// Index of every asset/file path under the game roots, for `filepath`
    /// reference checks (CW113). Empty unless the CLI populated it.
    pub file_index: FileIndex,
    /// Project-wide set of defined variable names, for `variable_field`
    /// reference checks (CW246). Empty unless the CLI populated it.
    pub var_index: VarIndex,
}

impl TypeIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// Return true if `type_name` has a known instance called `instance`.
    /// Paradox script identifiers are case-insensitive, so a reference like
    /// `LBA_AI_BEHAVIOR` resolves to the `LBA_ai_behavior` definition.
    pub fn contains(&self, type_name: &str, instance: &str) -> bool {
        self.map
            .get(type_name)
            .map(|v| {
                v.iter()
                    .any(|(_, ti)| ti.name.eq_ignore_ascii_case(instance))
            })
            .unwrap_or(false)
    }

    /// Return true if `name` is a known instance of ANY type. Used to recognise
    /// scope-opening keys: HOI4 from-data scope links (links.cwt) let an instance
    /// of a referenced type (character, state, ideology, ...) open its own scope,
    /// e.g. `LBA_some_character = { ... }`.
    pub fn is_any_instance(&self, name: &str) -> bool {
        self.name_counts.contains_key(name)
    }

    /// All instances for a type (across all files).
    pub fn instances(&self, type_name: &str) -> &[(String, TypeInstance)] {
        self.map.get(type_name).map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// Every `(type_name, instance)` defined in `file_uri`. Scans the whole
    /// index (O(total instances)); used by document-symbol/outline, which is
    /// on-demand and infrequent. Lets `FileInfo` avoid a second per-file copy.
    pub fn instances_in_file<'a>(&'a self, file_uri: &str) -> Vec<(&'a str, &'a TypeInstance)> {
        let mut out = Vec::new();
        for (type_name, entries) in &self.map {
            for (uri, inst) in entries {
                if uri == file_uri {
                    out.push((type_name.as_str(), inst));
                }
            }
        }
        out
    }

    /// Merge per-file results into the index.
    pub fn merge(&mut self, file_uri: &str, per_type: HashMap<String, Vec<TypeInstance>>) {
        for (type_name, instances) in per_type {
            let entry = self.map.entry(type_name).or_default();
            for inst in instances {
                *self.name_counts.entry(inst.name.clone()).or_insert(0) += 1;
                entry.push((file_uri.to_string(), inst));
            }
        }
    }

    /// Remove all instances contributed by `file_uri`.
    pub fn remove_file(&mut self, file_uri: &str) {
        for v in self.map.values_mut() {
            v.retain(|(uri, inst)| {
                let keep = uri != file_uri;
                if !keep
                    && let Some(count) = self.name_counts.get_mut(&inst.name) {
                        *count -= 1;
                        if *count == 0 {
                            self.name_counts.remove(&inst.name);
                        }
                    }
                keep
            });
        }
        self.map.retain(|_, v| !v.is_empty());
    }
}

// ── Path matching ─────────────────────────────────────────────────────────────

/// True if `pat` occurs in `dir` as a whole path segment (or run of segments),
/// e.g. `gfx/models` is contained in `dlc/dlc022/gfx/models/units`. Mirrors the
/// validation side (`find_type_by_path_and_key` → `path_contains_segment`) so a
/// file is INDEXED by the same type that VALIDATES it. A bare `starts_with` would
/// miss base-game content nested under `dlc/<id>/…`, leaving its instances
/// unindexed while the referencing files still validate (false CW500s).
fn dir_contains_segment(haystack: &str, needle: &str) -> bool {
    if needle.is_empty() {
        return true;
    }
    let mut start = 0;
    while let Some(pos) = haystack[start..].find(needle) {
        let abs = start + pos;
        let left_ok = abs == 0 || haystack.as_bytes().get(abs - 1) == Some(&b'/');
        let right = abs + needle.len();
        let right_ok = right == haystack.len() || haystack.as_bytes().get(right) == Some(&b'/');
        if left_ok && right_ok {
            return true;
        }
        start = abs + 1;
        if start >= haystack.len() {
            break;
        }
    }
    false
}

/// Returns true when `logical_path` (e.g. `"events/my_events.txt"`) is covered
/// by `path_options`. The directory must equal the pattern when `path_strict`,
/// else contain it as a path segment (so base-game content nested under
/// `dlc/<id>/…` is indexed by the same type that validates it).
fn check_path_dir(opts: &PathOptions, logical_path: &str) -> bool {
    if opts.paths.is_empty() {
        return true;
    }

    // Normalise separators and split into directory and filename.
    let norm = logical_path.replace('\\', "/");
    let dir = match norm.rfind('/') {
        Some(idx) => &norm[..idx],
        None => "",
    };
    let dir_lower = dir.to_lowercase();

    if opts.paths_lower.is_empty() && !opts.paths.is_empty() {
        // Fallback for PathOptions built without reindex() (e.g. tests).
        for p in &opts.paths {
            let pat = p.replace('\\', "/");
            let pat = pat.trim_matches('/');
            let pat_lower = pat.to_lowercase();
            if opts.path_strict {
                if dir_lower == pat_lower {
                    return true;
                }
            } else if dir_contains_segment(&dir_lower, &pat_lower) {
                return true;
            }
        }
        return false;
    }

    for pat_lower in &opts.paths_lower {
        if opts.path_strict {
            if dir_lower == *pat_lower {
                return true;
            }
        } else if dir_contains_segment(&dir_lower, pat_lower) {
            return true;
        }
    }
    false
}

// ── skip_root_key helper ─────────────────────────────────────────────────────

fn skip_root_key_matches(srk: &SkipRootKey, key: &str) -> bool {
    match srk {
        SkipRootKey::SpecificKey(k) => k.eq_ignore_ascii_case(key),
        SkipRootKey::AnyKey => true,
        SkipRootKey::MultipleKeys(keys, should_match) => {
            keys.iter().any(|k| k.eq_ignore_ascii_case(key)) == *should_match
        }
    }
}

// ── type_key_filter helper ────────────────────────────────────────────────────

fn type_key_filter_matches(td: &TypeDefinition, key: &str) -> bool {
    match &td.type_key_filter {
        None => true,
        Some((keys, negate)) => {
            let hit = keys.iter().any(|k| k.eq_ignore_ascii_case(key));
            if *negate { !hit } else { hit }
        }
    }
}

// ── starts_with helper ────────────────────────────────────────────────────────

fn starts_with_matches(td: &TypeDefinition, key: &str) -> bool {
    match &td.starts_with {
        None => true,
        Some(prefix) => key.to_lowercase().starts_with(&prefix.to_lowercase()),
    }
}

// ── Collect instances from a single node under skip_root_key navigation ──────

/// Extract the instance name from a clause-typed element (honours `name_field`).
/// `children` is the list of children inside the clause.
fn instance_name_from_children(
    td: &TypeDefinition,
    node_key: &str,
    children: &[Child],
    arena: &Arena,
    table: &StringTable,
) -> Option<String> {
    match &td.name_field {
        None => Some(unquote(node_key).to_string()),
        Some(field_name) => {
            // The instance name comes from a child leaf whose key equals `name_field`.
            // Quoted values (e.g. spriteType `name = "GFX_x"`) are stored with their
            // quotes, so strip them to match unquoted references like `icon = GFX_x`.
            for child in children {
                if let Child::Leaf(li) = child {
                    let leaf = &arena.leaves[*li as usize];
                    let k = table.get_string(leaf.key.normal).unwrap_or_default();
                    if k.eq_ignore_ascii_case(field_name) {
                        let v = leaf_value_string(&leaf.value, table);
                        let v = unquote(&v);
                        if !v.is_empty() {
                            return Some(v.to_string());
                        }
                    }
                }
            }
            None
        }
    }
}

/// Recurse through skip_root_key layers, then collect matching instances.
/// `child` is a single top-level child (can be Node or Leaf-with-Clause).
fn collect_skip_root_child(
    td: &TypeDefinition,
    skip_stack: &[SkipRootKey],
    child: &Child,
    arena: &Arena,
    table: &StringTable,
    out: &mut Vec<TypeInstance>,
) {
    // Extract key, children-slice, and position from either Node or Leaf(Clause)
    let (key, clause_children, start_line, start_col): (String, &[Child], u32, u16) = match child {
        Child::Node(ni) => {
            let node = &arena.nodes[*ni as usize];
            let k = table.get_string(node.key.normal).unwrap_or_default();
            (
                k,
                node.children.as_slice(),
                node.pos.start.line,
                node.pos.start.col,
            )
        }
        Child::Leaf(li) => {
            let leaf = &arena.leaves[*li as usize];
            let k = table.get_string(leaf.key.normal).unwrap_or_default();
            match &leaf.value {
                Value::Clause(ch) => (k, ch.as_slice(), leaf.pos.start.line, leaf.pos.start.col),
                _ => return, // not a clause leaf — skip
            }
        }
        _ => return,
    };

    match skip_stack {
        [] => {
            // We are at the instance node.
            if type_key_filter_matches(td, &key) && starts_with_matches(td, &key)
                && let Some(name) =
                    instance_name_from_children(td, &key, clause_children, arena, table)
                {
                    out.push(TypeInstance {
                        name,
                        location: SourceLocation {
                            line: start_line,
                            col: start_col,
                        },
                    });
                }
        }
        [head, tail @ ..] => {
            // Must match the skip-root layer; then descend into children.
            if skip_root_key_matches(head, &key) {
                for inner_child in clause_children {
                    collect_skip_root_child(td, tail, inner_child, arena, table, out);
                }
            }
        }
    }
}

/// Collect all type instances defined in `file` for the given `logical_path`.
///
/// Returns a map from type name → list of instances found in this file.
/// Collect type instances from AST nodes, applying skip_root_key navigation.
#[tracing::instrument(skip_all)]
pub fn collect_type_instances(
    ruleset: &RuleSet,
    file: &ParsedFile,
    logical_path: &str,
    table: &StringTable,
) -> HashMap<String, Vec<TypeInstance>> {
    let mut result: HashMap<String, Vec<TypeInstance>> = HashMap::new();

    for td in &ruleset.types {
        // Path filter (mirrors CheckPathDir)
        if !check_path_dir(&td.path_options, logical_path) {
            continue;
        }

        let mut instances: Vec<TypeInstance> = Vec::new();

        if td.type_per_file {
            // The file itself is the instance; the name is the file stem.
            let name = logical_path
                .rsplit('/')
                .next()
                .unwrap_or(logical_path)
                .trim_end_matches(".txt")
                .trim_end_matches(".gfx")
                .trim_end_matches(".gui")
                .to_string();
            instances.push(TypeInstance {
                name,
                location: SourceLocation { line: 1, col: 0 },
            });
        } else {
            // Walk top-level children of the file (both Node and Leaf-with-Clause)
            for child in &file.root_children {
                collect_skip_root_child(
                    td,
                    &td.skip_root_key,
                    child,
                    &file.arena,
                    table,
                    &mut instances,
                );
            }
        }

        if !instances.is_empty() {
            result.entry(td.name.clone()).or_default().extend(instances);
        }
    }

    result
}

// ══════════════════════════════════════════════════════════════════════════════
// Item 2 — Rule-driven variable / value_set collection
// ══════════════════════════════════════════════════════════════════════════════

/// A defined variable entry (either @-style or rule-driven value_set).
#[derive(Debug, Clone)]
pub struct DefinedVariable {
    pub name: String,
    pub namespace: Option<String>, // value_set namespace, if any
    pub location: SourceLocation,
}

/// Collect variable definitions from a file, using the ruleset's `values` table
/// for value_set namespaces and also collecting `@var` at-prefix variables.
///
/// Returns a map from `namespace` (or `"@"` for at-vars) → list of names.
pub fn collect_defined_variables(
    ruleset: &RuleSet,
    file: &ParsedFile,
    table: &StringTable,
) -> HashMap<String, Vec<DefinedVariable>> {
    let mut result: HashMap<String, Vec<DefinedVariable>> = HashMap::new();

    // Build a set of known value_set namespaces from the ruleset values table.
    let value_set_namespaces: HashSet<&str> =
        ruleset.values.iter().map(|(ns, _)| ns.as_str()).collect();

    collect_vars_recursive(
        &file.root_children,
        &file.arena,
        table,
        ruleset,
        &value_set_namespaces,
        &mut result,
    );

    result
}

// `ruleset` / `value_set_namespaces` are threaded for the rule-aware value_set
// collection that this @-prefix pass doesn't yet implement; kept so the
// signature is ready and the caller's API stays stable.
#[allow(clippy::only_used_in_recursion)]
fn collect_vars_recursive(
    children: &[Child],
    arena: &Arena,
    table: &StringTable,
    ruleset: &RuleSet,
    value_set_namespaces: &HashSet<&str>,
    out: &mut HashMap<String, Vec<DefinedVariable>>,
) {
    for child in children {
        match child {
            Child::Leaf(idx) => {
                let leaf = &arena.leaves[*idx as usize];
                let key = table.get_string(leaf.key.normal).unwrap_or_default();

                // @var = value  (classic at-prefix variable)
                if key.starts_with('@') {
                    out.entry("@".to_string())
                        .or_default()
                        .push(DefinedVariable {
                            name: key.clone(),
                            namespace: None,
                            location: SourceLocation {
                                line: leaf.pos.start.line,
                                col: leaf.pos.start.col,
                            },
                        });
                }

                // Recurse into clause values
                if let Value::Clause(ch) = &leaf.value {
                    collect_vars_recursive(ch, arena, table, ruleset, value_set_namespaces, out);
                }
            }
            Child::Node(idx) => {
                let node = &arena.nodes[*idx as usize];
                collect_vars_recursive(
                    &node.children,
                    arena,
                    table,
                    ruleset,
                    value_set_namespaces,
                    out,
                );
            }
            _ => {}
        }
    }
}

/// Collect variables using full rule-tree walking.
/// For each leaf where the rule field is `VariableSetField(ns)`, record the
/// variable name under namespace `ns`.
///
/// When `at_vars` is `Some`, those entries are used as the "@" namespace
/// instead of re-scanning the AST for `@`-prefix leaves (avoids a redundant
/// walk when the caller already collected them via the heuristic pass).
pub fn collect_defined_variables_from_rules(
    ruleset: &RuleSet,
    file: &ParsedFile,
    logical_path: &str,
    table: &StringTable,
    at_vars: Option<Vec<DefinedVariable>>,
) -> HashMap<String, Vec<DefinedVariable>> {
    let mut result: HashMap<String, Vec<DefinedVariable>> = HashMap::new();

    match at_vars {
        Some(vars) if !vars.is_empty() => {
            result.insert("@".to_string(), vars);
        }
        _ => {
            collect_at_vars(&file.root_children, &file.arena, table, &mut result);
        }
    }

    // Walk type instances (path-filtered) and scan their rules for VariableSetField
    for td in &ruleset.types {
        if !check_path_dir(&td.path_options, logical_path) {
            continue;
        }
        // Find the TypeRule for this typedef in root_rules
        for root_rule in &ruleset.root_rules {
            if let cwtools_rules::rules_types::RootRule::TypeRule(name, (rule_type, _opts)) =
                root_rule
            {
                if name != &td.name {
                    continue;
                }
                if let RuleType::NodeRule { rules, .. } = rule_type {
                    // Scan all file children against these rules
                    for child in &file.root_children {
                        if let Child::Node(ni) = child {
                            scan_node_for_varset(
                                *ni as usize,
                                &file.arena,
                                table,
                                rules,
                                &mut result,
                            );
                        }
                    }
                }
            }
        }
    }

    result
}

fn collect_at_vars(
    children: &[Child],
    arena: &Arena,
    table: &StringTable,
    out: &mut HashMap<String, Vec<DefinedVariable>>,
) {
    for child in children {
        match child {
            Child::Leaf(idx) => {
                let leaf = &arena.leaves[*idx as usize];
                let key = table.get_string(leaf.key.normal).unwrap_or_default();
                if key.starts_with('@') {
                    out.entry("@".to_string())
                        .or_default()
                        .push(DefinedVariable {
                            name: key.clone(),
                            namespace: None,
                            location: SourceLocation {
                                line: leaf.pos.start.line,
                                col: leaf.pos.start.col,
                            },
                        });
                }
                if let Value::Clause(ch) = &leaf.value {
                    collect_at_vars(ch, arena, table, out);
                }
            }
            Child::Node(idx) => {
                let node = &arena.nodes[*idx as usize];
                collect_at_vars(&node.children, arena, table, out);
            }
            _ => {}
        }
    }
}

fn scan_node_for_varset(
    node_idx: usize,
    arena: &Arena,
    table: &StringTable,
    rules: &[(
        cwtools_rules::rules_types::RuleType,
        cwtools_rules::rules_types::Options,
    )],
    out: &mut HashMap<String, Vec<DefinedVariable>>,
) {
    let node = &arena.nodes[node_idx];
    for child in &node.children {
        match child {
            Child::Leaf(li) => {
                let leaf = &arena.leaves[*li as usize];
                let key = table.get_string(leaf.key.normal).unwrap_or_default();
                let val = leaf_value_string(&leaf.value, table);
                for (rule_type, _opts) in rules {
                    match rule_type {
                        RuleType::LeafRule {
                            left: NewField::VariableSetField(ns),
                            ..
                        } => {
                            // Key is the defined name
                            out.entry(ns.clone()).or_default().push(DefinedVariable {
                                name: key.clone(),
                                namespace: Some(ns.clone()),
                                location: SourceLocation {
                                    line: leaf.pos.start.line,
                                    col: leaf.pos.start.col,
                                },
                            });
                        }
                        RuleType::LeafRule {
                            right: NewField::VariableSetField(ns),
                            ..
                        }
                            // Value is the defined name
                            if !val.is_empty() => {
                                out.entry(ns.clone()).or_default().push(DefinedVariable {
                                    name: val.clone(),
                                    namespace: Some(ns.clone()),
                                    location: SourceLocation {
                                        line: leaf.pos.start.line,
                                        col: leaf.pos.start.col,
                                    },
                                });
                            }
                        _ => {}
                    }
                }
            }
            Child::Node(ni) => {
                // recurse
                for (rule_type, _) in rules {
                    if let RuleType::NodeRule { rules: inner, .. } = rule_type {
                        scan_node_for_varset(*ni as usize, arena, table, inner, out);
                    }
                }
            }
            _ => {}
        }
    }
}

/// The set of effect/trigger names that DEFINE a `value_set[variable]` (e.g.
/// `set_variable`, `set_temp_variable`, `add_to_variable`). An alias qualifies
/// when its rule body contains a `VariableSetField("variable")`. Config-driven,
/// so it tracks whatever the game's `.cwt` declares rather than a hardcoded list.
pub fn variable_defining_effects(ruleset: &RuleSet) -> HashSet<String> {
    fn is_var_set(f: &NewField) -> bool {
        matches!(f, NewField::VariableSetField(ns) if ns == "variable")
    }
    fn defines(rule: &RuleType) -> bool {
        match rule {
            RuleType::LeafRule { left, right } => is_var_set(left) || is_var_set(right),
            RuleType::LeafValueRule { right } => is_var_set(right),
            RuleType::NodeRule { left, rules } => {
                is_var_set(left) || rules.iter().any(|(rt, _)| defines(rt))
            }
            RuleType::ValueClauseRule { rules } | RuleType::SubtypeRule { rules, .. } => {
                rules.iter().any(|(rt, _)| defines(rt))
            }
        }
    }
    let mut out = HashSet::new();
    for (name, (rule, _opts)) in &ruleset.aliases {
        if let Some((cat, key)) = name.split_once(':')
            && (cat == "effect" || cat == "trigger")
            && defines(rule)
        {
            out.insert(key.to_ascii_lowercase());
        }
    }
    out
}

/// Scan a file's AST for variable definitions and push each raw name into `out`.
/// For every block whose key is a variable-defining effect, the defined name is
/// the value of an explicit `var`/`variable` child, or — in the shorthand form
/// `set_variable = { my_var = 3 }` — the inner assignment's key. The rule-driven
/// [`collect_defined_variables_from_rules`] misses these because they live inside
/// `alias[effect]` expansions the type-rule walk never reaches; this direct scan
/// does not depend on rule matching.
pub fn collect_set_variable_names(
    file: &ParsedFile,
    table: &StringTable,
    effects: &HashSet<String>,
    out: &mut Vec<String>,
) {
    fn extract(children: &[Child], arena: &Arena, table: &StringTable, out: &mut Vec<String>) {
        // Explicit form: a `var`/`variable` child holds the name as its value.
        let mut explicit = false;
        for child in children {
            if let Child::Leaf(li) = child {
                let leaf = &arena.leaves[*li as usize];
                let key = table.get_string(leaf.key.normal).unwrap_or_default();
                if key.eq_ignore_ascii_case("var") || key.eq_ignore_ascii_case("variable") {
                    let v = leaf_value_string(&leaf.value, table);
                    if !v.is_empty() {
                        out.push(v);
                    }
                    explicit = true;
                }
            }
        }
        if explicit {
            return;
        }
        // Shorthand form: the inner assignment key is the variable name.
        for child in children {
            let key = match child {
                Child::Leaf(li) => table
                    .get_string(arena.leaves[*li as usize].key.normal)
                    .unwrap_or_default(),
                Child::Node(ni) => table
                    .get_string(arena.nodes[*ni as usize].key.normal)
                    .unwrap_or_default(),
                _ => continue,
            };
            if !matches!(
                key.to_ascii_lowercase().as_str(),
                "value" | "tooltip" | "var" | "variable" | "amount"
            ) {
                out.push(key);
            }
        }
    }

    fn walk(
        children: &[Child],
        arena: &Arena,
        table: &StringTable,
        effects: &HashSet<String>,
        out: &mut Vec<String>,
    ) {
        for child in children {
            match child {
                Child::Leaf(li) => {
                    let leaf = &arena.leaves[*li as usize];
                    if let Value::Clause(ch) = &leaf.value {
                        let key = table.get_string(leaf.key.normal).unwrap_or_default();
                        if effects.contains(&key.to_ascii_lowercase()) {
                            extract(ch, arena, table, out);
                        }
                        walk(ch, arena, table, effects, out);
                    }
                }
                Child::Node(ni) => {
                    let node = &arena.nodes[*ni as usize];
                    let key = table.get_string(node.key.normal).unwrap_or_default();
                    if effects.contains(&key.to_ascii_lowercase()) {
                        extract(&node.children, arena, table, out);
                    }
                    walk(&node.children, arena, table, effects, out);
                }
                _ => {}
            }
        }
    }

    walk(&file.root_children, &file.arena, table, effects, out);
}

// ══════════════════════════════════════════════════════════════════════════════
// Item 3 — Saved event targets with position
// ══════════════════════════════════════════════════════════════════════════════

/// A saved event target and where it was defined.
#[derive(Debug, Clone)]
pub struct SavedEventTarget {
    pub name: String,
    pub location: SourceLocation,
    /// true = global (save_global_event_target_as)
    pub is_global: bool,
}

/// Collect `save_event_target_as` / `save_global_event_target_as` from a file.
pub fn collect_saved_event_targets(
    file: &ParsedFile,
    table: &StringTable,
) -> Vec<SavedEventTarget> {
    let mut out = Vec::new();
    collect_event_targets_rec(&file.root_children, &file.arena, table, &mut out);
    out
}

fn collect_event_targets_rec(
    children: &[Child],
    arena: &Arena,
    table: &StringTable,
    out: &mut Vec<SavedEventTarget>,
) {
    for child in children {
        match child {
            Child::Leaf(idx) => {
                let leaf = &arena.leaves[*idx as usize];
                let key = table.get_string(leaf.key.normal).unwrap_or_default();
                let val = leaf_value_string(&leaf.value, table);

                if (key == "save_event_target_as" || key == "save_global_event_target_as")
                    && !val.is_empty() {
                        out.push(SavedEventTarget {
                            name: val,
                            location: SourceLocation {
                                line: leaf.pos.start.line,
                                col: leaf.pos.start.col,
                            },
                            is_global: key == "save_global_event_target_as",
                        });
                    }

                if let Value::Clause(ch) = &leaf.value {
                    collect_event_targets_rec(ch, arena, table, out);
                }
            }
            Child::Node(idx) => {
                let node = &arena.nodes[*idx as usize];
                collect_event_targets_rec(&node.children, arena, table, out);
            }
            _ => {}
        }
    }
}

// ══════════════════════════════════════════════════════════════════════════════
// Item 4 — Position query for hover / goto-definition
// ══════════════════════════════════════════════════════════════════════════════

/// A hint about what kind of reference a leaf's value or key represents.
/// Used by the LSP for hover text and goto-definition.
///
/// NOTE: This is a best-effort classification.  The full rule-tree walker is not
/// yet implemented, so `TypeRef` / `EnumRef` are only returned when the leaf's
/// value literally matches a `TypeField` or `ValueField(Enum)` rule at depth 1
/// of the matched typedef rules.  Deeper nesting (e.g. inside `if = { … }`) is
/// not yet resolved — `Unknown` is returned in that case.
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
    /// Classification was not possible with current rule depth.
    Unknown,
}

/// The element at the cursor position plus any rule-derived hint.
#[derive(Debug, Clone)]
pub struct PositionInfo {
    pub location: SourceLocation,
    pub element: PositionElement,
    pub hint: ReferenceHint,
}

/// Which kind of AST element is at the cursor.
#[derive(Debug, Clone)]
pub enum PositionElement {
    /// A `key = { … }` node; cursor is on the key.
    Node { key: String },
    /// A `key = value` leaf.
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
            Child::Node(idx) => {
                let node = &arena.nodes[*idx as usize];
                if pos_in_range(target, &node.pos) {
                    if let Some(inner) =
                        find_element_in_children(&node.children, arena, target, table)
                    {
                        return Some(inner);
                    }
                    let key = table.get_string(node.key.normal).unwrap_or_default();
                    return Some(PositionElement::Node { key });
                }
            }
            Child::Leaf(idx) => {
                let leaf = &arena.leaves[*idx as usize];
                if pos_in_range(target, &leaf.pos) {
                    let key = table.get_string(leaf.key.normal).unwrap_or_default();
                    let value = leaf_value_string(&leaf.value, table);
                    if let Value::Clause(ch) = &leaf.value
                        && let Some(inner) = find_element_in_children(ch, arena, target, table) {
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

/// Find the element at `(line, col)` in `file` and, when possible, classify its
/// rule-field type using `ruleset`.
///
/// Limitation: rule matching is only done for leaves at the top or one level
/// deep.  Aliases and nested nodes are not fully walked.
pub fn info_at_position(
    file: &ParsedFile,
    line: u32,
    col: u16,
    ruleset: &RuleSet,
    logical_path: &str,
    table: &StringTable,
) -> Option<PositionInfo> {
    let target = cwtools_parser::ast::SourcePos { line, col };

    // Walk all children and find the deepest element that contains the position.
    find_pos_in_children(
        &file.root_children,
        &file.arena,
        &target,
        table,
        ruleset,
        logical_path,
    )
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

fn find_pos_in_children(
    children: &[Child],
    arena: &Arena,
    target: &cwtools_parser::ast::SourcePos,
    table: &StringTable,
    ruleset: &RuleSet,
    logical_path: &str,
) -> Option<PositionInfo> {
    for child in children {
        match child {
            Child::Node(idx) => {
                let node = &arena.nodes[*idx as usize];
                if pos_in_range(target, &node.pos) {
                    // Try children first (deeper match wins)
                    if let Some(inner) = find_pos_in_children(
                        &node.children,
                        arena,
                        target,
                        table,
                        ruleset,
                        logical_path,
                    ) {
                        return Some(inner);
                    }
                    // Cursor is on the node key itself
                    let key = table.get_string(node.key.normal).unwrap_or_default();
                    return Some(PositionInfo {
                        location: SourceLocation {
                            line: node.pos.start.line,
                            col: node.pos.start.col,
                        },
                        element: PositionElement::Node { key: key.clone() },
                        hint: classify_node_key(&key, ruleset, logical_path),
                    });
                }
            }
            Child::Leaf(idx) => {
                let leaf = &arena.leaves[*idx as usize];
                if pos_in_range(target, &leaf.pos) {
                    let key = table.get_string(leaf.key.normal).unwrap_or_default();
                    let value = leaf_value_string(&leaf.value, table);
                    // If value is a clause, try to recurse into it
                    if let Value::Clause(ch) = &leaf.value
                        && let Some(inner) =
                            find_pos_in_children(ch, arena, target, table, ruleset, logical_path)
                        {
                            return Some(inner);
                        }
                    let hint = classify_leaf_value(&key, &value, ruleset, logical_path, table);
                    return Some(PositionInfo {
                        location: SourceLocation {
                            line: leaf.pos.start.line,
                            col: leaf.pos.start.col,
                        },
                        element: PositionElement::Leaf {
                            key: key.clone(),
                            value: value.clone(),
                        },
                        hint,
                    });
                }
            }
            Child::LeafValue(idx) => {
                let lv = &arena.leaf_values[*idx as usize];
                if pos_in_range(target, &lv.pos) {
                    let value = leaf_value_string(&lv.value, table);
                    return Some(PositionInfo {
                        location: SourceLocation {
                            line: lv.pos.start.line,
                            col: lv.pos.start.col,
                        },
                        element: PositionElement::LeafValue {
                            value: value.clone(),
                        },
                        hint: ReferenceHint::Unknown,
                    });
                }
            }
            _ => {}
        }
    }
    None
}

/// Best-effort: classify a node key using path-matched type definitions.
fn classify_node_key(key: &str, ruleset: &RuleSet, logical_path: &str) -> ReferenceHint {
    for td in &ruleset.types {
        if !check_path_dir(&td.path_options, logical_path) {
            continue;
        }
        if type_key_filter_matches(td, key) {
            return ReferenceHint::TypeRef {
                type_name: td.name.clone(),
                value: key.to_string(),
            };
        }
    }
    ReferenceHint::Unknown
}

/// Best-effort: classify a leaf value using ruleset root_rules.
fn classify_leaf_value(
    key: &str,
    value: &str,
    ruleset: &RuleSet,
    logical_path: &str,
    _table: &StringTable,
) -> ReferenceHint {
    // Check if value looks like a type reference (<type>)
    if value.starts_with('<') && value.ends_with('>') {
        let inner = &value[1..value.len() - 1];
        return ReferenceHint::TypeRef {
            type_name: inner.to_string(),
            value: inner.to_string(),
        };
    }

    // Walk root rules shallowly looking for a LeafRule whose left matches `key`
    for root_rule in &ruleset.root_rules {
        let (name, (rule_type, _opts)) = match root_rule {
            cwtools_rules::rules_types::RootRule::TypeRule(n, r) => (n, r),
            cwtools_rules::rules_types::RootRule::AliasRule(n, r) => (n, r),
            cwtools_rules::rules_types::RootRule::SingleAliasRule(n, r) => (n, r),
        };

        let rules = match rule_type {
            RuleType::NodeRule { rules, .. } => rules.as_slice(),
            _ => continue,
        };

        // Only try path-matching type rules
        if let cwtools_rules::rules_types::RootRule::TypeRule(..) = root_rule
            && let Some(&idx) = ruleset.type_by_name.get(name) {
                let td = &ruleset.types[idx];
                if !check_path_dir(&td.path_options, logical_path) {
                    continue;
                }
            }

        for (inner_rule, _) in rules {
            if let RuleType::LeafRule { left, right } = inner_rule {
                let left_matches = match left {
                    NewField::SpecificField(k) => k.eq_ignore_ascii_case(key),
                    _ => false,
                };
                if !left_matches {
                    continue;
                }
                match right {
                    NewField::TypeField(cwtools_rules::rules_types::TypeType::Simple(t)) => {
                        return ReferenceHint::TypeRef {
                            type_name: t.clone(),
                            value: value.to_string(),
                        };
                    }
                    NewField::ValueField(cwtools_rules::rules_types::ValueType::Enum(e)) => {
                        return ReferenceHint::EnumRef {
                            enum_name: e.clone(),
                            value: value.to_string(),
                        };
                    }
                    NewField::LocalisationField { .. } => {
                        return ReferenceHint::LocRef {
                            key: value.to_string(),
                        };
                    }
                    NewField::FilepathField { .. } => {
                        return ReferenceHint::FileRef {
                            path: value.to_string(),
                        };
                    }
                    _ => {}
                }
            }
        }
    }

    ReferenceHint::Unknown
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
}

/// InfoService holds computed data for all files in a workspace.
pub struct InfoService {
    pub files: HashMap<String, FileInfo>,
    /// Union of all type definitions across files (rule-driven + heuristic).
    pub all_type_defs: HashMap<String, Vec<(String, SourceLocation)>>,
    /// Cross-file type-instance index.
    pub type_index: TypeIndex,
    pub all_event_targets: HashSet<String>,
    pub all_variables: HashSet<String>,
    pub all_inline_scripts: HashSet<String>,
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
            all_event_targets: HashSet::new(),
            all_variables: HashSet::new(),
            all_inline_scripts: HashSet::new(),
        }
    }

    /// One-line size summary for profiling (counts only, not bytes).
    pub fn profile_summary(&self) -> String {
        let cross_file: usize = self.type_index.map.values().map(|v| v.len()).sum();
        format!(
            "info: {} files | type_index {} instances / {} types | {} vars | {} targets | {} type_defs",
            self.files.len(),
            cross_file,
            self.type_index.map.len(),
            self.all_variables.len(),
            self.all_event_targets.len(),
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
        let mut type_names: HashSet<String> = HashSet::new();
        for t in &ruleset.types {
            type_names.insert(t.name.clone());
        }
        for child in &ast.root_children {
            Self::index_child_heuristic(child, &ast.arena, table, &type_names, &mut info);
        }

        // ── Rule-driven: type-instance index ─────────────────────────────────
        // Move the instances straight into the cross-file index. We don't keep a
        // second per-file copy on `FileInfo` (that doubled ~190K instances on
        // MD); document-symbol derives a file's instances from the index instead.
        let instances = collect_type_instances(ruleset, ast, logical_path, table);
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
            })
            .collect();
        info.defined_variables_ns =
            collect_defined_variables_from_rules(ruleset, ast, logical_path, table, Some(at_vars));
        // Flatten non-@-var entries back into the legacy map for compat.
        for vars in info.defined_variables_ns.values() {
            for v in vars {
                if v.name.starts_with('@') {
                    info.defined_variables.insert(v.name.clone(), v.location);
                }
            }
        }

        // saved_event_targets_detailed is populated by index_child_heuristic
        // (it detects save_event_target_as / save_global_event_target_as).
        info.saved_event_targets = info
            .saved_event_targets_detailed
            .iter()
            .map(|e| e.name.clone())
            .collect();

        // ── Merge into global indexes ─────────────────────────────────────────
        for (type_name, locs) in &info.type_definitions {
            self.all_type_defs
                .entry(type_name.clone())
                .or_default()
                .extend(locs.iter().map(|l| (uri.to_string(), *l)));
        }
        self.all_event_targets
            .extend(info.saved_event_targets.iter().cloned());
        for vars in info.defined_variables_ns.values() {
            for v in vars {
                self.all_variables.insert(v.name.clone());
            }
        }
        self.all_inline_scripts
            .extend(info.inline_scripts.keys().cloned());

        self.files.insert(uri.to_string(), info);
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
                let still_exists = self
                    .files
                    .values()
                    .any(|f| f.saved_event_targets.contains(et));
                if !still_exists {
                    self.all_event_targets.remove(et);
                }
            }
            // Variables
            for var in info.defined_variables.keys() {
                let still_exists = self
                    .files
                    .values()
                    .any(|f| f.defined_variables.contains_key(var));
                if !still_exists {
                    self.all_variables.remove(var);
                }
            }
            // Inline scripts
            for script in info.inline_scripts.keys() {
                let still_exists = self
                    .files
                    .values()
                    .any(|f| f.inline_scripts.contains_key(script));
                if !still_exists {
                    self.all_inline_scripts.remove(script);
                }
            }
        }
    }

    /// Find all heuristic definitions of a given symbol name.
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

    // ── Heuristic child walker (unchanged from original) ─────────────────────

    fn index_child_heuristic(
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

                info.top_level_keys.push((
                    key.clone(),
                    SourceLocation {
                        line: node.pos.start.line,
                        col: node.pos.start.col,
                    },
                ));

                if type_names.contains(&key) {
                    info.type_definitions
                        .entry(key.clone())
                        .or_default()
                        .push(SourceLocation {
                            line: node.pos.start.line,
                            col: node.pos.start.col,
                        });
                }

                for c in &node.children {
                    Self::index_child_heuristic(c, arena, table, type_names, info);
                }
            }
            Child::Leaf(idx) => {
                let leaf = &arena.leaves[*idx as usize];
                let key = table.get_string(leaf.key.normal).unwrap_or_default();

                if let Value::Clause(_) = &leaf.value {
                    info.top_level_keys.push((
                        key.clone(),
                        SourceLocation {
                            line: leaf.pos.start.line,
                            col: leaf.pos.start.col,
                        },
                    ));

                    if type_names.contains(&key) {
                        info.type_definitions.entry(key.clone()).or_default().push(
                            SourceLocation {
                                line: leaf.pos.start.line,
                                col: leaf.pos.start.col,
                            },
                        );
                    }
                }

                let value_str = leaf_value_string(&leaf.value, table);

                if key.starts_with('@') {
                    info.defined_variables.insert(
                        key.clone(),
                        SourceLocation {
                            line: leaf.pos.start.line,
                            col: leaf.pos.start.col,
                        },
                    );
                }

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

                if let Value::Clause(children) = &leaf.value {
                    for c in children {
                        Self::index_child_heuristic(c, arena, table, type_names, info);
                    }
                }

                if key.starts_with("event_target:") {
                    let target = key.strip_prefix("event_target:").unwrap_or("");
                    if !target.is_empty() {
                        info.saved_event_targets.insert(target.to_string());
                    }
                }

                if (key == "save_event_target_as" || key == "save_global_event_target_as")
                    && !value_str.is_empty() {
                        info.saved_event_targets_detailed.push(SavedEventTarget {
                            name: value_str.clone(),
                            location: SourceLocation {
                                line: leaf.pos.start.line,
                                col: leaf.pos.start.col,
                            },
                            is_global: key == "save_global_event_target_as",
                        });
                    }

                if key == "inline_script"
                    && let Value::Clause(children) = &leaf.value {
                        for c in children {
                            if let Child::Leaf(script_idx) = c {
                                let script_leaf = &arena.leaves[*script_idx as usize];
                                let script_key =
                                    table.get_string(script_leaf.key.normal).unwrap_or_default();
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

// ══════════════════════════════════════════════════════════════════════════════
// Tests
// ══════════════════════════════════════════════════════════════════════════════

#[cfg(test)]
mod tests {
    use super::*;
    use cwtools_parser::parser::parse_string;
    use cwtools_rules::rules_types::{PathOptions, TypeDefinition};

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
        let type_names = HashSet::new();
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

    // ── Item 2 — defined variables ────────────────────────────────────────────

    #[test]
    fn test_at_vars_collected() {
        let source = "@min_manpower = 100\n@max_tech = 5";
        let table = StringTable::new();
        let parsed = parse_string(source, &table).unwrap();

        let rs = RuleSet::new();
        let vars = collect_defined_variables(&rs, &parsed, &table);
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
        let targets = collect_saved_event_targets(&parsed, &table);

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
    fn test_info_at_position_leaf() {
        let source = "foo = bar\n";
        let table = StringTable::new();
        let parsed = parse_string(source, &table).unwrap();
        let rs = RuleSet::new();

        let info = info_at_position(&parsed, 1, 6, &rs, "test/a.txt", &table);
        assert!(info.is_some(), "should find element at (1,6)");
        let info = info.unwrap();
        match &info.element {
            PositionElement::Leaf { key, value } => {
                assert_eq!(key, "foo");
                assert_eq!(value, "bar");
            }
            other => panic!("expected Leaf, got {:?}", other),
        }
    }

    #[test]
    fn test_info_at_position_type_ref_angle() {
        let source = "ethos = <my_ethos>\n";
        let table = StringTable::new();
        let parsed = parse_string(source, &table).unwrap();
        let rs = RuleSet::new();

        let info = info_at_position(&parsed, 1, 8, &rs, "test/a.txt", &table);
        let info = info.expect("should find element");
        match &info.hint {
            ReferenceHint::TypeRef { type_name, .. } => assert_eq!(type_name, "my_ethos"),
            other => panic!("expected TypeRef, got {:?}", other),
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
}
