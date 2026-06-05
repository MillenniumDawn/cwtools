use crate::cache_format::CachedFile;
use std::fs::File;
use std::io::{Read, Write};
use std::path::Path;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CacheError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("Serialization error: {0}")]
    Serialize(String),
    #[error("Deserialization error: {0}")]
    Deserialize(String),
    #[error("Compression error: {0}")]
    Compression(String),
}

const ZSTD_LEVEL: i32 = 3;

/// Serialize a `CachedFile` to a `.cwb` file (zstd-compressed rkyv).
pub fn serialize_to_file(cached: &CachedFile, path: &Path) -> Result<(), CacheError> {
    let bytes = rkyv::to_bytes::<rkyv::rancor::Error>(cached)
        .map_err(|e| CacheError::Serialize(format!("{:?}", e)))?;

    let compressed = zstd::encode_all(&bytes[..], ZSTD_LEVEL)
        .map_err(|e| CacheError::Compression(format!("{:?}", e)))?;

    let mut file = File::create(path)?;
    file.write_all(&compressed)?;
    Ok(())
}

/// Deserialize a `CachedFile` from a `.cwb` file (zstd-decompressed rkyv).
pub fn deserialize_from_file(path: &Path) -> Result<CachedFile, CacheError> {
    let mut file = File::open(path)?;
    let mut compressed = Vec::new();
    file.read_to_end(&mut compressed)?;

    let bytes = zstd::decode_all(&compressed[..])
        .map_err(|e| CacheError::Compression(format!("{:?}", e)))?;

    let deserialized: CachedFile = rkyv::from_bytes::<CachedFile, rkyv::rancor::Error>(&bytes)
        .map_err(|e| CacheError::Deserialize(format!("{:?}", e)))?;

    Ok(deserialized)
}
