use cwtools_parser::ast::{Arena, Child, ParsedFile, Value};
use cwtools_rules::rules_types::{
    NewField, PathOptions, RuleSet, RuleType, SkipRootKey, TypeDefinition,
};
use cwtools_string_table::string_table::StringTable;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

pub mod dynamic_values;
pub mod vanilla_cache;

// ── Shared helpers ────────────────────────────────────────────────────────────

/// Strip one layer of surrounding double-quotes, if present.
fn unquote(s: &str) -> &str {
    s.strip_prefix('"')
        .and_then(|t| t.strip_suffix('"'))
        .unwrap_or(s)
}

/// Extract a plain string from a leaf value.
pub fn leaf_value_string(value: &Value, table: &StringTable) -> String {
    match value {
        Value::String(t) | Value::QString(t) => table.get_string(t.normal).unwrap_or_default(),
        Value::Float(f) => f.to_string(),
        Value::Int(i) => i.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Clause(_) => String::new(),
    }
}

/// Call `f` with the leaf value as a `&str`. String-typed values borrow straight
/// from the string table (no allocation); numeric/bool values are formatted into
/// a scratch buffer that is owned by this call. Clauses yield `""`. Internal
/// allocation-free counterpart to [`leaf_value_string`] for the index collectors.
fn with_leaf_value_str<R>(value: &Value, table: &StringTable, f: impl FnOnce(&str) -> R) -> R {
    match value {
        Value::String(t) | Value::QString(t) => {
            // `Some` already invoked `f`; a `None` (out-of-range id) maps to `""`,
            // matching `get_string(..).unwrap_or_default()`.
            let mut f = Some(f);
            match table.with_string(t.normal, |s| (f.take().unwrap())(s)) {
                Some(r) => r,
                None => (f.take().unwrap())(""),
            }
        }
        Value::Float(n) => f(&n.to_string()),
        Value::Int(i) => f(&i.to_string()),
        Value::Bool(b) => f(&b.to_string()),
        Value::Clause(_) => f(""),
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
    pub fn normalize_into(raw: &str, buf: &mut String) {
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
        if let Some(count) = self.names.get_mut(&n) {
            *count -= 1;
            if *count == 0 {
                self.names.remove(&n);
            }
        }
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

/// Whether an index key is a subtype-qualified membership key (`"type.subtype"`,
/// produced by [`SubtypeCollector`]) rather than a plain `type` key. The `.`
/// separator is the discriminator: CWT `type[...]` names are bare identifiers and
/// never contain a dot, so any key with one is a subtype membership entry. Such
/// keys feed `contains` (so `<type.subtype>` references resolve) but are kept out
/// of `name_counts` / document-symbol output. This invariant is relied on in
/// `merge`, `remove_file`, `instances_in_file`, and the validator's CW500 check,
/// so it lives here in one place.
pub fn is_subtype_key(type_name: &str) -> bool {
    type_name.contains('.')
}

#[derive(Debug, Default)]
pub struct TypeIndex {
    /// type_name → Vec<(file_uri, instance)>
    pub map: HashMap<String, Vec<(Arc<str>, TypeInstance)>>,
    /// lowercased instance name → how many definitions carry that name (across all
    /// types and files). Lets `is_any_instance` be O(1) instead of scanning every
    /// instance. A refcount so `remove_file` can drop a name only when its last
    /// definition goes. Keyed lowercase because Paradox identifiers are
    /// case-insensitive (same normalization as `contains`/`instance_sets`).
    name_counts: HashMap<String, usize>,
    /// type_name → (lowercased instance name → refcount). Makes `contains` an O(1)
    /// hash lookup instead of a linear scan over every instance of the type, which
    /// was quadratic over the corpus for high-cardinality types (state, character,
    /// country_event). The refcount lets `remove_file` drop a name only when its
    /// last definition in that type goes.
    instance_sets: HashMap<String, HashMap<String, usize>>,
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
        self.instance_sets
            .get(type_name)
            .map(|names| names.contains_key(&instance.to_ascii_lowercase()))
            .unwrap_or(false)
    }

    /// Return true if `name` is a known instance of ANY type. Used to recognise
    /// scope-opening keys: HOI4 from-data scope links (links.cwt) let an instance
    /// of a referenced type (character, state, ideology, ...) open its own scope,
    /// e.g. `LBA_some_character = { ... }`.
    pub fn is_any_instance(&self, name: &str) -> bool {
        self.name_counts.contains_key(&name.to_ascii_lowercase())
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
        self.map.get(type_name)?.iter().find_map(|(_, inst)| {
            if inst.name.eq_ignore_ascii_case(name) {
                inst.primary_loc_key.as_deref()
            } else {
                None
            }
        })
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
    pub fn loc_bindable_names_iter(&self) -> impl Iterator<Item = &str> + '_ {
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

    /// Remove all instances contributed by `file_uri`.
    pub fn remove_file(&mut self, file_uri: &str) {
        self.complex_enum_values.remove_file(file_uri);
        self.value_set_values.remove_file(file_uri);
        for (type_name, v) in self.map.iter_mut() {
            // Subtype-qualified keys never contributed to `name_counts` (see
            // `merge`), so they must not decrement it here.
            let subtype_key = is_subtype_key(type_name);
            v.retain(|(uri, inst)| {
                let keep = uri.as_ref() != file_uri;
                if !keep {
                    let lower = inst.name.to_ascii_lowercase();
                    if !subtype_key && let Some(count) = self.name_counts.get_mut(&lower) {
                        *count -= 1;
                        if *count == 0 {
                            self.name_counts.remove(&lower);
                        }
                    }
                    if let Some(set) = self.instance_sets.get_mut(type_name)
                        && let Some(count) = set.get_mut(&lower)
                    {
                        *count -= 1;
                        if *count == 0 {
                            set.remove(&lower);
                        }
                    }
                }
                keep
            });
        }
        self.map.retain(|_, v| !v.is_empty());
        self.instance_sets.retain(|_, names| !names.is_empty());
    }
}

// ── Path matching ─────────────────────────────────────────────────────────────

/// True if `needle` occurs in `haystack` as a whole path segment (or run of
/// segments), e.g. `gfx/models` is contained in `dlc/dlc022/gfx/models/units`.
/// Both inputs must already be lowercased and use '/' separators. This is THE
/// segment scan for both the indexer and the validator
/// (`cwtools_validation::resolve` imports it), so a file is INDEXED by the same
/// type that VALIDATES it. A bare `starts_with` would miss base-game content
/// nested under `dlc/<id>/…`, leaving its instances unindexed while the
/// referencing files still validate (false CW500s).
pub fn path_contains_segment(haystack: &str, needle: &str) -> bool {
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
        // Advance by the char width at `abs` to avoid splitting a multi-byte
        // UTF-8 sequence (paths are ASCII-dominated but latent on non-Latin dirs).
        let char_width = haystack[abs..].chars().next().map_or(1, char::len_utf8);
        start = abs + char_width;
        if start >= haystack.len() {
            break;
        }
    }
    false
}

/// The one per-pattern directory test shared by the indexer (`check_path_dir`)
/// and the validator (`find_type_by_path_and_key` / `type_path_matches`).
/// `path_strict` means the file sits DIRECTLY in the pattern directory: the dir
/// must equal the pattern or end with `/<pattern>` (so base-game content nested
/// under `dlc/<id>/…` still matches). Non-strict allows the pattern anywhere as
/// a whole segment run. Both inputs must be lowercased, '/'-separated, with no
/// trailing slash.
pub fn dir_matches_pattern(dir_lower: &str, pat_lower: &str, strict: bool) -> bool {
    if strict {
        dir_lower == pat_lower
            || (dir_lower.len() > pat_lower.len()
                && dir_lower.ends_with(pat_lower)
                && dir_lower.as_bytes()[dir_lower.len() - pat_lower.len() - 1] == b'/')
    } else {
        path_contains_segment(dir_lower, pat_lower)
    }
}

/// Returns true when `logical_path` (e.g. `"events/my_events.txt"`) is covered
/// by `path_options`. The directory must equal the pattern when `path_strict`,
/// else contain it as a path segment (so base-game content nested under
/// `dlc/<id>/…` is indexed by the same type that validates it).
///
/// Also enforces `path_file` (exact filename match) and `path_extension` (extension
/// match), mirroring the validator's `find_type_by_path_and_key` behaviour.
pub fn check_path_dir(opts: &PathOptions, logical_path: &str) -> bool {
    // Normalise separators and split into directory and filename.
    let norm = logical_path.replace('\\', "/");
    let basename = norm.rsplit('/').next().unwrap_or(&norm);
    let basename_lower = basename.to_lowercase();

    // path_file: exact filename constraint (precomputed by reindex when available).
    if let Some(pf_lower) = &opts.path_file_lower {
        if basename_lower != *pf_lower {
            return false;
        }
    } else if let Some(pf) = &opts.path_file
        && basename_lower != pf.to_lowercase()
    {
        return false;
    }

    // path_extension: file extension constraint (precomputed by reindex when available).
    let check_ext = |ext: &str| {
        if !ext.is_empty() {
            let has_ext = basename_lower.rsplit('.').next().is_some_and(|e| e == ext);
            if !has_ext {
                return false;
            }
        }
        true
    };
    if let Some(ext) = &opts.path_ext_lower {
        if !check_ext(ext) {
            return false;
        }
    } else if let Some(ext) = &opts.path_extension {
        let ext = ext.to_lowercase();
        let ext = ext.strip_prefix('.').unwrap_or(&ext);
        if !check_ext(ext) {
            return false;
        }
    }

    if opts.paths.is_empty() {
        return true;
    }

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
            if dir_matches_pattern(&dir_lower, &pat_lower, opts.path_strict) {
                return true;
            }
        }
        return false;
    }

    for pat_lower in &opts.paths_lower {
        if dir_matches_pattern(&dir_lower, pat_lower, opts.path_strict) {
            return true;
        }
    }
    false
}

// ── skip_root_key helper ─────────────────────────────────────────────────────

/// Does this `skip_root_key` rule match `key`? Case-insensitive (matching the
/// engine), and honours the `should_match` negation flag on `MultipleKeys`.
/// Shared with the validator (cwtools_validation::resolve) so indexing and
/// validation agree on which root keys to skip.
pub fn skip_root_key_matches(srk: &SkipRootKey, key: &str) -> bool {
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
        // Paradox keys/prefixes are ASCII identifiers; an ASCII case-insensitive
        // prefix test matches `to_lowercase().starts_with(to_lowercase())` without
        // allocating a lowercased copy of either string per call.
        Some(prefix) => {
            key.len() >= prefix.len()
                && key.as_bytes()[..prefix.len()].eq_ignore_ascii_case(prefix.as_bytes())
        }
    }
}

// ── Collect instances from a single node under skip_root_key navigation ──────

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
/// insensitive), unquoted. Mirrors the `name_field` extraction in
/// [`instance_name_from_children`].
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
        Some(field_name) => {
            // The instance name comes from a child leaf whose key equals `name_field`.
            // Quoted values (e.g. spriteType `name = "GFX_x"`) are stored with their
            // quotes, so strip them to match unquoted references like `icon = GFX_x`.
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
    }
}

/// Recurse through skip_root_key layers, then collect matching instances.
/// `child` is a single top-level child (must be a keyed clause).
fn collect_skip_root_child(
    td: &TypeDefinition,
    skip_stack: &[SkipRootKey],
    child: &Child,
    arena: &Arena,
    table: &StringTable,
    out: &mut Vec<TypeInstance>,
) {
    let Some(kc) = arena.keyed_clause(child) else {
        return; // not a keyed clause — skip
    };
    let (clause_children, start_line, start_col) =
        (kc.children, kc.pos.start.line, kc.pos.start.col);

    table.with_string(kc.key.normal, |key| match skip_stack {
        [] => {
            // We are at the instance node.
            if type_key_filter_matches(td, key)
                && starts_with_matches(td, key)
                && let Some(name) =
                    instance_name_from_children(td, key, clause_children, arena, table)
            {
                // Capture the explicit-field primary loc key (e.g. an event's
                // `title`) so hover can resolve the localised title cross-file.
                let primary_loc_key = primary_explicit_loc_field(td).and_then(|field| {
                    field_value_from_children(field, clause_children, arena, table)
                });
                out.push(TypeInstance {
                    name,
                    location: SourceLocation {
                        line: start_line,
                        col: start_col,
                    },
                    primary_loc_key,
                });
            }
        }
        [head, tail @ ..] => {
            // Must match the skip-root layer; then descend into children.
            if skip_root_key_matches(head, key) {
                for inner_child in clause_children {
                    collect_skip_root_child(td, tail, inner_child, arena, table, out);
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
    for td in &ruleset.types {
        if td.type_per_file
            || td.subtypes.is_empty()
            || !check_path_dir(&td.path_options, logical_path)
        {
            continue;
        }
        for child in &file.root_children {
            walk_instance_node(td, &td.skip_root_key, child, &file.arena, table, f);
        }
    }
}

fn walk_instance_node<F>(
    td: &TypeDefinition,
    skip_stack: &[SkipRootKey],
    child: &Child,
    arena: &Arena,
    table: &StringTable,
    f: &mut F,
) where
    F: FnMut(&TypeDefinition, &str, &str, &[Child], SourceLocation),
{
    let Some(kc) = arena.keyed_clause(child) else {
        return;
    };
    let clause_children = kc.children;
    let location = SourceLocation {
        line: kc.pos.start.line,
        col: kc.pos.start.col,
    };
    table.with_string(kc.key.normal, |key| match skip_stack {
        [] => {
            if type_key_filter_matches(td, key)
                && starts_with_matches(td, key)
                && let Some(name) =
                    instance_name_from_children(td, key, clause_children, arena, table)
            {
                f(td, &name, key, clause_children, location);
            }
        }
        [head, tail @ ..] => {
            if skip_root_key_matches(head, key) {
                for inner_child in clause_children {
                    walk_instance_node(td, tail, inner_child, arena, table, f);
                }
            }
        }
    });
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

    for td in &ruleset.types {
        // Path filter (mirrors CheckPathDir)
        if !check_path_dir(&td.path_options, logical_path) {
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
                location: SourceLocation { line: 1, col: 0 },
                // type_per_file types have no node body to read a field from.
                primary_loc_key: None,
            });
        } else {
            // Walk the file's top-level keyed clauses.
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
    /// The value assigned at this definition site, when the rule shape provides
    /// one (`set_variable = { var = x value = 5 }` or shorthand
    /// `set_variable = { x = 5 }`). `None` when no value is statically known.
    pub value: Option<String>,
}

// (collect_defined_variables and collect_vars_recursive deleted: no production
//  callers; collect_defined_variables_from_rules is the production entry point,
//  and collect_at_vars covers the @-prefix path without the duplicate walk.)

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

    // Index the root TypeRules by name once so the per-type lookup is O(1) instead
    // of an O(types × root_rules) linear scan. A type name can carry more than one
    // TypeRule, so group them (each matching rule is still scanned, preserving the
    // original multiplicity).
    let mut type_rules: HashMap<&str, Vec<&RuleType>> = HashMap::new();
    for root_rule in &ruleset.root_rules {
        if let cwtools_rules::rules_types::RootRule::TypeRule(name, (rule_type, _opts)) = root_rule
        {
            type_rules.entry(name.as_str()).or_default().push(rule_type);
        }
    }

    // Walk type instances (path-filtered) and scan their rules for VariableSetField
    for td in &ruleset.types {
        if !check_path_dir(&td.path_options, logical_path) {
            continue;
        }
        let Some(rules_for_type) = type_rules.get(td.name.as_str()) else {
            continue;
        };
        for rule_type in rules_for_type {
            if let RuleType::NodeRule { rules, .. } = rule_type {
                // Scan each root instance's children against these rules.
                for child in &file.root_children {
                    if let Some(kc) = file.arena.keyed_clause(child) {
                        scan_children_for_varset(
                            kc.children,
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

    result
}

fn collect_at_vars(
    children: &[Child],
    arena: &Arena,
    table: &StringTable,
    out: &mut HashMap<String, Vec<DefinedVariable>>,
) {
    for child in children {
        if let Child::Leaf(idx) = child {
            let leaf = &arena.leaves[*idx as usize];
            let key = table.get_string(leaf.key.normal).unwrap_or_default();
            if key.starts_with('@') {
                let value = leaf_value_string(&leaf.value, table);
                out.entry("@".to_string())
                    .or_default()
                    .push(DefinedVariable {
                        name: key.clone(),
                        namespace: None,
                        location: SourceLocation {
                            line: leaf.pos.start.line,
                            col: leaf.pos.start.col,
                        },
                        value: (!value.is_empty()).then_some(value),
                    });
            }
            if let Value::Clause(ch) = &leaf.value {
                collect_at_vars(ch, arena, table, out);
            }
        }
    }
}

/// The value of a `value`/`amount`/`add` child leaf in `children`, used to
/// recover the assigned value for the explicit `var = X / value = Y` form.
fn sibling_value_in_children(
    children: &[Child],
    arena: &Arena,
    table: &StringTable,
) -> Option<String> {
    for child in children {
        if let Child::Leaf(li) = child {
            let leaf = &arena.leaves[*li as usize];
            let is_value_key = table
                .with_string(leaf.key.normal, |k| {
                    matches!(k.to_ascii_lowercase().as_str(), "value" | "amount" | "add")
                })
                .unwrap_or(false);
            if is_value_key {
                let v = leaf_value_string(&leaf.value, table);
                if !v.is_empty() {
                    return Some(v);
                }
            }
        }
    }
    None
}

fn scan_children_for_varset(
    children: &[Child],
    arena: &Arena,
    table: &StringTable,
    rules: &[(
        cwtools_rules::rules_types::RuleType,
        cwtools_rules::rules_types::Options,
    )],
    out: &mut HashMap<String, Vec<DefinedVariable>>,
) {
    // For the explicit `var = value_set[variable] / value = variable_field` form
    // the assigned value lives in a sibling `value` leaf of the same block. Find
    // it once so the var-defining leaf can record it.
    let sibling_value = sibling_value_in_children(children, arena, table);
    for child in children {
        // A keyed clause (`key = { ... }`) takes the NodeRule path.
        if let Some(kc) = arena.keyed_clause(child) {
            // Resolve the clause key with `get_string` (which releases the table
            // lock) rather than holding `with_string` across the recursive
            // `scan_children_for_varset` calls below — those re-acquire the table
            // lock, which would risk a re-entrant read-lock deadlock under writer
            // contention during parallel indexing.
            let child_key = table.get_string(kc.key.normal).unwrap_or_default();
            for (rule_type, _) in rules {
                // NodeRule(VariableSetField): the clause's key IS the defined
                // variable name (F# InfoService fNode).
                if let RuleType::NodeRule {
                    left: NewField::VariableSetField(ns),
                    ..
                } = rule_type
                {
                    if !child_key.is_empty() {
                        out.entry(ns.clone()).or_default().push(DefinedVariable {
                            name: child_key.clone(),
                            namespace: Some(ns.clone()),
                            location: SourceLocation {
                                line: kc.pos.start.line,
                                col: kc.pos.start.col,
                            },
                            value: None,
                        });
                    }
                } else if let RuleType::NodeRule {
                    left: NewField::SpecificField(expected_key),
                    rules: inner,
                    ..
                } = rule_type
                {
                    // Only recurse when the child's key matches the rule's
                    // expected key. Previously ALL NodeRules were applied to
                    // every child node, recording junk variable names.
                    if child_key.eq_ignore_ascii_case(expected_key) {
                        scan_children_for_varset(kc.children, arena, table, inner, out);
                    }
                } else if let RuleType::NodeRule { rules: inner, .. } = rule_type {
                    // Non-SpecificField node rule (e.g. alias or scalar key):
                    // recurse unconditionally as before.
                    scan_children_for_varset(kc.children, arena, table, inner, out);
                }
            }
            continue;
        }
        match child {
            Child::Leaf(li) => {
                let leaf = &arena.leaves[*li as usize];
                // Resolve key and value sequentially (each releases the table lock)
                // rather than nesting two `with_string` borrows, which would risk a
                // re-entrant read-lock deadlock under writer contention.
                let key = table.get_string(leaf.key.normal).unwrap_or_default();
                let val = leaf_value_string(&leaf.value, table);
                for (rule_type, _opts) in rules {
                    match rule_type {
                        // left = VariableSetField: the leaf's key IS the defined
                        // variable name, and its RHS is the assigned value
                        // (shorthand `set_variable = { my_var = 5 }`). Only applies
                        // when the rule's left is a pure variable-set field (no
                        // specific key to match against).
                        RuleType::LeafRule {
                            left: NewField::VariableSetField(ns),
                            ..
                        } => {
                            out.entry(ns.clone()).or_default().push(DefinedVariable {
                                name: key.clone(),
                                namespace: Some(ns.clone()),
                                location: SourceLocation {
                                    line: leaf.pos.start.line,
                                    col: leaf.pos.start.col,
                                },
                                value: (!val.is_empty()).then(|| val.clone()),
                            });
                        }
                        // right = VariableSetField: the leaf's VALUE is the defined
                        // variable name (explicit `var = my_var`), but only when the
                        // leaf's key matches the rule's expected key (SpecificField).
                        // The assigned value comes from the sibling `value` leaf.
                        RuleType::LeafRule {
                            left: NewField::SpecificField(expected_key),
                            right: NewField::VariableSetField(ns),
                        } if !val.is_empty() && key.eq_ignore_ascii_case(expected_key) => {
                            out.entry(ns.clone()).or_default().push(DefinedVariable {
                                name: val.clone(),
                                namespace: Some(ns.clone()),
                                location: SourceLocation {
                                    line: leaf.pos.start.line,
                                    col: leaf.pos.start.col,
                                },
                                value: sibling_value.clone(),
                            });
                        }
                        _ => {}
                    }
                }
            }
            // LeafValueRule(VariableSetField): a bare value inside a block is the
            // defined variable name (F# InfoService fLeafValue).
            Child::LeafValue(lvi) => {
                let lv = &arena.leaf_values[*lvi as usize];
                with_leaf_value_str(&lv.value, table, |val| {
                    if !val.is_empty() {
                        for (rule_type, _opts) in rules {
                            if let RuleType::LeafValueRule {
                                right: NewField::VariableSetField(ns),
                            } = rule_type
                            {
                                out.entry(ns.clone()).or_default().push(DefinedVariable {
                                    name: val.to_string(),
                                    namespace: Some(ns.clone()),
                                    location: SourceLocation {
                                        line: lv.pos.start.line,
                                        col: lv.pos.start.col,
                                    },
                                    value: None,
                                });
                            }
                        }
                    }
                });
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
    let mut defs = Vec::new();
    collect_set_variable_defs(file, table, effects, &mut defs);
    out.extend(defs.into_iter().map(|d| d.name));
}

/// Like [`collect_set_variable_names`] but keeps each definition's source
/// location and, where the block provides one, its assigned value (the `value`
/// child for the explicit form, or the RHS for the shorthand form). Used by the
/// LSP so hover/goto can point at a variable's definition and show its value.
pub fn collect_set_variable_defs(
    file: &ParsedFile,
    table: &StringTable,
    effects: &HashSet<String>,
    out: &mut Vec<DefinedVariable>,
) {
    fn def(name: String, value: Option<String>, line: u32, col: u16) -> DefinedVariable {
        DefinedVariable {
            name,
            namespace: Some("variable".to_string()),
            location: SourceLocation { line, col },
            value,
        }
    }

    fn extract(
        children: &[Child],
        arena: &Arena,
        table: &StringTable,
        out: &mut Vec<DefinedVariable>,
    ) {
        // Explicit form: a `var`/`variable` child holds the name as its value;
        // the assigned value (if any) is the sibling `value`/`amount`/`add` leaf.
        let mut explicit = false;
        let sibling_value = sibling_value_in_children(children, arena, table);
        for child in children {
            if let Child::Leaf(li) = child {
                let leaf = &arena.leaves[*li as usize];
                let is_var_key = table
                    .with_string(leaf.key.normal, |k| {
                        k.eq_ignore_ascii_case("var") || k.eq_ignore_ascii_case("variable")
                    })
                    .unwrap_or(false);
                if is_var_key {
                    let v = leaf_value_string(&leaf.value, table);
                    if !v.is_empty() {
                        out.push(def(
                            v,
                            sibling_value.clone(),
                            leaf.pos.start.line,
                            leaf.pos.start.col,
                        ));
                    }
                    explicit = true;
                }
            }
        }
        if explicit {
            return;
        }
        // Shorthand form: the inner assignment key is the variable name and its
        // RHS (if a leaf) is the assigned value.
        for child in children {
            let (key, value, line, col) = match child {
                Child::Leaf(li) => {
                    let leaf = &arena.leaves[*li as usize];
                    let k = table.get_string(leaf.key.normal).unwrap_or_default();
                    let v = leaf_value_string(&leaf.value, table);
                    (
                        k,
                        (!v.is_empty()).then_some(v),
                        leaf.pos.start.line,
                        leaf.pos.start.col,
                    )
                }
                _ => continue,
            };
            const SKIP_KEYS: &[&str] = &["value", "tooltip", "var", "variable", "amount", "which"];
            // Case-insensitive compare without allocating a lowercased copy of the
            // key just to probe the skip-list (paradox keys are ASCII).
            if !SKIP_KEYS.iter().any(|k| key.eq_ignore_ascii_case(k)) {
                out.push(def(key, value, line, col));
            }
        }
    }

    fn walk(
        children: &[Child],
        arena: &Arena,
        table: &StringTable,
        effects: &HashSet<String>,
        out: &mut Vec<DefinedVariable>,
    ) {
        for child in children {
            if let Child::Leaf(li) = child {
                let leaf = &arena.leaves[*li as usize];
                if let Value::Clause(ch) = &leaf.value {
                    let in_effects = table
                        .with_string(leaf.key.normal, |k| {
                            effects.contains(k.to_ascii_lowercase().as_str())
                        })
                        .unwrap_or(false);
                    if in_effects {
                        extract(ch, arena, table, out);
                    }
                    walk(ch, arena, table, effects, out);
                }
            }
        }
    }

    walk(&file.root_children, &file.arena, table, effects, out);
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

// collect_saved_event_targets and collect_event_targets_rec deleted:
// no production callers.

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
