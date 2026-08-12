//! The CW diagnostic catalog, as the CLI sees it.
//!
//! `cwtools_error_codes` exposes one `pub const` per code but no list of them,
//! so the list is mirrored here: `--ignore-code` / `--only-code` validate
//! against it (a typo is an error, not a silent no-op). Not every mirrored code
//! is currently emitted; pending codes are marked in `PENDING_CODES`. The SARIF
//! report only turns emitted codes into `tool.driver.rules`.

use cwtools_error_codes::ErrorCode;

macro_rules! catalog {
    ($($name:ident),+ $(,)?) => {
        /// Known diagnostic codes, including wired and pending checks.
        const CATALOG: &[(&str, ErrorCode)] = &[
            $((stringify!($name), cwtools_error_codes::$name),)+
        ];
    };
}

catalog![
    CW001_PARSE_ERROR,
    CW100_MISSING_LOCALISATION,
    CW104_INCORRECT_TRIGGER_SCOPE,
    CW105_INCORRECT_EFFECT_SCOPE,
    CW106_INCORRECT_SCOPE_SCOPE,
    CW107_EVENT_EVERY_TICK,
    CW108_RESEARCH_LEADER_AREA,
    CW109_RESEARCH_LEADER_TECH,
    CW110_TECH_CAT_MISSING,
    CW113_MISSING_FILE,
    CW120_POSSIBLE_PRETRIGGER,
    CW121_EMPTY_IF,
    CW122_LOC_KEY_IN_INLINE,
    CW220_UNSAVED_EVENT_TARGET,
    CW221_MAYBE_UNSAVED_EVENT_TARGET,
    CW222_UNDEFINED_EVENT,
    CW223_INCORRECT_NOT_USAGE,
    CW225_UNDEFINED_LOC_REFERENCE,
    CW226_INVALID_LOC_COMMAND,
    CW227_UNKNOWN_SECTION_TEMPLATE,
    CW228_MISSING_SECTION_SLOT,
    CW229_UNKNOWN_COMPONENT_TEMPLATE,
    CW230_MISMATCHED_COMPONENT_AND_SLOT,
    CW231_UNUSED_TECH,
    CW233_UNDEFINED_ENTITY,
    CW234_REPLACE_ME_LOC,
    CW235_ZERO_MODIFIER,
    CW236_DEPRECATED_ELSE,
    CW237_AMBIGUOUS_IF_ELSE,
    CW238_IF_ELSE_ORDER,
    CW239_UNUSED_TYPE,
    CW240_UNEXPECTED_VALUE,
    CW242_WRONG_NUMBER,
    CW243_TARGET_WRONG_SCOPE,
    CW244_INVALID_TARGET,
    CW245_ERROR_IN_TARGET,
    CW246_UNSET_VARIABLE,
    CW247_RULE_WRONG_SCOPE,
    CW248_INVALID_SCOPE_COMMAND,
    CW250_PLANET_KILLER_MISSING,
    CW251_UNNECESSARY_BOOLEAN,
    CW253_DEPRECATED_SET_NAME,
    CW254_WRONG_ENCODING,
    CW255_MISSING_LOC_FILE_LANG,
    CW256_MISSING_LOC_FILE_LANG_HEADER,
    CW257_LOC_FILE_LANG_MISMATCH,
    CW259_RECURSIVE_LOC_REF,
    CW260_LOC_COMMAND_WRONG_SCOPE,
    CW261_DUPLICATE_TYPE_DEF,
    CW262_UNEXPECTED_PROPERTY_NODE,
    CW263_UNEXPECTED_PROPERTY_LEAF,
    CW264_UNEXPECTED_PROPERTY_LEAF_VALUE,
    CW265_UNEXPECTED_PROPERTY_VALUE_CLAUSE,
    CW266_LOC_COMMAND_NOT_IN_DATA_TYPE,
    CW267_UNEXPECTED_ALIAS_KEY_VALUE,
    CW268_LOC_MISSING_QUOTE,
    CW269_OPTIMISATION_MERGE_LIST,
    CW270_VARIABLE_TOO_SMALL,
    CW271_VARIABLE_INT_ONLY,
    CW272_FROM_RULES_CUSTOM_ERROR,
    CW273_UNDEFINED_MODIFIER_TYPE,
    CW274_INLINE_SCRIPT_ERROR,
    CW275_LOC_INVALID_CHARS,
    CW276_LOC_KEY_INVALID_CHARS,
    CW277_ALIAS_BRANCH_LIMIT,
    CW280_REDUNDANT_DEFAULT_FIELD,
    CW281_EMPTY_LIMIT,
    CW282_REDUNDANT_DEFAULT_BOOL,
    CW500_TYPE_NOT_FOUND,
];

// Codes defined in `error_codes` but not yet wired.
const PENDING_CODES: &[&str] = &[
    "CW220", "CW221", "CW228", "CW230", "CW233", "CW269", "CW273", "CW274",
];

fn is_pending_code(id: &str) -> bool {
    PENDING_CODES.iter().any(|p| p.eq_ignore_ascii_case(id))
}

/// The catalog entry for `id` (case-insensitive), or `None` when no CLI parse-time
/// validation entry exists for that code.
pub(crate) fn entry(id: &str) -> Option<&'static (&'static str, ErrorCode)> {
    CATALOG.iter().find(|(_, c)| c.id.eq_ignore_ascii_case(id))
}

/// The entry for emitted codes only, for SARIF rule metadata.
pub(crate) fn emitted_entry(id: &str) -> Option<&'static (&'static str, ErrorCode)> {
    entry(id).filter(|(_, c)| !is_pending_code(c.id))
}

/// Parse an `--ignore-code` / `--only-code` value, normalising it to the
/// catalog's spelling so the filter compares case-insensitively. For clap's
/// `value_parser`: an unknown code fails the run instead of quietly matching
/// nothing.
pub(crate) fn parse_code(s: &str) -> Result<String, String> {
    entry(s).map(|(_, c)| c.id.to_string()).ok_or_else(|| {
        format!(
            "unknown diagnostic code '{s}': expected a CWxxx code the validator emits \
             (e.g. CW100, CW113, CW225); docs/ERROR_CODES.md lists them all"
        )
    })
}

/// Whether `code` survives an `--only-code` / `--ignore-code` policy. Both empty
/// (the default) keeps everything, so today's runs are unaffected.
pub(crate) fn wanted(code: &str, only: &[String], ignored: &[String]) -> bool {
    (only.is_empty() || only.iter().any(|c| c == code)) && !ignored.iter().any(|c| c == code)
}

/// The words of a catalog const name, minus its `CW###_` prefix.
fn words(const_name: &str) -> impl Iterator<Item = &str> {
    const_name
        .split_once('_')
        .map_or(const_name, |(_, rest)| rest)
        .split('_')
}

/// SARIF `rule.name`: `CW100_MISSING_LOCALISATION` → `MissingLocalisation`.
pub(crate) fn rule_name(const_name: &str) -> String {
    words(const_name).fold(String::new(), |mut out, w| {
        let mut cs = w.chars();
        if let Some(first) = cs.next() {
            out.extend(first.to_uppercase());
            out.extend(cs.flat_map(char::to_lowercase));
        }
        out
    })
}

/// Prose form of a catalog const name, for the SARIF description of the codes
/// whose message template is a bare pass-through.
pub(crate) fn rule_summary(const_name: &str) -> String {
    let mut out = String::new();
    for w in words(const_name) {
        if out.is_empty() {
            out.push_str(&rule_name(w));
        } else {
            out.push(' ');
            out.extend(w.chars().flat_map(char::to_lowercase));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The enumeration above is hand-mirrored, so a code added to
    /// `cwtools_error_codes` without a line here could be invisible to
    /// `--ignore-code`. Diff the two lists straight from the source rather than
    /// trusting the mirror.
    #[test]
    fn catalog_covers_supported_error_code_consts() {
        let src = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../error_codes/src/lib.rs"),
        )
        .expect("error_codes source is a sibling crate in the same workspace");
        const RETIRED_CODES: &[&str] = &["CW258_LOC_FILE_LANG_WRONG_PLACE"];

        let mut declared: Vec<&str> = src
            .lines()
            .filter_map(|l| {
                let (name, ty) = l.strip_prefix("pub const ")?.split_once(':')?;
                ty.trim_start().starts_with("ErrorCode").then_some(name)
            })
            .filter(|name| !RETIRED_CODES.contains(name))
            .collect();
        declared.sort_unstable();
        let mut mirrored: Vec<&str> = CATALOG.iter().map(|(name, _)| *name).collect();
        mirrored.sort_unstable();
        assert_eq!(
            declared, mirrored,
            "crates/cli/src/codes.rs must list all supported ErrorCode consts in crates/error_codes"
        );
    }

    /// Every diagnostic links to `docs/ERROR_CODES.md#cw###`, so a code with no
    /// anchor there lands the reader at the top of the file instead of on its
    /// row. Checked against the doc itself, not a second list to keep in step.
    #[test]
    fn every_code_has_a_documentation_anchor() {
        let doc = std::fs::read_to_string(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../docs/ERROR_CODES.md"),
        )
        .expect("the error-code reference is checked in beside the crates");

        let missing: Vec<&str> = CATALOG
            .iter()
            .map(|(_, c)| c.id)
            .filter(|id| !doc.contains(&format!("<a id=\"{}\">", id.to_ascii_lowercase())))
            .collect();
        assert!(
            missing.is_empty(),
            "docs/ERROR_CODES.md needs an <a id=\"cw###\"> anchor for: {missing:?}"
        );
    }

    #[test]
    fn ids_are_unique() {
        let mut ids: Vec<&str> = CATALOG.iter().map(|(_, c)| c.id).collect();
        ids.sort_unstable();
        let before = ids.len();
        ids.dedup();
        assert_eq!(before, ids.len(), "duplicate CW id in the catalog");
    }

    #[test]
    fn parse_code_is_case_insensitive_and_normalises() {
        assert_eq!(parse_code("cw100").unwrap(), "CW100");
        assert_eq!(parse_code("CW100").unwrap(), "CW100");
    }

    #[test]
    fn parse_code_rejects_an_unknown_code() {
        let err = parse_code("CW999").unwrap_err();
        assert!(err.contains("CW999"), "got: {err}");
        assert!(err.contains("ERROR_CODES.md"), "got: {err}");
    }

    #[test]
    fn parse_code_rejects_retired_code() {
        let err = parse_code("CW258").unwrap_err();
        assert!(err.contains("CW258"), "got: {err}");
    }

    #[test]
    fn parse_code_accepts_pending_codes() {
        assert_eq!(parse_code("CW220").unwrap(), "CW220");
        assert_eq!(parse_code("CW269").unwrap(), "CW269");
    }

    #[test]
    fn emitted_entry_skips_pending_codes() {
        assert!(entry("CW220").is_some(), "pending code is still parseable");
        assert!(
            emitted_entry("CW220").is_none(),
            "pending code stays out of SARIF rules"
        );
        assert!(emitted_entry("CW113").is_some());
    }

    #[test]
    fn wanted_defaults_to_keeping_everything() {
        assert!(wanted("CW100", &[], &[]));
    }

    #[test]
    fn wanted_applies_only_then_ignore() {
        let only = vec!["CW100".to_string(), "CW113".to_string()];
        let ignored = vec!["CW113".to_string()];
        assert!(wanted("CW100", &only, &[]));
        assert!(!wanted("CW225", &only, &[]));
        assert!(!wanted("CW113", &only, &ignored), "ignore wins over only");
        assert!(!wanted("CW113", &[], &ignored));
    }

    #[test]
    fn rule_names_drop_the_code_prefix() {
        assert_eq!(
            rule_name("CW100_MISSING_LOCALISATION"),
            "MissingLocalisation"
        );
        assert_eq!(
            rule_summary("CW100_MISSING_LOCALISATION"),
            "Missing localisation"
        );
        assert_eq!(rule_name("CW001_PARSE_ERROR"), "ParseError");
    }
}
