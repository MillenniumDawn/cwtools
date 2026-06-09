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

/// Magic bytes at the start of every `.cwb` file. Lets `deserialize_from_file`
/// reject files written by an incompatible layout before rkyv gets confused.
const MAGIC: &[u8; 4] = b"CWB\x00";

/// Format version. Bump whenever the rkyv layout changes (e.g. widening a field
/// from u16 → u32) so old `.cwb` files are rejected cleanly instead of being
/// silently misread.
///
/// v1: initial versioned format (adds magic+version header to the raw zstd).
const FORMAT_VERSION: u8 = 1;

/// Serialize a `CachedFile` to a `.cwb` file (zstd-compressed rkyv).
///
/// Layout: `MAGIC (4 bytes) | FORMAT_VERSION (1 byte) | zstd(rkyv bytes)`.
pub fn serialize_to_file(cached: &CachedFile, path: &Path) -> Result<(), CacheError> {
    let bytes = rkyv::to_bytes::<rkyv::rancor::Error>(cached)
        .map_err(|e| CacheError::Serialize(format!("{:?}", e)))?;

    let compressed = zstd::encode_all(&bytes[..], ZSTD_LEVEL)
        .map_err(|e| CacheError::Compression(format!("{:?}", e)))?;

    let mut file = File::create(path)?;
    file.write_all(MAGIC)?;
    file.write_all(&[FORMAT_VERSION])?;
    file.write_all(&compressed)?;
    Ok(())
}

/// Deserialize a `CachedFile` from a `.cwb` file (zstd-decompressed rkyv).
pub fn deserialize_from_file(path: &Path) -> Result<CachedFile, CacheError> {
    let mut file = File::open(path)?;
    let mut data = Vec::new();
    file.read_to_end(&mut data)?;

    // Validate magic + version header. Reject anything written before this
    // header was added (or by a future incompatible version) rather than
    // letting rkyv silently misread mismatched bytes.
    if data.len() < MAGIC.len() + 1
        || &data[..MAGIC.len()] != MAGIC
        || data[MAGIC.len()] != FORMAT_VERSION
    {
        return Err(CacheError::Deserialize(
            "incompatible or missing cache header".into(),
        ));
    }
    let compressed = &data[MAGIC.len() + 1..];

    let bytes =
        zstd::decode_all(compressed).map_err(|e| CacheError::Compression(format!("{:?}", e)))?;

    let deserialized: CachedFile = rkyv::from_bytes::<CachedFile, rkyv::rancor::Error>(&bytes)
        .map_err(|e| CacheError::Deserialize(format!("{:?}", e)))?;

    Ok(deserialized)
}
