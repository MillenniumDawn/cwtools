//! Persistent per-file parse cache for the workspace scan.
//!
//! Each file is keyed by a content hash (FNV-1a of the text). A `settings.sig`
//! file in the workspace cache directory records a fingerprint derived from the
//! game type, ruleset shape, and workspace root so the entire cache is cleared
//! automatically when any of those change.

use std::fs;
use std::path::{Path, PathBuf};

use cwtools_cache::convert::{archived_to_arena, arena_to_cached};
use cwtools_cache::io::{serialize_to_file, with_archived_file};
use cwtools_parser::ast::ParsedFile;
use cwtools_rules::rules_types::RuleSet;
use cwtools_string_table::string_table::StringTable;

/// Cache format version. Bump when the `CachedFile` layout changes (or the
/// fingerprint algorithm changes) so stale `.cwb` files are ignored
/// automatically.
///
/// v2: switched fingerprinting from `DefaultHasher` (SipHash, toolchain-unstable)
/// to a stable FNV-1a. The version is folded into the fingerprint, so old
/// SipHash-keyed cache directories no longer match and are treated as a miss
/// (one-time cold rebuild).
/// v3: dropped `CachedNode`/`CachedChild::Node` from the `CachedFile` layout.
const CACHE_VERSION: u32 = 3;

// ── Fingerprinting ──────────────────────────────────────────────────────────

/// FNV-1a over `bytes`, continuing from `hash`. A stable, dependency-free hash
/// (unlike `std::hash::DefaultHasher`, whose SipHash output isn't guaranteed
/// stable across Rust toolchains) so cache keys stay comparable across restarts.
/// Mirrors `cwtools_info::vanilla_cache::fnv1a`.
fn fnv1a(bytes: &[u8], mut hash: u64) -> u64 {
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// FNV-1a offset basis — the conventional seed for a fresh hash.
const FNV_OFFSET: u64 = 0xcbf2_9ce4_8422_2325;

/// Content hash of a file's text. FNV-1a is fast for short-to-medium files and
/// the collision surface is tiny (local cache only, not security-critical).
pub fn content_hash(text: &str) -> u64 {
    fnv1a(text.as_bytes(), FNV_OFFSET)
}

/// Settings fingerprint: encodes everything that changes the parse or validation
/// output for a workspace. If the fingerprint differs from `settings.sig`, the
/// cached workspace directory is stale and must be cleared.
pub fn settings_fingerprint(language: &str, ruleset: &RuleSet, workspace_root: &Path) -> u64 {
    // A record separator between fields so concatenation is unambiguous
    // (`a` + `bc` can't collide with `ab` + `c`).
    let mut h = FNV_OFFSET;
    let sep = |h: u64| fnv1a(b"\x1e", h);
    // Game/language — changes scope definitions, keywords, etc.
    h = fnv1a(language.as_bytes(), h);
    h = sep(h);
    // Workspace root — distinguishes two mods opened in different windows that
    // happen to share the same ruleset.
    h = fnv1a(workspace_root.to_string_lossy().as_bytes(), h);
    h = sep(h);
    // Ruleset shape — we can't hash the full RuleSet cheaply (no Hash impl),
    // so hash the counts and names of its top-level components. This is a
    // fast approximation; if two rulesets have identical shape they almost
    // certainly produce identical parse/validation output.
    h = fnv1a(&ruleset.types.len().to_le_bytes(), h);
    for t in &ruleset.types {
        h = fnv1a(t.name.as_bytes(), h);
        h = sep(h);
    }
    h = fnv1a(&ruleset.aliases.len().to_le_bytes(), h);
    for (name, _) in &ruleset.aliases {
        h = fnv1a(name.as_bytes(), h);
        h = sep(h);
    }
    h = fnv1a(&ruleset.single_aliases.len().to_le_bytes(), h);
    for (name, _) in &ruleset.single_aliases {
        h = fnv1a(name.as_bytes(), h);
        h = sep(h);
    }
    h = fnv1a(&ruleset.enums.len().to_le_bytes(), h);
    for e in &ruleset.enums {
        h = fnv1a(e.key.as_bytes(), h);
        h = sep(h);
    }
    h = fnv1a(&ruleset.complex_enums.len().to_le_bytes(), h);
    h = fnv1a(&ruleset.root_rules.len().to_le_bytes(), h);
    h = fnv1a(&ruleset.modifiers.len().to_le_bytes(), h);
    h = fnv1a(&ruleset.link_inputs.len().to_le_bytes(), h);
    h = fnv1a(&ruleset.scope_inputs.len().to_le_bytes(), h);
    // Bump version together so a format change also invalidates.
    fnv1a(&CACHE_VERSION.to_le_bytes(), h)
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
    if let Err(e) = fs::create_dir_all(dir) {
        tracing::warn!(dir = %dir.display(), error = %e, "settings.sig: create_dir_all failed");
    }
    if let Err(e) = fs::write(settings_sig_path(dir), sig.to_le_bytes()) {
        tracing::warn!(dir = %dir.display(), error = %e, "settings.sig: write failed");
    }
}

/// Validate (and update) the settings signature. Returns `true` if the cache is
/// still valid; `false` if the directory was cleared and must be rebuilt.
///
/// Also sweeps sibling `parse-cache/<fp>/` directories that don't match the
/// current fingerprint so old workspaces don't accumulate forever on disk.
pub fn validate_or_clear(cache_dir: &Path, fingerprint: u64) -> bool {
    let dir = workspace_cache_dir(cache_dir, fingerprint);
    sweep_orphan_dirs(cache_dir, fingerprint);
    match read_settings_sig(&dir) {
        Some(stored) if stored == fingerprint => {
            // Valid cache: evict stale-content `.cwb` entries if the dir has
            // grown past the cap. (A cleared dir below is already empty.)
            prune_cache_dir(&dir);
            true
        }
        _ => {
            // Stale or missing — wipe the directory and recreate.
            if let Err(e) = fs::remove_dir_all(&dir)
                && e.kind() != std::io::ErrorKind::NotFound
            {
                tracing::warn!(dir = %dir.display(), error = %e, "cache reset: remove_dir_all failed");
            }
            if let Err(e) = fs::create_dir_all(&dir) {
                tracing::warn!(dir = %dir.display(), error = %e, "cache reset: create_dir_all failed");
            }
            write_settings_sig(&dir, fingerprint);
            false
        }
    }
}

/// Remove any `parse-cache/<old-fp>/` sibling directories whose fingerprint
/// hex name differs from `current_fingerprint`. This prevents old per-workspace
/// directories from accumulating on disk across ruleset/workspace-root changes.
fn sweep_orphan_dirs(cache_dir: &Path, current_fingerprint: u64) {
    let parse_cache_root = cache_dir.join("parse-cache");
    let current_hex = format!("{:016x}", current_fingerprint);
    let Ok(rd) = fs::read_dir(&parse_cache_root) else {
        return;
    };
    for entry in rd.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        if path
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|name| name != current_hex)
            && let Err(e) = fs::remove_dir_all(&path)
        {
            tracing::warn!(path = %path.display(), error = %e, "orphan cache dir sweep failed");
        }
    }
}

// ── Bounded cleanup ───────────────────────────────────────────────────────────

/// Cap on the number of `.cwb` entries kept in a single workspace cache dir.
/// Each file's content hash gets its own entry and nothing is evicted on edit,
/// so without a bound, every distinct version of every file accumulates forever.
const MAX_CACHE_ENTRIES: usize = 50_000;

/// Cap on total `.cwb` bytes in a single workspace cache dir.
const MAX_CACHE_BYTES: u64 = 2 * 1024 * 1024 * 1024; // 2 GiB

/// Prune to ~80% of the caps so we don't re-prune on every scan.
const PRUNE_TARGET_RATIO: f64 = 0.8;

/// If the cache dir exceeds either cap (entry count or total size), delete the
/// oldest `.cwb` entries by mtime until it's back under ~80% of both caps. One
/// directory scan; runs at most once per workspace scan. No-op when under cap.
fn prune_cache_dir(dir: &Path) {
    prune_cache_dir_with_caps(dir, MAX_CACHE_ENTRIES, MAX_CACHE_BYTES);
}

/// Cap-parameterized core of [`prune_cache_dir`] (lets tests use small caps
/// instead of writing 50k files).
fn prune_cache_dir_with_caps(dir: &Path, max_entries: usize, max_bytes: u64) {
    let Ok(rd) = fs::read_dir(dir) else {
        return;
    };
    // (mtime, size, path) for each `.cwb` entry.
    let mut entries: Vec<(std::time::SystemTime, u64, PathBuf)> = Vec::new();
    let mut total_bytes: u64 = 0;
    for entry in rd.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|e| e != "cwb") {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let size = meta.len();
        let mtime = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
        total_bytes += size;
        entries.push((mtime, size, path));
    }

    let over_count = entries.len() > max_entries;
    let over_bytes = total_bytes > max_bytes;
    if !over_count && !over_bytes {
        return;
    }

    // Oldest first, so we evict least-recently-written entries.
    entries.sort_by_key(|(mtime, _, _)| *mtime);

    let target_count = (max_entries as f64 * PRUNE_TARGET_RATIO) as usize;
    let target_bytes = (max_bytes as f64 * PRUNE_TARGET_RATIO) as u64;
    let mut cur_count = entries.len();
    let mut cur_bytes = total_bytes;
    let mut pruned_count = 0usize;
    let mut pruned_bytes = 0u64;

    for (_, size, path) in &entries {
        if cur_count <= target_count && cur_bytes <= target_bytes {
            break;
        }
        if fs::remove_file(path).is_ok() {
            cur_count -= 1;
            cur_bytes = cur_bytes.saturating_sub(*size);
            pruned_count += 1;
            pruned_bytes += *size;
        }
    }

    tracing::info!(
        dir = %dir.display(),
        pruned_entries = pruned_count,
        pruned_bytes,
        remaining_entries = cur_count,
        remaining_bytes = cur_bytes,
        "pruned parse cache (over {} entries or {} bytes)",
        max_entries,
        max_bytes,
    );
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

    // Zero-copy hit path: intern straight from the archived buffer instead of
    // materializing an owned CachedFile first (halves per-string allocation).
    with_archived_file(&path, |archived| {
        let (arena, root_children) = archived_to_arena(archived, table);
        ParsedFile {
            arena,
            root_children,
            errors: vec![],
        }
    })
    .ok()
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
    if let Err(e) = serialize_to_file(&cached, &path) {
        tracing::warn!(path = %path.display(), error = %e, "parse cache write failed");
    }
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
        assert_ne!(
            base,
            settings_fingerprint("hoi4", &rs, Path::new("/tmp/other"))
        );
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

    /// Cold (parse + store) vs warm (deserialize) over the real Millennium Dawn
    /// corpus. The cache only earns its keep if `load` beats `parse_string`.
    ///
    /// Ignored by default (needs the MD mod on disk + is slow). Run with:
    ///   cargo test -p cwtools_lsp --bin cwtools-server -- \
    ///     --ignored --nocapture bench_parse_cache_vs_parse
    #[test]
    #[ignore]
    fn bench_parse_cache_vs_parse() {
        use std::time::Instant;

        let root = Path::new("/mnt/Linux/Millennium-Dawn");
        if !root.exists() {
            eprintln!("SKIP: {} not present", root.display());
            return;
        }
        let mut files = Vec::new();
        for sub in ["common", "events", "history"] {
            collect_txt(&root.join(sub), &mut files);
        }
        let texts: Vec<String> = files
            .iter()
            .filter_map(|p| std::fs::read_to_string(p).ok())
            .collect();
        eprintln!("corpus: {} readable .txt files", texts.len());

        let table = StringTable::new();
        let tmp = tempfile::tempdir().unwrap();
        let fp = 0xabc;
        validate_or_clear(tmp.path(), fp);

        // Cold pass: parse + persist.
        let t0 = Instant::now();
        let mut parsed_ok = 0usize;
        for text in &texts {
            if let Ok(parsed) = parse_string(text, &table) {
                store(tmp.path(), fp, text, &parsed, &table);
                parsed_ok += 1;
            }
        }
        let cold = t0.elapsed();

        // Warm pass: deserialize from cache.
        let t1 = Instant::now();
        let mut hits = 0usize;
        for text in &texts {
            if load(tmp.path(), fp, text, &table).is_some() {
                hits += 1;
            }
        }
        let warm = t1.elapsed();

        eprintln!(
            "cold parse+store: {:.3}s ({} parsed)\nwarm load:        {:.3}s ({} hits)\nspeedup: {:.2}x",
            cold.as_secs_f64(),
            parsed_ok,
            warm.as_secs_f64(),
            hits,
            cold.as_secs_f64() / warm.as_secs_f64().max(1e-9),
        );
        assert!(hits > 0, "expected cache hits on the warm pass");
    }

    fn collect_txt(dir: &Path, out: &mut Vec<PathBuf>) {
        let Ok(rd) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in rd.flatten() {
            let p = entry.path();
            if p.is_dir() {
                collect_txt(&p, out);
            } else if p.extension().is_some_and(|e| e == "txt") {
                out.push(p);
            }
        }
    }

    #[test]
    fn prune_cache_dir_evicts_oldest_over_entry_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        let max_entries = 10usize;
        // Write more `.cwb` files than the (small) cap, staggered mtimes.
        let n = max_entries + 5;
        let base = std::time::SystemTime::now() - std::time::Duration::from_secs(n as u64 + 10);
        let mut paths = Vec::with_capacity(n);
        for i in 0..n {
            let p = dir.join(format!("{:016x}.cwb", i as u64));
            fs::write(&p, b"x").unwrap();
            filetime_set(&p, base + std::time::Duration::from_secs(i as u64));
            paths.push(p);
        }
        assert_eq!(count_cwb(dir), n);
        prune_cache_dir_with_caps(dir, max_entries, u64::MAX);
        let remaining = count_cwb(dir);
        // Pruned down to ~80% of the cap.
        let target = (max_entries as f64 * PRUNE_TARGET_RATIO) as usize;
        assert!(
            remaining <= target + 1,
            "pruned to {remaining}, want ~{target}"
        );
        // The oldest entries are gone; the newest survives.
        assert!(paths.last().unwrap().exists(), "newest entry was evicted");
        assert!(!paths.first().unwrap().exists(), "oldest entry survived");
    }

    #[test]
    fn prune_cache_dir_noop_under_cap() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        for i in 0..10u64 {
            fs::write(dir.join(format!("{:016x}.cwb", i)), b"x").unwrap();
        }
        prune_cache_dir_with_caps(dir, 50, u64::MAX);
        assert_eq!(count_cwb(dir), 10);
    }

    fn count_cwb(dir: &Path) -> usize {
        fs::read_dir(dir)
            .unwrap()
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "cwb"))
            .count()
    }

    fn filetime_set(path: &Path, t: std::time::SystemTime) {
        // Set mtime via `File::set_modified` (stable since 1.75), no extra crate.
        let f = fs::File::options().write(true).open(path).unwrap();
        f.set_modified(t).unwrap();
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
