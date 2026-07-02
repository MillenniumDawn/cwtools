use std::fmt;
use std::sync::Arc;

/// Supported languages across all games.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Lang {
    English,
    French,
    German,
    Spanish,
    Russian,
    Polish,
    BrazPor,
    SimpChinese,
    Japanese,
    Korean,
    Turkish,
    /// `l_default` — used by Stellaris / Custom as a wildcard language.
    Default,
}

impl fmt::Display for Lang {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Lang::English => write!(f, "english"),
            Lang::French => write!(f, "french"),
            Lang::German => write!(f, "german"),
            Lang::Spanish => write!(f, "spanish"),
            Lang::Russian => write!(f, "russian"),
            Lang::Polish => write!(f, "polish"),
            Lang::BrazPor => write!(f, "braz_por"),
            Lang::SimpChinese => write!(f, "simp_chinese"),
            Lang::Japanese => write!(f, "japanese"),
            Lang::Korean => write!(f, "korean"),
            Lang::Turkish => write!(f, "turkish"),
            Lang::Default => write!(f, "default"),
        }
    }
}

impl Lang {
    /// Parse a plain language name (`english`, `simp_chinese`, …) into a `Lang`.
    /// Tolerant of case and of an optional `l_` prefix, so both the loc-file
    /// header form (`l_english`) and a bare setting value (`English`) resolve.
    /// Also accepts the editor-setting spellings (`Chinese`, `Braz_Por`).
    pub fn from_name(name: &str) -> Option<Lang> {
        let lower = name.trim().to_ascii_lowercase();
        // Editor-facing aliases that differ from the `l_xxx` loc-file keys.
        match lower.as_str() {
            "chinese" | "simp_chinese" => return Some(Lang::SimpChinese),
            "braz_por" | "brazilian" | "brazilian_portuguese" => return Some(Lang::BrazPor),
            _ => {}
        }
        let key = if lower.starts_with("l_") {
            lower
        } else {
            format!("l_{lower}")
        };
        key_to_language(&key)
    }
}

/// Parse an `l_xxx` prefix into a Lang variant.
///
/// Accepts all known language keys including `l_default`.
pub(crate) fn key_to_language(prefix: &str) -> Option<Lang> {
    match prefix {
        "l_english" => Some(Lang::English),
        "l_french" => Some(Lang::French),
        "l_german" => Some(Lang::German),
        "l_spanish" => Some(Lang::Spanish),
        "l_russian" => Some(Lang::Russian),
        "l_polish" => Some(Lang::Polish),
        "l_braz_por" => Some(Lang::BrazPor),
        "l_simp_chinese" => Some(Lang::SimpChinese),
        "l_japanese" => Some(Lang::Japanese),
        "l_korean" => Some(Lang::Korean),
        "l_turkish" => Some(Lang::Turkish),
        "l_default" => Some(Lang::Default),
        _ => None,
    }
}

/// Returns the set of valid `l_xxx` language tokens for the given game.
///
/// Mirrors the per-game `keyToLanguage` functions in `YAMLLocalisationParser.fs`
/// (lines 222–375) plus the `l_default` wildcard for Stellaris. Games with no
/// distinct loc set (`None`, CK2, VIC2, Custom) accept the full known set.
pub fn languages_for_game(game: Option<cwtools_game::constants::Game>) -> &'static [Lang] {
    use cwtools_game::constants::Game as G;
    match game {
        Some(G::Stellaris) => &[
            Lang::English,
            Lang::French,
            Lang::German,
            Lang::Spanish,
            Lang::Russian,
            Lang::Polish,
            Lang::BrazPor,
            Lang::SimpChinese,
            Lang::Japanese,
            Lang::Korean,
            Lang::Default,
        ],
        Some(G::Hoi4) => &[
            Lang::English,
            Lang::French,
            Lang::German,
            Lang::Spanish,
            Lang::Russian,
            Lang::Polish,
            Lang::BrazPor,
            Lang::SimpChinese,
            Lang::Japanese,
        ],
        Some(G::Eu4) => &[Lang::English, Lang::French, Lang::German, Lang::Spanish],
        Some(G::Ck3) => &[
            Lang::English,
            Lang::French,
            Lang::German,
            Lang::Spanish,
            Lang::SimpChinese,
            Lang::Russian,
            Lang::Korean,
        ],
        Some(G::Vic3) | Some(G::Eu5) => &[
            Lang::English,
            Lang::French,
            Lang::German,
            Lang::Spanish,
            Lang::SimpChinese,
            Lang::Russian,
            Lang::Korean,
            Lang::Japanese,
            Lang::BrazPor,
            Lang::Polish,
            Lang::Turkish,
        ],
        Some(G::Ir) => &[
            Lang::English,
            Lang::French,
            Lang::German,
            Lang::Spanish,
            Lang::SimpChinese,
            Lang::Russian,
        ],
        // None, CK2, VIC2, Custom: accept all known languages.
        _ => &[
            Lang::English,
            Lang::French,
            Lang::German,
            Lang::Spanish,
            Lang::Russian,
            Lang::Polish,
            Lang::BrazPor,
            Lang::SimpChinese,
            Lang::Japanese,
            Lang::Korean,
            Lang::Turkish,
            Lang::Default,
        ],
    }
}

/// Parse an `l_xxx` prefix into a `Lang` for the given game.
///
/// Returns `None` if the key is not valid for that game.
/// Keeps `key_to_language` for generic / backwards-compatible use.
pub fn key_to_language_for_game(
    game: Option<cwtools_game::constants::Game>,
    prefix: &str,
) -> Option<Lang> {
    let lang = key_to_language(prefix)?;
    if languages_for_game(game).contains(&lang) {
        Some(lang)
    } else {
        None
    }
}

/// A localized entry.
#[derive(Debug, Clone, PartialEq)]
pub struct LocEntry {
    pub key: String,
    pub value: Option<u32>,
    pub desc: String,
    pub position: Position,
    pub error_range: Option<Position>,
    // Parsed elements (lazy, computed on demand)
    pub refs: Vec<String>,
    pub commands: Vec<String>,
    /// Each inner Vec is one `[...]` bracket's command chain.
    /// `[overlord.owner.GetName]` → `vec![vec!["overlord", "owner", "GetName"]]`.
    /// Multiple brackets produce multiple inner Vecs.
    pub jomini_commands: Vec<Vec<crate::loc_string::JominiCommand>>,
}

/// Position in a source file.
#[derive(Debug, Clone, PartialEq)]
pub struct Position {
    /// Shared file path. One `Arc` allocation per file; every entry in the
    /// file holds a cheap clone of the same pointer.
    pub stream_name: Arc<str>,
    pub line: usize,
    pub column: usize,
}

impl Position {
    pub fn new(stream_name: Arc<str>, line: usize, column: usize) -> Self {
        Self {
            stream_name,
            line,
            column,
        }
    }
}

/// A line-level parse failure recorded during lenient recovery.
///
/// The parser skips malformed lines rather than aborting; each skip
/// produces one of these so the pipeline can emit CW001.
#[derive(Debug, Clone, PartialEq)]
pub struct LocParseError {
    /// 1-based line number where the malformed line was found.
    pub line: usize,
    /// Human-readable description of the problem.
    pub message: String,
}

/// A parsed localization file.
#[derive(Debug, Clone, PartialEq)]
pub struct LocFile {
    /// Source path (or logical name) this file was parsed from.
    pub path: String,
    pub language_prefix: String,
    pub lang: Option<Lang>,
    pub entries: Vec<LocEntry>,
    /// File-level diagnostics (BOM, header/filename mismatches, etc.).
    /// Empty when there are no issues.
    pub file_diagnostics: Vec<String>,
    /// Line-level parse errors collected during lenient recovery (CW001).
    /// Empty for well-formed files.
    pub parse_errors: Vec<LocParseError>,
    /// On-disk encoding, when the file was read from disk (used to enforce the
    /// UTF-8-BOM rule, CW254). `None` when built from already-decoded text
    /// (LSP single-file edits, tests) where the original bytes aren't available.
    pub encoding: Option<cwtools_file_manager::FileEncoding>,
}

/* ======================================================================== */
/* Tests                                                                   */
/* ======================================================================== */

#[cfg(test)]
mod tests {
    use super::*;
    use cwtools_game::constants::Game;

    #[test]
    fn test_key_to_language_default() {
        assert_eq!(key_to_language("l_default"), Some(Lang::Default));
    }

    #[test]
    fn test_key_to_language_all_known() {
        assert_eq!(key_to_language("l_english"), Some(Lang::English));
        assert_eq!(key_to_language("l_turkish"), Some(Lang::Turkish));
        assert_eq!(key_to_language("l_unknown"), None);
    }

    #[test]
    fn test_lang_from_name_tolerant() {
        // bare name, case-insensitive, and the l_ prefix all resolve
        assert_eq!(Lang::from_name("english"), Some(Lang::English));
        assert_eq!(Lang::from_name("English"), Some(Lang::English));
        assert_eq!(Lang::from_name("  German "), Some(Lang::German));
        assert_eq!(Lang::from_name("simp_chinese"), Some(Lang::SimpChinese));
        assert_eq!(Lang::from_name("l_french"), Some(Lang::French));
        // editor-setting spellings
        assert_eq!(Lang::from_name("Chinese"), Some(Lang::SimpChinese));
        assert_eq!(Lang::from_name("Braz_Por"), Some(Lang::BrazPor));
        assert_eq!(Lang::from_name("klingon"), None);
    }

    #[test]
    fn test_key_to_language_for_game_stellaris() {
        // Stellaris supports l_korean and l_default
        assert_eq!(
            key_to_language_for_game(Some(Game::Stellaris), "l_korean"),
            Some(Lang::Korean)
        );
        assert_eq!(
            key_to_language_for_game(Some(Game::Stellaris), "l_default"),
            Some(Lang::Default)
        );
        // Turkish is NOT in Stellaris set
        assert_eq!(
            key_to_language_for_game(Some(Game::Stellaris), "l_turkish"),
            None
        );
    }

    #[test]
    fn test_key_to_language_for_game_eu4() {
        // EU4 only has English, French, German, Spanish
        assert_eq!(
            key_to_language_for_game(Some(Game::Eu4), "l_english"),
            Some(Lang::English)
        );
        assert_eq!(key_to_language_for_game(Some(Game::Eu4), "l_russian"), None);
        assert_eq!(key_to_language_for_game(Some(Game::Eu4), "l_default"), None);
    }

    #[test]
    fn test_key_to_language_for_game_hoi4() {
        assert_eq!(
            key_to_language_for_game(Some(Game::Hoi4), "l_japanese"),
            Some(Lang::Japanese)
        );
        // HOI4 does not have Korean
        assert_eq!(key_to_language_for_game(Some(Game::Hoi4), "l_korean"), None);
    }

    #[test]
    fn test_key_to_language_for_game_custom_has_default() {
        // Custom has no distinct loc set, so it accepts the full known list.
        assert_eq!(
            key_to_language_for_game(Some(Game::Custom), "l_default"),
            Some(Lang::Default)
        );
    }

    #[test]
    fn test_generic_accepts_all() {
        assert_eq!(
            key_to_language_for_game(None, "l_turkish"),
            Some(Lang::Turkish)
        );
        assert_eq!(
            key_to_language_for_game(None, "l_default"),
            Some(Lang::Default)
        );
    }

    #[test]
    fn test_lang_display() {
        assert_eq!(format!("{}", Lang::Default), "default");
        assert_eq!(format!("{}", Lang::BrazPor), "braz_por");
    }
}
