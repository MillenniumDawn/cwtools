//! Type/path resolution: pick which `TypeDefinition` (and its rules) a root key
//! or file path resolves to.

use cwtools_index::dir_matches_pattern;
use cwtools_rules::rules_types::*;

/// Check if `key` is a level-1 skip_root_key wrapper for this type.
///
/// Only the FIRST entry of the stack is tested: each element in
/// `skip_root_key` is a distinct nesting level (block form `{ A B }`
/// produces `[SpecificKey("A"), AnyKey]` — the first entry is the root
/// wrapper, the rest are deeper levels).  Using `.any()` over the whole
/// Vec would incorrectly treat every key as a wrapper for types that have
/// `[SpecificKey("ideas"), AnyKey]`.
pub(crate) fn should_skip_root_key(key: &str, type_def: &TypeDefinition) -> bool {
    type_def
        .skip_root_key
        .first()
        .is_some_and(|sk| cwtools_index::skip_root_key_matches(sk, key))
}

/// Return the remaining skip levels after the first one has been consumed
/// (i.e. the tail of the skip stack).  Empty when there is at most one level.
pub(crate) fn skip_root_key_tail(
    type_def: &TypeDefinition,
) -> &[cwtools_rules::rules_types::SkipRootKey] {
    type_def.skip_root_key.get(1..).unwrap_or(&[])
}

/// Look up both the TypeDefinition and the actual validation rules for a given type name.
pub(crate) fn find_type_and_rules<'a>(
    name: &str,
    ruleset: &'a RuleSet,
) -> Option<(&'a TypeDefinition, &'a [(RuleType, Options)])> {
    let type_def = ruleset.type_by_name.get(name).map(|&i| &ruleset.types[i])?;
    let rules = find_rules_by_name(name, ruleset);
    Some((type_def, rules))
}

/// True if `t` has no `path_extension` constraint, or `file_path` satisfies it.
pub(crate) fn type_extension_matches(file_path: &str, t: &TypeDefinition) -> bool {
    match &t.path_options.path_ext_lower {
        None => true,
        Some(ext) => {
            if ext.is_empty() {
                return true;
            }
            let path_lower = file_path.to_lowercase().replace('\\', "/");
            let basename = path_lower.rsplit('/').next().unwrap_or(&path_lower);
            basename
                .rsplit('.')
                .next()
                .is_some_and(|e| e == ext.as_str())
        }
    }
}

/// Resolve a top-level entity's type by its root key, honoring `path_extension`.
///
/// The fast path matches the key against type NAMES (`find_type_and_rules`).
/// But several types can share a `## type_key_filter` + path and differ only by
/// `path_extension`: `music` is the `.txt` song lists while `musicasset` is the
/// `.asset` definitions, both keyed `music`. The by-name lookup always returns
/// `music`, so `.asset` bodies (name/file/volume) wrongly flag as unexpected and
/// `song` reads as missing. When the by-name type is gated to an extension the
/// file lacks, defer to the path/extension-aware resolver instead.
pub(crate) fn find_type_and_rules_for_file<'a>(
    name: &str,
    file_path: &str,
    ruleset: &'a RuleSet,
) -> Option<(&'a TypeDefinition, &'a [(RuleType, Options)])> {
    let by_name = find_type_and_rules(name, ruleset);
    if let Some((td, _)) = by_name {
        if type_extension_matches(file_path, td) {
            return by_name;
        }
        // Extension mismatch: try the path-aware lookup first.
        let file_path_lower = file_path.to_lowercase();
        if let Some(t) = find_type_by_path_and_key(&file_path_lower, Some(name), ruleset) {
            return Some((t, find_rules_by_name(&t.name, ruleset)));
        }
        // No path match either: the by-name hit was for a different extension;
        // returning it would validate the wrong rule body.
        return None;
    }
    by_name
}

/// Find the actual validation rules for a type by looking in root_rules.
pub(crate) fn find_rules_by_name<'a>(
    name: &str,
    ruleset: &'a RuleSet,
) -> &'a [(RuleType, Options)] {
    if let Some(&i) = ruleset.type_rules_idx.get(name)
        && let RootRule::TypeRule(_, (rule, _)) = &ruleset.root_rules[i]
        && let RuleType::NodeRule { rules, .. } = rule
    {
        return rules.as_slice();
    }
    &[]
}

/// The `Options` of a type's root rule (carries `## replace_scope` / `## push_scope`
/// that seed the instance's scope, e.g. the state-history `state` object).
pub(crate) fn find_type_rule_opts<'a>(name: &str, ruleset: &'a RuleSet) -> Option<&'a Options> {
    let i = *ruleset.type_rules_idx.get(name)?;
    if let RootRule::TypeRule(_, (_, opts)) = &ruleset.root_rules[i] {
        Some(opts)
    } else {
        None
    }
}

/// Find a type whose path_options match the given file path.
/// Returns the MOST SPECIFIC match (longest path string) so that
/// `common/ai_strategy_plans` wins over generic `common`.
pub(crate) fn find_type_by_path<'a>(
    file_path: &str,
    ruleset: &'a RuleSet,
) -> Option<&'a TypeDefinition> {
    let lower = file_path.to_lowercase().replace('\\', "/");
    find_type_by_path_and_key(&lower, None, ruleset)
}

/// A path-matched type with its base weight (path length + path_file bonus).
/// The key-dependent bonuses (`skip_key_bonus`, `tkf_bonus`) are added later
/// by [`find_type_from_candidates`] so that path filtering is done once per
/// file while key scoring is done once per root child.
pub(crate) struct PathCandidate<'a> {
    pub type_def: &'a TypeDefinition,
    /// Largest `p_lower.len() + path_file_bonus` over all matching paths.
    pub base_weight: usize,
}

/// Pre-filter types to those whose path options match `file_path_lower`.
/// Returns one entry per matching type (the highest base weight across all
/// matching paths_lower entries).  Call once per file, then reuse the slice
/// across all root children via [`find_type_from_candidates`].
pub(crate) fn path_candidates_for_file<'a>(
    file_path_lower: &str,
    ruleset: &'a RuleSet,
) -> Vec<PathCandidate<'a>> {
    // Logical paths are `/`-separated. A backslash path (Windows, if a caller
    // didn't normalize) would make `rsplit('/')` treat the whole path as the
    // basename and match no type — silently breaking hover/goto/validation
    // (e.g. trigger doc tooltips) for that file.
    let normalized = file_path_lower.replace('\\', "/");
    let file_path_lower = normalized.as_str();
    let basename = file_path_lower
        .rsplit('/')
        .next()
        .unwrap_or(file_path_lower);
    let dir = file_path_lower
        .strip_suffix(basename)
        .unwrap_or(file_path_lower)
        .trim_end_matches('/');
    let ext = basename.rsplit('.').next();

    let mut out = Vec::new();
    for t in &ruleset.types {
        if let Some(pf) = &t.path_options.path_file_lower
            && basename != pf.as_str()
        {
            continue;
        }
        if let Some(req_ext) = &t.path_options.path_ext_lower
            && ext.is_none_or(|e| e != req_ext.as_str())
        {
            continue;
        }
        let path_file_bonus = if t.path_options.path_file.is_some() {
            1000
        } else {
            0
        };
        let mut best_weight = 0usize;
        for p_lower in &t.path_options.paths_lower {
            if dir_matches_pattern(dir, p_lower, t.path_options.path_strict) {
                let w = p_lower.len() + path_file_bonus;
                if w > best_weight {
                    best_weight = w;
                }
            }
        }
        if best_weight > 0 {
            out.push(PathCandidate {
                type_def: t,
                base_weight: best_weight,
            });
        }
    }
    out
}

/// Pick the best-matching type from path-prefiltered candidates, applying
/// key-dependent bonuses (`skip_root_key` and `type_key_filter`).
pub(crate) fn find_type_from_candidates<'a>(
    candidates: &[PathCandidate<'a>],
    root_key: Option<&str>,
) -> Option<&'a TypeDefinition> {
    let mut best: Option<&TypeDefinition> = None;
    let mut best_len = 0usize;

    for c in candidates {
        let t = c.type_def;
        // `## type_key_filter` gates a NON-wrapper type to nodes whose own key
        // satisfies the filter. A matching filter also earns a bonus so the
        // filtered type beats an unfiltered one on the same path.
        // (For skip_root_key wrappers the filter applies to GRANDCHILDREN,
        // handled in validate_wrapper_grandchildren, so it is not gated here.)
        let tkf_bonus = match (root_key, t.skip_root_key.is_empty(), &t.type_key_filter) {
            (Some(rk), true, Some((keys, negate))) => {
                let hit = keys.iter().any(|k| k.eq_ignore_ascii_case(rk));
                if hit != *negate {
                    5_000
                } else {
                    continue; // filter excludes this key: the type does not apply
                }
            }
            _ => 0,
        };
        let skip_key_bonus = match root_key {
            Some(rk) if should_skip_root_key(rk, t) => 10_000,
            _ => 0,
        };
        let weight = c.base_weight + skip_key_bonus + tkf_bonus;
        if weight > best_len {
            best = Some(t);
            best_len = weight;
        }
    }
    best
}

/// Like `find_type_by_path` but also considers the root key of the child
/// being validated. Types whose `skip_root_key` matches `root_key` are
/// given a large bonus, so they beat a longer-path type that has no
/// skip_root_key and would otherwise win on path length alone.
///
/// `file_path_lower` must already be lowercased (ASCII) by the caller so
/// that per-child calls in a hot loop share a single allocation.
///
/// For hot loops processing many root children of the same file, prefer
/// calling [`path_candidates_for_file`] once and [`find_type_from_candidates`]
/// per child.
///
/// This mirrors F# behaviour where `type[pdxmesh] { skip_root_key = objectTypes }`
/// correctly wins over `type[light] { path = "gfx/entities" }` for
/// a `objectTypes = { pdxmesh = { ... } }` root node in a `.gfx` file.
pub(crate) fn find_type_by_path_and_key<'a>(
    file_path_lower: &str,
    root_key: Option<&str>,
    ruleset: &'a RuleSet,
) -> Option<&'a TypeDefinition> {
    let candidates = path_candidates_for_file(file_path_lower, ruleset);
    find_type_from_candidates(&candidates, root_key)
}

/// True if `t`'s `path_options` select `file_path`. Mirrors the per-path test in
/// [`find_type_by_path_and_key`] without the scoring, for use when several types
/// share a path.
pub(crate) fn type_path_matches(file_path: &str, t: &TypeDefinition) -> bool {
    let path_lower = file_path.to_lowercase().replace('\\', "/");
    let basename = path_lower.rsplit('/').next().unwrap_or(&path_lower);
    let dir = path_lower
        .strip_suffix(basename)
        .unwrap_or(&path_lower)
        .trim_end_matches('/');
    if let Some(pf) = &t.path_options.path_file_lower
        && basename != pf.as_str()
    {
        return false;
    }
    if let Some(ext) = &t.path_options.path_ext_lower
        && basename
            .rsplit('.')
            .next()
            .is_none_or(|e| e != ext.as_str())
    {
        return false;
    }
    t.path_options
        .paths_lower
        .iter()
        .any(|p_lower| dir_matches_pattern(dir, p_lower, t.path_options.path_strict))
}

/// Resolve which type a `skip_root_key` wrapper's grandchild belongs to, by the
/// grandchild's own key. Several types can share a path AND `skip_root_key`
/// (e.g. `pdxmesh`, `pdxparticle`, `entity` all sit under `objectTypes` in `.gfx`
/// files); `## type_key_filter` is what disambiguates them. Prefer a candidate
/// whose filter selects `gc_key`; otherwise fall back to a wrapper type that has
/// no filter. Returns `None` when nothing fits, in which case the caller keeps
/// the type that won the path lookup (so single-type wrappers are unaffected).
pub(crate) fn find_grandchild_type<'a>(
    file_path: &str,
    wrapper_root_key: &str,
    gc_key: &str,
    ruleset: &'a RuleSet,
) -> Option<&'a TypeDefinition> {
    let mut generic: Option<&TypeDefinition> = None;
    for t in &ruleset.types {
        if !should_skip_root_key(wrapper_root_key, t) || !type_path_matches(file_path, t) {
            continue;
        }
        match &t.type_key_filter {
            Some((keys, negative)) => {
                let in_list = keys.iter().any(|k| k.eq_ignore_ascii_case(gc_key));
                // `negative` = `## type_key_filter <> ...` (exclude); otherwise include.
                if in_list != *negative {
                    return Some(t);
                }
            }
            None => {
                if generic.is_none() {
                    generic = Some(t);
                }
            }
        }
    }
    generic
}

#[cfg(test)]
mod tests {
    use super::*;
    use cwtools_parser::parser::parse_string;
    use cwtools_rules::rules_converter::ast_to_ruleset;
    use cwtools_string_table::string_table::StringTable;

    #[test]
    fn path_candidates_handle_backslash_paths() {
        let table = StringTable::new();
        let cwt = "types = { type[foo] = { path = \"common/foo\" } }";
        let parsed = parse_string(cwt, &table).unwrap();
        let rs = ast_to_ruleset(&parsed, &table);

        // Forward-slash resolves the type.
        assert!(
            !path_candidates_for_file("common/foo/x.txt", &rs).is_empty(),
            "forward-slash path should resolve type foo"
        );
        // A Windows backslash path must resolve the same type, not silently
        // fail (which would break hover/goto/validation for the file).
        assert!(
            !path_candidates_for_file("common\\foo\\x.txt", &rs).is_empty(),
            "backslash path should resolve type foo too"
        );
    }

    #[test]
    fn type_path_matches_handles_backslash_paths() {
        let table = StringTable::new();
        let cwt = "types = { type[foo] = { path = \"common/foo\" } }";
        let parsed = parse_string(cwt, &table).unwrap();
        let rs = ast_to_ruleset(&parsed, &table);
        let t = &rs.types[0];
        assert!(
            type_path_matches("common/foo/x.txt", t),
            "forward-slash path should match"
        );
        assert!(
            type_path_matches("common\\foo\\x.txt", t),
            "backslash path should match too"
        );
    }
}
