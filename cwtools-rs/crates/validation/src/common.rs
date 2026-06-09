//! Shared leaf helpers and the [`ValidationError`] type used across the
//! validation submodules.

use cwtools_game::scope_engine::{ScopeContext, ScopeId};
use cwtools_game::scope_registry::ScopeRegistry;
use cwtools_parser::ast::{Child, ParsedFile, Value};
use cwtools_rules::rules_types::*;
use cwtools_string_table::string_table::{StringTable, StringTokens};
use std::collections::HashMap;

use cwtools_error_codes::ErrorCode;
pub use cwtools_error_codes::ErrorSeverity;

#[derive(Debug, Clone, PartialEq)]
pub struct ValidationError {
    pub message: String,
    pub severity: ErrorSeverity,
    pub line: u32,
    pub col: u16,
    pub file: String,
    /// CW### error code, e.g. "CW262" for an unexpected property node.
    pub code: Option<String>,
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
            code: Some(code.id.to_string()),
        }
    }
}

pub(crate) fn get_scope_name(scope: ScopeId, registry: &ScopeRegistry) -> String {
    registry.name_of(scope)
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
    let k = key.to_ascii_lowercase();
    matches!(
        k.as_str(),
        "this"
            | "root"
            | "prev"
            | "prevprev"
            | "prevprevprev"
            | "from"
            | "fromfrom"
            | "fromfromfrom"
            | "fromfromfromfrom"
    ) || ctx.registry.id_of(&k).is_some()
        || ctx.registry.links.contains_key(&k)
}

/// Whether the trigger/effect/target scope checks (CW104/105/106/243/244/245/248)
/// are on. Now ON by default (the scope engine is config-driven and accurate);
/// set `CWTOOLS_NO_SCOPE_CHECKS=1` as an escape hatch to turn them off.
pub(crate) fn scope_checks_enabled() -> bool {
    static ON: std::sync::LazyLock<bool> =
        std::sync::LazyLock::new(|| std::env::var("CWTOOLS_NO_SCOPE_CHECKS").is_err());
    *ON
}

/// Whether the project-wide "variable has not been set" check (CW246) is on.
/// OFF by default: it needs a COMPLETE variable index, and a mod that defines
/// variables through dynamic `@`-concatenation or base-game scripts the index
/// hasn't collected would flood. Opt in with `CWTOOLS_VAR_CHECKS=1` once the
/// index is proven complete for a corpus. The local numeric checks (CW270/271)
/// run regardless of this gate.
pub(crate) fn var_checks_enabled() -> bool {
    static ON: std::sync::LazyLock<bool> =
        std::sync::LazyLock::new(|| std::env::var("CWTOOLS_VAR_CHECKS").is_ok());
    *ON
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
        Child::ValueClause(i) => {
            let vc = &ast.arena.value_clauses[*i as usize];
            Some((vc.pos.start.line, vc.pos.start.col))
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
    let parts: Vec<&str> = s.splitn(4, '.').collect();
    parts.len() == 3
        && parts[0].parse::<i32>().is_ok()
        && parts[1].parse::<u32>().is_ok()
        && parts[2].parse::<u32>().is_ok()
}

/// Check that a string has the YYYY.MM.DD or YYYY.MM.DD.HH shape for a CW
/// datetime field. Mirrors F# `IsValidDateTime` which accepts both 3 and 4
/// dot-separated numeric parts (3-part dates are valid for datetime fields).
pub(crate) fn is_datetime_shape(s: &str) -> bool {
    let parts: Vec<&str> = s.splitn(5, '.').collect();
    match parts.len() {
        3 => is_date_shape(s),
        4 => {
            parts[0].parse::<i32>().is_ok()
                && parts[1].parse::<u32>().is_ok()
                && parts[2].parse::<u32>().is_ok()
                && parts[3].parse::<u32>().is_ok()
        }
        _ => false,
    }
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
    enum_map: &HashMap<&str, &EnumDefinition>,
    enum_name: &str,
    value: &str,
) -> bool {
    match enum_map.get(enum_name) {
        Some(def) if !def.values.is_empty() => {
            // Enum membership is case-insensitive (F# lowercases both the enum
            // values and the checked key — FieldValidators.fs `getLowerKey` +
            // RuleValidationService.fs `.lower`). e.g. `containerOrientations`
            // is authored UPPER_LEFT/CENTER but files use upper_left/center.
            if def.values.iter().any(|v| v.eq_ignore_ascii_case(value)) {
                return true;
            }
            // An enum whose members are `@`-prefixed scripted constants (e.g.
            // `enum[command_cap_increase] = { @tier1_cp_cap_increase ... }`) accepts
            // the resolved literal value too (`command_cap_increase = 10`), which we
            // can't resolve statically — be permissive.
            if def.values.iter().any(|v| v.starts_with('@')) {
                return true;
            }
            // Large enums are likely incomplete game-data catalogues — accept any
            // non-empty value rather than flag every unlisted member.
            // Small enums (≤ 5 members) are authoritative; an unlisted value is
            // a genuine error.
            if def.values.len() > 5 {
                return !value.is_empty();
            }
            false
        }
        _ => true,
    }
}

pub(crate) fn match_text(table: &StringTable, t: &StringTokens) -> String {
    let s = table.get_string(t.normal).unwrap_or_default();
    if s.len() >= 2 && s.starts_with('"') && s.ends_with('"') {
        s[1..s.len() - 1].to_string()
    } else {
        s
    }
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

pub(crate) fn severity_to_error(sev: Severity) -> ErrorSeverity {
    match sev {
        Severity::Error => ErrorSeverity::Error,
        Severity::Warning => ErrorSeverity::Warning,
        Severity::Information => ErrorSeverity::Information,
        Severity::Hint => ErrorSeverity::Hint,
    }
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
