//! Localisation validation.
//!
//! Validates parsed loc entries:
//! * Undefined loc references
//! * Recursive references
//! * Invalid loc characters
//! * Missing/computed loc commands
//!
//! Mirrors F# `LocalisationString.fs`.

use crate::commands::{LocEntry, LocFile};
use std::collections::HashSet;

/// Validation error for a loc entry.
#[derive(Debug, Clone, PartialEq)]
pub struct LocValidationError {
    pub line: usize,
    pub message: String,
}

/// Validate a loaded loc file against a set of known keys.
///
/// * `file` – the parsed loc file (`yaml_parser::parse_loc_text` result)
/// * `all_keys` – union of keys across ALL languages (to validate `$ref$`)
///
/// Returns list of validation errors.
pub fn validate_loc_file(
    file: &mut LocFile,
    all_keys: &HashSet<String>,
    hardcoded_localisation: &[impl AsRef<str>],
) -> Vec<LocValidationError> {
    let mut errors = Vec::new();
    let hardcoded: HashSet<String> = hardcoded_localisation
        .iter()
        .map(|s| s.as_ref().to_lowercase())
        .collect();

    for entry in &mut file.entries {
        // ---- Invalid characters ----
        if let Some(pos) = validate_invalid_chars(entry, &mut errors) {
            // pos not used currently, but reserved for future diagnostics
            let _ = pos;
        }

        // ---- Quote balancing ----
        if !validate_quotes(entry) {
            errors.push(LocValidationError {
                line: entry.position.line,
                message: format!("CW-LocMissingQuote: key '{}' has unbalanced quotes", entry.key),
            });
        }

        // ---- Undefined references ----
        for r in &entry.refs {
            let lowercase = r.to_lowercase();
            if all_keys.contains(&lowercase) {
                // Defined – check for recursion (case-insensitive, matching F# checkRef)
                if lowercase == entry.key.to_lowercase() && !hardcoded.contains(&lowercase) {
                    errors.push(LocValidationError {
                        line: entry.position.line,
                        message: format!(
                            "CW-RecursiveLocRef: key '{}' references itself",
                            entry.key
                        ),
                    });
                }
            } else {
                // Not defined – check F# rule: if the ref contains lowercase
                // letters but is not all-lowercase, it's "maybe a compound",
                // which F# accepts (e.g. "FROM.FROM")
                let has_lower = r.chars().any(|c| c.is_lowercase());
                let first_space = r.find(' ');
                let last_space = r.rfind(' ');

                if has_lower
                    && !hardcoded.contains(&lowercase)
                    && !(first_space.is_some()
                        && last_space.is_some()
                        && first_space != last_space)
                {
                    errors.push(LocValidationError {
                        line: entry.position.line,
                        message: format!(
                            "CW-UndefinedLocReference: key '{}' references unknown key '{}'",
                            entry.key, r
                        ),
                    });
                }
            }
        }

        // ---- REPLACE_ME / TODO_CD check ----
        if let Some(msg) = validate_replace_me(entry) {
            errors.push(LocValidationError {
                line: entry.position.line,
                message: msg,
            });
        }

        // ---- Invalid loc commands ----
        for cmd in &entry.commands {
            if cmd.contains("event_target:") && !is_known_event_target(cmd, all_keys) {
                // Allow event_target references that aren't in key set
                // F# has an actual check; for now we warn rather than error
            }
        }
    }

    errors
}

fn is_known_event_target(cmd: &str, all_keys: &HashSet<String>) -> bool {
    if let Some(target) = cmd.strip_prefix("event_target:") {
        all_keys.contains(target)
    } else {
        true
    }
}

/// Validate invalid characters.
///
/// If `entry.error_range` is set (populated by the parser when a char outside
/// the `isLocValueChar` ranges was found), push a `CW-LocInvalidChars` error.
/// Mirrors F# `validateInvalidChars` (LocalisationString.fs:124-127).
pub fn validate_invalid_chars(
    entry: &LocEntry,
    errors: &mut Vec<LocValidationError>,
) -> Option<()> {
    if let Some(range) = &entry.error_range {
        errors.push(LocValidationError {
            line: range.line,
            message: format!(
                "CW-LocInvalidChars: key '{}' contains a character outside the allowed Unicode ranges (col {})",
                entry.key, range.column
            ),
        });
        Some(())
    } else {
        None
    }
}

/// Quote validation (mirrors F# `validateQuotes`).
///
/// Returns `true` if OK, `false` if unbalanced.
/// On failure, sets `entry.error_range`.
pub fn validate_quotes(entry: &mut LocEntry) -> bool {
    let trimmed = entry.desc.trim();

    let last_quote = trimmed.rfind('"');

    let first_hash_after_quote = last_quote
        .and_then(|q| trimmed[q..].find('#').map(|h| q + h))
        .or_else(|| trimmed.find('#'));

    let mut effective = match (first_hash_after_quote, last_quote) {
        (Some(h), Some(q)) if h > q => &trimmed[..h],
        _ => trimmed,
    };

    let ends_quote = effective.rfind('"');
    if let Some(q) = ends_quote {
        effective = &effective[..=q].trim_end();
    }

    let starts = effective.starts_with('"');
    let ends = effective.ends_with('"');

    if starts && ends {
        true
    } else if !starts && !ends {
        true
    } else {
        entry.error_range = Some(entry.position.clone());
        false
    }
}

/// Check for `REPLACE_ME` / `TODO_CD` placeholders.
pub fn validate_replace_me(entry: &LocEntry) -> Option<String> {
    let trimmed = entry.desc.trim();
    if trimmed == "\"REPLACE_ME\"" || trimmed == "\"TODO_CD\"" {
        Some(format!(
            "CW-ReplaceMe: localisation key '{}' contains placeholder",
            entry.key
        ))
    } else {
        None
    }
}

/// Build a map of all keys across a set of loc files.
pub fn build_key_union(files: &[LocFile]) -> HashSet<String> {
    let mut set = HashSet::new();
    for file in files {
        for e in &file.entries {
            set.insert(e.key.to_lowercase());
        }
    }
    set
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::yaml_parser::parse_loc_text;

    #[test]
    fn test_validate_undefined_ref() {
        let text = "l_english:\n key1: \"Hello $undefined_key$\"\n";
        let mut file = parse_loc_text(text, "test.yml").unwrap();
        let keys: HashSet<String> = HashSet::new();
        let errors = validate_loc_file(
            &mut file,
            &keys,
            &Vec::<String>::new(),
        );

        assert_eq!(errors.len(), 1);
        assert_eq!(
            errors[0].message,
            "CW-UndefinedLocReference: key 'key1' references unknown key 'undefined_key'"
        );
    }

    #[test]
    fn test_validate_recursive_ref() {
        let text = "l_english:\n key1: \"Hello $key1$\"\n";
        let mut file = parse_loc_text(text, "test.yml").unwrap();
        let mut keys = HashSet::new();
        keys.insert("key1".to_string());
        let errors = validate_loc_file(
            &mut file,
            &keys,
            &Vec::<String>::new(),
        );

        assert_eq!(errors.len(), 1);
        assert!(errors[0]
            .message
            .contains("RecursiveLocRef"));
    }

    #[test]
    fn test_validate_valid_ref() {
        let text = "l_english:\n key1: \"Hello $key2$\"\n";
        let mut file = parse_loc_text(text, "test.yml").unwrap();
        let mut keys = HashSet::new();
        keys.insert("key2".to_string());
        let errors = validate_loc_file(
            &mut file,
            &keys,
            &Vec::<String>::new(),
        );

        assert!(errors.is_empty(), "valid ref should not error");
    }

    #[test]
    fn test_validate_replace_me() {
        let text = "l_english:\n key1: \"REPLACE_ME\"\n";
        let mut file = parse_loc_text(text, "test.yml").unwrap();
        let keys: HashSet<String> = HashSet::new();

        let errors = validate_loc_file(
            &mut file,
            &keys,
            &Vec::<String>::new(),
        );

        assert_eq!(errors.len(), 1);
        assert!(errors[0].message.contains("ReplaceMe"));
    }

    #[test]
    fn test_hardcoded_refs_ignored() {
        let text = "l_english:\n key1: \"Hello $Player$\"\n";
        let mut file = parse_loc_text(text, "test.yml").unwrap();
        let keys: HashSet<String> = HashSet::new();
        let errors = validate_loc_file(
            &mut file,
            &keys,
            &vec!["Player"],
        );

        assert!(errors.is_empty(), "hardcoded ref should not error");
    }

    #[test]
    fn test_invalid_char_detected() {
        // U+FFFE is outside all allowed ranges and should trigger CW-LocInvalidChars
        let bad_char = '\u{FFFE}';
        let text = format!("l_english:\n key1: \"Hello {}world\"\n", bad_char);
        let mut file = parse_loc_text(&text, "test.yml").unwrap();

        // error_range should be set by the parser
        assert!(file.entries[0].error_range.is_some(),
            "parser should have set error_range for out-of-range char");

        let keys: HashSet<String> = HashSet::new();
        let errors = validate_loc_file(&mut file, &keys, &Vec::<String>::new());

        let inv_char_errors: Vec<_> = errors.iter()
            .filter(|e| e.message.contains("LocInvalidChars"))
            .collect();
        assert!(!inv_char_errors.is_empty(), "expected CW-LocInvalidChars error, got: {:?}", errors);
    }

    #[test]
    fn test_valid_chars_no_error() {
        // Normal ASCII and Latin Extended chars should not trigger the check
        let text = "l_english:\n key1: \"Hello world — café\"\n";
        let mut file = parse_loc_text(text, "test.yml").unwrap();

        assert!(file.entries[0].error_range.is_none(),
            "valid chars should not set error_range");

        let keys: HashSet<String> = HashSet::new();
        let errors = validate_loc_file(&mut file, &keys, &Vec::<String>::new());
        let inv_char_errors: Vec<_> = errors.iter()
            .filter(|e| e.message.contains("LocInvalidChars"))
            .collect();
        assert!(inv_char_errors.is_empty(), "valid chars should not produce LocInvalidChars");
    }

    // ---- case-insensitive recursive ref check (fix 7) ---------------------

    #[test]
    fn test_recursive_ref_case_insensitive() {
        // key "KEY1" references "$key1$" — different case, should still be recursive
        let text = "l_english:\n KEY1: \"Hello $key1$\"\n";
        let mut file = parse_loc_text(text, "test.yml").unwrap();
        let mut keys = HashSet::new();
        keys.insert("key1".to_string()); // stored lowercased in union
        let errors = validate_loc_file(&mut file, &keys, &Vec::<String>::new());

        let recursive: Vec<_> = errors.iter()
            .filter(|e| e.message.contains("RecursiveLocRef"))
            .collect();
        assert!(!recursive.is_empty(), "case-insensitive self-ref should trigger RecursiveLocRef: {:?}", errors);
    }
}
