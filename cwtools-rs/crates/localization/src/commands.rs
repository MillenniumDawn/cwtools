use std::collections::{HashMap, HashSet};
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
        }
    }
}

/// Parse an `l_xxx` prefix into a Lang variant.
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
        _ => None,
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
