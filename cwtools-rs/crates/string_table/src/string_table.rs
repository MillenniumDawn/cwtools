use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;

/// A unique identifier for an interned string.
/// `NULL` (u32::MAX) is reserved and never assigned.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
#[repr(transparent)]
pub struct StringId(pub u32);

impl StringId {
    pub const NULL: StringId = StringId(u32::MAX);
}

/// Mirrors the F# `StringTokens` struct.
/// `lower`  → ID of the lower‑cased canonical form.
/// `normal` → ID of the exact (case‑preserving) string.
/// `quoted` → whether the original was surrounded by `"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StringTokens {
    pub lower: StringId,
    pub normal: StringId,
    pub quoted: bool,
}

/// Metadata computed once per canonical (lower‑cased) string.
/// Used by the rules / scope engines to avoid re‑scanning strings.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct StringMetadata {
    pub starts_with_amp: bool,
    pub contains_double_dollar: bool,
    pub contains_question_mark: bool,
    pub contains_hat: bool,
    pub starts_with_square_bracket: bool,
    pub contains_pipe: bool,
}

struct Inner {
    /// Lower‑cased key → the canonical lower token (`lower == normal`).
    lower_map: HashMap<String, StringTokens>,
    /// Exact (case‑preserving) key → the normal token that points to a lower ID.
    exact_map: HashMap<String, StringTokens>,
    /// Dense array: ID → original or lower‑cased text.
    id_to_string: Vec<String>,
    /// Dense array: ID → metadata (both normal and lower slots share the same metadata).
    id_to_metadata: Vec<StringMetadata>,
    /// Next free ID.  IDs are handed out consecutively starting at 1 (0 is the empty string).
    next_id: u32,
}

/// Thread‑safe string interner that preserves the F# `StringResourceManager`
/// semantics:
///
/// * Case‑insensitive lookup by lower‑cased key.
/// * Two IDs per logical entry: a *normal* ID (exact text) and a *lower* ID
///   (canonical lower‑cased form).  Multiple normal strings may share the same
///   lower ID.
/// * `StringMetadata` is attached to the canonical lower form and copied to
///   every normal variant.
/// * `quoted` is tracked per‑normal variant.
pub struct StringTable {
    // RwLock (not Mutex): validation is read-only on the table (only
    // `get_string`/`get_metadata`), so once parsing has interned everything the
    // validation threads read concurrently. Interning (`intern`) still takes the
    // write lock, so parse-time interning stays serialized.
    inner: Arc<RwLock<Inner>>,
}

impl Clone for StringTable {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl Default for StringTable {
    fn default() -> Self {
        Self::new()
    }
}

impl StringTable {
    pub fn new() -> Self {
        let mut id_to_string = Vec::with_capacity(1024);
        let mut id_to_metadata = Vec::with_capacity(1024);
        // Slot 0 = empty string (never returned by intern, but keeps the
        // array 1-based so that `StringId(0)` is safe to index).
        id_to_string.push(String::new());
        id_to_metadata.push(StringMetadata::default());

        Self {
            inner: Arc::new(RwLock::new(Inner {
                lower_map: HashMap::new(),
                exact_map: HashMap::new(),
                id_to_string,
                id_to_metadata,
                next_id: 1,
            })),
        }
    }

    /// Intern a string and return its `StringTokens`.
    ///
    /// * If the exact text has already been interned, the existing token is
    ///   returned (fast path via `exact_map`).
    /// * If the lower‑cased form exists but this exact text has never been
    ///   interned, a new `normal` ID is allocated that shares the existing `lower` ID.
    /// * If the lower‑cased form has never been seen, two consecutive IDs are
    ///   allocated: `normal` (exact text) and `lower` (lower‑cased text).
    pub fn intern(&self, s: &str) -> StringTokens {
        // Fast path: exact string already interned. This is the overwhelming
        // common case while parsing many files (identifiers repeat constantly),
        // and it takes a shared read lock so parse threads don't serialize on it.
        {
            let inner = self.inner.read();
            if let Some(&existing) = inner.exact_map.get(s) {
                return existing;
            }
        }

        let quoted = s.starts_with('"') && s.ends_with('"');
        let lower_key = s.to_lowercase();

        let mut inner = self.inner.write();

        // Re-check after upgrading to the write lock: another thread may have
        // interned this exact string in the gap (double-checked locking).
        if let Some(&existing) = inner.exact_map.get(s) {
            return existing;
        }

        // Fast path 2: lower key exists → allocate new normal variant.
        if let Some(&existing_lower) = inner.lower_map.get(&lower_key) {
            let normal_id = inner.next_id;
            inner.next_id += 1;
            inner.id_to_string.push(s.to_string());
            let meta = inner.id_to_metadata[existing_lower.lower.0 as usize];
            inner.id_to_metadata.push(meta);
            let token = StringTokens {
                lower: existing_lower.lower,
                normal: StringId(normal_id),
                quoted,
            };
            inner.exact_map.insert(s.to_string(), token);
            return token;
        }

        // Slow path: brand‑new lower key.
        let normal_id = inner.next_id;
        let lower_id = normal_id + 1;
        inner.next_id = lower_id + 1;

        let metadata = compute_metadata(&lower_key);

        inner.id_to_string.reserve_exact(2);
        inner.id_to_string.push(s.to_string()); // normal_id
        inner.id_to_string.push(lower_key.clone()); // lower_id
        inner.id_to_metadata.push(metadata); // normal_id
        inner.id_to_metadata.push(metadata); // lower_id

        let lower_token = StringTokens {
            lower: StringId(lower_id),
            normal: StringId(lower_id),
            quoted: false,
        };
        let normal_token = StringTokens {
            lower: StringId(lower_id),
            normal: StringId(normal_id),
            quoted,
        };

        inner.lower_map.insert(lower_key, lower_token);
        inner.exact_map.insert(s.to_string(), normal_token);
        normal_token
    }

    /// Retrieve the original (case‑preserving) text for a `StringId`.
    pub fn get_string(&self, id: StringId) -> Option<String> {
        let inner = self.inner.read();
        inner.id_to_string.get(id.0 as usize).cloned()
    }

    /// Retrieve the metadata for a `StringId`.
    pub fn get_metadata(&self, id: StringId) -> Option<StringMetadata> {
        let inner = self.inner.read();
        inner.id_to_metadata.get(id.0 as usize).copied()
    }

    /// Number of unique lower‑cased strings (not counting normal variants).
    pub fn len(&self) -> usize {
        let inner = self.inner.read();
        inner.lower_map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn compute_metadata(s: &str) -> StringMetadata {
    if s.is_empty() {
        return StringMetadata::default();
    }
    let starts_with_amp = s.starts_with('@');
    let contains_question_mark = s.contains('?');
    let contains_hat = s.contains('^');
    let first_dollar = s.find('$');
    let last_dollar = s.rfind('$');
    let contains_double_dollar = first_dollar.is_some() && first_dollar != last_dollar;
    let starts_with_square_bracket = s.starts_with('[') || s.starts_with(']');
    let contains_pipe = s.contains('|');

    StringMetadata {
        starts_with_amp,
        contains_double_dollar,
        contains_question_mark,
        contains_hat,
        starts_with_square_bracket,
        contains_pipe,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn basic_interning() {
        let table = StringTable::new();
        let a = table.intern("hello");
        let b = table.intern("HELLO");
        let c = table.intern("hello");

        assert_eq!(a, c); // same exact string → same token
        assert_eq!(a.lower, b.lower); // same lower key → same lower ID
        assert_ne!(a.normal, b.normal); // different exact strings → different normal IDs

        assert_eq!(table.get_string(a.normal), Some("hello".to_string()));
        assert_eq!(table.get_string(b.normal), Some("HELLO".to_string()));
        assert_eq!(table.get_string(a.lower), Some("hello".to_string()));
    }

    #[test]
    fn quoted_flag() {
        let table = StringTable::new();
        let a = table.intern("\"foo\"");
        let b = table.intern("foo");
        assert!(a.quoted);
        assert!(!b.quoted);
    }

    #[test]
    fn metadata() {
        let table = StringTable::new();
        let t = table.intern("@event_target|foo");
        let meta = table.get_metadata(t.normal).unwrap();
        assert!(meta.starts_with_amp);
        assert!(meta.contains_pipe);
    }

    #[test]
    fn shared_table() {
        let table = StringTable::new();
        let a = table.intern("hello");

        let table2 = table.clone();
        let b = table2.intern("hello");

        assert_eq!(a, b); // shared table → same token
    }
}
