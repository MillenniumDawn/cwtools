//! Pre-generated cache of base-game ("vanilla") type instances.
//!
//! Parsing and indexing a full game install on every run is slow, so a vanilla
//! TypeIndex is built once and serialized here as JSON. Loading it resolves
//! references into base-game content (sprites, operation_tokens, equipment, …)
//! without re-parsing, and without validating vanilla files (which carry known
//! base-game errors we never want to report). Shared by the CLI
//! (`cache-vanilla` / `validate --vanilla-cache`) and the LSP server.

use std::collections::HashMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::{SourceLocation, TypeIndex, TypeInstance};

// v2 adds `fingerprint` (game version) so a cache can be validated against the
// installed game and shared between users on the same version. v1 files fail the
// version check and are treated as a cache miss (rebuilt).
const CACHE_VERSION: u32 = 2;

#[derive(Serialize, Deserialize)]
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

#[derive(Serialize, Deserialize)]
struct VanillaCacheFile {
    version: u32,
    game: String,
    /// Game-version fingerprint (see [`fingerprint`]). A cache is valid only for
    /// the install that produced this fingerprint.
    #[serde(default)]
    fingerprint: String,
    instances: Vec<CachedInstance>,
}

/// A stable fingerprint of a base-game install, used to invalidate the cache
/// when the game updates. Prefers the Paradox launcher's `rawVersion` (portable:
/// the same across every user on that version, so a built cache can be shared),
/// and falls back to the install directory's mtime when no version file exists.
pub fn fingerprint(dir: &Path) -> String {
    let launcher = dir.join("launcher-settings.json");
    if let Ok(text) = std::fs::read_to_string(&launcher) {
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(&text) {
            if let Some(ver) = v.get("rawVersion").and_then(|x| x.as_str()) {
                return format!("v{ver}");
            }
            if let Some(ver) = v.get("version").and_then(|x| x.as_str()) {
                return format!("ver-{ver}");
            }
        }
    }
    if let Ok(meta) = std::fs::metadata(dir) {
        if let Ok(mtime) = meta.modified() {
            if let Ok(dur) = mtime.duration_since(std::time::UNIX_EPOCH) {
                return format!("mtime-{}", dur.as_secs());
            }
        }
    }
    "unknown".to_string()
}

fn write_cache(
    instances: Vec<CachedInstance>,
    game: &str,
    fingerprint: &str,
    path: &Path,
) -> std::io::Result<usize> {
    let count = instances.len();
    let cache = VanillaCacheFile {
        version: CACHE_VERSION,
        game: game.to_string(),
        fingerprint: fingerprint.to_string(),
        instances,
    };
    let json = serde_json::to_string(&cache).map_err(std::io::Error::other)?;
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    std::fs::write(path, json)?;
    Ok(count)
}

/// Serialize a vanilla type index to `path`. Returns the instance count written.
/// `fingerprint` ties the cache to a specific game version (see [`fingerprint`]).
pub fn save(
    index: &TypeIndex,
    game: &str,
    fingerprint: &str,
    path: &Path,
) -> std::io::Result<usize> {
    let instances = index
        .map
        .iter()
        .flat_map(|(type_name, entries)| {
            entries.iter().map(move |(file_uri, inst)| CachedInstance {
                t: type_name.clone(),
                n: inst.name.clone(),
                f: file_uri.clone(),
                l: inst.location.line,
                c: inst.location.col,
            })
        })
        .collect();
    write_cache(instances, game, fingerprint, path)
}

/// As [`save`], but from a per-type instance map (the form the LSP keeps its
/// vanilla index in). The source-file field is left blank (unused on load).
pub fn save_per_type(
    per_type: &HashMap<String, Vec<TypeInstance>>,
    game: &str,
    fingerprint: &str,
    path: &Path,
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
    write_cache(instances, game, fingerprint, path)
}

/// Load a vanilla cache file into per-type instances ready to merge into a
/// `TypeIndex` via `merge`. Returns `(game, fingerprint, per_type)`; the caller
/// compares `fingerprint` against the live install to decide whether it is fresh.
pub fn load(path: &Path) -> std::io::Result<(String, String, HashMap<String, Vec<TypeInstance>>)> {
    let json = std::fs::read_to_string(path)?;
    let cache: VanillaCacheFile = serde_json::from_str(&json).map_err(std::io::Error::other)?;
    if cache.version != CACHE_VERSION {
        return Err(std::io::Error::other(format!(
            "vanilla cache version {} unsupported (expected {})",
            cache.version, CACHE_VERSION
        )));
    }
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
    Ok((cache.game, cache.fingerprint, per_type))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let path = dir.join("cwtools_vanilla_cache_test.json");
        assert_eq!(save(&idx, "hoi4", "v1.16.4", &path).unwrap(), 2);

        let (game, fp, loaded) = load(&path).unwrap();
        assert_eq!(game, "hoi4");
        assert_eq!(fp, "v1.16.4");
        assert_eq!(loaded.get("spriteType").map(|v| v.len()), Some(2));

        let mut idx2 = TypeIndex::new();
        idx2.merge("<vanilla-cache>", loaded);
        assert!(idx2.contains("spriteType", "GFX_A"));
        assert!(idx2.contains("spriteType", "gfx_b"));
        let _ = std::fs::remove_file(&path);
    }
}
