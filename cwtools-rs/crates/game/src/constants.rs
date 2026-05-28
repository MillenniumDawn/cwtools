use crate::scope::{ModifierCategory, Scope, ScopeDef};

/// Game identifier.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Game {
    Hoi4,
    Stellaris,
    Eu4,
    Ck2,
    Ck3,
    Vic2,
    Vic3,
    Ir,
    Eu5,
    Custom,
}

impl std::fmt::Display for Game {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Game::Hoi4 => write!(f, "hoi4"),
            Game::Stellaris => write!(f, "stellaris"),
            Game::Eu4 => write!(f, "eu4"),
            Game::Ck2 => write!(f, "ck2"),
            Game::Ck3 => write!(f, "ck3"),
            Game::Vic2 => write!(f, "vic2"),
            Game::Vic3 => write!(f, "vic3"),
            Game::Ir => write!(f, "ir"),
            Game::Eu5 => write!(f, "eu5"),
            Game::Custom => write!(f, "custom"),
        }
    }
}

impl Game {
    pub fn from_str(s: &str) -> Option<Self> {
        match s.to_lowercase().as_str() {
            "hoi4" => Some(Game::Hoi4),
            "stellaris" | "stl" => Some(Game::Stellaris),
            "eu4" => Some(Game::Eu4),
            "ck2" => Some(Game::Ck2),
            "ck3" => Some(Game::Ck3),
            "vic2" => Some(Game::Vic2),
            "vic3" => Some(Game::Vic3),
            "ir" | "imperator" => Some(Game::Ir),
            "eu5" => Some(Game::Eu5),
            "custom" => Some(Game::Custom),
            _ => None,
        }
    }

    /// Default script folders to scan for this game.
    pub fn script_folders(&self) -> &'static [&'static str] {
        match self {
            Game::Hoi4 => &HOI4_FOLDERS,
            Game::Stellaris => &STELLARIS_FOLDERS,
            Game::Eu4 => &EU4_FOLDERS,
            Game::Ck2 => &CK2_FOLDERS,
            Game::Ck3 => &CK3_FOLDERS,
            Game::Vic2 => &VIC2_FOLDERS,
            Game::Vic3 => &VIC3_FOLDERS,
            Game::Ir => &IR_FOLDERS,
            Game::Eu5 => &EU5_FOLDERS,
            Game::Custom => &CUSTOM_FOLDERS,
        }
    }

    /// Default scope definitions for this game.
    pub fn scope_defs(&self) -> &'static [ScopeDef] {
        match self {
            Game::Hoi4 => &HOI4_SCOPES,
            Game::Stellaris => &STELLARIS_SCOPES,
            Game::Eu4 => &EU4_SCOPES,
            Game::Ck2 => &CK2_SCOPES,
            Game::Ck3 => &CK3_SCOPES,
            Game::Vic2 => &VIC2_SCOPES,
            Game::Vic3 => &VIC3_SCOPES,
            Game::Ir => &IR_SCOPES,
            Game::Eu5 => &EU5_SCOPES,
            Game::Custom => &CUSTOM_SCOPES,
        }
    }

    /// Default modifier categories for this game.
    pub fn modifier_categories(&self) -> &'static [ModifierCategory] {
        match self {
            Game::Hoi4 => &HOI4_MODIFIERS,
            Game::Stellaris => &STELLARIS_MODIFIERS,
            Game::Eu4 => &EU4_MODIFIERS,
            Game::Ck2 => &CK2_MODIFIERS,
            Game::Ck3 => &CK3_MODIFIERS,
            Game::Vic2 => &VIC2_MODIFIERS,
            Game::Vic3 => &VIC3_MODIFIERS,
            Game::Ir => &IR_MODIFIERS,
            Game::Eu5 => &EU5_MODIFIERS,
            Game::Custom => &CUSTOM_MODIFIERS,
        }
    }
}

// ── HOI4 ─────────────────────────────────────────────

const HOI4_FOLDERS: &[&str] = &[
    "common",
    "country_metadata",
    "events",
    "gfx",
    "interface",
    "localisation",
    "history",
    "map",
    "music",
    "portraits",
    "sound",
];

const HOI4_SCOPES: &[ScopeDef] = &[
    ScopeDef { name: "Country", aliases: &["country"], id: Scope(100) },
    ScopeDef { name: "State", aliases: &["state"], id: Scope(101) },
    ScopeDef { name: "Unit Leader", aliases: &["unit leader", "unit_leader"], id: Scope(102) },
    ScopeDef { name: "Air", aliases: &["air"], id: Scope(103) },
];

const HOI4_MODIFIERS: &[ModifierCategory] = &[
    ModifierCategory { name: "State", scopes: &[Scope(100)] },
    ModifierCategory { name: "Country", scopes: &[Scope(100)] },
    ModifierCategory { name: "Unit", scopes: &[Scope(102), Scope(100)] },
    ModifierCategory { name: "UnitLeader", scopes: &[Scope(102), Scope(100)] },
    ModifierCategory { name: "Air", scopes: &[Scope(103), Scope(100)] },
];

// ── Stellaris ────────────────────────────────────────

const STELLARIS_FOLDERS: &[&str] = &[
    "common",
    "events",
    "gfx",
    "interface",
    "localisation",
    "map",
    "music",
    "sound",
];

const STELLARIS_SCOPES: &[ScopeDef] = &[
    ScopeDef { name: "Country", aliases: &["country"], id: Scope(200) },
    ScopeDef { name: "Leader", aliases: &["leader"], id: Scope(201) },
    ScopeDef { name: "System", aliases: &["galacticobject", "system", "galactic_object"], id: Scope(202) },
    ScopeDef { name: "Planet", aliases: &["planet"], id: Scope(203) },
    ScopeDef { name: "Ship", aliases: &["ship"], id: Scope(204) },
    ScopeDef { name: "Fleet", aliases: &["fleet"], id: Scope(205) },
    ScopeDef { name: "Pop", aliases: &["pop"], id: Scope(206) },
    ScopeDef { name: "Army", aliases: &["army"], id: Scope(207) },
    ScopeDef { name: "Species", aliases: &["species"], id: Scope(208) },
    ScopeDef { name: "Pop Faction", aliases: &["popfaction", "pop_faction"], id: Scope(209) },
    ScopeDef { name: "Sector", aliases: &["sector"], id: Scope(210) },
    ScopeDef { name: "Federation", aliases: &["alliance", "federation", "Alliance"], id: Scope(211) },
    ScopeDef { name: "War", aliases: &["war"], id: Scope(212) },
    ScopeDef { name: "Megastructure", aliases: &["megastructure"], id: Scope(213) },
    ScopeDef { name: "Design", aliases: &["design"], id: Scope(214) },
    ScopeDef { name: "Starbase", aliases: &["starbase"], id: Scope(215) },
    ScopeDef { name: "Star", aliases: &["star"], id: Scope(216) },
    ScopeDef { name: "Deposit", aliases: &["deposit"], id: Scope(217) },
    ScopeDef { name: "Archaeological Site", aliases: &["archaeologicalsite", "archaeological_site"], id: Scope(218) },
];

const STELLARIS_MODIFIERS: &[ModifierCategory] = &[
    ModifierCategory { name: "Pop", scopes: &[Scope(206), Scope(203), Scope(202), Scope(200)] },
    ModifierCategory { name: "Science", scopes: &[Scope(204), Scope(200)] },
    ModifierCategory { name: "Country", scopes: &[Scope(200)] },
    ModifierCategory { name: "Army", scopes: &[Scope(207), Scope(203), Scope(200)] },
    ModifierCategory { name: "Leader", scopes: &[Scope(201), Scope(200)] },
    ModifierCategory { name: "Planet", scopes: &[Scope(203), Scope(202), Scope(200)] },
    ModifierCategory { name: "PopFaction", scopes: &[Scope(209), Scope(200)] },
    ModifierCategory { name: "ShipSize", scopes: &[Scope(204), Scope(215), Scope(200)] },
    ModifierCategory { name: "Ship", scopes: &[Scope(204), Scope(215), Scope(205), Scope(200)] },
    ModifierCategory { name: "Megastructure", scopes: &[Scope(213), Scope(200)] },
    ModifierCategory { name: "PlanetClass", scopes: &[Scope(203), Scope(206), Scope(200)] },
    ModifierCategory { name: "Starbase", scopes: &[Scope(215), Scope(200)] },
    ModifierCategory { name: "Resource", scopes: &[Scope(200), Scope(202), Scope(203), Scope(206), Scope(215), Scope(201), Scope(204)] },
    ModifierCategory { name: "Federation", scopes: &[Scope(211)] },
];

// ── EU4 ──────────────────────────────────────────────
const EU4_FOLDERS: &[&str] = &["common", "events", "gfx", "interface", "localisation", "history", "map", "music", "sound"];
const EU4_SCOPES: &[ScopeDef] = &[];
const EU4_MODIFIERS: &[ModifierCategory] = &[];

// ── CK2 ──────────────────────────────────────────────
const CK2_FOLDERS: &[&str] = &["common", "events", "gfx", "interface", "localisation", "history", "map", "music", "sound"];
const CK2_SCOPES: &[ScopeDef] = &[];
const CK2_MODIFIERS: &[ModifierCategory] = &[];

// ── CK3 ──────────────────────────────────────────────
const CK3_FOLDERS: &[&str] = &["common", "events", "gfx", "interface", "localization", "history", "map", "music", "sound"];
const CK3_SCOPES: &[ScopeDef] = &[];
const CK3_MODIFIERS: &[ModifierCategory] = &[];

// ── VIC2 ──────────────────────────────────────────────
const VIC2_FOLDERS: &[&str] = &["common", "events", "gfx", "interface", "localisation", "history", "map", "music", "sound"];
const VIC2_SCOPES: &[ScopeDef] = &[];
const VIC2_MODIFIERS: &[ModifierCategory] = &[];

// ── VIC3 ──────────────────────────────────────────────
const VIC3_FOLDERS: &[&str] = &["common", "events", "gfx", "interface", "localization", "history", "map", "music", "sound"];
const VIC3_SCOPES: &[ScopeDef] = &[];
const VIC3_MODIFIERS: &[ModifierCategory] = &[];

// ── IR ────────────────────────────────────────────────
const IR_FOLDERS: &[&str] = &["common", "events", "gfx", "interface", "localization", "history", "map", "music", "sound"];
const IR_SCOPES: &[ScopeDef] = &[];
const IR_MODIFIERS: &[ModifierCategory] = &[];

// ── EU5 ────────────────────────────────────────────────
const EU5_FOLDERS: &[&str] = &["common", "events", "gfx", "interface", "localization", "history", "map", "music", "sound"];
const EU5_SCOPES: &[ScopeDef] = &[];
const EU5_MODIFIERS: &[ModifierCategory] = &[];

// ── Custom ───────────────────────────────────────────
const CUSTOM_FOLDERS: &[&str] = &["common", "events", "gfx", "interface", "localisation", "map", "music", "sound"];
const CUSTOM_SCOPES: &[ScopeDef] = &[];
const CUSTOM_MODIFIERS: &[ModifierCategory] = &[];
