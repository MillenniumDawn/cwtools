//! The index data structures: cross-file type-instance index plus the file-path
//! and variable-name indexes it owns.

use rustc_hash::{FxHashMap, FxHashSet};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::dynamic_values;
use crate::{SourceLocation, dec_ref, is_subtype_key};

/// A single defined instance of a CW type (e.g. one event, one technology …).
#[derive(Debug, Clone)]
pub struct TypeInstance {
    /// The instance name (node key, or the value of `name_field` child).
    pub name: String,
    /// Where the definition starts in the source file.
    pub location: SourceLocation,
    /// The loc key for the type's `## primary` localisation when it is taken from
    /// an explicit field (e.g. an event's `title = <key>`), captured here so hover
    /// can show the localised title for a reference in another file without
    /// re-reading the definition. `None` when the type has no primary
    /// explicit-field localisation (name-derived keys are computed on demand).
    pub primary_loc_key: Option<String>,
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
        let entries = match std::fs::read_dir(dir) {
            Ok(entries) => entries,
            Err(e) => {
                tracing::warn!("FileIndex::walk: cannot read {}: {e}", dir.display());
                return;
            }
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                Self::walk(root, &path, out);
            } else if let Ok(rel) = path.strip_prefix(root)
                && let Some(s) = rel.to_str()
            {
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
    /// normalised (lowercased, forward slashes, leading slash stripped, repeated
    /// slashes collapsed — the engine treats `gfx//interface` as `gfx/interface`,
    /// and some mod files write the doubled form).
    pub fn contains(&self, path: &str) -> bool {
        thread_local! {
            static NORM_BUF: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
        }
        NORM_BUF.with(|buf| {
            let mut norm = buf.borrow_mut();
            norm.clear();
            // Single pass: split on both separators, drop empty segments
            // (collapsing repeated/leading slashes), join with '/', lowercase ASCII.
            let mut first = true;
            for seg in path.trim().split(['/', '\\']).filter(|s| !s.is_empty()) {
                if !first {
                    norm.push('/');
                }
                first = false;
                norm.extend(seg.chars().map(|c| c.to_ascii_lowercase()));
            }
            self.files.contains(norm.as_str())
        })
    }

    /// Add already-normalized relative paths (the vanilla-cache restore path).
    pub fn add_paths<I: IntoIterator<Item = String>>(&mut self, paths: I) {
        self.files.extend(paths);
    }

    /// The normalized relative paths, for persisting to the vanilla cache.
    pub fn paths(&self) -> impl Iterator<Item = &String> {
        self.files.iter()
    }

    /// Resolve `value` as a reference made relative to `referencing_file`'s own
    /// directory (the engine resolves a `.asset` `file =` beside the .asset, not
    /// under a fixed root prefix). `referencing_file` is the absolute on-disk
    /// path; its root-relative directory is recovered as the longest path-suffix
    /// that is itself an indexed file. Returns true when the directory-relative
    /// `value` resolves to an indexed path.
    pub fn resolve_relative(&self, referencing_file: &str, value: &str) -> bool {
        let segs: Vec<String> = referencing_file
            .split(['/', '\\'])
            .filter(|s| !s.is_empty())
            .map(|s| s.to_ascii_lowercase())
            .collect();
        if segs.len() < 2 {
            return false;
        }
        // Longest suffix first: the first suffix that is an indexed file is the
        // referencing file's own root-relative path. Everything before its
        // directory is the (un-indexed) root prefix.
        for start in 0..segs.len() - 1 {
            let self_path = segs[start..].join("/");
            if self.files.contains(&self_path) {
                let dir = &segs[start..segs.len() - 1];
                let sibling = if dir.is_empty() {
                    value.to_string()
                } else {
                    format!("{}/{}", dir.join("/"), value)
                };
                return self.contains(&sibling);
            }
        }
        false
    }
}

/// Project-wide set of defined script-variable names (every `value_set[...]`
/// definition collected across the mod + base game), used to check that a
/// `variable_field` reference resolves (CW246). Names are normalised to a
/// canonical key so a definition like `morale@ROOT` and a read like
/// `morale@GER` both resolve to `morale`. Empty unless the CLI populated it.
#[derive(Debug, Default)]
pub struct VarIndex {
    /// Normalized variable name → how many definitions carry it. A refcount so the
    /// LSP can drop a name on `clear_file` only when its last definition goes,
    /// while the bulk CLI path (which never removes) just keeps incrementing.
    names: HashMap<String, usize>,
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
        let mut buf = String::new();
        Self::normalize_into(raw, &mut buf);
        buf
    }

    /// Like [`normalize`](Self::normalize) but writes the canonical key into a
    /// reusable buffer (cleared first), avoiding a per-call allocation on the hot
    /// `contains` path. Identifiers are ASCII, so the lowercase fold is ASCII.
    pub(crate) fn normalize_into(raw: &str, buf: &mut String) {
        let s = raw.trim().trim_matches('"');
        let before_amp = s.split('@').next().unwrap_or(s);
        let last_seg = before_amp.rsplit('.').next().unwrap_or(before_amp);
        let core = last_seg.split(['?', '^']).next().unwrap_or(last_seg);
        buf.clear();
        buf.extend(core.trim().chars().map(|c| c.to_ascii_lowercase()));
    }

    pub fn add_name(&mut self, raw: &str) {
        let n = Self::normalize(raw);
        if !n.is_empty() {
            *self.names.entry(n).or_insert(0) += 1;
        }
    }

    /// Drop one definition of a name; removes the entry when its refcount hits 0.
    /// Used by the LSP's per-file `clear_file` so re-indexing a file refreshes its
    /// variables instead of leaking the old set.
    pub fn remove_name(&mut self, raw: &str) {
        let n = Self::normalize(raw);
        dec_ref(&mut self.names, n.as_str());
    }

    /// Whether a raw reference resolves to a known defined variable.
    pub fn contains(&self, raw: &str) -> bool {
        thread_local! {
            static NORM_BUF: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
        }
        NORM_BUF.with(|buf| {
            let mut buf = buf.borrow_mut();
            Self::normalize_into(raw, &mut buf);
            self.names.contains_key(buf.as_str())
        })
    }

    /// Fold another index's names into this one (e.g. base-game variables into
    /// the mod's index).
    pub fn merge(&mut self, other: &VarIndex) {
        for (name, count) in &other.names {
            *self.names.entry(name.clone()).or_insert(0) += count;
        }
    }

    /// The normalized defined names, for persisting to the vanilla cache.
    pub fn names(&self) -> impl Iterator<Item = &String> {
        self.names.keys()
    }
}

#[derive(Debug, Default)]
pub struct TypeIndex {
    /// type_name → Vec<(file_uri, instance)>
    pub map: FxHashMap<String, Vec<(Arc<str>, TypeInstance)>>,
    /// lowercased instance name → how many definitions carry that name (across all
    /// types and files). Lets `is_any_instance` be O(1) instead of scanning every
    /// instance. A refcount so `remove_file` can drop a name only when its last
    /// definition goes. Keyed lowercase because Paradox identifiers are
    /// case-insensitive (same normalization as `contains`/`instance_sets`).
    name_counts: FxHashMap<String, usize>,
    /// type_name → (lowercased instance name → refcount). Makes `contains` an O(1)
    /// hash lookup instead of a linear scan over every instance of the type, which
    /// was quadratic over the corpus for high-cardinality types (state, character,
    /// country_event). The refcount lets `remove_file` drop a name only when its
    /// last definition in that type goes.
    instance_sets: FxHashMap<String, FxHashMap<String, usize>>,
    /// file_uri → the set of `map` bucket keys (type names) that file contributes
    /// instances to. Lets [`remove_file`](Self::remove_file) visit only the
    /// buckets the file actually touched (O(the file's own entries)) instead of
    /// scanning every bucket of `self.map` (O(total instances) — the single
    /// largest cost of a reindex at Millennium Dawn scale). Every insertion path
    /// (`merge`, `merge_with_uris`) records its buckets here, and every removal
    /// path (`remove_file`, `remove_files`) prunes them, so the reverse map stays
    /// a faithful inverse of `map`'s uris. Not serialized: the vanilla cache
    /// stores only instances and reloads them through `merge_with_uris`, which
    /// rebuilds this map (same as `name_counts` / `instance_sets`).
    file_buckets: FxHashMap<Arc<str>, FxHashSet<String>>,
    /// Index of every asset/file path under the game roots, for `filepath`
    /// reference checks (CW113). Empty unless the CLI populated it.
    pub file_index: FileIndex,
    /// Project-wide set of defined variable names, for `variable_field`
    /// reference checks (CW246). Empty unless the CLI populated it.
    pub var_index: VarIndex,
    /// Whether this index includes vanilla (base-game) definitions. When
    /// `false`, CW500 type-reference checks are skipped to avoid false
    /// positives on valid vanilla cross-references. The driver sets this
    /// to `true` after merging vanilla data.
    pub complete: bool,
    /// Complex-enum members collected from indexed files (enum name -> values),
    /// e.g. `equipment_stat`, `country_tags`, `idea_name`. Completion-only.
    pub complex_enum_values: dynamic_values::NamedValueIndex,
    /// `value_set[...]` members collected from indexed files (namespace ->
    /// values), e.g. `country_flag`, `global_flag`. Completion-only.
    pub value_set_values: dynamic_values::NamedValueIndex,
}

impl TypeIndex {
    pub fn new() -> Self {
        Self::default()
    }

    /// Return true if `type_name` has a known instance called `instance`.
    /// Paradox script identifiers are case-insensitive, so a reference like
    /// `LBA_AI_BEHAVIOR` resolves to the `LBA_ai_behavior` definition.
    pub fn contains(&self, type_name: &str, instance: &str) -> bool {
        let Some(names) = self.instance_sets.get(type_name) else {
            return false;
        };
        // Borrow the key directly when it's already lowercase (the common case),
        // only allocating a lowercase copy when it actually has uppercase bytes.
        if instance.bytes().any(|b| b.is_ascii_uppercase()) {
            names.contains_key(&instance.to_ascii_lowercase())
        } else {
            names.contains_key(instance)
        }
    }

    /// Return true if `name` is a known instance of ANY type. Used to recognise
    /// scope-opening keys: HOI4 from-data scope links (links.cwt) let an instance
    /// of a referenced type (character, state, ideology, ...) open its own scope,
    /// e.g. `LBA_some_character = { ... }`.
    pub fn is_any_instance(&self, name: &str) -> bool {
        if name.bytes().any(|b| b.is_ascii_uppercase()) {
            self.name_counts.contains_key(&name.to_ascii_lowercase())
        } else {
            self.name_counts.contains_key(name)
        }
    }

    /// All instances for a type (across all files).
    pub fn instances(&self, type_name: &str) -> &[(Arc<str>, TypeInstance)] {
        self.map.get(type_name).map(|v| v.as_slice()).unwrap_or(&[])
    }

    /// Every definition site of an instance named `name` (case-insensitive),
    /// across all types. Used by goto-definition's fallback for dotted ids
    /// (events, decisions) that the heuristic def index keys by node-key rather
    /// than by the instance id. Scans the index (rare interactive path).
    pub fn instance_locations(&self, name: &str) -> Vec<(Arc<str>, SourceLocation)> {
        self.map
            .values()
            .flatten()
            .filter(|(_, inst)| inst.name.eq_ignore_ascii_case(name))
            .map(|(uri, inst)| (uri.clone(), inst.location))
            .collect()
    }

    /// The explicit-field primary loc key captured for `name`'s instance of
    /// `type_name` (e.g. an event's `title` loc key), if any. Lets hover show the
    /// localised title for a reference. Case-insensitive on the instance name.
    pub fn primary_loc_key(&self, type_name: &str, name: &str) -> Option<&str> {
        self.map
            .get(type_name)?
            .iter()
            .filter(|(_, inst)| inst.name.eq_ignore_ascii_case(name))
            .find_map(|(_, inst)| inst.primary_loc_key.as_deref())
    }

    /// Names a loc `$ref$` may bind to besides loc keys: every type-instance
    /// name (dynamic modifiers, ideas, buildings, …) and every defined variable,
    /// lowercased. The caller unions modifiers / vanilla loc keys on top. Lets
    /// loc validation accept `$education_dynamic_modifier$` / `$some_variable$`
    /// embeds without a CW225 while genuine typos (matching nothing) still flag.
    pub fn loc_bindable_names(&self) -> impl Iterator<Item = String> + '_ {
        // `name_counts` keys and `var_index` names are already lowercased /
        // normalised, matching the loc validator's case-insensitive lookup.
        self.loc_bindable_names_iter().map(str::to_string)
    }

    /// Borrowing form of [`loc_bindable_names`](Self::loc_bindable_names): yields
    /// each bindable name by reference, no per-name allocation. Use this when the
    /// caller only needs to read the names (membership, iteration) rather than own
    /// them.
    pub(crate) fn loc_bindable_names_iter(&self) -> impl Iterator<Item = &str> + '_ {
        self.name_counts
            .keys()
            .map(String::as_str)
            .chain(self.var_index.names().map(String::as_str))
    }

    /// Whether `name` is a loc-bindable name (a type-instance name or defined
    /// variable). `name` is matched against the already-lowercased index keys, so
    /// the caller must pass a lowercased name (as the loc validator does). O(1)
    /// instead of building/scanning the whole bindable-name set.
    pub fn contains_loc_bindable(&self, name: &str) -> bool {
        self.name_counts.contains_key(name) || self.var_index.names.contains_key(name)
    }

    /// Every `(type_name, instance)` defined in `file_uri`. Scans the whole
    /// index (O(total instances)); used by document-symbol/outline, which is
    /// on-demand and infrequent. Lets `FileInfo` avoid a second per-file copy.
    pub fn instances_in_file<'a>(&'a self, file_uri: &str) -> Vec<(&'a str, &'a TypeInstance)> {
        let mut out = Vec::new();
        for (type_name, entries) in &self.map {
            // Skip subtype-qualified membership keys: the instance already
            // appears under its base `type`, so listing it again would duplicate
            // the outline / document-symbol entry.
            if is_subtype_key(type_name) {
                continue;
            }
            for (uri, inst) in entries {
                if uri.as_ref() == file_uri {
                    out.push((type_name.as_str(), inst));
                }
            }
        }
        out
    }

    /// Merge per-file results into the index.
    ///
    /// A subtype-qualified key (`"type.subtype"`, recognised by the `.`) is a
    /// membership entry produced by [`SubtypeCollector`]. Such entries feed
    /// `contains` (so `<type.subtype>` references resolve) but are deliberately
    /// kept out of `name_counts` — they share the instance's name with the base
    /// `type` entry, and double-counting would skew `is_any_instance` refcounts
    /// and document-symbol output without adding a distinct definition.
    pub fn merge(&mut self, file_uri: &str, per_type: HashMap<String, Vec<TypeInstance>>) {
        let uri: Arc<str> = Arc::from(file_uri);
        for (type_name, instances) in per_type {
            let subtype_key = is_subtype_key(&type_name);
            // Record the bucket in the reverse map so `remove_file` can find it
            // without scanning every bucket. All of this file's instances share
            // the one `uri`, so a single insert per type covers them.
            self.file_buckets
                .entry(Arc::clone(&uri))
                .or_default()
                .insert(type_name.clone());
            let set = self.instance_sets.entry(type_name.clone()).or_default();
            let entry = self.map.entry(type_name).or_default();
            for inst in instances {
                let lower = inst.name.to_ascii_lowercase();
                if !subtype_key {
                    *self.name_counts.entry(lower.clone()).or_insert(0) += 1;
                }
                *set.entry(lower).or_insert(0) += 1;
                entry.push((Arc::clone(&uri), inst));
            }
        }
    }

    /// Merge instances that each carry their own source URI. Like [`merge`], but
    /// the per-instance URI is stored as-is instead of a single shared key, so a
    /// batch spanning many files (the vanilla index, where every base-game file
    /// contributes a few instances) keeps each instance pointing at its real
    /// source file. `remove_files` drops such a batch by URI.
    pub fn merge_with_uris(
        &mut self,
        per_type: impl IntoIterator<Item = (String, Vec<(Arc<str>, TypeInstance)>)>,
    ) {
        for (type_name, instances) in per_type {
            let subtype_key = is_subtype_key(&type_name);
            let set = self.instance_sets.entry(type_name.clone()).or_default();
            let entry = self.map.entry(type_name.clone()).or_default();
            for (uri, inst) in instances {
                let lower = inst.name.to_ascii_lowercase();
                if !subtype_key {
                    *self.name_counts.entry(lower.clone()).or_insert(0) += 1;
                }
                *set.entry(lower).or_insert(0) += 1;
                // Each instance can come from a different file, so record this
                // bucket under its own uri. Clone the type name only the first
                // time a uri contributes to it (the batch's common case is many
                // instances of one type per file).
                let bucket = self.file_buckets.entry(Arc::clone(&uri)).or_default();
                if !bucket.contains(type_name.as_str()) {
                    bucket.insert(type_name.clone());
                }
                entry.push((uri, inst));
            }
        }
    }

    /// Remove every instance contributed by any file in `file_uris`, in a single
    /// pass over the index. Use this to drop a large multi-file contribution (the
    /// whole vanilla index) at once: [`remove_file`](Self::remove_file) re-scans
    /// the map on every call, so removing thousands of files one at a time would
    /// be quadratic. Only touches the type instances; the dynamic-value indexes
    /// are keyed separately and untouched.
    pub fn remove_files(&mut self, file_uris: &HashSet<Arc<str>>) {
        if file_uris.is_empty() {
            return;
        }
        for (type_name, v) in self.map.iter_mut() {
            let subtype_key = is_subtype_key(type_name);
            v.retain(|(uri, inst)| {
                let keep = !file_uris.contains(uri);
                if !keep {
                    let lower = inst.name.to_ascii_lowercase();
                    if !subtype_key {
                        dec_ref(&mut self.name_counts, lower.as_str());
                    }
                    if let Some(set) = self.instance_sets.get_mut(type_name) {
                        dec_ref(set, lower.as_str());
                    }
                }
                keep
            });
        }
        self.map.retain(|_, v| !v.is_empty());
        self.instance_sets.retain(|_, names| !names.is_empty());
        // Drop the reverse-map entries for the removed files. Surviving files
        // still contribute to exactly the buckets they did before (their
        // instances were untouched), and any bucket emptied here had no
        // surviving contributor, so no survivor's `file_buckets` set is left
        // pointing at a bucket that no longer exists.
        for uri in file_uris {
            self.file_buckets.remove(uri);
        }
    }

    /// Remove all instances contributed by `file_uri`.
    ///
    /// Visits only the `map` buckets the reverse map (`file_buckets`) records for
    /// this file, so the cost is proportional to the file's own entries rather
    /// than the whole index. Empty buckets are pruned exactly as the old
    /// scan-everything version did: a `map` bucket empties only when its last
    /// contributor is removed, and that contributor always has the bucket in its
    /// `file_buckets` set, so the emptied bucket is always visited and dropped
    /// here — no lingering empty bucket survives (matching `remove_files`).
    pub fn remove_file(&mut self, file_uri: &str) {
        self.complex_enum_values.remove_file(file_uri);
        self.value_set_values.remove_file(file_uri);
        // Take the file's bucket set out of the reverse map (dropping its entry).
        // A file that contributed no type instances has no entry: nothing to do.
        let Some(buckets) = self.file_buckets.remove(file_uri) else {
            return;
        };
        for type_name in &buckets {
            // Subtype-qualified keys never contributed to `name_counts` (see
            // `merge`), so they must not decrement it here.
            let subtype_key = is_subtype_key(type_name);
            let Some(v) = self.map.get_mut(type_name) else {
                continue;
            };
            v.retain(|(uri, inst)| {
                let keep = uri.as_ref() != file_uri;
                if !keep {
                    let lower = inst.name.to_ascii_lowercase();
                    if !subtype_key {
                        dec_ref(&mut self.name_counts, lower.as_str());
                    }
                    if let Some(set) = self.instance_sets.get_mut(type_name) {
                        dec_ref(set, lower.as_str());
                    }
                }
                keep
            });
            if v.is_empty() {
                self.map.remove(type_name);
                self.instance_sets.remove(type_name);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_index_collapses_double_slashes() {
        // The engine collapses repeated slashes, so a `gfx//interface/x.dds`
        // reference (as some MD .gfx files write) must resolve to the indexed
        // `gfx/interface/x.dds`, not flag CW113.
        let mut idx = FileIndex::new();
        idx.add_paths(vec!["gfx/interface/x.dds".to_string()]);
        assert!(
            idx.contains("gfx//interface/x.dds"),
            "double-slash reference must resolve"
        );
        assert!(idx.contains("gfx/interface/x.dds"));
    }

    #[test]
    fn instance_locations_finds_dotted_id_case_insensitive() {
        // goto-definition (#39): an event/decision reference resolves by its
        // dotted id (the instance name), case-insensitively.
        let mut idx = TypeIndex::new();
        let mut map = HashMap::new();
        map.insert(
            "event".to_string(),
            vec![TypeInstance {
                name: "GER_some.1".to_string(),
                location: SourceLocation { line: 7, col: 4 },
                primary_loc_key: None,
            }],
        );
        idx.merge("file://e.txt", map);
        let locs = idx.instance_locations("ger_some.1");
        assert_eq!(locs.len(), 1, "should resolve case-insensitively");
        assert_eq!(locs[0].1.line, 7);
        assert!(idx.instance_locations("nope.1").is_empty());
    }

    #[test]
    fn loc_bindable_names_includes_instances_and_variables() {
        let mut idx = TypeIndex::new();
        let mut per_type: HashMap<String, Vec<TypeInstance>> = HashMap::new();
        per_type.insert(
            "ln".to_string(),
            vec![TypeInstance {
                name: "Education_Dynamic_Modifier".to_string(),
                location: SourceLocation { line: 1, col: 0 },
                primary_loc_key: None,
            }],
        );
        idx.merge("common/lns/x.txt", per_type);
        idx.var_index.add_name("My_Variable");

        let names: std::collections::HashSet<String> = idx.loc_bindable_names().collect();
        assert!(
            names.contains("education_dynamic_modifier"),
            "instance names (lowercased) must be bindable, got {:?}",
            names
        );
        assert!(
            names.contains("my_variable"),
            "defined variables (lowercased) must be bindable, got {:?}",
            names
        );
    }

    // ── removal parity (reverse-map narrowed removal) ────────────────────────

    fn inst(name: &str, line: u32) -> TypeInstance {
        TypeInstance {
            name: name.to_string(),
            location: SourceLocation { line, col: 0 },
            primary_loc_key: None,
        }
    }

    /// Comparable projection of every observable index structure. Sorted so the
    /// comparison is order-independent (removal preserves order, a from-scratch
    /// rebuild reproduces it, but sorting keeps the assertion robust either way).
    type Snap = (
        std::collections::BTreeMap<String, Vec<(String, String, u32)>>,
        std::collections::BTreeMap<String, usize>,
        std::collections::BTreeMap<String, std::collections::BTreeMap<String, usize>>,
    );

    fn snapshot(idx: &TypeIndex) -> Snap {
        use std::collections::BTreeMap;
        let mut map = BTreeMap::new();
        for (ty, entries) in &idx.map {
            let mut v: Vec<(String, String, u32)> = entries
                .iter()
                .map(|(uri, i)| (uri.to_string(), i.name.clone(), i.location.line))
                .collect();
            v.sort();
            map.insert(ty.clone(), v);
        }
        let name_counts: BTreeMap<String, usize> = idx
            .name_counts
            .iter()
            .map(|(k, v)| (k.clone(), *v))
            .collect();
        let instance_sets: BTreeMap<String, BTreeMap<String, usize>> = idx
            .instance_sets
            .iter()
            .map(|(k, m)| {
                (
                    k.clone(),
                    m.iter().map(|(kk, vv)| (kk.clone(), *vv)).collect(),
                )
            })
            .collect();
        (map, name_counts, instance_sets)
    }

    /// Removing a `merge`-contributed file must leave exactly the state of a
    /// rebuild that never saw it. Exercises the single-uri insertion path.
    #[test]
    fn remove_file_parity_removing_merge_file() {
        let build = |include_b: bool| -> TypeIndex {
            let mut idx = TypeIndex::new();
            idx.merge(
                "file://a.txt",
                HashMap::from([
                    (
                        "event".to_string(),
                        vec![inst("shared_ev", 1), inst("a_only", 2)],
                    ),
                    ("tech".to_string(), vec![inst("a_tech", 3)]),
                ]),
            );
            if include_b {
                idx.merge(
                    "file://b.txt",
                    HashMap::from([("event".to_string(), vec![inst("shared_ev", 5)])]),
                );
            }
            idx.merge_with_uris(vec![
                (
                    "event".to_string(),
                    vec![
                        (Arc::<str>::from("file://c.txt"), inst("shared_ev", 7)),
                        (Arc::<str>::from("file://d.txt"), inst("d_ev", 8)),
                    ],
                ),
                (
                    "event.subt".to_string(),
                    vec![(Arc::<str>::from("file://c.txt"), inst("shared_ev", 7))],
                ),
            ]);
            idx
        };

        let mut full = build(true);
        full.remove_file("file://b.txt");
        assert_eq!(snapshot(&full), snapshot(&build(false)));
        assert!(full.contains("event", "shared_ev"));
        assert!(full.is_any_instance("shared_ev"));
    }

    /// Removing a `merge_with_uris`-contributed file must likewise match a
    /// rebuild without it. Exercises the per-instance-uri insertion path, whose
    /// reverse-map bookkeeping differs from the single-uri path.
    #[test]
    fn remove_file_parity_removing_merge_with_uris_file() {
        let build = |include_c: bool| -> TypeIndex {
            let mut idx = TypeIndex::new();
            idx.merge(
                "file://a.txt",
                HashMap::from([("event".to_string(), vec![inst("shared_ev", 1)])]),
            );
            let mut batch = vec![(
                "event".to_string(),
                vec![(Arc::<str>::from("file://d.txt"), inst("d_ev", 8))],
            )];
            if include_c {
                batch.push((
                    "event".to_string(),
                    vec![(Arc::<str>::from("file://c.txt"), inst("shared_ev", 7))],
                ));
                batch.push((
                    "event.subt".to_string(),
                    vec![(Arc::<str>::from("file://c.txt"), inst("shared_ev", 7))],
                ));
            }
            idx.merge_with_uris(batch);
            idx
        };

        let mut full = build(true);
        full.remove_file("file://c.txt");
        assert_eq!(snapshot(&full), snapshot(&build(false)));
        // c was the only source of the subtype membership.
        assert!(!full.contains("event.subt", "shared_ev"));
        assert!(full.contains("event", "shared_ev")); // still via a
    }

    /// merge → remove → re-merge → remove cycles leave the index bit-empty each
    /// time, including the reverse map, with empty buckets pruned.
    #[test]
    fn merge_remove_remerge_cycles_stay_clean() {
        let mut idx = TypeIndex::new();
        let payload = || HashMap::from([("event".to_string(), vec![inst("ev", 1)])]);
        for _ in 0..3 {
            idx.merge("file://x.txt", payload());
            assert!(idx.contains("event", "ev"));
            idx.remove_file("file://x.txt");
            assert!(!idx.contains("event", "ev"));
            assert!(!idx.is_any_instance("ev"));
            assert!(idx.instances("event").is_empty());
            assert!(
                !idx.map.contains_key("event"),
                "empty bucket must be pruned"
            );
        }
        assert!(idx.map.is_empty());
        assert!(idx.name_counts.is_empty());
        assert!(idx.instance_sets.is_empty());
    }

    /// Bulk `remove_files` followed by a singular `remove_file`: the vanilla
    /// batch drops in one pass, then the last mod file drops, leaving nothing.
    #[test]
    fn remove_files_bulk_then_remove_file_singular() {
        let mut idx = TypeIndex::new();
        idx.merge_with_uris(vec![(
            "event".to_string(),
            vec![
                (Arc::<str>::from("v1"), inst("e1", 1)),
                (Arc::<str>::from("v2"), inst("e2", 2)),
            ],
        )]);
        idx.merge(
            "m.txt",
            HashMap::from([("event".to_string(), vec![inst("me", 3)])]),
        );

        let mut bulk = HashSet::new();
        bulk.insert(Arc::<str>::from("v1"));
        bulk.insert(Arc::<str>::from("v2"));
        idx.remove_files(&bulk);
        assert!(!idx.contains("event", "e1"));
        assert!(!idx.contains("event", "e2"));
        assert!(idx.contains("event", "me"));

        idx.remove_file("m.txt");
        assert!(!idx.contains("event", "me"));
        assert!(idx.map.is_empty());
        assert!(idx.name_counts.is_empty());
        assert!(idx.instance_sets.is_empty());
    }

    /// Removing a URI that never contributed anything is a no-op.
    #[test]
    fn remove_file_with_no_entries_is_noop() {
        let mut idx = TypeIndex::new();
        idx.merge(
            "file://a.txt",
            HashMap::from([("event".to_string(), vec![inst("ev", 1)])]),
        );
        let before = snapshot(&idx);
        idx.remove_file("file://never-merged.txt");
        assert_eq!(before, snapshot(&idx));
        assert!(idx.contains("event", "ev"));
    }

    /// A subtype-qualified membership key never feeds `name_counts`, so removing
    /// the file must drive the base name's count to zero exactly once.
    #[test]
    fn subtype_key_removal_preserves_name_counts_exemption() {
        let mut idx = TypeIndex::new();
        idx.merge_with_uris(vec![
            (
                "event".to_string(),
                vec![(Arc::<str>::from("f.txt"), inst("ev", 1))],
            ),
            (
                "event.subt".to_string(),
                vec![(Arc::<str>::from("f.txt"), inst("ev", 1))],
            ),
        ]);
        assert_eq!(idx.name_counts.get("ev").copied(), Some(1));
        assert!(idx.contains("event.subt", "ev"));
        idx.remove_file("f.txt");
        assert!(!idx.name_counts.contains_key("ev"));
        assert!(idx.map.is_empty());
        assert!(idx.instance_sets.is_empty());
    }

    #[test]
    fn file_index_resolves_reference_relative_to_asset_dir() {
        // A sound `.asset` `file =` resolves beside the .asset, not under the
        // field's `sound/` root prefix. The referencing file's path is absolute;
        // its root-relative dir is recovered as the longest indexed path-suffix.
        let mut fi = FileIndex::new();
        fi.add_paths([
            "sound/zom/zom_vo.asset".to_string(),
            "sound/zom/zom_idle_001.wav".to_string(),
        ]);

        assert!(
            fi.resolve_relative(
                "/home/user/Millennium-Dawn/sound/zom/zom_vo.asset",
                "zom_idle_001.wav"
            ),
            "a sibling beside the .asset should resolve"
        );
        assert!(
            !fi.resolve_relative(
                "/home/user/Millennium-Dawn/sound/zom/zom_vo.asset",
                "ku_move_007.wav"
            ),
            "a genuinely-missing sibling must not resolve"
        );
    }
}
