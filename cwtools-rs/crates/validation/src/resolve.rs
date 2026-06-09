//! Type/path resolution: pick which `TypeDefinition` (and its rules) a root key
//! or file path resolves to.

use cwtools_rules::rules_types::*;

use crate::common::path_contains_segment;

/// Check if this type says its root key should be skipped (children are the real entries).
pub(crate) fn should_skip_root_key(_key: &str, type_def: &TypeDefinition) -> bool {
    type_def.skip_root_key.iter().any(|sk| match sk {
        SkipRootKey::AnyKey => true,
        SkipRootKey::SpecificKey(v) => v == _key,
        SkipRootKey::MultipleKeys(keys, _) => keys.iter().any(|k| k == _key),
    })
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
    match &t.path_options.path_extension {
        None => true,
        Some(ext) => {
            let ext = ext.to_lowercase();
            let ext = ext.strip_prefix('.').unwrap_or(&ext);
            if ext.is_empty() {
                return true;
            }
            let path_lower = file_path.to_lowercase();
            let basename = path_lower.rsplit('/').next().unwrap_or(&path_lower);
            basename.rsplit('.').next().is_some_and(|e| e == ext)
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
        if let Some(t) = find_type_by_path_and_key(file_path, Some(name), ruleset) {
            return Some((t, find_rules_by_name(&t.name, ruleset)));
        }
    }
    by_name
}

/// Find the actual validation rules for a type by looking in root_rules.
pub(crate) fn find_rules_by_name<'a>(
    name: &str,
    ruleset: &'a RuleSet,
) -> &'a [(RuleType, Options)] {
    for rr in &ruleset.root_rules {
        if let RootRule::TypeRule(rule_name, (rule, _opts)) = rr
            && rule_name == name
            && let RuleType::NodeRule { rules, .. } = rule
        {
            return rules.as_slice();
        }
    }
    &[]
}

/// The `Options` of a type's root rule (carries `## replace_scope` / `## push_scope`
/// that seed the instance's scope, e.g. the state-history `state` object).
pub(crate) fn find_type_rule_opts<'a>(name: &str, ruleset: &'a RuleSet) -> Option<&'a Options> {
    for rr in &ruleset.root_rules {
        if let RootRule::TypeRule(rule_name, (_rule, opts)) = rr
            && rule_name == name
        {
            return Some(opts);
        }
    }
    None
}

/// Find a type whose path_options match the given file path.
/// Returns the MOST SPECIFIC match (longest path string) so that
/// `common/ai_strategy_plans` wins over generic `common`.
pub(crate) fn find_type_by_path<'a>(
    file_path: &str,
    ruleset: &'a RuleSet,
) -> Option<&'a TypeDefinition> {
    find_type_by_path_and_key(file_path, None, ruleset)
}

/// Like `find_type_by_path` but also considers the root key of the child
/// being validated. Types whose `skip_root_key` matches `root_key` are
/// given a large bonus, so they beat a longer-path type that has no
/// skip_root_key and would otherwise win on path length alone.
///
/// This mirrors F# behaviour where `type[pdxmesh] { skip_root_key = objectTypes }`
/// correctly wins over `type[light] { path = "gfx/entities" }` for
/// a `objectTypes = { pdxmesh = { ... } }` root node in a `.gfx` file.
pub(crate) fn find_type_by_path_and_key<'a>(
    file_path: &str,
    root_key: Option<&str>,
    ruleset: &'a RuleSet,
) -> Option<&'a TypeDefinition> {
    let path_lower = file_path.to_lowercase();
    let basename = path_lower.rsplit('/').next().unwrap_or(&path_lower);
    // The file's directory (no filename, no trailing slash).
    let dir = path_lower
        .strip_suffix(basename)
        .unwrap_or(&path_lower)
        .trim_end_matches('/');
    let mut best: Option<&TypeDefinition> = None;
    let mut best_len = 0usize;

    for t in &ruleset.types {
        // path_file pins the type to one specific filename (e.g. several types
        // share path "map" but only airports.txt is the `airports` type).
        if let Some(pf) = &t.path_options.path_file
            && basename != pf.to_lowercase()
        {
            continue;
        }
        // path_extension restricts the type to files with a given extension
        // (e.g. sound types require `.asset`, so a `.txt` combat-sounds file must
        // NOT match them). Treat the extension as a hard filter.
        if let Some(ext) = &t.path_options.path_extension {
            let ext = ext.to_lowercase();
            let ext = ext.strip_prefix('.').unwrap_or(&ext);
            if basename.rsplit('.').next().is_none_or(|e| e != ext) {
                continue;
            }
        }
        // `## type_key_filter` gates a NON-wrapper type to nodes whose own key
        // satisfies the filter: a top-level `animation = { ... }` node is only an
        // instance of `type[model_animation] { type_key_filter = animation }`, not
        // of `type[light]` that merely shares the path. A matching filter also
        // earns a bonus so the filtered type beats an unfiltered one on the same
        // path. (For skip_root_key wrappers the filter applies to GRANDCHILDREN,
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
        for p in &t.path_options.paths {
            let p_lower = p.to_lowercase();
            // path_strict: the file must be DIRECTLY in this directory (so
            // `path_strict` type[unit] at common/units does NOT swallow files in
            // common/units/names/). Otherwise it may be in a subdirectory.
            let matches = if t.path_options.path_strict {
                dir == p_lower || dir.ends_with(&format!("/{}", p_lower))
            } else {
                path_contains_segment(dir, &p_lower)
            };
            // A path_file match is more specific than any bare directory match.
            // A skip_root_key match for the current root key gets a large bonus
            // so that e.g. `type[pdxmesh] { skip_root_key = objectTypes }` beats
            // `type[light] { path = "gfx/entities" }` for an objectTypes node.
            let skip_key_bonus = if let Some(rk) = root_key {
                if should_skip_root_key(rk, t) {
                    10_000
                } else {
                    0
                }
            } else {
                0
            };
            let weight = p_lower.len()
                + skip_key_bonus
                + tkf_bonus
                + if t.path_options.path_file.is_some() {
                    1000
                } else {
                    0
                };
            if matches && weight > best_len {
                best = Some(t);
                best_len = weight;
            }
        }
    }
    best
}

/// True if `t`'s `path_options` select `file_path`. Mirrors the per-path test in
/// [`find_type_by_path_and_key`] without the scoring, for use when several types
/// share a path.
pub(crate) fn type_path_matches(file_path: &str, t: &TypeDefinition) -> bool {
    let path_lower = file_path.to_lowercase();
    let basename = path_lower.rsplit('/').next().unwrap_or(&path_lower);
    let dir = path_lower
        .strip_suffix(basename)
        .unwrap_or(&path_lower)
        .trim_end_matches('/');
    if let Some(pf) = &t.path_options.path_file
        && basename != pf.to_lowercase()
    {
        return false;
    }
    if let Some(ext) = &t.path_options.path_extension {
        let ext = ext.to_lowercase();
        let ext = ext.strip_prefix('.').unwrap_or(&ext);
        if basename.rsplit('.').next().is_none_or(|e| e != ext) {
            return false;
        }
    }
    t.path_options.paths.iter().any(|p| {
        let p_lower = p.to_lowercase();
        if t.path_options.path_strict {
            dir == p_lower || dir.ends_with(&format!("/{}", p_lower))
        } else {
            path_contains_segment(dir, &p_lower)
        }
    })
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
