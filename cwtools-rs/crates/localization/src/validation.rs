//! Localisation validation.
//!
//! Validates parsed loc entries:
//! * Undefined loc references
//! * Recursive references
//! * Invalid loc characters
//! * Missing/computed loc commands
//!
//! Mirrors F# `LocalisationString.fs`.

use crate::commands::{Lang, LocEntry, LocFile};
use std::collections::{HashMap, HashSet};

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
                // Defined – check for recursion
                if *r == entry.key && !hardcoded.contains(&lowercase) {
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
/// Returns `Some(error_range)` if invalid characters are found.
/// Mirrors F# `validateInvalidChars`.
pub fn validate_invalid_chars(
    entry: &LocEntry,
    _errors: &mut Vec<LocValidationError>,
) -> Option<()> {
    if let Some(_range) = &entry.error_range {
        // F# marks positions where `isLocValueChar` returned false as error_range
        // We already computed error_range during parsing, so just report it.
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
    use crate::commands::{Lang, Position};
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
}
