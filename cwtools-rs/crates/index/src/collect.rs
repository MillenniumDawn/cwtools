//! Collecting type instances from parsed files, and building a [`TypeIndex`]
//! from discovered files.

use cwtools_parser::ast::{Arena, Child, ParsedFile};
use cwtools_rules::rules_types::{RuleSet, SkipRootKey, TypeDefinition};
use cwtools_string_table::string_table::StringTable;
use std::collections::{HashMap, HashSet};

use crate::dynamic_values;
use crate::{
    NormalizedPath, SourceLocation, TypeIndex, TypeInstance, check_path_dir_norm,
    collect_set_variable_names, leaf_value_string, unquote,
};

/// Does this `skip_root_key` rule match `key`? Case-insensitive (matching the
/// engine), and honours the `should_match` negation flag on `MultipleKeys`.
/// Shared with the validator (cwtools_validation::resolve) so indexing and
/// validation agree on which root keys to skip.
pub fn skip_root_key_matches(srk: &SkipRootKey, key: &str) -> bool {
    match srk {
        SkipRootKey::SpecificKey(k) => k.eq_ignore_ascii_case(key),
        SkipRootKey::AnyKey => true,
        SkipRootKey::MultipleKeys(keys, match_kind) => {
            keys.iter().any(|k| k.eq_ignore_ascii_case(key)) == match_kind.is_equals()
        }
    }
}

fn type_key_filter_matches(td: &TypeDefinition, key: &str) -> bool {
    match &td.type_key_filter {
        None => true,
        Some((keys, negate)) => {
            let hit = keys.iter().any(|k| k.eq_ignore_ascii_case(key));
            if *negate { !hit } else { hit }
        }
    }
}

fn starts_with_matches(td: &TypeDefinition, key: &str) -> bool {
    match &td.starts_with {
        None => true,
        // Paradox keys/prefixes are ASCII identifiers; an ASCII case-insensitive
        // prefix test matches `to_lowercase().starts_with(to_lowercase())` without
        // allocating a lowercased copy of either string per call.
        Some(prefix) => {
            key.len() >= prefix.len()
                && key.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
        }
    }
}

// F# `type_key_prefix` compares the type's prefix against a node's own KeyPrefix
// token from Imperator-style prefixed nodes (`prefix key = { .. }`), which this
// AST doesn't model. We take the conservative reading: the key must carry the
// declared prefix (ASCII case-insensitive, like `starts_with`), name unchanged.
fn key_prefix_matches(td: &TypeDefinition, key: &str) -> bool {
    match &td.key_prefix {
        None => true,
        Some(prefix) => {
            key.len() >= prefix.len()
                && key.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
        }
    }
}

/// The field name an instance's `## primary` localisation is taken from, when it
/// is an explicit field (e.g. an event's `title = title` → `Some("title")`).
/// `None` for name-derived (`$`-pattern) primary keys or types with no primary
/// localisation — those need nothing captured at index time.
fn primary_explicit_loc_field(td: &TypeDefinition) -> Option<&str> {
    td.localisation
        .iter()
        .find(|l| l.primary && l.explicit_field.is_some())
        .and_then(|l| l.explicit_field.as_deref())
}

/// Read the value of the child leaf whose key equals `field_name` (case-
/// insensitive), unquoted. The shared lookup behind `name_field` and primary
/// explicit-field localisation.
fn field_value_from_children(
    field_name: &str,
    children: &[Child],
    arena: &Arena,
    table: &StringTable,
) -> Option<String> {
    for child in children {
        if let Child::Leaf(li) = child {
            let leaf = &arena.leaves[*li as usize];
            let matches = table
                .with_string(leaf.key.normal, |k| k.eq_ignore_ascii_case(field_name))
                .unwrap_or(false);
            if matches {
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
        // The instance name comes from a child leaf whose key equals `name_field`.
        // Quoted values (e.g. spriteType `name = "GFX_x"`) are stored with their
        // quotes, so strip them to match unquoted references like `icon = GFX_x`.
        Some(field_name) => field_value_from_children(field_name, children, arena, table),
    }
}

/// Recurse through skip_root_key layers, then visit each matching instance node.
/// `child` is a single top-level child (must be a keyed clause); at the instance
/// node the key must pass the `type_key_filter` + `starts_with` gates and yield a
/// name, then `visit` is invoked with the resolved name (owned), the node's own
/// key, its clause children, and its location. The single skip-root-key navigator
/// behind both [`collect_type_instances`] (builds `TypeInstance`s) and
/// [`for_each_instance_node`] (invokes a caller callback); they differ only in
/// what `visit` does at the leaf.
fn walk_skip_root_child<V>(
    td: &TypeDefinition,
    skip_stack: &[SkipRootKey],
    child: &Child,
    arena: &Arena,
    table: &StringTable,
    visit: &mut V,
) where
    V: FnMut(&TypeDefinition, String, &str, &[Child], SourceLocation),
{
    let Some(kc) = arena.keyed_clause(child) else {
        return; // not a keyed clause — skip
    };
    let clause_children = kc.children;
    let location = SourceLocation {
        line: kc.pos.start.line,
        col: kc.pos.start.col,
        end: (kc.pos.end.line, kc.pos.end.col),
    };

    table.with_string(kc.key.normal, |key| match skip_stack {
        [] => {
            // We are at the instance node.
            if type_key_filter_matches(td, key)
                && starts_with_matches(td, key)
                && key_prefix_matches(td, key)
                && let Some(name) =
                    instance_name_from_children(td, key, clause_children, arena, table)
            {
                visit(td, name, key, clause_children, location);
            }
        }
        [head, tail @ ..] => {
            // Must match the skip-root layer; then descend into children.
            if skip_root_key_matches(head, key) {
                for inner_child in clause_children {
                    walk_skip_root_child(td, tail, inner_child, arena, table, visit);
                }
            }
        }
    });
}

/// A function that derives a file's subtype-qualified instances
/// (`"type.subtype" -> [instances]`) from its parsed AST. Implemented in the
/// `validation` crate (it needs the subtype matcher) and injected into
/// [`index_discovered_files`] so the index crate stays free of a validation
/// dependency.
pub type SubtypeCollector =
    fn(&RuleSet, &ParsedFile, &str, &StringTable) -> HashMap<String, Vec<TypeInstance>>;

/// Visit every type *instance node* in `file` whose type declares subtypes,
/// invoking `f` with the matched type, the resolved instance name, the node's own
/// key, and the node's clause children. Mirrors [`collect_type_instances`]'s
/// navigation (path filter + skip_root_key + type_key_filter + name_field) but
/// exposes the node body so a caller can compute per-instance facts (e.g. which
/// subtypes are active).
///
/// Types with no subtypes are skipped: the sole purpose here is computing
/// subtype membership, so walking (and resolving the name of) instances that can
/// have no subtype facts is wasted work — and most types declare no subtypes, so
/// the skip avoids a second full instance navigation across the corpus on top of
/// [`collect_type_instances`].
///
/// `type_per_file` types are also skipped — the file *is* the instance, so there
/// is no node body to inspect for subtypes.
pub fn for_each_instance_node<F>(
    ruleset: &RuleSet,
    file: &ParsedFile,
    logical_path: &str,
    table: &StringTable,
    f: &mut F,
) where
    F: FnMut(&TypeDefinition, &str, &str, &[Child], SourceLocation),
{
    let np = NormalizedPath::new(logical_path);
    for td in &ruleset.types {
        if td.type_per_file || td.subtypes.is_empty() || !check_path_dir_norm(&td.path_options, &np)
        {
            continue;
        }
        let mut visit =
            |td: &TypeDefinition, name: String, key: &str, children: &[Child], location| {
                f(td, &name, key, children, location);
            };
        for child in &file.root_children {
            walk_skip_root_child(td, &td.skip_root_key, child, &file.arena, table, &mut visit);
        }
    }
}

/// Hash one exported symbol's identity, with separators so distinct parts can't
/// run together (`a|bc` vs `ab|c`).
pub fn mix_export_symbol(parts: &[&str]) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    for p in parts {
        p.hash(&mut h);
        0xffu8.hash(&mut h);
    }
    h.finish()
}

/// Order-independent hash of a file's exported type instances, computed from the
/// per-file `type -> instances` map produced at index time. Mirrors the symbol
/// mixing used for variables/event targets in
/// [`InfoService::export_fingerprint`].
pub fn hash_instance_exports(per_type: &HashMap<String, Vec<TypeInstance>>) -> u64 {
    let mut acc: u64 = 0;
    for (ty, instances) in per_type {
        for inst in instances {
            acc = acc.wrapping_add(mix_export_symbol(&["t", ty, &inst.name]));
        }
    }
    acc
}

/// Collect all type instances defined in `file` for the given `logical_path`,
/// applying skip_root_key navigation. Returns a map from type name to the list
/// of instances found in this file.
#[tracing::instrument(skip_all)]
pub fn collect_type_instances(
    ruleset: &RuleSet,
    file: &ParsedFile,
    logical_path: &str,
    table: &StringTable,
) -> HashMap<String, Vec<TypeInstance>> {
    let mut result: HashMap<String, Vec<TypeInstance>> = HashMap::new();

    let np = NormalizedPath::new(logical_path);
    for td in &ruleset.types {
        // Path filter (mirrors CheckPathDir)
        if !check_path_dir_norm(&td.path_options, &np) {
            continue;
        }

        let mut instances: Vec<TypeInstance> = Vec::new();

        if td.type_per_file {
            // The file itself is the instance; the name is the file stem.
            // Normalise separators first: the LSP on Windows derives logical
            // paths with backslashes (`check_path_dir` already normalises, this
            // must too), else the stem becomes the whole path and references
            // like `load_oob = "MY_OOB"` flag as false positives.
            let norm = logical_path.replace('\\', "/");
            let name = norm
                .rsplit('/')
                .next()
                .unwrap_or(norm.as_str())
                .trim_end_matches(".txt")
                .trim_end_matches(".gfx")
                .trim_end_matches(".gui")
                .to_string();
            instances.push(TypeInstance {
                name,
                // The file itself is the instance: no single node span is the
                // definition, so a deliberately degenerate span marks it rather
                // than borrowing some root child's range (root_children IS in
                // scope, but any node's range would be a fabrication here).
                location: SourceLocation {
                    line: 1,
                    col: 0,
                    end: (1, 0),
                },
                // type_per_file types have no node body to read a field from.
                primary_loc_key: None,
            });
        } else {
            // Walk the file's top-level keyed clauses.
            let arena = &file.arena;
            let mut visit = |td: &TypeDefinition,
                             name: String,
                             _key: &str,
                             clause_children: &[Child],
                             location| {
                // Capture the explicit-field primary loc key (e.g. an event's
                // `title`) so hover can resolve the localised title cross-file.
                let primary_loc_key = primary_explicit_loc_field(td).and_then(|field| {
                    field_value_from_children(field, clause_children, arena, table)
                });
                instances.push(TypeInstance {
                    name,
                    location,
                    primary_loc_key,
                });
            };
            for child in &file.root_children {
                walk_skip_root_child(td, &td.skip_root_key, child, arena, table, &mut visit);
            }
        }

        if !instances.is_empty() {
            result.entry(td.name.clone()).or_default().extend(instances);
        }
    }

    result
}

/// Build a [`TypeIndex`] from already-discovered+parsed files. Shared by the CLI
/// (`index_game_dir`) and LSP (`index_vanilla_dir`) base-game indexing paths so
/// the per-file merge loop lives in one place. Each file's AST is consumed in
/// place (no re-parse) and its type instances are stream-merged.
///
/// When `var_effects` is `Some(non_empty)`, base-game variable definitions are
/// also folded into `index.var_index` (so a mod referencing a vanilla variable
/// isn't flagged as unset, CW246). Pass `None` to skip variable collection.
///
/// When `subtype_collector` is `Some`, each file's subtype-qualified membership
/// (`"type.subtype" -> instances`) is also merged, so `<type.subtype>` references
/// into base-game content resolve. The collector lives in the `validation` crate
/// (it needs the subtype matcher); see [`SubtypeCollector`].
pub fn index_discovered_files(
    files: impl IntoIterator<Item = cwtools_file_manager::file_manager::ParsedFile>,
    ruleset: &RuleSet,
    table: &StringTable,
    var_effects: Option<&HashSet<String>>,
    subtype_collector: Option<SubtypeCollector>,
) -> TypeIndex {
    use rayon::prelude::*;

    let var_effects = var_effects.filter(|e| !e.is_empty());

    // Collect into a Vec so rayon can split it across threads. The Vec is then
    // consumed by into_par_iter() so we don't need Clone on the AST types.
    let files: Vec<cwtools_file_manager::file_manager::ParsedFile> = files.into_iter().collect();

    // Parallel collection: all collector functions take only &-borrows of the
    // shared ruleset/table, so each file's work is independent. into_par_iter()
    // on a Vec preserves input order in the output Vec after collect().
    type PerFileData = (
        String,                             // path
        HashMap<String, Vec<TypeInstance>>, // type instances
        Vec<String>,                        // variable names
        HashMap<String, Vec<String>>,       // complex enum values
        HashMap<String, Vec<String>>,       // value set members
    );
    let per_file: Vec<PerFileData> = files
        .into_par_iter()
        .map(|file| {
            let path = file.path.to_str().unwrap_or("").to_string();
            let pf = ParsedFile {
                arena: file.arena,
                root_children: file.root_children,
                errors: vec![],
            };
            let mut instances = collect_type_instances(ruleset, &pf, &file.logical_path, table);
            if let Some(collect_subtypes) = subtype_collector {
                for (k, v) in collect_subtypes(ruleset, &pf, &file.logical_path, table) {
                    instances.entry(k).or_default().extend(v);
                }
            }
            let mut var_names: Vec<String> = Vec::new();
            if let Some(effects) = var_effects {
                collect_set_variable_names(&pf, table, effects, &mut var_names);
            }
            let complex = dynamic_values::collect_complex_enum_values(
                ruleset,
                &pf,
                &file.logical_path,
                table,
            );
            let value_sets = dynamic_values::collect_value_set_members(ruleset, &pf, table);
            (path, instances, var_names, complex, value_sets)
        })
        .collect();

    // Sequential merge in original file order — preserves TypeIndex.merge call
    // order so goto-def "first match" and refcount semantics are unchanged.
    let mut index = TypeIndex::new();
    for (path, instances, var_names, complex, value_sets) in per_file {
        index.merge(&path, instances);
        for n in &var_names {
            index.var_index.add_name(n);
        }
        index.complex_enum_values.merge_file(&path, complex);
        index.value_set_values.merge_file(&path, value_sets);
    }
    index
}

#[cfg(test)]
mod tests {
    use super::*;
    use cwtools_parser::parser::parse_string;
    use cwtools_rules::rules_types::PathOptions;

    fn type_def(name: &str, path: &str) -> TypeDefinition {
        TypeDefinition {
            name: name.to_string(),
            name_field: None,
            path_options: PathOptions {
                paths: vec![path.to_string()],
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

    fn ruleset_with(td: TypeDefinition) -> RuleSet {
        let mut rs = RuleSet::new();
        rs.types.push(td);
        rs
    }

    fn names(result: &HashMap<String, Vec<TypeInstance>>, ty: &str) -> Vec<String> {
        let mut v: Vec<String> = result
            .get(ty)
            .map(|is| is.iter().map(|i| i.name.clone()).collect())
            .unwrap_or_default();
        v.sort();
        v
    }

    // A type declaring `type_key_prefix` collects only prefixed keys (case-
    // insensitive), and the instance name keeps the prefix intact.
    #[test]
    fn key_prefix_filters_and_keeps_name_intact() {
        let source = "MY_thing = { } my_other = { } NOPE_thing = { }";
        let table = StringTable::new();
        let parsed = parse_string(source, &table).unwrap();

        let mut td = type_def("thing", "common/things");
        td.key_prefix = Some("MY_".to_string());
        let rs = ruleset_with(td);

        let result = collect_type_instances(&rs, &parsed, "common/things/00_things.txt", &table);
        assert_eq!(names(&result, "thing"), vec!["MY_thing", "my_other"]);
    }

    // A type with no `type_key_prefix` is unaffected — every key is collected.
    #[test]
    fn no_key_prefix_collects_all() {
        let source = "MY_thing = { } NOPE_thing = { }";
        let table = StringTable::new();
        let parsed = parse_string(source, &table).unwrap();

        let rs = ruleset_with(type_def("thing", "common/things"));

        let result = collect_type_instances(&rs, &parsed, "common/things/00_things.txt", &table);
        assert_eq!(names(&result, "thing"), vec!["MY_thing", "NOPE_thing"]);
    }

    // An instance's location spans its whole definition: the start is the key,
    // the end is the spot just past the closing brace (the parser's
    // `SourceRange.end`). Cleanup features (rename/delete a definition) need the
    // full extent, so a multi-line clause must record an end on the brace's line.
    #[test]
    fn instance_location_end_is_closing_brace() {
        // `}` is the last char, on line 3 col 0; the range end lands one past it.
        let source = "thing_a = {\n    x = 1\n}";
        let table = StringTable::new();
        let parsed = parse_string(source, &table).unwrap();

        let rs = ruleset_with(type_def("thing", "common/things"));
        let result = collect_type_instances(&rs, &parsed, "common/things/00_things.txt", &table);

        let inst = &result.get("thing").expect("thing instances")[0];
        assert_eq!(inst.name, "thing_a");
        assert_eq!((inst.location.line, inst.location.col), (1, 0));
        assert_eq!(
            inst.location.end,
            (3, 1),
            "end must point just past the closing brace on line 3"
        );
        assert_ne!(
            (inst.location.line, inst.location.col),
            inst.location.end,
            "a multi-line definition has a non-degenerate span"
        );
    }
}
