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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct StringTokens {
    pub lower: StringId,
    pub normal: StringId,
}

struct Inner {
    /// Lower‑cased key → the canonical lower token (`lower == normal`).
    /// `Arc<str>` key shares the allocation with `id_to_string[lower_id]`.
    lower_map: HashMap<Arc<str>, StringTokens>,
    /// Exact (case‑preserving) key → the normal token that points to a lower ID.
    /// `Arc<str>` key shares the allocation with `id_to_string[normal_id]`.
    exact_map: HashMap<Arc<str>, StringTokens>,
    /// Dense array: ID → original or lower‑cased text.
    /// Each entry is the same `Arc<str>` cloned into the corresponding map key,
    /// so each string is stored once on the heap regardless of how many maps
    /// reference it.
    id_to_string: Vec<Arc<str>>,
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
pub struct StringTable {
    // RwLock (not Mutex): validation is read-only on the table (only
    // `get_string`), so once parsing has interned everything the
    // validation threads read concurrently. Interning (`intern`) still takes the
    // write lock, so parse-time interning stays serialized.
    inner: Arc<RwLock<Inner>>,
}

impl Clone for StringTable {
    /// NOTE: this is an *aliasing* clone, not a deep copy. The clone shares the
    /// same underlying interner (`Arc<RwLock<Inner>>`) as the original, so a
    /// string interned through one handle is visible through the other. This is
    /// intentional (see the `shared_table` test) — cloning a `StringTable` just
    /// hands out another handle to the one process-wide table.
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
        // Slot 0 = empty string (never returned by intern, but keeps the
        // array 1-based so that `StringId(0)` is safe to index).
        id_to_string.push(Arc::from(""));

        Self {
            inner: Arc::new(RwLock::new(Inner {
                lower_map: HashMap::new(),
                exact_map: HashMap::new(),
                id_to_string,
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
        // Reserved slot-0: the empty string maps to id 0 without consuming a
        // fresh id. All other strings start from id 1 (next_id initialised to 1).
        if s.is_empty() {
            return StringTokens {
                lower: StringId(0),
                normal: StringId(0),
            };
        }

        // Fast path: exact string already interned. This is the overwhelming
        // common case while parsing many files (identifiers repeat constantly),
        // and it takes a shared read lock so parse threads don't serialize on it.
        {
            let inner = self.inner.read();
            if let Some(&existing) = inner.exact_map.get(s) {
                return existing;
            }
        }

        let mut inner = self.inner.write();
        intern_locked(&mut inner, s)
    }

    /// Intern many strings under a single write-lock acquisition.
    ///
    /// Returns one [`StringTokens`] per input, in order. The result for each
    /// string is byte-for-byte identical to calling [`intern`](Self::intern) on
    /// it individually (same ID assignment order, same lower-companion
    /// interning) — this just amortizes the lock and double-checked-locking
    /// overhead across the whole batch, which matters on cache load where every
    /// string is a fresh miss.
    pub fn intern_batch<'a, I>(&self, it: I) -> Vec<StringTokens>
    where
        I: IntoIterator<Item = &'a str>,
    {
        let it = it.into_iter();
        let mut out = Vec::with_capacity(it.size_hint().0);
        let mut inner = self.inner.write();
        for s in it {
            // Mirror `intern`: the empty string maps to slot-0 without locking a
            // fresh id, and `intern_locked` assumes a non-empty input.
            if s.is_empty() {
                out.push(StringTokens {
                    lower: StringId(0),
                    normal: StringId(0),
                });
            } else {
                out.push(intern_locked(&mut inner, s));
            }
        }
        out
    }

    /// Run `f` while holding the read lock once, giving it a [`StringResolver`]
    /// that resolves `StringId`s to `&str` without per-call locking or cloning.
    ///
    /// Prefer this over many [`get_string`](Self::get_string) calls on hot paths
    /// (e.g. cache serialization) that resolve a large batch of ids: the read
    /// lock is acquired a single time for the whole closure.
    pub fn with_read<R>(&self, f: impl FnOnce(StringResolver<'_>) -> R) -> R {
        let inner = self.inner.read();
        f(StringResolver { inner: &inner })
    }

    /// Retrieve the original (case‑preserving) text for a `StringId`.
    pub fn get_string(&self, id: StringId) -> Option<String> {
        let inner = self.inner.read();
        inner
            .id_to_string
            .get(id.0 as usize)
            .map(|s| s.as_ref().to_string())
    }

    /// Borrow the original (case-preserving) text for a `StringId` without
    /// cloning it. Takes the read lock once and calls `f` on the borrowed
    /// `&str`, returning `f`'s result (or `None` if the id is out of range).
    ///
    /// Prefer this over [`get_string`](Self::get_string) on hot paths that only
    /// need to compare or inspect the text (e.g. `== "NOT"`,
    /// `eq_ignore_ascii_case`): it avoids a per-call `String` allocation.
    pub fn with_string<R>(&self, id: StringId, f: impl FnOnce(&str) -> R) -> Option<R> {
        let inner = self.inner.read();
        inner.id_to_string.get(id.0 as usize).map(|s| f(s.as_ref()))
    }

    /// Number of unique lower‑cased strings (not counting normal variants).
    pub fn len(&self) -> usize {
        let inner = self.inner.read();
        inner.lower_map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Approximate heap footprint of the interner, for profiling. Counts the
    /// `id_to_string` byte payload, the metadata array, and the two key maps'
    /// payloads. Pointer/control overhead is ignored, so this is a lower bound.
    pub fn stats(&self) -> StringTableStats {
        let inner = self.inner.read();
        let id_to_string_bytes: usize = inner.id_to_string.iter().map(|s| s.len()).sum();
        let map_key_bytes: usize = inner
            .lower_map
            .keys()
            .chain(inner.exact_map.keys())
            .map(|s| s.len())
            .sum();
        StringTableStats {
            entries: inner.id_to_string.len(),
            id_to_string_bytes,
            map_key_bytes,
        }
    }
}

/// Approximate per-component heap footprint of a [`StringTable`].
#[derive(Debug, Clone, Copy, Default)]
pub struct StringTableStats {
    /// Number of slots in `id_to_string` (≈ interned strings, normal + lower).
    pub entries: usize,
    /// Total bytes of the interned string payloads.
    pub id_to_string_bytes: usize,
    /// Total bytes of the lower_map + exact_map key payloads.
    pub map_key_bytes: usize,
}

impl StringTableStats {
    /// Sum of all counted byte fields (a lower bound on heap use).
    pub fn total_bytes(&self) -> usize {
        self.id_to_string_bytes + self.map_key_bytes
    }
}

/// Borrowed resolver handed to [`StringTable::with_read`]. Holds the read lock
/// for its lifetime so a batch of id lookups pays the locking cost once.
pub struct StringResolver<'a> {
    inner: &'a Inner,
}

impl StringResolver<'_> {
    /// Resolve a `StringId` to its borrowed text, or `None` if out of range.
    pub fn get(&self, id: StringId) -> Option<&str> {
        self.inner
            .id_to_string
            .get(id.0 as usize)
            .map(|s| s.as_ref())
    }
}

/// Core interning logic, run with the write lock already held. Assumes `s` is
/// non-empty (the empty-string slot-0 case is handled before locking) and that
/// the exact-string fast path has already been checked under a read lock —
/// it re-checks `exact_map` here so it is also correct when called directly
/// under the write lock (double-checked locking / batch interning).
fn intern_locked(inner: &mut Inner, s: &str) -> StringTokens {
    // Re-check after acquiring the write lock: another thread may have interned
    // this exact string in the gap (double-checked locking).
    if let Some(&existing) = inner.exact_map.get(s) {
        return existing;
    }

    let lower_key = s.to_lowercase();

    // Fast path 2: lower key exists → allocate new normal variant.
    if let Some(&existing_lower) = inner.lower_map.get(lower_key.as_str()) {
        debug_assert!(
            inner.next_id < u32::MAX,
            "StringTable id space exhausted (would collide with StringId::NULL)"
        );
        let normal_id = inner.next_id;
        inner.next_id += 1;
        let normal_arc: Arc<str> = Arc::from(s);
        inner.id_to_string.push(Arc::clone(&normal_arc));
        let token = StringTokens {
            lower: existing_lower.lower,
            normal: StringId(normal_id),
        };
        inner.exact_map.insert(normal_arc, token);
        return token;
    }

    // Slow path: brand‑new lower key.
    debug_assert!(
        inner.next_id < u32::MAX - 1,
        "StringTable id space exhausted (would collide with StringId::NULL)"
    );
    let normal_id = inner.next_id;
    let lower_id = normal_id + 1;
    inner.next_id = lower_id + 1;

    // Allocate each string once; share the same Arc between id_to_string and
    // the corresponding map key so there is only one heap allocation per string.
    let normal_arc: Arc<str> = Arc::from(s);
    let lower_arc: Arc<str> = Arc::from(lower_key.as_str());

    inner.id_to_string.reserve_exact(2);
    inner.id_to_string.push(Arc::clone(&normal_arc)); // normal_id
    inner.id_to_string.push(Arc::clone(&lower_arc)); // lower_id

    let lower_token = StringTokens {
        lower: StringId(lower_id),
        normal: StringId(lower_id),
    };
    let normal_token = StringTokens {
        lower: StringId(lower_id),
        normal: StringId(normal_id),
    };

    inner.lower_map.insert(lower_arc, lower_token);
    inner.exact_map.insert(normal_arc, normal_token);
    normal_token
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
    fn with_string_borrows_without_clone() {
        let table = StringTable::new();
        let a = table.intern("NOT");
        // Borrow + compare without allocating an owned String.
        assert_eq!(table.with_string(a.normal, |s| s == "NOT"), Some(true));
        assert_eq!(
            table.with_string(a.lower, |s| s.eq_ignore_ascii_case("not")),
            Some(true)
        );
        // Out-of-range id yields None and never calls the closure.
        assert_eq!(table.with_string(StringId(9_999), |_| true), None);
        // Same text as get_string.
        assert_eq!(
            table.with_string(a.normal, |s| s.to_string()),
            table.get_string(a.normal)
        );
    }

    #[test]
    fn intern_batch_matches_per_string() {
        // A fresh table built via intern_batch must hand out byte-identical
        // tokens (same ids, same order) to one built with per-string intern.
        let inputs = [
            "foo", "FOO", "foo", "bar", "Bar", "", "\"q\"", "baz", "FOO", "bar",
        ];

        let single = StringTable::new();
        let want: Vec<_> = inputs.iter().map(|s| single.intern(s)).collect();

        let batch = StringTable::new();
        let got = batch.intern_batch(inputs.iter().copied());

        assert_eq!(want, got);
        // And the resolved text agrees for every id.
        for (a, b) in want.iter().zip(got.iter()) {
            assert_eq!(single.get_string(a.normal), batch.get_string(b.normal));
            assert_eq!(single.get_string(a.lower), batch.get_string(b.lower));
        }
    }

    #[test]
    fn with_read_resolves_without_per_call_lock() {
        let table = StringTable::new();
        let a = table.intern("hello");
        let b = table.intern("WORLD");
        table.with_read(|r| {
            assert_eq!(r.get(a.normal), Some("hello"));
            assert_eq!(r.get(b.normal), Some("WORLD"));
            assert_eq!(r.get(StringId(9_999)), None);
        });
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
