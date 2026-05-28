//! YAML localisation file parser.
//!
//! Parses `l_xxx.yml` files into `LocFile` structures.
//!
//! Handles HOI4's "cursed quotes" semantics:
//! * Quotes inside loc strings are DATA, not delimiters
//! * The VALIDATOR (not the parser) checks balancing via `LastIndexOf('"')`
//! * `desc` = raw text after colon (and optional version) to end of line
//! * Comments (`#`) are NOT stripped at parse time — they are part of desc
//!
//! Examples:
//! * `key: "value"` → desc = `"value"` (stored raw, quotes included)
//! * `key: "this is "also" valid"` → desc = `"this is "also" valid"`
//! * `key: "a" #comment` → desc = `"a" #comment`

use crate::commands::{key_to_language, Lang, LocEntry, LocFile, Position};
use crate::loc_string::parse_loc_elements;

/// Parse a single YAML localisation file from text.
///
/// Returns `Err` if the language header is malformed.
/// Entries with invalid content still parse; the caller is responsible for
/// calling `validate_quotes` / `validate_invalid_chars`.
pub fn parse_loc_text(text: &str, name: &str) -> Result<LocFile, String> {
    let lines: Vec<&str> = text.lines().collect();
    let mut i = 0;

    // 1.  Skip leading blank lines and comments
    while i < lines.len() {
        let trimmed = lines[i].trim_start();
        if trimmed.is_empty() || trimmed.starts_with('#') {
            i += 1;
        } else {
            break;
        }
    }

    if i >= lines.len() {
        return Err("empty file after stripping comments".to_string());
    }

    // 2.  Language header:  `l_english:`  (colon required)
    let header = lines[i];
    let colon = header
        .find(':')
        .ok_or_else(|| format!("missing ':' in language header: {header:?}"))?;
    let language_key = header[..colon].trim_end();
    let lang = key_to_language(language_key);
    i += 1;

    let mut entries = Vec::new();

    // 3.  Entry lines: key:value"desc text"
    //      key:123 "desc text"       (with version)
    //      key: "desc text"          (without version)
    //      key: "desc text"#comment  (comment is part of desc)
    while i < lines.len() {
        let line = lines[i];
        let trimmed = line.trim_start();

        if trimmed.is_empty() || trimmed.starts_with('#') {
            i += 1;
            continue;
        }

        let colon_pos = trimmed.find(':');
        if colon_pos.is_none() {
            i += 1;
            continue; // malformed line, skip (parser tolerance)
        }
        let colon_pos = colon_pos.unwrap();
        let key = trimmed[..colon_pos].trim_end();

        // remainder after the colon
        let mut remainder = &trimmed[colon_pos + 1..];

        // optional version number (digits right after colon with no space)
        let version = if !remainder.is_empty()
            && remainder.starts_with(|c: char| c.is_ascii_digit())
        {
            let digit_str: String = remainder.chars().take_while(|c| c.is_ascii_digit()).collect();
            let v = digit_str.parse::<u32>().ok();
            remainder = &remainder[digit_str.len()..];
            v
        } else {
            None
        };

        // strip one leading space (the convention after `:`)
        // but keep everything else including # comments,
        // because in F# `#` is a valid `isLocValueChar`.
        let desc = if remainder.starts_with(' ') {
            &remainder[1..]
        } else {
            remainder
        };

        let position = Position::new(name, i + 1, 1); // 1-based line numbers

        // Lazy-parse loc elements (refs, commands, etc.)
        let elements = parse_loc_elements(desc);
        let refs: Vec<String> = elements
            .iter()
            .filter_map(|e| match e {
                crate::loc_string::LocElement::Ref(s) => Some(s.clone()),
                _ => None,
            })
            .collect();
        let commands: Vec<String> = elements
            .iter()
            .filter_map(|e| match e {
                crate::loc_string::LocElement::Command(s) => Some(s.clone()),
                _ => None,
            })
            .collect();
        let jomini_commands: Vec<crate::commands::JominiCommand> = elements
            .iter()
            .filter_map(|e| match e {
                crate::loc_string::LocElement::JominiCommand(cmds) => {
                    Some(cmds.iter().map(|c| crate::commands::JominiCommand {
                        key: c.key.clone(),
                        params: c.params.iter().map(|p| match p {
                            crate::loc_string::JominiParam::Literal(s) => crate::commands::JominiParam::Literal(s.clone()),
                            crate::loc_string::JominiParam::Commands(_) => crate::commands::JominiParam::Literal("nested".to_string()),
                        }).collect(),
                    }).collect::<Vec<_>>())
                }
                _ => None,
            })
            .flatten()
            .collect();

        entries.push(LocEntry {
            key: key.to_string(),
            value: version,
            desc: desc.to_string(),
            position,
            error_range: None, // populated later during validation
            refs,
            commands,
            jomini_commands,
        });

        i += 1;
    }

    Ok(LocFile {
        language_prefix: language_key.to_string(),
        lang,
        entries,
    })
}

/// Quote / imbalanced-quote validation (mirrors F# `validateQuotes`).
///
/// Returns `true` if the entry passes validation (no CW-quote-error).
/// Populates `entry.error_range` and returns `false` on failure.
pub fn validate_quotes(entry: &mut LocEntry) -> bool {
    let trimmed = entry.desc.trim();

    // 1.  Find last double-quote in trimmed desc
    let last_quote = trimmed.rfind('"');

    // 2.  Find first '#' after that last quote
    let first_hash_after_quote = last_quote
        .and_then(|q| {
            trimmed[q..].find('#').map(|h| q + h)
        })
        .or_else(|| {
            // no last quote → look for first '#' overall
            trimmed.find('#')
        });

    // 3.  Truncate at the first '#' after last quote (if any)
    let mut effective = match (first_hash_after_quote, last_quote) {
        (Some(h), Some(q)) if h > q => {
            &trimmed[..h]
        }
        _ => {
            trimmed
        }
    };

    // Edge: HOI4 allows quoted text like:
    //   "...text" #comment   → effective = "...text"  (trim stops at #)
    // The Trim() would have already removed leading/trailing spaces.
    // We strip trailing spaces after hash truncation.
    effective = effective.trim_end();

    // 4.  HOI4 "cursed" behaviour:  if the desc contains quotes but
    //     doesn't START with a quote and doesn't END with a quote,
    //     we DON'T generate a quote error.
    let starts = effective.starts_with('"');
    let ends = effective.ends_with('"');

    if starts && ends {
        // fully quoted → fine (CW doesn't complain)
        true
    } else if !starts && !ends {
        // no quotes at all → fine
        true
    } else {
        // starts XOR ends → imbalanced quotes
        entry.error_range = Some(entry.position.clone());
        false
    }
}

/// Check for REPLACE_ME / TODO_CD placeholders.
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

/* ======================================================================== */
/* Tests                                                                   */
/* ======================================================================== */

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::Lang;

    #[test]
    fn test_parse_language() {
        assert_eq!(key_to_language("l_english"), Some(Lang::English));
        assert_eq!(key_to_language("l_french"), Some(Lang::French));
        assert_eq!(key_to_language("l_unknown"), None);
    }

    #[test]
    fn test_hoi4_cursed_quotes() {
        let text = "l_english:\n loc_key1: \"this is valid loc\"\n loc_key2: \"this is \"also\" valid loc\"\n loc_key3: \"this is \\\"also\\\" valid loc\"\n loc_key4: \"this is \\\"also valid loc\"\n loc_key5: \"this is \"also valid loc\"\n loc_key6: \"this is invalid loc\n loc_key7: this is invalid loc\"\n loc_key8: this is invalid loc\n loc_key9: \"this is valid loc\" but with invalid stuff outside\n loc_key10: \"this is valid loc\" #but with comment\n";

        let file = parse_loc_text(text, "test.yml").unwrap();
        assert_eq!(file.lang, Some(Lang::English));
        assert_eq!(file.entries.len(), 10);

        assert_eq!(file.entries[0].key, "loc_key1");
        assert_eq!(file.entries[0].desc, "\"this is valid loc\"");

        assert_eq!(file.entries[1].key, "loc_key2");
        assert_eq!(file.entries[1].desc, "\"this is \"also\" valid loc\"");

        assert_eq!(file.entries[2].key, "loc_key3");
        assert_eq!(file.entries[2].desc, "\"this is \\\"also\\\" valid loc\"");

        assert_eq!(file.entries[3].key, "loc_key4");
        assert_eq!(file.entries[3].desc, "\"this is \\\"also valid loc\"");

        assert_eq!(file.entries[4].key, "loc_key5");
        assert_eq!(file.entries[4].desc, "\"this is \"also valid loc\"");

        assert_eq!(file.entries[5].key, "loc_key6");
        assert_eq!(file.entries[5].desc, "\"this is invalid loc");

        assert_eq!(file.entries[6].key, "loc_key7");
        assert_eq!(file.entries[6].desc, "this is invalid loc\"");

        assert_eq!(file.entries[7].key, "loc_key8");
        assert_eq!(file.entries[7].desc, "this is invalid loc");

        assert_eq!(file.entries[8].key, "loc_key9");
        assert_eq!(file.entries[8].desc, "\"this is valid loc\" but with invalid stuff outside");

        assert_eq!(file.entries[9].key, "loc_key10");
        assert_eq!(file.entries[9].desc, "\"this is valid loc\" #but with comment");
    }

    #[test]
    fn test_validate_cursed_quotes() {
        let text = "l_english:\n loc_key1: \"this is valid loc\"\n loc_key2: \"this is \"also\" valid loc\"\n loc_key6: \"this is invalid loc\n loc_key7: this is invalid loc\"\n";

        let mut file = parse_loc_text(text, "test.yml").unwrap();

        // Valid: balanced quotes
        assert!(validate_quotes(&mut file.entries[0]), "loc1 should pass");
        assert!(file.entries[0].error_range.is_none());

        // Valid: balanced quotes (embedded quotes are inside)
        assert!(validate_quotes(&mut file.entries[1]), "loc2 should pass");
        assert!(file.entries[1].error_range.is_none());

        // Invalid: opening quote but no closing
        let ok = validate_quotes(&mut file.entries[2]);
        assert!(!ok, "loc6 should fail (missing closing quote)");

        // Invalid: closing quote but no opening
        let ok = validate_quotes(&mut file.entries[3]);
        assert!(!ok, "loc7 should fail (missing opening quote)");
    }

    #[test]
    fn test_version_number() {
        let text = "l_english:\n key:0 \"desc\" \n";
        let file = parse_loc_text(text, "test.yml").unwrap();
        assert_eq!(file.entries[0].value, Some(0));
        assert_eq!(file.entries[0].desc, "\"desc\" ");
    }

    #[test]
    fn test_comments_in_desc() {
        let text = "l_english:\n key: \"a\"#comment\n";
        let file = parse_loc_text(text, "test.yml").unwrap();
        // `#comment` is part of desc (per F# parser)
        assert_eq!(file.entries[0].desc, "\"a\"#comment");

        let mut entry = file.entries[0].clone();
        assert!(validate_quotes(&mut entry), "hash truncation test");
    }

    #[test]
    fn test_loc_key11_complex() {
        let text = "l_english:\n loc_key11: \"this is valid loc\" #but this is also valid and read as part of the string due to quote after\n";
        let mut file = parse_loc_text(text, "test.yml").unwrap();
        assert_eq!(file.entries[0].key, "loc_key11");
        assert!(validate_quotes(&mut file.entries[0]), "loc11 should pass");
    }

    #[test]
    fn test_empty_file() {
        assert!(parse_loc_text("", "test.yml").is_err());
    }

    #[test]
    fn test_commands_in_desc() {
        let text = "l_english:\n key: \"Hello $TITLE$ [GetName]\"\n";
        let file = parse_loc_text(text, "test.yml").unwrap();
        let entry = &file.entries[0];
        assert_eq!(entry.desc, "\"Hello $TITLE$ [GetName]\"");
        assert_eq!(entry.refs, vec!["TITLE"]);
        assert_eq!(entry.commands, vec!["GetName"]);
    }

    #[test]
    fn test_event_target_command() {
        let text = "l_english:\n key: \"[event_target:foo]\"\n";
        let file = parse_loc_text(text, "test.yml").unwrap();
        assert_eq!(file.entries[0].commands, vec!["event_target:foo"]);
    }

    #[test]
    fn test_question_variable() {
        let text = "l_english:\n key: \"[?my_var]\"\n";
        let file = parse_loc_text(text, "test.yml").unwrap();
        assert_eq!(file.entries[0].commands, vec!["?my_var"]);
    }
}
