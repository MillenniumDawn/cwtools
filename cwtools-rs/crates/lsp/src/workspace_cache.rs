//! Persistent per-file parse cache for the workspace scan.
//!
//! Each file is keyed by a content hash (FNV-1a of the text). A `settings.sig`
//! file in the workspace cache directory records a fingerprint derived from the
//! game type, ruleset shape, and workspace root so the entire cache is cleared
//! automatically when any of those change.

use std::collections::hash_map::DefaultHasher;
use std::fs;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};

use cwtools_cache::convert::{arena_to_cached, cached_to_arena};
use cwtools_cache::io::{deserialize_from_file, serialize_to_file};
use cwtools_parser::ast::ParsedFile;
use cwtools_rules::rules_types::RuleSet;
use cwtools_string_table::string_table::StringTable;

/// Cache format version. Bump when the `CachedFile` layout changes so stale
/// `.cwb` files are ignored automatically.
const CACHE_VERSION: u32 = 1;

// ── Fingerprinting ──────────────────────────────────────────────────────────

/// Content hash of a file's text. FNV-1a is fast for short-to-medium files and
/// the collision surface is tiny (local cache only, not security-critical).
pub fn content_hash(text: &str) -> u64 {
    let mut h = DefaultHasher::new();
    text.hash(&mut h);
    h.finish()
}

/// Settings fingerprint: encodes everything that changes the parse or validation
/// output for a workspace. If the fingerprint differs from `settings.sig`, the
/// cached workspace directory is stale and must be cleared.
pub fn settings_fingerprint(language: &str, ruleset: &RuleSet, workspace_root: &Path) -> u64 {
    let mut h = DefaultHasher::new();
    // Game/language — changes scope definitions, keywords, etc.
    language.hash(&mut h);
    // Workspace root — distinguishes two mods opened in different windows that
    // happen to share the same ruleset.
    workspace_root.hash(&mut h);
    // Ruleset shape — we can't hash the full RuleSet cheaply (no Hash impl),
    // so hash the counts and names of its top-level components. This is a
    // fast approximation; if two rulesets have identical shape they almost
    // certainly produce identical parse/validation output.
    ruleset.types.len().hash(&mut h);
    for t in &ruleset.types {
        t.name.hash(&mut h);
    }
    ruleset.aliases.len().hash(&mut h);
    for (name, _) in &ruleset.aliases {
        name.hash(&mut h);
    }
    ruleset.single_aliases.len().hash(&mut h);
    for (name, _) in &ruleset.single_aliases {
        name.hash(&mut h);
    }
    ruleset.enums.len().hash(&mut h);
    for e in &ruleset.enums {
        e.key.hash(&mut h);
    }
    ruleset.complex_enums.len().hash(&mut h);
    ruleset.root_rules.len().hash(&mut h);
    ruleset.modifiers.len().hash(&mut h);
    ruleset.link_inputs.len().hash(&mut h);
    ruleset.scope_inputs.len().hash(&mut h);
    // Bump version together so a format change also invalidates.
    CACHE_VERSION.hash(&mut h);
    h.finish()
}

// ── Directory layout ────────────────────────────────────────────────────────

/// Resolve the workspace parse-cache directory.
///
/// Layout: `<cache_dir>/parse-cache/<workspace-fingerprint-hex>/`
///
/// Returns `None` if no base cache dir can be resolved.
fn workspace_cache_dir(cache_dir: &Path, fingerprint: u64) -> PathBuf {
    cache_dir
        .join("parse-cache")
        .join(format!("{:016x}", fingerprint))
}

/// Path of the `settings.sig` file inside a workspace cache directory.
fn settings_sig_path(dir: &Path) -> PathBuf {
    dir.join("settings.sig")
}

/// Path of a per-file `.cwb` cache entry.
fn file_cache_path(dir: &Path, hash: u64) -> PathBuf {
    dir.join(format!("{:016x}.cwb", hash))
}

// ── Settings sig ────────────────────────────────────────────────────────────

/// Read the stored fingerprint from `settings.sig`. Returns `None` if the file
/// doesn't exist or can't be parsed.
fn read_settings_sig(dir: &Path) -> Option<u64> {
    let bytes = fs::read(settings_sig_path(dir)).ok()?;
    if bytes.len() != 8 {
        return None;
    }
    let mut buf = [0u8; 8];
    buf.copy_from_slice(&bytes);
    Some(u64::from_le_bytes(buf))
}

/// Write the current fingerprint to `settings.sig`.
fn write_settings_sig(dir: &Path, sig: u64) {
    let _ = fs::create_dir_all(dir);
    let _ = fs::write(settings_sig_path(dir), sig.to_le_bytes());
}

/// Validate (and update) the settings signature. Returns `true` if the cache is
/// still valid; `false` if the directory was cleared and must be rebuilt.
pub fn validate_or_clear(cache_dir: &Path, fingerprint: u64) -> bool {
    let dir = workspace_cache_dir(cache_dir, fingerprint);
    match read_settings_sig(&dir) {
        Some(stored) if stored == fingerprint => true,
        _ => {
            // Stale or missing — wipe the directory and recreate.
            let _ = fs::remove_dir_all(&dir);
            let _ = fs::create_dir_all(&dir);
            write_settings_sig(&dir, fingerprint);
            false
        }
    }
}

// ── Per-file load / store ───────────────────────────────────────────────────

/// Try to load a previously cached `ParsedFile` for `text`.
///
/// Returns `Some(ParsedFile)` on cache hit, `None` on miss.
pub fn load(
    cache_dir: &Path,
    fingerprint: u64,
    text: &str,
    table: &StringTable,
) -> Option<ParsedFile> {
    let dir = workspace_cache_dir(cache_dir, fingerprint);
    let hash = content_hash(text);
    let path = file_cache_path(&dir, hash);

    let cached = deserialize_from_file(&path).ok()?;
    let (arena, root_children) = cached_to_arena(&cached, table);
    Some(ParsedFile {
        arena,
        root_children,
        errors: vec![],
    })
}

/// Persist a successfully parsed (error-free) `ParsedFile` to the cache.
///
/// Files with parse errors are intentionally NOT cached — the user will edit
/// them, the content hash will change, and we'll re-parse. Caching error-free
/// files only keeps the hot path fast for the common case.
pub fn store(
    cache_dir: &Path,
    fingerprint: u64,
    text: &str,
    parsed: &ParsedFile,
    table: &StringTable,
) {
    // Don't cache files that had parse errors — diagnostics would be lost.
    if !parsed.errors.is_empty() {
        return;
    }
    let dir = workspace_cache_dir(cache_dir, fingerprint);
    let hash = content_hash(text);
    let path = file_cache_path(&dir, hash);

    let cached = arena_to_cached(&parsed.arena, &parsed.root_children, table);
    let _ = serialize_to_file(&cached, &path);
}

#[cfg(test)]
mod tests {
    use super::*;
    use cwtools_parser::ast::ParseError;
    use cwtools_parser::parser::parse_string;

    #[test]
    fn content_hash_is_deterministic_and_distinguishes() {
        assert_eq!(content_hash("foo = 1"), content_hash("foo = 1"));
        assert_ne!(content_hash("foo = 1"), content_hash("foo = 2"));
    }

    #[test]
    fn settings_fingerprint_stable_and_sensitive() {
        let rs = RuleSet::new();
        let root = Path::new("/tmp/ws");
        let base = settings_fingerprint("hoi4", &rs, root);
        // Identical inputs -> identical fingerprint.
        assert_eq!(base, settings_fingerprint("hoi4", &rs, root));
        // A language/game change must invalidate.
        assert_ne!(base, settings_fingerprint("stellaris", &rs, root));
        // A workspace-root change must invalidate.
        assert_ne!(base, settings_fingerprint("hoi4", &rs, Path::new("/tmp/other")));
    }

    #[test]
    fn validate_or_clear_first_miss_then_hit() {
        let tmp = tempfile::tempdir().unwrap();
        let fp = 0xdead_beef_u64;
        // No settings.sig yet -> not valid (dir created + sig written).
        assert!(!validate_or_clear(tmp.path(), fp));
        // Same fingerprint on the next scan -> valid.
        assert!(validate_or_clear(tmp.path(), fp));
    }

    #[test]
    fn store_then_load_round_trips() {
        let tmp = tempfile::tempdir().unwrap();
        let table = StringTable::new();
        let fp = 1234;
        validate_or_clear(tmp.path(), fp); // create the dir + sig
        let text = "foo = { bar = 1 baz = \"two\" }\n";
        let parsed = parse_string(text, &table).unwrap();

        // Miss before anything is stored.
        assert!(load(tmp.path(), fp, text, &table).is_none());

        store(tmp.path(), fp, text, &parsed, &table);

        // Hit after store, with equivalent structure and no errors.
        let loaded = load(tmp.path(), fp, text, &table).expect("expected a cache hit");
        assert_eq!(loaded.root_children.len(), parsed.root_children.len());
        assert!(loaded.errors.is_empty());
    }

    #[test]
    fn load_misses_on_changed_text() {
        let tmp = tempfile::tempdir().unwrap();
        let table = StringTable::new();
        let fp = 99;
        validate_or_clear(tmp.path(), fp);
        let text = "a = 1\n";
        let parsed = parse_string(text, &table).unwrap();
        store(tmp.path(), fp, text, &parsed, &table);
        // Edited content hashes to a different .cwb path -> miss (forces re-parse).
        assert!(load(tmp.path(), fp, "a = 2\n", &table).is_none());
    }

    #[test]
    fn store_skips_files_with_parse_errors() {
        let tmp = tempfile::tempdir().unwrap();
        let table = StringTable::new();
        let fp = 7;
        validate_or_clear(tmp.path(), fp);
        let text = "x = 1\n";
        let mut parsed = parse_string(text, &table).unwrap();
        parsed.errors.push(ParseError::General("boom".into()));
        store(tmp.path(), fp, text, &parsed, &table);
        // A file with parse errors must not be cached (diagnostics would be lost).
        assert!(load(tmp.path(), fp, text, &table).is_none());
    }

    #[test]
    fn different_fingerprint_uses_separate_cache() {
        let tmp = tempfile::tempdir().unwrap();
        let table = StringTable::new();
        let text = "k = 1\n";
        let parsed = parse_string(text, &table).unwrap();
        validate_or_clear(tmp.path(), 1);
        store(tmp.path(), 1, text, &parsed, &table);
        // Same text, different settings fingerprint -> different dir -> miss.
        assert!(load(tmp.path(), 2, text, &table).is_none());
        // The original fingerprint still hits.
        assert!(load(tmp.path(), 1, text, &table).is_some());
    }
}
