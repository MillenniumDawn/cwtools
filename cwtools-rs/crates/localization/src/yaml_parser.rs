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

use crate::commands::{Lang, LocEntry, LocFile, Position, key_to_language};
use crate::loc_string::parse_loc_elements;

// ---- UTF-8 BOM check -------------------------------------------------------

/// UTF-8 byte-order mark bytes.
const UTF8_BOM: [u8; 3] = [0xEF, 0xBB, 0xBF];

/// Diagnostic produced when a `.yml` loc file is missing the UTF-8 BOM.
///
/// Mirrors F# `STLLocalisationString.checkFileEncoding`.
#[derive(Debug, Clone, PartialEq)]
pub struct MissingBomDiagnostic {
    /// The file name / stream name that failed the check.
    pub file: String,
}

/// Check whether `bytes` starts with the UTF-8 BOM (0xEF 0xBB 0xBF).
///
/// Returns `Ok(())` when the BOM is present, `Err(MissingBomDiagnostic)`
/// otherwise, so callers can emit the diagnostic without panicking.
pub fn check_utf8_bom(bytes: &[u8], file: &str) -> Result<(), MissingBomDiagnostic> {
    if bytes.len() >= 3 && bytes[..3] == UTF8_BOM {
        Ok(())
    } else {
        Err(MissingBomDiagnostic {
            file: file.to_string(),
        })
    }
}

// ---- Language-header / filename diagnostics --------------------------------

/// Diagnostic kind for YAML localisation filename / header issues.
///
/// Mirrors F# `STLLocalisationString.checkLocFileName` error codes.
#[derive(Debug, Clone, PartialEq)]
pub enum LangHeaderDiagnostic {
    /// The file has no recognisable `l_xxx:` header (MissingLocFileLangHeader).
    MissingLocFileLangHeader { file: String },
    /// The filename carries no recognised language tag (MissingLocFileLang).
    MissingLocFileLang { file: String },
    /// The filename language tag and the header language tag disagree
    /// (LocFileLangMismatch).
    LocFileLangMismatch {
        file: String,
        filename_lang: Lang,
        header_lang: Lang,
    },
}

/// Extract the language tag from a filename (stem only, without extension).
///
/// `"l_english"` is matched anywhere in the stem — e.g. a file named
/// `events_l_english.yml` returns `Some(Lang::English)`.
pub fn lang_from_filename(stem: &str) -> Option<Lang> {
    let lower = stem.to_ascii_lowercase();
    if lower.contains("l_english") {
        Some(Lang::English)
    } else if lower.contains("l_french") {
        Some(Lang::French)
    } else if lower.contains("l_german") {
        Some(Lang::German)
    } else if lower.contains("l_spanish") {
        Some(Lang::Spanish)
    } else if lower.contains("l_russian") {
        Some(Lang::Russian)
    } else if lower.contains("l_polish") {
        Some(Lang::Polish)
    } else if lower.contains("l_braz_por") {
        Some(Lang::BrazPor)
    } else if lower.contains("l_simp_chinese") {
        Some(Lang::SimpChinese)
    } else if lower.contains("l_japanese") {
        Some(Lang::Japanese)
    } else if lower.contains("l_korean") {
        Some(Lang::Korean)
    } else if lower.contains("l_turkish") {
        Some(Lang::Turkish)
    } else if lower.contains("l_default") {
        Some(Lang::Default)
    } else {
        None
    }
}

/// Validate the language header of a YAML loc file against the filename.
///
/// * If the file is `languages.yml`, skip all checks (special file).
/// * If the header is `l_default`, treat as wildcard (OK for any filename).
/// * If the header language is unrecognised → `MissingLocFileLangHeader`.
/// * If the filename carries no language tag → `MissingLocFileLang`.
/// * If header and filename disagree → `LocFileLangMismatch`.
///
/// `header_key` is the raw `l_xxx` token found in the file (without `:`).
/// `file` is the path / name used for diagnostics.
pub fn check_loc_file_lang(file: &str, header_key: &str) -> Option<LangHeaderDiagnostic> {
    // Derive the stem (basename without extension) from `file`
    let stem = std::path::Path::new(file)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(file);

    // Special-case: languages.yml is exempt from all checks
    if stem.eq_ignore_ascii_case("languages") {
        return None;
    }

    let header_lang = key_to_language(header_key);

    // l_default in the header is always OK (mirrors F# `STLLang.Default` branch)
    if header_key.eq_ignore_ascii_case("l_default") {
        return None;
    }

    // Unrecognised header
    let header_lang = match header_lang {
        Some(l) => l,
        None => {
            return Some(LangHeaderDiagnostic::MissingLocFileLangHeader {
                file: file.to_string(),
            });
        }
    };

    // Filename carries no language tag
    let filename_lang = match lang_from_filename(stem) {
        Some(l) => l,
        None => {
            return Some(LangHeaderDiagnostic::MissingLocFileLang {
                file: file.to_string(),
            });
        }
    };

    // Mismatch
    if filename_lang != header_lang {
        return Some(LangHeaderDiagnostic::LocFileLangMismatch {
            file: file.to_string(),
            filename_lang,
            header_lang,
        });
    }

    None
}

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
        let version =
            if !remainder.is_empty() && remainder.starts_with(|c: char| c.is_ascii_digit()) {
                let digit_str: String = remainder
                    .chars()
                    .take_while(|c| c.is_ascii_digit())
                    .collect();
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

        // Check for chars outside the allowed loc-value Unicode ranges.
        // Mirrors F# parser `desc` production which stops at the first
        // char where `isLocValueChar` returns false, then records the
        // position of that char as `errorRange`.
        let error_range = find_invalid_loc_char(desc).map(|byte_off| {
            // Work out which column the bad char is at (1-based)
            let col = desc[..byte_off].chars().count() + 1;
            Position::new(name, i + 1, col)
        });

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
                crate::loc_string::LocElement::JominiCommand(cmds) => Some(
                    cmds.iter()
                        .map(|c| crate::commands::JominiCommand {
                            key: c.key.clone(),
                            params: c
                                .params
                                .iter()
                                .map(|p| match p {
                                    crate::loc_string::JominiParam::Literal(s) => {
                                        crate::commands::JominiParam::Literal(s.clone())
                                    }
                                    crate::loc_string::JominiParam::Commands(_) => {
                                        crate::commands::JominiParam::Literal("nested".to_string())
                                    }
                                })
                                .collect(),
                        })
                        .collect::<Vec<_>>(),
                ),
                _ => None,
            })
            .flatten()
            .collect();

        entries.push(LocEntry {
            key: key.to_string(),
            value: version,
            desc: desc.to_string(),
            position,
            error_range, // set by isLocValueChar check above
            refs,
            commands,
            jomini_commands,
        });

        i += 1;
    }

    // Collect file-level diagnostics: header/filename lang validation
    let mut file_diagnostics = Vec::new();
    if let Some(diag) = check_loc_file_lang(name, language_key) {
        let msg = match &diag {
            LangHeaderDiagnostic::MissingLocFileLangHeader { file } => {
                format!(
                    "CW-MissingLocFileLangHeader: '{}' has no recognised language header",
                    file
                )
            }
            LangHeaderDiagnostic::MissingLocFileLang { file } => {
                format!(
                    "CW-MissingLocFileLang: '{}' filename carries no language tag",
                    file
                )
            }
            LangHeaderDiagnostic::LocFileLangMismatch {
                file,
                filename_lang,
                header_lang,
            } => {
                format!(
                    "CW-LocFileLangMismatch: '{}' filename says '{}' but header says '{}'",
                    file, filename_lang, header_lang
                )
            }
        };
        file_diagnostics.push(msg);
    }

    Ok(LocFile {
        language_prefix: language_key.to_string(),
        lang,
        entries,
        file_diagnostics,
    })
}

// ---- isLocValueChar --------------------------------------------------------

/// Check whether a char falls within the allowed Unicode ranges for a loc value.
///
/// Mirrors F# `isLocValueChar` (YAMLLocalisationParser.fs:17-30).
pub fn is_loc_value_char(c: char) -> bool {
    let u = c as u32;
    // ASCII letters
    c.is_ascii_alphabetic()
    // U+0020–U+007E  (printable ASCII)
    || (u >= 0x0020 && u <= 0x007E)
    // U+00A0–U+024F  (Latin Extended)
    || (u >= 0x00A0 && u <= 0x024F)
    // U+0401–U+045F  (Cyrillic)
    || (u >= 0x0401 && u <= 0x045F)
    // U+0490–U+0491  (Cyrillic supplement)
    || (u >= 0x0490 && u <= 0x0491)
    // U+1E00–U+1EFF  (Latin Extended Additional)
    || (u >= 0x1E00 && u <= 0x1EFF)
    // U+2013–U+2044  (General Punctuation subset)
    || (u >= 0x2013 && u <= 0x2044)
    // U+2460–U+24FF  (Enclosed Alphanumerics)
    || (u >= 0x2460 && u <= 0x24FF)
    // U+4E00–U+9FFF  (CJK Unified Ideographs)
    || (u >= 0x4E00 && u <= 0x9FFF)
    // U+3000–U+30FF  (CJK Symbols + Katakana/Hiragana)
    || (u >= 0x3000 && u <= 0x30FF)
    // U+FE30–U+FE4F  (CJK Compatibility Forms)
    || (u >= 0xFE30 && u <= 0xFE4F)
    // U+FF00–U+FFEF  (Halfwidth and Fullwidth Forms)
    || (u >= 0xFF00 && u <= 0xFFEF)
}

/// Scan `desc` for the first character that fails `is_loc_value_char`.
///
/// Returns `Some(byte_offset)` at the position of the offending char,
/// or `None` if all chars are valid.
pub fn find_invalid_loc_char(desc: &str) -> Option<usize> {
    for (offset, c) in desc.char_indices() {
        if !is_loc_value_char(c) {
            return Some(offset);
        }
    }
    None
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
        .and_then(|q| trimmed[q..].find('#').map(|h| q + h))
        .or_else(|| {
            // no last quote → look for first '#' overall
            trimmed.find('#')
        });

    // 3.  Truncate at the first '#' after last quote (if any)
    let mut effective = match (first_hash_after_quote, last_quote) {
        (Some(h), Some(q)) if h > q => &trimmed[..h],
        _ => trimmed,
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
        assert_eq!(
            file.entries[8].desc,
            "\"this is valid loc\" but with invalid stuff outside"
        );

        assert_eq!(file.entries[9].key, "loc_key10");
        assert_eq!(
            file.entries[9].desc,
            "\"this is valid loc\" #but with comment"
        );
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

    // ---- UTF-8 BOM tests ---------------------------------------------------

    #[test]
    fn test_bom_present() {
        let bytes: &[u8] = &[0xEF, 0xBB, 0xBF, b'l', b'_'];
        assert!(check_utf8_bom(bytes, "test.yml").is_ok());
    }

    #[test]
    fn test_bom_missing() {
        let bytes: &[u8] = b"l_english:\n";
        let result = check_utf8_bom(bytes, "test.yml");
        assert!(result.is_err());
        let diag = result.unwrap_err();
        assert_eq!(diag.file, "test.yml");
    }

    #[test]
    fn test_bom_too_short() {
        let bytes: &[u8] = &[0xEF, 0xBB];
        assert!(check_utf8_bom(bytes, "short.yml").is_err());
    }

    #[test]
    fn test_bom_wrong_bytes() {
        // UTF-16 LE BOM — not a UTF-8 BOM
        let bytes: &[u8] = &[0xFF, 0xFE, 0x00];
        assert!(check_utf8_bom(bytes, "utf16.yml").is_err());
    }

    // ---- lang-header / filename diagnostics tests --------------------------

    #[test]
    fn test_lang_header_matching_filename() {
        // events_l_english.yml with l_english: header — no diagnostic
        let text = "l_english:\n key: \"value\"\n";
        let file = parse_loc_text(text, "events_l_english.yml").unwrap();
        assert!(
            file.file_diagnostics.is_empty(),
            "should have no diagnostics: {:?}",
            file.file_diagnostics
        );
    }

    #[test]
    fn test_lang_header_mismatch() {
        // events_l_english.yml but header says l_french:
        let text = "l_french:\n key: \"value\"\n";
        let file = parse_loc_text(text, "events_l_english.yml").unwrap();
        assert_eq!(file.file_diagnostics.len(), 1);
        assert!(
            file.file_diagnostics[0].contains("LocFileLangMismatch"),
            "{:?}",
            file.file_diagnostics
        );
    }

    #[test]
    fn test_lang_header_missing_from_filename() {
        // file with no lang tag in name, valid header
        let text = "l_english:\n key: \"value\"\n";
        let file = parse_loc_text(text, "events.yml").unwrap();
        assert_eq!(file.file_diagnostics.len(), 1);
        assert!(
            file.file_diagnostics[0].contains("MissingLocFileLang"),
            "{:?}",
            file.file_diagnostics
        );
    }

    #[test]
    fn test_lang_header_unrecognised_header() {
        // unrecognised header key
        let text = "l_klingon:\n key: \"value\"\n";
        let file = parse_loc_text(text, "events_l_english.yml").unwrap();
        assert_eq!(file.file_diagnostics.len(), 1);
        assert!(
            file.file_diagnostics[0].contains("MissingLocFileLangHeader"),
            "{:?}",
            file.file_diagnostics
        );
    }

    #[test]
    fn test_lang_header_default_is_ok() {
        // l_default header should always pass
        let text = "l_default:\n key: \"value\"\n";
        let file = parse_loc_text(text, "events_l_english.yml").unwrap();
        assert!(
            file.file_diagnostics.is_empty(),
            "l_default should not produce diagnostics: {:?}",
            file.file_diagnostics
        );
    }

    #[test]
    fn test_languages_yml_exempt() {
        // languages.yml is special-cased
        let text = "l_english:\n key: \"value\"\n";
        let file = parse_loc_text(text, "languages.yml").unwrap();
        assert!(
            file.file_diagnostics.is_empty(),
            "languages.yml should be exempt: {:?}",
            file.file_diagnostics
        );
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
