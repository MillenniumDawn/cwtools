//! Shared leaf helpers and the [`ValidationError`] type used across the
//! validation submodules.

use cwtools_game::scope_engine::ScopeContext;
use cwtools_parser::ast::{Child, ParsedFile, Value};
use cwtools_rules::rules_types::*;
use cwtools_string_table::string_table::{StringTable, StringTokens};

use cwtools_error_codes::ErrorCode;
pub use cwtools_error_codes::ErrorSeverity;

#[derive(Debug, Clone, PartialEq)]
pub struct ValidationError {
    pub message: String,
    pub severity: ErrorSeverity,
    pub line: u32,
    pub col: u16,
    pub file: String,
    /// CW### error code, e.g. "CW262" for an unexpected property node. The id is
    /// `&'static` (the catalog `ErrorCode.id`), so no per-error allocation.
    pub code: Option<&'static str>,
}

impl ValidationError {
    /// Build a diagnostic from a catalog [`ErrorCode`]: pulls severity and id
    /// from the code and formats its template with `args`. Centralizes the
    /// code→severity mapping so call sites don't restate it.
    pub(crate) fn from_code(
        code: &ErrorCode,
        file: &str,
        line: u32,
        col: u16,
        args: &[&str],
    ) -> Self {
        ValidationError {
            message: code.format(args),
            severity: code.severity,
            line,
            col,
            file: file.to_string(),
            code: Some(code.id),
        }
    }

    /// Like [`from_code`](Self::from_code) but with an explicit `severity` and a
    /// pre-built `message`, for the sites whose severity is decided at runtime
    /// (cardinality) or whose message is assembled from a match arm. Still tags
    /// the diagnostic with the catalog `code.id`.
    pub(crate) fn from_code_with(
        code: &ErrorCode,
        severity: ErrorSeverity,
        file: &str,
        line: u32,
        col: u16,
        message: String,
    ) -> Self {
        ValidationError {
            message,
            severity,
            line,
            col,
            file: file.to_string(),
            code: Some(code.id),
        }
    }
}

/// Number of significant decimal places in a numeric string; trailing zeros do
/// not count (`0.1230` has 3). Used for the CW270 32-bit precision check.
pub(crate) fn decimal_places(s: &str) -> usize {
    match s.split_once('.') {
        Some((_, frac)) => frac.trim_end_matches('0').len(),
        None => 0,
    }
}

/// Whether `key` names a scope (keyword, scope link, or iterator) rather than a
/// variable. A `variable_field` value naming a scope must not be flagged as an
/// unset variable (CW246).
pub(crate) fn resolves_as_scope_key(ctx: &ScopeContext, key: &str) -> bool {
    const KEYWORDS: &[&str] = &[
        "this",
        "root",
        "prev",
        "prevprev",
        "prevprevprev",
        "from",
        "fromfrom",
        "fromfromfrom",
        "fromfromfromfrom",
    ];
    if KEYWORDS.iter().any(|kw| key.eq_ignore_ascii_case(kw)) {
        return true;
    }
    // Only the registry lookups need a lowercased key; allocate at most once.
    let k = key.to_ascii_lowercase();
    ctx.registry.id_of(&k).is_some() || ctx.registry.links.contains_key(&k)
}

/// True when a leaf value is numerically zero (`0`, `0.0`, `"0"`, …). Used by
/// the CW235 zero-modifier check.
pub(crate) fn value_is_zero(value: &Value) -> bool {
    match value {
        Value::Int(n) => *n == 0,
        Value::Float(f) => *f == 0.0,
        Value::String(_) | Value::QString(_) => false,
        _ => false,
    }
}

/// Whole-segment path containment (prevents `events` from matching
/// `.../my_events_backup/x.txt`). One shared implementation with the indexer so
/// a file is indexed by the same type that validates it.
pub(crate) use cwtools_index::path_contains_segment;

/// Start (line, col) of a child node, for locating block-level diagnostics.
pub(crate) fn child_start_pos(child: &Child, ast: &ParsedFile) -> Option<(u32, u16)> {
    match child {
        Child::Leaf(i) => {
            let l = &ast.arena.leaves[*i as usize];
            Some((l.pos.start.line, l.pos.start.col))
        }
        Child::LeafValue(i) => {
            let lv = &ast.arena.leaf_values[*i as usize];
            Some((lv.pos.start.line, lv.pos.start.col))
        }
        _ => None,
    }
}

pub(crate) fn child_key_matches(
    child: &Child,
    ast: &ParsedFile,
    table: &StringTable,
    filter_key: &str,
) -> bool {
    match child {
        Child::Leaf(idx) => {
            let leaf = &ast.arena.leaves[*idx as usize];
            table
                .with_string(leaf.key.normal, |s| {
                    unquote_key(s).eq_ignore_ascii_case(unquote_key(filter_key))
                })
                .unwrap_or(false)
        }
        _ => false,
    }
}

/// A block key that isn't a known scope command but resolves to a scope via the
/// game data: a numeric state/province id, an upper-case country/state tag, or a
/// `prefix:data` reference. Plain lowercase effect/trigger names are excluded.
pub(crate) fn looks_like_data_ref(key: &str) -> bool {
    if key.is_empty() {
        return false;
    }
    key.contains(':')
        || key.bytes().all(|b| b.is_ascii_digit())
        || key.chars().any(|c| c.is_ascii_uppercase())
}

/// Check that a string has the YYYY.MM.DD shape for a CW date field.
pub(crate) fn is_date_shape(s: &str) -> bool {
    // Exactly YYYY.MM.DD — three numeric parts separated by dots.
    let mut parts = s.splitn(4, '.');
    let (Some(y), Some(m), Some(d), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    y.parse::<i32>().is_ok() && m.parse::<u32>().is_ok() && d.parse::<u32>().is_ok()
}

/// Check that a string has the YYYY.MM.DD or YYYY.MM.DD.HH shape for a CW
/// datetime field. Mirrors F# `IsValidDateTime` which accepts both 3 and 4
/// dot-separated numeric parts (3-part dates are valid for datetime fields).
pub(crate) fn is_datetime_shape(s: &str) -> bool {
    let mut parts = s.splitn(5, '.');
    match (
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
        parts.next(),
    ) {
        (Some(_), Some(_), Some(_), None, None) => is_date_shape(s),
        (Some(y), Some(m), Some(d), Some(h), None) => {
            y.parse::<i32>().is_ok()
                && m.parse::<u32>().is_ok()
                && d.parse::<u32>().is_ok()
                && h.parse::<u32>().is_ok()
        }
        _ => false,
    }
}

/// Size heuristic shared by every enum-membership check: a populated enum is
/// treated as authoritative only when it is small (≤ 5 members). Larger enums
/// are likely incomplete game-data catalogues (equipment_categories, tech
/// folders, idea tokens, …) that the CWT rules rarely enumerate in full, so an
/// unlisted value is accepted rather than flagged. Keep this in one place so the
/// `enum_contains` / `parsed_pattern_matches` / `field_matches_key` sites stay
/// in agreement.
pub(crate) fn enum_is_authoritative(def: &EnumDefinition) -> bool {
    def.values.len() > 5
}

/// Enum membership test. An absent or empty enum (members come from game data
/// that isn't statically loaded — provinces, ship_units, ...) is permissive.
///
/// For populated enums, we use a size heuristic: small enums (≤ 5 members) are
/// treated as authoritative — an unlisted value is a genuine error.  Larger
/// enums are likely incomplete game-data catalogues (equipment_categories,
/// tech folders, idea tokens, …) and are treated as advisory — any non-empty
/// value is accepted, because the CWT rules rarely enumerate every member.
pub(crate) fn enum_contains(
    ruleset: &cwtools_rules::rules_types::RuleSet,
    enum_name: &str,
    value: &str,
) -> bool {
    match ruleset.enum_by_name.get(enum_name) {
        Some(&idx) if !ruleset.enums[idx].values.is_empty() => {
            // Enum membership is case-insensitive (F# lowercases both the enum
            // values and the checked key — FieldValidators.fs `getLowerKey` +
            // RuleValidationService.fs `.lower`). e.g. `containerOrientations`
            // is authored UPPER_LEFT/CENTER but files use upper_left/center.
            if ruleset.enum_values_contains_ci(idx, value) {
                return true;
            }
            // An enum whose members are `@`-prefixed scripted constants (e.g.
            // `enum[command_cap_increase] = { @tier1_cp_cap_increase ... }`) accepts
            // the resolved literal value too (`command_cap_increase = 10`), which we
            // can't resolve statically — be permissive.
            if ruleset.enum_has_at_constant(idx) {
                return true;
            }
            // Large enums are likely incomplete game-data catalogues — accept any
            // non-empty value rather than flag every unlisted member.
            // Small enums (≤ 5 members) are authoritative; an unlisted value is
            // a genuine error.
            if enum_is_authoritative(&ruleset.enums[idx]) {
                return !value.is_empty();
            }
            false
        }
        _ => true,
    }
}

/// Zero-copy variant of `match_text`: borrows the string from the table,
/// strips surrounding quotes via a slice (no allocation), and passes the
/// resulting `&str` to `f`.  Returns `f`'s value, or the default if the id
/// is out of range.
pub(crate) fn with_match_text<R: Default>(
    table: &StringTable,
    t: &StringTokens,
    f: impl FnOnce(&str) -> R,
) -> R {
    table
        .with_string(t.normal, |s| {
            let unquoted = if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
                &s[1..s.len() - 1]
            } else {
                s
            };
            f(unquoted)
        })
        .unwrap_or_default()
}

/// Strip a balanced pair of surrounding double-quotes from a child key.
pub(crate) fn unquote_key(s: &str) -> &str {
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        &s[1..s.len() - 1]
    } else {
        s
    }
}

pub(crate) fn leaf_value_to_string(value: &Value, table: &StringTable) -> String {
    match value {
        Value::String(t) | Value::QString(t) => table.get_string(t.normal).unwrap_or_default(),
        Value::Float(f) => f.to_string(),
        Value::Int(i) => i.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Clause(_) => "{...}".to_string(),
    }
}

pub(crate) fn severity_to_error(sev: &Severity) -> ErrorSeverity {
    sev.into()
}

pub fn error_hash(error: &ValidationError) -> String {
    let sev_str = match error.severity {
        ErrorSeverity::Error => "error",
        ErrorSeverity::Warning => "warning",
        ErrorSeverity::Information => "information",
        ErrorSeverity::Hint => "hint",
    };
    format!(
        "{}|{}|{}|{}",
        sev_str, error.file, error.line, error.message
    )
}
