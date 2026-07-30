use crate::cache_format::{ArchivedCachedFile, CachedErrors, CachedFile};
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CacheError {
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("serialization error")]
    Serialize(#[source] rkyv::rancor::Error),
    // `Deserialize` covers both rkyv failures (with a source) and the
    // header-validation rejection (`msg` set, no source).
    #[error("deserialization error: {msg}")]
    Deserialize {
        msg: &'static str,
        #[source]
        source: Option<rkyv::rancor::Error>,
    },
    // zstd returns `io::Error`; a `#[from]` here would collide with `Io`'s, so
    // the source is attached explicitly instead.
    #[error("compression error")]
    Compression(#[source] std::io::Error),
}

/// zstd compression level for cache bodies. Shared by the `.cwb` parse cache
/// (here) and the vanilla index cache (`cwtools_index::vanilla_cache`) so both
/// caches compress at the same ratio. Only the `.cwb` writer adds a frame
/// checksum on top; see `serialize_to_file`.
pub const ZSTD_LEVEL: i32 = 3;

/// Magic bytes at the start of every `.cwb` file. Lets `read_archive_bytes`
/// reject files written by an incompatible layout before rkyv gets confused.
const MAGIC: &[u8; 4] = b"CWB\x00";

/// Format version. Bump whenever the rkyv layout changes (e.g. widening a field
/// from u16 → u32) so old `.cwb` files are rejected cleanly instead of being
/// silently misread.
///
/// v1: initial versioned format (adds magic+version header to the raw zstd).
/// v2: dropped `CachedNode`/`CachedChild::Node` (the AST has one clause
///     representation, `Leaf` + `Value::Clause`; nothing ever wrote Nodes).
/// v3: dropped CachedValueClause/CachedChild::ValueClause (the dead parallel
///     clause slab; the AST/cache use only Leaf + Value::Clause).
const FORMAT_VERSION: u8 = 3;

const ERRORS_MAGIC: &[u8; 4] = b"CWE\x00";
const ERRORS_FORMAT_VERSION: u8 = 1;

static TEMP_FILE_ID: AtomicU64 = AtomicU64::new(0);

fn temp_path(path: &Path) -> PathBuf {
    let id = TEMP_FILE_ID.fetch_add(1, Ordering::Relaxed);
    let mut temp = path.as_os_str().to_owned();
    temp.push(format!(".tmp-{}-{id}", std::process::id()));
    PathBuf::from(temp)
}

fn write_atomically(
    path: &Path,
    write: impl FnOnce(&mut File) -> std::io::Result<()>,
) -> Result<(), CacheError> {
    let temp = temp_path(path);
    let write_result = (|| -> std::io::Result<()> {
        let mut file = File::create(&temp)?;
        write(&mut file)
    })();
    if let Err(error) = write_result {
        let _ = std::fs::remove_file(&temp);
        return Err(CacheError::Io(error));
    }

    if let Err(error) = std::fs::rename(&temp, path) {
        #[cfg(windows)]
        if path.exists() {
            // Windows rename does not replace an existing destination. Removing
            // it is safe because readers already treat a miss as a re-parse.
            std::fs::remove_file(path)?;
            std::fs::rename(&temp, path)?;
            return Ok(());
        }
        let _ = std::fs::remove_file(&temp);
        return Err(CacheError::Io(error));
    }
    Ok(())
}

/// Serialize a `CachedFile` to a `.cwb` file (zstd-compressed rkyv).
///
/// Layout: `MAGIC (4 bytes) | FORMAT_VERSION (1 byte) | zstd(rkyv bytes)`.
pub fn serialize_to_file(cached: &CachedFile, path: &Path) -> Result<(), CacheError> {
    let bytes = rkyv::to_bytes::<rkyv::rancor::Error>(cached).map_err(CacheError::Serialize)?;

    // Frame checksum on. rkyv's checked `access` validates structure, not
    // content, so a flipped byte can decompress into a different-but-valid
    // archive and get served as a cache hit. The checksum turns that into a
    // decode error, which callers already degrade to a re-parse. Readers need
    // no change: a frame without one still decodes, so old `.cwb` files stay
    // loadable and the format version doesn't move.
    let compressed = {
        let mut encoder =
            zstd::stream::Encoder::new(Vec::new(), ZSTD_LEVEL).map_err(CacheError::Compression)?;
        encoder
            .include_checksum(true)
            .map_err(CacheError::Compression)?;
        encoder.write_all(&bytes).map_err(CacheError::Compression)?;
        encoder.finish().map_err(CacheError::Compression)?
    };

    write_atomically(path, |file| {
        file.write_all(MAGIC)?;
        file.write_all(&[FORMAT_VERSION])?;
        file.write_all(&compressed)
    })
}

/// Read a `.cwb` file, validate its header, and return the decompressed rkyv
/// bytes in an aligned buffer suitable for archived access.
fn read_archive_bytes(path: &Path) -> Result<rkyv::util::AlignedVec, CacheError> {
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
        return Err(CacheError::Deserialize {
            msg: "incompatible or missing cache header",
            source: None,
        });
    }
    let compressed = &data[MAGIC.len() + 1..];

    let mut aligned = rkyv::util::AlignedVec::new();
    zstd::stream::copy_decode(compressed, &mut aligned).map_err(CacheError::Compression)?;
    Ok(aligned)
}

/// Run `f` on the checked archived view of a `.cwb` file without
/// materializing an owned `CachedFile`. The only per-load allocations are the
/// file read and one aligned decompression buffer; every cached string is
/// borrowed straight out of that buffer.
pub fn with_archived_file<R>(
    path: &Path,
    f: impl FnOnce(&ArchivedCachedFile) -> R,
) -> Result<R, CacheError> {
    let bytes = read_archive_bytes(path)?;
    let archived =
        rkyv::access::<ArchivedCachedFile, rkyv::rancor::Error>(&bytes).map_err(|e| {
            CacheError::Deserialize {
                msg: "rkyv access failed",
                source: Some(e),
            }
        })?;
    Ok(f(archived))
}

/// Serialize recovered parse errors to the sidecar paired with a `.cwb`.
pub fn serialize_errors_to_file(cached: &CachedErrors, path: &Path) -> Result<(), CacheError> {
    let bytes = rkyv::to_bytes::<rkyv::rancor::Error>(cached).map_err(CacheError::Serialize)?;
    write_atomically(path, |file| {
        file.write_all(ERRORS_MAGIC)?;
        file.write_all(&[ERRORS_FORMAT_VERSION])?;
        file.write_all(&bytes)
    })
}

/// Read and validate a recovered-parse-error sidecar.
pub fn read_errors_from_file(path: &Path) -> Result<CachedErrors, CacheError> {
    let data = std::fs::read(path)?;
    if data.len() < ERRORS_MAGIC.len() + 1
        || &data[..ERRORS_MAGIC.len()] != ERRORS_MAGIC
        || data[ERRORS_MAGIC.len()] != ERRORS_FORMAT_VERSION
    {
        return Err(CacheError::Deserialize {
            msg: "incompatible or missing error-cache header",
            source: None,
        });
    }
    let mut aligned = rkyv::util::AlignedVec::<16>::new();
    aligned.extend_from_slice(&data[ERRORS_MAGIC.len() + 1..]);
    rkyv::from_bytes::<CachedErrors, rkyv::rancor::Error>(&aligned).map_err(|error| {
        CacheError::Deserialize {
            msg: "error-cache rkyv access failed",
            source: Some(error),
        }
    })
}
