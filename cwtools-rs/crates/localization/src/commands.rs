use std::collections::HashMap;
use std::fmt;

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

/// Parse an `l_xxx` prefix into a Lang variant.
///
/// Accepts all known language keys including `l_default`.
pub fn key_to_language(prefix: &str) -> Option<Lang> {
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

/// Game identifier for per-game language restriction.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Game {
    Stellaris,
    HOI4,
    EU4,
    CK3,
    VIC3,
    EU5,
    IR,
    Custom,
    /// Catch-all: accept all known languages.
    Generic,
}

/// Returns the set of valid `l_xxx` language tokens for the given game.
///
/// Mirrors the per-game `keyToLanguage` functions in `YAMLLocalisationParser.fs`
/// (lines 222–375) plus the `l_default` wildcard for Stellaris/Custom.
pub fn languages_for_game(game: Game) -> &'static [Lang] {
    match game {
        Game::Stellaris => &[
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
        Game::HOI4 => &[
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
        Game::EU4 => &[Lang::English, Lang::French, Lang::German, Lang::Spanish],
        Game::CK3 => &[
            Lang::English,
            Lang::French,
            Lang::German,
            Lang::Spanish,
            Lang::SimpChinese,
            Lang::Russian,
            Lang::Korean,
        ],
        Game::VIC3 | Game::EU5 => &[
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
        Game::IR => &[
            Lang::English,
            Lang::French,
            Lang::German,
            Lang::Spanish,
            Lang::SimpChinese,
            Lang::Russian,
        ],
        Game::Custom => &[
            Lang::English,
            Lang::French,
            Lang::German,
            Lang::Spanish,
            Lang::SimpChinese,
            Lang::Russian,
            Lang::Polish,
            Lang::BrazPor,
            Lang::Default,
        ],
        Game::Generic => &[
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
pub fn key_to_language_for_game(game: Game, prefix: &str) -> Option<Lang> {
    let lang = key_to_language(prefix)?;
    if languages_for_game(game).contains(&lang) {
        Some(lang)
    } else {
        None
    }
}

/// A parsed localization file.

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
    pub jomini_commands: Vec<JominiCommand>,
}

/// Position in a source file.
#[derive(Debug, Clone, PartialEq)]
pub struct Position {
    pub stream_name: String,
    pub line: usize,
    pub column: usize,
}

impl Position {
    pub fn new(stream_name: impl Into<String>, line: usize, column: usize) -> Self {
        Self {
            stream_name: stream_name.into(),
            line,
            column,
        }
    }
}

/// A Jomini command chain (CK3/VIC3).
#[derive(Debug, Clone, PartialEq)]
pub struct JominiCommand {
    pub key: String,
    pub params: Vec<JominiParam>,
}

/// A Jomini parameter.
#[derive(Debug, Clone, PartialEq)]
pub enum JominiParam {
    Literal(String),
    Commands(Vec<String>),
}

/// A parsed localization file.
#[derive(Debug, Clone, PartialEq)]
pub struct LocFile {
    pub language_prefix: String,
    pub lang: Option<Lang>,
    pub entries: Vec<LocEntry>,
    /// File-level diagnostics (BOM, header/filename mismatches, etc.).
    /// Empty when there are no issues.
    pub file_diagnostics: Vec<String>,
}

/// Per-language API over loaded loc files.
pub struct LocApi {
    entries: HashMap<String, LocEntry>,
    pub keys: Vec<String>,
}

impl LocApi {
    pub fn new(entries: HashMap<String, LocEntry>) -> Self {
        let keys = entries.keys().cloned().collect::<Vec<_>>();
        Self { entries, keys }
    }

    pub fn get_desc(&self, key: &str) -> String {
        self.entries
            .get(key)
            .map(|e| e.desc.clone())
            .unwrap_or_else(|| key.to_string())
    }

    pub fn get_entry(&self, key: &str) -> Option<&LocEntry> {
        self.entries.get(key)
    }

    pub fn contains(&self, key: &str) -> bool {
        self.entries.contains_key(key)
    }
}

/* ======================================================================== */
/* Tests                                                                   */
/* ======================================================================== */

#[cfg(test)]
mod tests {
    use super::*;

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
    fn test_key_to_language_for_game_stellaris() {
        // Stellaris supports l_korean and l_default
        assert_eq!(
            key_to_language_for_game(Game::Stellaris, "l_korean"),
            Some(Lang::Korean)
        );
        assert_eq!(
            key_to_language_for_game(Game::Stellaris, "l_default"),
            Some(Lang::Default)
        );
        // Turkish is NOT in Stellaris set
        assert_eq!(key_to_language_for_game(Game::Stellaris, "l_turkish"), None);
    }

    #[test]
    fn test_key_to_language_for_game_eu4() {
        // EU4 only has English, French, German, Spanish
        assert_eq!(
            key_to_language_for_game(Game::EU4, "l_english"),
            Some(Lang::English)
        );
        assert_eq!(key_to_language_for_game(Game::EU4, "l_russian"), None);
        assert_eq!(key_to_language_for_game(Game::EU4, "l_default"), None);
    }

    #[test]
    fn test_key_to_language_for_game_hoi4() {
        assert_eq!(
            key_to_language_for_game(Game::HOI4, "l_japanese"),
            Some(Lang::Japanese)
        );
        // HOI4 does not have Korean
        assert_eq!(key_to_language_for_game(Game::HOI4, "l_korean"), None);
    }

    #[test]
    fn test_key_to_language_for_game_custom_has_default() {
        assert_eq!(
            key_to_language_for_game(Game::Custom, "l_default"),
            Some(Lang::Default)
        );
    }

    #[test]
    fn test_generic_accepts_all() {
        assert_eq!(
            key_to_language_for_game(Game::Generic, "l_turkish"),
            Some(Lang::Turkish)
        );
        assert_eq!(
            key_to_language_for_game(Game::Generic, "l_default"),
            Some(Lang::Default)
        );
    }

    #[test]
    fn test_lang_display() {
        assert_eq!(format!("{}", Lang::Default), "default");
        assert_eq!(format!("{}", Lang::BrazPor), "braz_por");
    }
}
