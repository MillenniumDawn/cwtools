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

const CACHE_VERSION: u32 = 1;

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
    instances: Vec<CachedInstance>,
}

/// Serialize a vanilla type index to `path`. Returns the instance count written.
pub fn save(index: &TypeIndex, game: &str, path: &Path) -> std::io::Result<usize> {
    let mut instances = Vec::new();
    for (type_name, entries) in &index.map {
        for (file_uri, inst) in entries {
            instances.push(CachedInstance {
                t: type_name.clone(),
                n: inst.name.clone(),
                f: file_uri.clone(),
                l: inst.location.line,
                c: inst.location.col,
            });
        }
    }
    let count = instances.len();
    let cache = VanillaCacheFile {
        version: CACHE_VERSION,
        game: game.to_string(),
        instances,
    };
    let json = serde_json::to_string(&cache).map_err(std::io::Error::other)?;
    std::fs::write(path, json)?;
    Ok(count)
}

/// Load a vanilla cache file into per-type instances ready to merge into a
/// `TypeIndex` via `merge`. Returns `(game, per_type)`.
pub fn load(path: &Path) -> std::io::Result<(String, HashMap<String, Vec<TypeInstance>>)> {
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
            location: SourceLocation { line: ci.l, col: ci.c },
        });
    }
    Ok((cache.game, per_type))
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
                TypeInstance { name: "GFX_a".into(), location: SourceLocation { line: 2, col: 1 } },
                TypeInstance { name: "GFX_b".into(), location: SourceLocation { line: 5, col: 3 } },
            ],
        );
        idx.merge("vanilla/x.gfx", per);

        let dir = std::env::temp_dir();
        let path = dir.join("cwtools_vanilla_cache_test.json");
        assert_eq!(save(&idx, "hoi4", &path).unwrap(), 2);

        let (game, loaded) = load(&path).unwrap();
        assert_eq!(game, "hoi4");
        assert_eq!(loaded.get("spriteType").map(|v| v.len()), Some(2));

        let mut idx2 = TypeIndex::new();
        idx2.merge("<vanilla-cache>", loaded);
        assert!(idx2.contains("spriteType", "GFX_A"));
        assert!(idx2.contains("spriteType", "gfx_b"));
        let _ = std::fs::remove_file(&path);
    }
}
