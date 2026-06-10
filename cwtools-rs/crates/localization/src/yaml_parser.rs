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

use crate::commands::{Lang, LocEntry, LocFile, LocParseError, Position, key_to_language};
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
    // Strip leading UTF-8 BOM(s). Loc files are required to be UTF-8-with-BOM
    // (see CW254) and the disk reader keeps the BOM in the string, so without
    // this the `l_english:` header parses as `\u{FEFF}l_english` and the
    // language comes back unknown — which silently empties the loc-key index.
    // Some real files in the wild carry a doubled BOM (`\u{FEFF}\u{FEFF}`);
    // F# tolerates it via a substring match, so strip every leading BOM.
    let text = text.trim_start_matches('\u{feff}');
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
    // Trim both ends: a header may carry leading whitespace (`  l_english:`),
    // which F# tolerates via `line.Trim()`. `trim_end` alone would leave the
    // leading space and fail the exact `key_to_language` lookup.
    let language_key = header[..colon].trim();
    let lang = key_to_language(language_key);
    i += 1;

    let mut entries = Vec::new();
    let mut parse_errors: Vec<LocParseError> = Vec::new();

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
            // Malformed line: no colon separator. Record a CW001 parse error and
            // continue recovering (lenient parser; mirrors F# `Failure` path).
            parse_errors.push(LocParseError {
                line: i + 1,
                message: format!("unexpected content (no ':' separator): {:?}", trimmed),
            });
            i += 1;
            continue;
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
        let desc = remainder.strip_prefix(' ').unwrap_or(remainder);

        let position = Position::new(name, i + 1, 1); // 1-based line numbers

        // Compute the column offset of `desc`'s start within the full line.
        // The key + colon + version + optional space are all ASCII, so byte
        // offset == char offset for that prefix.
        let leading_ws = line.len() - trimmed.len();
        // desc starts at (trimmed.len() - desc.len()) bytes into trimmed.
        let desc_col_offset = leading_ws + (trimmed.len() - desc.len());

        // Check for chars outside the allowed loc-value Unicode ranges.
        // Mirrors F# parser `desc` production which stops at the first
        // char where `isLocValueChar` returns false, then records the
        // position of that char as `errorRange`.
        let error_range = find_invalid_loc_char(desc).map(|byte_off| {
            // Column within desc (0-based) + prefix offset gives the line column.
            let col = desc_col_offset + desc[..byte_off].chars().count() + 1;
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
        let jomini_commands: Vec<Vec<crate::commands::JominiCommand>> = elements
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
        path: name.to_string(),
        language_prefix: language_key.to_string(),
        lang,
        entries,
        file_diagnostics,
        parse_errors,
        // Unknown here; set by the disk-reading path (`LocService`) where the
        // raw bytes are available.
        encoding: None,
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
    || (0x0020..=0x007E).contains(&u)
    // U+00A0–U+024F  (Latin Extended)
    || (0x00A0..=0x024F).contains(&u)
    // U+0401–U+045F  (Cyrillic)
    || (0x0401..=0x045F).contains(&u)
    // U+0490–U+0491  (Cyrillic supplement)
    || (0x0490..=0x0491).contains(&u)
    // U+1E00–U+1EFF  (Latin Extended Additional)
    || (0x1E00..=0x1EFF).contains(&u)
    // U+2013–U+2044  (General Punctuation subset)
    || (0x2013..=0x2044).contains(&u)
    // U+2460–U+24FF  (Enclosed Alphanumerics)
    || (0x2460..=0x24FF).contains(&u)
    // U+4E00–U+9FFF  (CJK Unified Ideographs)
    || (0x4E00..=0x9FFF).contains(&u)
    // U+3000–U+30FF  (CJK Symbols + Katakana/Hiragana — Japanese)
    || (0x3000..=0x30FF).contains(&u)
    // U+3400–U+4DBF  (CJK Unified Ideographs Extension A — Chinese)
    || (0x3400..=0x4DBF).contains(&u)
    // U+FE30–U+FE4F  (CJK Compatibility Forms)
    || (0xFE30..=0xFE4F).contains(&u)
    // U+FF00–U+FFEF  (Halfwidth and Fullwidth Forms)
    || (0xFF00..=0xFFEF).contains(&u)
    // ── Intentional divergence from F# `isLocValueChar`: accept scripts the
    // game renders fine but F# rejected. Per project goal, the game wins over
    // strict F# parity (see project memory). ──
    // U+1100–U+11FF  (Hangul Jamo — Korean)
    || (0x1100..=0x11FF).contains(&u)
    // U+3130–U+318F  (Hangul Compatibility Jamo — Korean)
    || (0x3130..=0x318F).contains(&u)
    // U+AC00–U+D7A3  (Hangul Syllables — Korean)
    || (0xAC00..=0xD7A3).contains(&u)
    // U+0600–U+06FF  (Arabic)
    || (0x0600..=0x06FF).contains(&u)
    // U+0750–U+077F  (Arabic Supplement)
    || (0x0750..=0x077F).contains(&u)
    // U+FB50–U+FDFF  (Arabic Presentation Forms-A)
    || (0xFB50..=0xFDFF).contains(&u)
    // U+FE70–U+FEFF  (Arabic Presentation Forms-B)
    || (0xFE70..=0xFEFF).contains(&u)
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

/* ======================================================================== */
/* Tests                                                                   */
/* ======================================================================== */

#[cfg(test)]
mod tests {
    use super::*;
    use crate::commands::Lang;

    #[test]
    fn test_loc_value_char_accepts_supported_scripts() {
        // Korean (Hangul syllable + jamo), Arabic, Japanese (hiragana/katakana/
        // kanji) and Chinese (incl. Ext A) must all be accepted.
        for c in [
            '한', 'ᄀ', 'ㄱ', // Korean
            'م', 'ا', // Arabic
            'あ', 'カ', '日', // Japanese
            '中', '文', '㐀', // Chinese (㐀 = U+3400, Ext A)
        ] {
            assert!(
                is_loc_value_char(c),
                "char {c:?} (U+{:04X}) should be valid",
                c as u32
            );
        }
    }

    #[test]
    fn test_loc_value_char_finds_no_invalid_in_korean_value() {
        // A realistic Korean loc value must not report an invalid character.
        assert_eq!(
            find_invalid_loc_char("\"전쟁이 시작되었다 [USA.GetName]\""),
            None
        );
    }

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
    }

    #[test]
    fn test_loc_key11_complex() {
        let text = "l_english:\n loc_key11: \"this is valid loc\" #but this is also valid and read as part of the string due to quote after\n";
        let file = parse_loc_text(text, "test.yml").unwrap();
        assert_eq!(file.entries[0].key, "loc_key11");
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
    fn test_bom_prefixed_header_resolves_language() {
        // Loc files are UTF-8-with-BOM and the disk reader keeps the BOM in the
        // string. The header must still resolve to a language (else the loc-key
        // index silently empties → mass false CW100). See the BOM-strip in
        // parse_loc_text.
        let text = "\u{feff}l_english:\n KEY_A: \"value\"\n";
        let file = parse_loc_text(text, "abilities_l_english.yml").unwrap();
        assert_eq!(file.lang, Some(Lang::English), "BOM must not hide the lang");
        assert_eq!(file.entries.len(), 1);
        assert_eq!(file.entries[0].key, "KEY_A");
    }

    #[test]
    fn test_leading_space_header_resolves_language() {
        // `<BOM> l_english:` — a leading space before the language token. F#
        // trims the line before matching, so this must resolve to English (and
        // produce no CW256). Real MD files ship this.
        let text = "\u{feff} l_english:\n KEY_A: \"value\"\n";
        let file = parse_loc_text(text, "factions_l_english.yml").unwrap();
        assert_eq!(file.lang, Some(Lang::English));
        assert_eq!(file.language_prefix, "l_english");
        assert!(
            file.file_diagnostics.is_empty(),
            "leading-space header should not flag: {:?}",
            file.file_diagnostics
        );
    }

    #[test]
    fn test_double_bom_header_resolves_language() {
        // `<BOM><BOM>l_french:` — a doubled BOM. F# matches the language as a
        // substring and tolerates it; strip every leading BOM.
        let text = "\u{feff}\u{feff}l_french:\n KEY_A: \"value\"\n";
        let file = parse_loc_text(text, "lockeys_l_french.yml").unwrap();
        assert_eq!(file.lang, Some(Lang::French));
        assert!(
            file.file_diagnostics.is_empty(),
            "double-BOM header should not flag: {:?}",
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
