//! Pre-generated cache of base-game ("vanilla") data.
//!
//! Parsing and indexing a full game install on every run is slow, so the
//! vanilla data is built once and serialized here. Loading it resolves
//! references into base-game content (sprites, operation_tokens, equipment, …)
//! without re-parsing, and without validating vanilla files (which carry known
//! base-game errors we never want to report). Shared by the CLI
//! (`cache-vanilla` / `validate --vanilla-cache`) and the LSP server.
//!
//! Besides the type instances the cache also carries the vanilla loc-key sets
//! (per language), the vanilla file-path set (for CW113 `filepath` checks) and
//! the vanilla script-variable names, so a cache hit skips walking the install
//! for loc and file indexing too. Vanilla loc *entries* (command chains) are
//! NOT cached: the only consumer is the scope-aware command check on vanilla's
//! own content, which we never validate.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::path::Path;

use cwtools_rules::rules_types::{RuleSet, SkipRootKey};

use crate::{SourceLocation, TypeIndex, TypeInstance};

/// Magic bytes at the start of every vanilla cache file. Distinct from the
/// `.cwb` parse cache magic (`CWB\0`) so the two can never be confused.
const MAGIC: &[u8; 4] = b"CWV\x00";

// v2 adds `fingerprint` (game version) so a cache can be validated against the
// installed game and shared between users on the same version. v1 files fail the
// version check and are treated as a cache miss (rebuilt).
// v3 folds the ruleset shape into the fingerprint (see `combined_fingerprint`):
// the cached instances are extracted *by the .cwt rules*, so a rules change makes
// a same-game-version cache stale. v2 files fail the version check (rebuilt).
// v4 switches the on-disk format from JSON to magic+version-framed zstd(rkyv)
// and adds loc keys, file paths, and variable names. Older JSON files fail the
// magic check and are treated as a cache miss (rebuilt).
// v5 adds complex-enum members and value_set members (completion data).
// v6 adds subtype-qualified membership keys (`type.subtype`) to the cached
// instances so `<type.subtype>` references into base-game content resolve. v5
// caches lack them, so they must rebuild (else e.g. naval equipment variants
// referencing a vanilla archetype lose their subtype).
const CACHE_VERSION: u8 = 6;

#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
struct CachedInstance {
    /// type name
    t: String,
    /// instance name
    n: String,
    /// source file (kept for future goto-into-vanilla; unused on load today)
    f: String,
    /// start line
    l: u32,
    /// start column
    c: u16,
}

#[derive(rkyv::Archive, rkyv::Serialize, rkyv::Deserialize)]
struct VanillaCacheFile {
    game: String,
    /// Game-version fingerprint (see [`fingerprint`]). A cache is valid only for
    /// the install that produced this fingerprint.
    fingerprint: String,
    instances: Vec<CachedInstance>,
    /// language name (`english`, `simp_chinese`, …) -> lowercased loc keys.
    loc_keys: Vec<(String, Vec<String>)>,
    /// Normalized relative paths of every file under the install (the
    /// `FileIndex` form: lowercased, forward slashes).
    file_paths: Vec<String>,
    /// Script-variable names defined in vanilla (`VarIndex` form).
    var_names: Vec<String>,
    /// Complex-enum members extracted from vanilla files (enum name -> values).
    complex_enum_values: Vec<(String, Vec<String>)>,
    /// `value_set[...]` members written by vanilla files (namespace -> values).
    value_set_values: Vec<(String, Vec<String>)>,
}

/// The non-instance half of the cache payload. Built by whoever walks the
/// install (CLI `cache-vanilla`, the stale-rebuild paths) and stored alongside
/// the type instances.
#[derive(Default)]
pub struct VanillaCacheAux {
    /// language name -> lowercased loc keys
    pub loc_keys: Vec<(String, Vec<String>)>,
    /// normalized relative file paths (FileIndex form)
    pub file_paths: Vec<String>,
    /// script-variable names
    pub var_names: Vec<String>,
    /// complex-enum members (enum name -> values)
    pub complex_enum_values: Vec<(String, Vec<String>)>,
    /// `value_set[...]` members (namespace -> values)
    pub value_set_values: Vec<(String, Vec<String>)>,
}

/// Everything a loaded cache provides, ready to merge into a session.
#[derive(Debug)]
pub struct VanillaCacheData {
    pub per_type: HashMap<String, Vec<TypeInstance>>,
    /// language name -> lowercased loc keys
    pub loc_keys: Vec<(String, Vec<String>)>,
    /// normalized relative file paths (FileIndex form)
    pub file_paths: Vec<String>,
    /// script-variable names
    pub var_names: Vec<String>,
    /// complex-enum members (enum name -> values)
    pub complex_enum_values: Vec<(String, Vec<String>)>,
    /// `value_set[...]` members (namespace -> values)
    pub value_set_values: Vec<(String, Vec<String>)>,
}

/// A stable fingerprint of a base-game install, used to invalidate the cache
/// when the game updates. Prefers the Paradox launcher's `rawVersion` (portable:
/// the same across every user on that version, so a built cache can be shared),
/// and falls back to the install directory's mtime when no version file exists.
pub fn fingerprint(dir: &Path) -> String {
    let launcher = dir.join("launcher-settings.json");
    if let Ok(text) = std::fs::read_to_string(&launcher)
        && let Ok(v) = serde_json::from_str::<serde_json::Value>(&text)
    {
        if let Some(ver) = v.get("rawVersion").and_then(|x| x.as_str()) {
            return format!("v{ver}");
        }
        if let Some(ver) = v.get("version").and_then(|x| x.as_str()) {
            return format!("ver-{ver}");
        }
    }
    if let Ok(meta) = std::fs::metadata(dir)
        && let Ok(mtime) = meta.modified()
        && let Ok(dur) = mtime.duration_since(std::time::UNIX_EPOCH)
    {
        return format!("mtime-{}", dur.as_secs());
    }
    // No version file and no readable mtime: hash the install path so two
    // different unreadable installs don't collide on one "unknown" cache key.
    let h = fnv1a(dir.to_string_lossy().as_bytes(), 0xcbf2_9ce4_8422_2325u64);
    format!("unknown-{h:016x}")
}

/// FNV-1a over `bytes`, continuing from `hash`. A stable, dependency-free hash
/// (unlike `std::hash::DefaultHasher`, whose output isn't guaranteed across Rust
/// versions) so a cache fingerprint stays comparable across restarts/toolchains.
fn fnv1a(bytes: &[u8], mut hash: u64) -> u64 {
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    for &b in bytes {
        hash ^= b as u64;
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// A stable hash of the parts of the ruleset that decide *which* vanilla type
/// instances get extracted and under *what name* (`collect_type_instances`):
/// type name, paths, `name_field`, `skip_root_key`, `starts_with`,
/// `type_per_file`, `key_prefix`, `type_key_filter`, `unique`, and subtype
/// key fields. When these change, a cache built from the old rules is stale even
/// if the game version is identical, so this is folded into the fingerprint.
pub fn ruleset_shape_hash(ruleset: &RuleSet) -> String {
    let skip_str = |s: &SkipRootKey| match s {
        SkipRootKey::SpecificKey(k) => format!("s:{k}"),
        SkipRootKey::AnyKey => "any".to_string(),
        SkipRootKey::MultipleKeys(ks, b) => format!("m:{}:{b}", ks.join(",")),
    };
    let mut parts: Vec<String> = ruleset
        .types
        .iter()
        .map(|t| {
            let mut paths = t.path_options.paths.clone();
            paths.sort();
            let skip = t.skip_root_key.iter().map(skip_str).collect::<Vec<_>>();
            let mut subs = t
                .subtypes
                .iter()
                .map(|s| {
                    format!(
                        "{}|{}|{:?}|{}",
                        s.name,
                        s.type_key_field.as_deref().unwrap_or(""),
                        s.type_key_filter,
                        s.starts_with.as_deref().unwrap_or(""),
                    )
                })
                .collect::<Vec<_>>();
            subs.sort();
            format!(
                "{}|nf={}|paths={}|skip={}|sw={}|tpf={}|kp={}|tkf={:?}|uniq={}|subs={}",
                t.name,
                t.name_field.as_deref().unwrap_or(""),
                paths.join(","),
                skip.join(","),
                t.starts_with.as_deref().unwrap_or(""),
                t.type_per_file,
                t.key_prefix.as_deref().unwrap_or(""),
                t.type_key_filter,
                t.unique,
                subs.join(";"),
            )
        })
        .collect();
    parts.sort();
    // The config's folders.cwt scopes which subdirectories get indexed, so a
    // folder-list change must invalidate the cache like any type-shape change.
    if !ruleset.folders.is_empty() {
        let mut folders = ruleset.folders.clone();
        folders.sort();
        parts.push(format!("folders={}", folders.join(",")));
    }
    let mut h = 0xcbf2_9ce4_8422_2325u64;
    for p in &parts {
        h = fnv1a(p.as_bytes(), h);
        h = fnv1a(b"\x1e", h); // record separator so concatenation is unambiguous
    }
    format!("{h:016x}")
}

/// The fingerprint a cache should be keyed by: the game-version fingerprint
/// ([`fingerprint`]) combined with the ruleset-shape hash ([`ruleset_shape_hash`]).
/// Use this for both [`save`] and the freshness comparison on [`load`].
pub fn combined_fingerprint(dir: &Path, ruleset: &RuleSet) -> String {
    format!("{}|rs:{}", fingerprint(dir), ruleset_shape_hash(ruleset))
}

/// zstd level for the cache body — matches the `.cwb` parse cache.
const ZSTD_LEVEL: i32 = 3;

fn write_cache(
    instances: Vec<CachedInstance>,
    game: &str,
    fingerprint: &str,
    path: &Path,
    aux: VanillaCacheAux,
) -> std::io::Result<usize> {
    let count = instances.len();
    let cache = VanillaCacheFile {
        game: game.to_string(),
        fingerprint: fingerprint.to_string(),
        instances,
        loc_keys: aux.loc_keys,
        file_paths: aux.file_paths,
        var_names: aux.var_names,
        complex_enum_values: aux.complex_enum_values,
        value_set_values: aux.value_set_values,
    };
    let bytes = rkyv::to_bytes::<rkyv::rancor::Error>(&cache).map_err(std::io::Error::other)?;
    let compressed = zstd::encode_all(&bytes[..], ZSTD_LEVEL)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let mut file = std::fs::File::create(path)?;
    file.write_all(MAGIC)?;
    file.write_all(&[CACHE_VERSION])?;
    file.write_all(&compressed)?;
    Ok(count)
}

/// Serialize a vanilla type index (plus aux data) to `path`. Returns the
/// instance count written. `fingerprint` ties the cache to a specific game
/// version (see [`fingerprint`]).
pub fn save(
    index: &TypeIndex,
    game: &str,
    fingerprint: &str,
    path: &Path,
    aux: VanillaCacheAux,
) -> std::io::Result<usize> {
    let instances = index
        .map
        .iter()
        .flat_map(|(type_name, entries)| {
            entries.iter().map(move |(file_uri, inst)| CachedInstance {
                t: type_name.clone(),
                n: inst.name.clone(),
                f: file_uri.to_string(),
                l: inst.location.line,
                c: inst.location.col,
            })
        })
        .collect();
    write_cache(instances, game, fingerprint, path, aux)
}

/// As [`save`], but from a per-type instance map (the form the LSP keeps its
/// vanilla index in). The source-file field is left blank (unused on load).
pub fn save_per_type(
    per_type: &HashMap<String, Vec<TypeInstance>>,
    game: &str,
    fingerprint: &str,
    path: &Path,
    aux: VanillaCacheAux,
) -> std::io::Result<usize> {
    let instances = per_type
        .iter()
        .flat_map(|(type_name, insts)| {
            insts.iter().map(move |inst| CachedInstance {
                t: type_name.clone(),
                n: inst.name.clone(),
                f: String::new(),
                l: inst.location.line,
                c: inst.location.col,
            })
        })
        .collect();
    write_cache(instances, game, fingerprint, path, aux)
}

/// Load a vanilla cache file. Returns `(game, fingerprint, data)`; the caller
/// compares `fingerprint` against the live install to decide whether it is
/// fresh. Old JSON caches (pre-v4) fail the magic check and read as a miss.
pub fn load(path: &Path) -> std::io::Result<(String, String, VanillaCacheData)> {
    let mut data = Vec::new();
    std::fs::File::open(path)?.read_to_end(&mut data)?;
    if data.len() < MAGIC.len() + 1 || &data[..MAGIC.len()] != MAGIC {
        return Err(std::io::Error::other(
            "not a vanilla cache file (old JSON format or wrong file); rebuild with cache-vanilla",
        ));
    }
    if data[MAGIC.len()] != CACHE_VERSION {
        return Err(std::io::Error::other(format!(
            "vanilla cache version {} unsupported (expected {})",
            data[MAGIC.len()],
            CACHE_VERSION
        )));
    }
    let bytes = zstd::decode_all(&data[MAGIC.len() + 1..])?;
    let cache: VanillaCacheFile = rkyv::from_bytes::<VanillaCacheFile, rkyv::rancor::Error>(&bytes)
        .map_err(std::io::Error::other)?;
    let mut per_type: HashMap<String, Vec<TypeInstance>> = HashMap::new();
    for ci in cache.instances {
        per_type.entry(ci.t).or_default().push(TypeInstance {
            name: ci.n,
            location: SourceLocation {
                line: ci.l,
                col: ci.c,
            },
        });
    }
    Ok((
        cache.game,
        cache.fingerprint,
        VanillaCacheData {
            per_type,
            loc_keys: cache.loc_keys,
            file_paths: cache.file_paths,
            var_names: cache.var_names,
            complex_enum_values: cache.complex_enum_values,
            value_set_values: cache.value_set_values,
        },
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_distinguishes_unreadable_installs() {
        // Two installs with no launcher file and no readable mtime must not
        // collide on a single "unknown" cache key (#9).
        let a = fingerprint(Path::new("/nonexistent/install/alpha"));
        let b = fingerprint(Path::new("/nonexistent/install/beta"));
        assert_ne!(
            a, b,
            "distinct unreadable installs need distinct fingerprints"
        );
        assert_ne!(a, "unknown");
    }

    #[test]
    fn round_trip_preserves_instances() {
        let mut idx = TypeIndex::new();
        let mut per: HashMap<String, Vec<TypeInstance>> = HashMap::new();
        per.insert(
            "spriteType".to_string(),
            vec![
                TypeInstance {
                    name: "GFX_a".into(),
                    location: SourceLocation { line: 2, col: 1 },
                },
                TypeInstance {
                    name: "GFX_b".into(),
                    location: SourceLocation { line: 5, col: 3 },
                },
            ],
        );
        idx.merge("vanilla/x.gfx", per);

        let dir = std::env::temp_dir();
        let path = dir.join("cwtools_vanilla_cache_test.cwv");
        let aux = VanillaCacheAux {
            loc_keys: vec![("english".into(), vec!["key_a".into(), "key_b".into()])],
            file_paths: vec!["gfx/interface/icon.dds".into()],
            var_names: vec!["my_var".into()],
            complex_enum_values: vec![("equipment_stat".into(), vec!["build_cost_ic".into()])],
            value_set_values: vec![("country_flag".into(), vec!["my_flag".into()])],
        };
        assert_eq!(save(&idx, "hoi4", "v1.16.4", &path, aux).unwrap(), 2);

        let (game, fp, loaded) = load(&path).unwrap();
        assert_eq!(game, "hoi4");
        assert_eq!(fp, "v1.16.4");
        assert_eq!(loaded.per_type.get("spriteType").map(|v| v.len()), Some(2));
        assert_eq!(loaded.loc_keys.len(), 1);
        assert_eq!(loaded.loc_keys[0].0, "english");
        assert_eq!(loaded.file_paths, vec!["gfx/interface/icon.dds"]);
        assert_eq!(loaded.var_names, vec!["my_var"]);
        assert_eq!(loaded.complex_enum_values[0].0, "equipment_stat");
        assert_eq!(loaded.value_set_values[0].0, "country_flag");

        let mut idx2 = TypeIndex::new();
        idx2.merge("<vanilla-cache>", loaded.per_type);
        assert!(idx2.contains("spriteType", "GFX_A"));
        assert!(idx2.contains("spriteType", "gfx_b"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn old_json_cache_is_a_clean_miss() {
        let dir = std::env::temp_dir();
        let path = dir.join("cwtools_vanilla_cache_old_json.json");
        std::fs::write(&path, r#"{"version":3,"game":"hoi4","instances":[]}"#).unwrap();
        let err = load(&path).unwrap_err();
        assert!(err.to_string().contains("not a vanilla cache file"));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn ruleset_shape_hash_is_stable_and_sensitive() {
        use cwtools_rules::rules_types::{PathOptions, TypeDefinition};

        let mk = |name: &str, name_field: Option<&str>| TypeDefinition {
            name: name.to_string(),
            name_field: name_field.map(str::to_string),
            path_options: PathOptions {
                paths: vec!["common/foo".into()],
                path_strict: false,
                path_file: None,
                path_extension: None,
                paths_lower: vec![],
                ..Default::default()
            },
            subtypes: vec![],
            type_key_filter: None,
            skip_root_key: vec![],
            starts_with: None,
            type_per_file: false,
            key_prefix: None,
            warning_only: false,
            unique: false,
            should_be_referenced: false,
            localisation: vec![],
            graph_related_types: vec![],
            modifiers: vec![],
        };

        let mut a = RuleSet::new();
        a.types = vec![mk("event", None), mk("tech", Some("id"))];
        let mut b = RuleSet::new();
        // Same content, different declaration order → same hash (order-independent).
        b.types = vec![mk("tech", Some("id")), mk("event", None)];
        assert_eq!(ruleset_shape_hash(&a), ruleset_shape_hash(&b));

        // A meaningful shape change (name_field) flips the hash.
        let mut c = RuleSet::new();
        c.types = vec![mk("event", Some("id")), mk("tech", Some("id"))];
        assert_ne!(ruleset_shape_hash(&a), ruleset_shape_hash(&c));
    }
}
