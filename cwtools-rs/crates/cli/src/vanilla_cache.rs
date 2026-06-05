//! Pre-generated cache of base-game ("vanilla") type instances.
//!
//! Parsing and indexing a full game install on every run is slow, so
//! `cache-vanilla` does it once and writes the resulting type index here as
//! JSON. `validate --vanilla-cache <file>` loads it to resolve references into
//! base-game content (sprites, operation_tokens, equipment, …) without
//! re-parsing, and crucially without validating vanilla files, which carry
//! known base-game errors we never want to report.

use std::collections::HashMap;
use std::path::Path;

use cwtools_info::{SourceLocation, TypeIndex, TypeInstance};
use serde::{Deserialize, Serialize};

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
pub fn save(index: &TypeIndex, game: &str, path: &Path) -> anyhow::Result<usize> {
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
    std::fs::write(path, serde_json::to_string(&cache)?)?;
    Ok(count)
}

/// Load a vanilla cache file into per-type instances ready to merge into a
/// `TypeIndex`. Returns `(game, per_type)`.
pub fn load(path: &Path) -> anyhow::Result<(String, HashMap<String, Vec<TypeInstance>>)> {
    let json = std::fs::read_to_string(path)?;
    let cache: VanillaCacheFile = serde_json::from_str(&json)?;
    if cache.version != CACHE_VERSION {
        anyhow::bail!(
            "vanilla cache version {} unsupported (expected {})",
            cache.version,
            CACHE_VERSION
        );
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

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("van.json");
        assert_eq!(save(&idx, "hoi4", &path).unwrap(), 2);

        let (game, loaded) = load(&path).unwrap();
        assert_eq!(game, "hoi4");
        assert_eq!(loaded.get("spriteType").map(|v| v.len()), Some(2));

        // Merging the loaded cache resolves references case-insensitively.
        let mut idx2 = TypeIndex::new();
        idx2.merge("<vanilla-cache>", loaded);
        assert!(idx2.contains("spriteType", "GFX_A"));
        assert!(idx2.contains("spriteType", "gfx_b"));
    }
}
