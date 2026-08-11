//! Bounds on cache inputs (#162).
//!
//! A cache path is chosen by a CLI flag or an LSP client, so neither the read
//! nor the zstd decode behind it may run unbounded. Every rejection here has to
//! surface as a `CacheError`, which is what `cwtools_cache::workspace::load`
//! collapses to a re-parse.

use cwtools_cache::io::{self, decode_capped, read_capped};
use std::fs::File;
use std::io::Write;
use std::path::Path;

// Mirrors of the caps in `cwtools_cache::io`, which are private to it.
const MAX_ARCHIVE_FILE_BYTES: u64 = 64 * 1024 * 1024;
const MAX_ERRORS_FILE_BYTES: u64 = 16 * 1024 * 1024;

const MAGIC: [u8; 4] = *b"CWB\x00";
const FORMAT_VERSION: u8 = 3;
const ERRORS_MAGIC: [u8; 4] = *b"CWE\x00";
const ERRORS_FORMAT_VERSION: u8 = 1;

/// A zstd frame of `len` zero bytes whose header does not declare how much it
/// decompresses to, because the streaming encoder does not know the total up
/// front. That is the shape the byte-by-byte bound has to catch on its own.
fn undeclared_frame(len: usize) -> Vec<u8> {
    let mut encoder = zstd::stream::Encoder::new(Vec::new(), 3).unwrap();
    encoder.write_all(&vec![0u8; len]).unwrap();
    let frame = encoder.finish().unwrap();
    assert_eq!(
        zstd::zstd_safe::get_frame_content_size(&frame).unwrap(),
        None,
        "fixture must not declare its size, or it never reaches the streaming bound"
    );
    frame
}

/// Grow `path` to `len` without writing the bytes, so a test for an over-cap
/// file costs no disk.
fn extend_sparse(path: &Path, header: &[u8], len: u64) {
    let mut file = File::create(path).unwrap();
    file.write_all(header).unwrap();
    file.set_len(len).unwrap();
}

#[cfg(unix)]
#[test]
fn read_capped_refuses_a_character_device() {
    // `/dev/zero` reports length 0, so a size check alone waves it through and
    // then reads to EOF. Only the regular-file gate stops it.
    let err = read_capped(Path::new("/dev/zero"), 1024).unwrap_err();
    assert!(err.to_string().contains("not a regular file"), "{err}");

    let err = io::with_archived_file(Path::new("/dev/zero"), |_| ()).unwrap_err();
    assert!(err.to_string().contains("not a regular file"), "{err}");
}

#[test]
fn read_capped_refuses_a_directory() {
    // Windows refuses the open outright and Unix gets as far as the metadata,
    // so only the rejection itself is portable.
    let tmp = tempfile::tempdir().unwrap();
    assert!(read_capped(tmp.path(), 1024).is_err());
    assert!(io::with_archived_file(tmp.path(), |_| ()).is_err());
}

#[test]
fn an_archive_over_the_read_cap_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("huge.cwb");
    let mut header = MAGIC.to_vec();
    header.push(FORMAT_VERSION);
    extend_sparse(&path, &header, MAX_ARCHIVE_FILE_BYTES + 1);

    let err = io::with_archived_file(&path, |_| ()).unwrap_err();
    assert!(err.to_string().contains("cache read cap"), "{err}");
}

#[test]
fn an_error_sidecar_over_the_read_cap_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let path = tmp.path().join("huge.cwe");
    let mut header = ERRORS_MAGIC.to_vec();
    header.push(ERRORS_FORMAT_VERSION);
    extend_sparse(&path, &header, MAX_ERRORS_FILE_BYTES + 1);

    let err = io::read_errors_from_file(&path).unwrap_err();
    assert!(err.to_string().contains("cache read cap"), "{err}");
}

#[test]
fn decode_capped_rejects_one_byte_over_the_cap() {
    const CAP: u64 = 64 * 1024;

    let mut out = Vec::new();
    decode_capped(&undeclared_frame(CAP as usize), CAP, &mut out)
        .expect("a body of exactly the cap must decode");
    assert_eq!(out.len() as u64, CAP);

    let mut out = Vec::new();
    let err = decode_capped(&undeclared_frame(CAP as usize + 1), CAP, &mut out).unwrap_err();
    assert!(err.to_string().contains("decompresses past"), "{err}");
    assert!(
        out.len() as u64 <= CAP,
        "the buffer must never grow past the cap, got {}",
        out.len()
    );
}

#[test]
fn decode_capped_rejects_a_declared_oversize_frame_before_decoding() {
    // 4 MiB of zeros costs a few dozen bytes to store, which is the whole
    // attack. `encode_all` sees the whole input, so this frame declares what it
    // expands to and the cap can be answered without decompressing any of it.
    let bomb = zstd::encode_all(&vec![0u8; 4 * 1024 * 1024][..], 3).unwrap();
    assert!(bomb.len() < 4096, "fixture should be tiny: {}", bomb.len());

    let mut out = Vec::new();
    let err = decode_capped(&bomb, 1024, &mut out).unwrap_err();
    assert!(err.to_string().contains("decompresses past"), "{err}");
    assert!(
        out.is_empty(),
        "a frame refused on its declared size must not write anything"
    );
}
