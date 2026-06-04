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

    /// Scope definitions for this game (name, aliases, numeric id, subscope_of).
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

    /// Modifier categories for this game.
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

// ── HOI4 ─────────────────────────────────────────────────────────────────────
// IDs: Country=100, State=101, Unit Leader=102, Air=103

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
    ScopeDef { name: "Country",     aliases: &["country"],                           id: Scope(100), subscope_of: &[] },
    ScopeDef { name: "State",       aliases: &["state"],                             id: Scope(101), subscope_of: &[] },
    ScopeDef { name: "Unit Leader", aliases: &["unit leader", "unit_leader"],        id: Scope(102), subscope_of: &[] },
    ScopeDef { name: "Air",         aliases: &["air"],                               id: Scope(103), subscope_of: &[] },
];

const HOI4_MODIFIERS: &[ModifierCategory] = &[
    ModifierCategory { name: "State",      scopes: &[Scope(101)] },
    ModifierCategory { name: "Country",    scopes: &[Scope(100)] },
    ModifierCategory { name: "Unit",       scopes: &[Scope(102), Scope(100)] },
    ModifierCategory { name: "UnitLeader", scopes: &[Scope(102), Scope(100)] },
    ModifierCategory { name: "Air",        scopes: &[Scope(103), Scope(100)] },
];

// ── Stellaris ─────────────────────────────────────────────────────────────────
// IDs: Country=200, Leader=201, System=202, Planet=203, Ship=204, Fleet=205,
//      Pop=206, Army=207, Species=208, Pop Faction=209, Sector=210,
//      Federation=211, War=212, Megastructure=213, Design=214, Starbase=215,
//      Star=216, Deposit=217, Archaeological Site=218, Ambient Object=219

const STELLARIS_FOLDERS: &[&str] = &[
    "common", "events", "gfx", "interface", "localisation", "map", "music", "sound",
];

const STELLARIS_SCOPES: &[ScopeDef] = &[
    ScopeDef { name: "Country",             aliases: &["country"],                                     id: Scope(200), subscope_of: &[] },
    ScopeDef { name: "Leader",              aliases: &["leader"],                                      id: Scope(201), subscope_of: &[] },
    ScopeDef { name: "System",              aliases: &["galacticobject", "system", "galactic_object"], id: Scope(202), subscope_of: &[] },
    ScopeDef { name: "Planet",              aliases: &["planet"],                                      id: Scope(203), subscope_of: &[] },
    ScopeDef { name: "Ship",               aliases: &["ship"],                                        id: Scope(204), subscope_of: &[] },
    ScopeDef { name: "Fleet",              aliases: &["fleet"],                                       id: Scope(205), subscope_of: &[] },
    ScopeDef { name: "Pop",                aliases: &["pop"],                                         id: Scope(206), subscope_of: &[] },
    ScopeDef { name: "Army",               aliases: &["army"],                                        id: Scope(207), subscope_of: &[] },
    ScopeDef { name: "Species",            aliases: &["species"],                                     id: Scope(208), subscope_of: &[] },
    ScopeDef { name: "Pop Faction",        aliases: &["popfaction", "pop_faction"],                   id: Scope(209), subscope_of: &[] },
    ScopeDef { name: "Sector",             aliases: &["sector"],                                      id: Scope(210), subscope_of: &[] },
    ScopeDef { name: "Federation",         aliases: &["alliance", "federation", "Alliance"],          id: Scope(211), subscope_of: &[] },
    ScopeDef { name: "War",                aliases: &["war"],                                         id: Scope(212), subscope_of: &[] },
    ScopeDef { name: "Megastructure",      aliases: &["megastructure"],                               id: Scope(213), subscope_of: &[] },
    ScopeDef { name: "Design",             aliases: &["design"],                                      id: Scope(214), subscope_of: &[] },
    ScopeDef { name: "Starbase",           aliases: &["starbase"],                                    id: Scope(215), subscope_of: &[] },
    ScopeDef { name: "Star",               aliases: &["star"],                                        id: Scope(216), subscope_of: &[] },
    ScopeDef { name: "Deposit",            aliases: &["deposit"],                                     id: Scope(217), subscope_of: &[] },
    ScopeDef { name: "Archaeological Site",aliases: &["archaeologicalsite", "archaeological_site"],   id: Scope(218), subscope_of: &[] },
    ScopeDef { name: "Ambient Object",     aliases: &["ambientobject", "ambient_object"],             id: Scope(219), subscope_of: &[] },
];

const STELLARIS_MODIFIERS: &[ModifierCategory] = &[
    ModifierCategory { name: "Pop",          scopes: &[Scope(206), Scope(203), Scope(202), Scope(200)] },
    ModifierCategory { name: "Science",      scopes: &[Scope(204), Scope(200)] },
    ModifierCategory { name: "Country",      scopes: &[Scope(200)] },
    ModifierCategory { name: "Army",         scopes: &[Scope(207), Scope(203), Scope(200)] },
    ModifierCategory { name: "Leader",       scopes: &[Scope(201), Scope(200)] },
    ModifierCategory { name: "Planet",       scopes: &[Scope(203), Scope(202), Scope(200)] },
    ModifierCategory { name: "PopFaction",   scopes: &[Scope(209), Scope(200)] },
    ModifierCategory { name: "ShipSize",     scopes: &[Scope(204), Scope(215), Scope(200)] },
    ModifierCategory { name: "Ship",         scopes: &[Scope(204), Scope(215), Scope(205), Scope(200)] },
    ModifierCategory { name: "Megastructure",scopes: &[Scope(213), Scope(200)] },
    ModifierCategory { name: "PlanetClass",  scopes: &[Scope(203), Scope(206), Scope(200)] },
    ModifierCategory { name: "Starbase",     scopes: &[Scope(215), Scope(200)] },
    ModifierCategory { name: "Resource",     scopes: &[Scope(200), Scope(202), Scope(203), Scope(206), Scope(215), Scope(201), Scope(204)] },
    ModifierCategory { name: "Federation",   scopes: &[Scope(211)] },
];

// ── EU4 ───────────────────────────────────────────────────────────────────────
// IDs: Country=300, Province=301, Trade Node=302 (subscope of Province),
//      Unit=303, Monarch=304, Heir=305, Consort=306, Rebel Faction=307,
//      Religion=308, Culture=309, Advisor=310
// F# source: CWTools/Common/EU4Constants.fs defaultScopes

const EU4_FOLDERS: &[&str] = &[
    "common", "events", "gfx", "interface", "localisation", "history", "map", "music", "sound",
];

const EU4_SCOPES: &[ScopeDef] = &[
    ScopeDef { name: "Country",       aliases: &["country"],                  id: Scope(300), subscope_of: &[] },
    ScopeDef { name: "Province",      aliases: &["province"],                 id: Scope(301), subscope_of: &[] },
    // Trade Node isSubscopeOf Province (F# EU4Constants.fs line 9: [ "province" ])
    ScopeDef { name: "Trade Node",    aliases: &["trade_node", "tradenode"],  id: Scope(302), subscope_of: &[Scope(301)] },
    ScopeDef { name: "Unit",          aliases: &["unit"],                     id: Scope(303), subscope_of: &[] },
    ScopeDef { name: "Monarch",       aliases: &["monarch"],                  id: Scope(304), subscope_of: &[] },
    ScopeDef { name: "Heir",          aliases: &["heir"],                     id: Scope(305), subscope_of: &[] },
    ScopeDef { name: "Consort",       aliases: &["consort"],                  id: Scope(306), subscope_of: &[] },
    ScopeDef { name: "Rebel Faction", aliases: &["rebel_faction"],            id: Scope(307), subscope_of: &[] },
    ScopeDef { name: "Religion",      aliases: &["religion"],                 id: Scope(308), subscope_of: &[] },
    ScopeDef { name: "Culture",       aliases: &["culture"],                  id: Scope(309), subscope_of: &[] },
    ScopeDef { name: "Advisor",       aliases: &["advisor"],                  id: Scope(310), subscope_of: &[] },
];

const EU4_MODIFIERS: &[ModifierCategory] = &[
    ModifierCategory { name: "Country",  scopes: &[Scope(300)] },
    ModifierCategory { name: "Province", scopes: &[Scope(301)] },
];

// ── CK2 ───────────────────────────────────────────────────────────────────────
// IDs: Character=400, Title=401, Province=402, Offmap=403, War=404,
//      Siege=405, Unit=406, Religion=407, Culture=408, Society=409,
//      Artifact=410, Bloodline=411, Wonder=412
// F# source: CWTools/Common/CK2Constants.fs defaultScopes

const CK2_FOLDERS: &[&str] = &[
    "common", "events", "gfx", "interface", "localisation", "history", "map", "music", "sound",
];

const CK2_SCOPES: &[ScopeDef] = &[
    ScopeDef { name: "Character",    aliases: &["character"],   id: Scope(400), subscope_of: &[] },
    ScopeDef { name: "Title",        aliases: &["title"],       id: Scope(401), subscope_of: &[] },
    ScopeDef { name: "Province",     aliases: &["province"],    id: Scope(402), subscope_of: &[] },
    ScopeDef { name: "Offmap",       aliases: &["offmap"],      id: Scope(403), subscope_of: &[] },
    ScopeDef { name: "War",          aliases: &["war"],         id: Scope(404), subscope_of: &[] },
    ScopeDef { name: "Siege",        aliases: &["siege"],       id: Scope(405), subscope_of: &[] },
    ScopeDef { name: "Unit",         aliases: &["unit"],        id: Scope(406), subscope_of: &[] },
    ScopeDef { name: "Religion",     aliases: &["religion"],    id: Scope(407), subscope_of: &[] },
    ScopeDef { name: "Culture",      aliases: &["culture"],     id: Scope(408), subscope_of: &[] },
    ScopeDef { name: "Society",      aliases: &["society"],     id: Scope(409), subscope_of: &[] },
    ScopeDef { name: "Artifact",     aliases: &["artifact"],    id: Scope(410), subscope_of: &[] },
    ScopeDef { name: "Bloodline",    aliases: &["bloodline"],   id: Scope(411), subscope_of: &[] },
    ScopeDef { name: "Wonder",       aliases: &["wonder"],      id: Scope(412), subscope_of: &[] },
];

const CK2_MODIFIERS: &[ModifierCategory] = &[
    ModifierCategory { name: "Character", scopes: &[Scope(400)] },
    ModifierCategory { name: "Province",  scopes: &[Scope(402)] },
    ModifierCategory { name: "Unit",      scopes: &[Scope(406), Scope(402)] },
];

// ── CK3 ───────────────────────────────────────────────────────────────────────
// IDs: Value=500, Bool=501, Flag=502, Color=503, Country=504, Character=505,
//      Province=506, Combat=507, Unit=508, Pop=509, Family=510, Party=511,
//      Religion=512, Culture=513, Job=514, CultureGroup=515, Area=516,
//      State=517, Subunit=518, Governorship=519, Region=520
// F# source: CWTools/Common/CK3Constants.fs defaultScopes

const CK3_FOLDERS: &[&str] = &[
    "common", "events", "gfx", "gui", "localization", "history", "map_data",
];

const CK3_SCOPES: &[ScopeDef] = &[
    ScopeDef { name: "Value",        aliases: &["value"],        id: Scope(500), subscope_of: &[] },
    ScopeDef { name: "Bool",         aliases: &["bool"],         id: Scope(501), subscope_of: &[] },
    ScopeDef { name: "Flag",         aliases: &["flag"],         id: Scope(502), subscope_of: &[] },
    ScopeDef { name: "Color",        aliases: &["color"],        id: Scope(503), subscope_of: &[] },
    ScopeDef { name: "Country",      aliases: &["country"],      id: Scope(504), subscope_of: &[] },
    ScopeDef { name: "Character",    aliases: &["character"],    id: Scope(505), subscope_of: &[] },
    ScopeDef { name: "Province",     aliases: &["province"],     id: Scope(506), subscope_of: &[] },
    ScopeDef { name: "Combat",       aliases: &["combat"],       id: Scope(507), subscope_of: &[] },
    ScopeDef { name: "Unit",         aliases: &["unit"],         id: Scope(508), subscope_of: &[] },
    ScopeDef { name: "Pop",          aliases: &["pop"],          id: Scope(509), subscope_of: &[] },
    ScopeDef { name: "Family",       aliases: &["family"],       id: Scope(510), subscope_of: &[] },
    ScopeDef { name: "Party",        aliases: &["party"],        id: Scope(511), subscope_of: &[] },
    ScopeDef { name: "Religion",     aliases: &["religion"],     id: Scope(512), subscope_of: &[] },
    ScopeDef { name: "Culture",      aliases: &["culture"],      id: Scope(513), subscope_of: &[] },
    ScopeDef { name: "Job",          aliases: &["job"],          id: Scope(514), subscope_of: &[] },
    ScopeDef { name: "CultureGroup", aliases: &["culture group"],id: Scope(515), subscope_of: &[] },
    ScopeDef { name: "Area",         aliases: &["area"],         id: Scope(516), subscope_of: &[] },
    ScopeDef { name: "State",        aliases: &["state"],        id: Scope(517), subscope_of: &[] },
    ScopeDef { name: "Subunit",      aliases: &["subunit"],      id: Scope(518), subscope_of: &[] },
    ScopeDef { name: "Governorship", aliases: &["governorship"], id: Scope(519), subscope_of: &[] },
    ScopeDef { name: "Region",       aliases: &["region"],       id: Scope(520), subscope_of: &[] },
];

const CK3_MODIFIERS: &[ModifierCategory] = &[
    ModifierCategory { name: "Province",  scopes: &[Scope(506), Scope(504)] },
    ModifierCategory { name: "Country",   scopes: &[Scope(504)] },
    ModifierCategory { name: "Unit",      scopes: &[Scope(508), Scope(504)] },
    ModifierCategory { name: "Character", scopes: &[Scope(505), Scope(504)] },
];

// ── VIC2 ──────────────────────────────────────────────────────────────────────
// F# VIC2Constants.fs has identical scope list to CK3 / IR.
// Using IDs 600-620 to avoid collision.

const VIC2_FOLDERS: &[&str] = &[
    "common", "events", "localisation", "map_data",
];

const VIC2_SCOPES: &[ScopeDef] = &[
    ScopeDef { name: "Value",        aliases: &["value"],        id: Scope(600), subscope_of: &[] },
    ScopeDef { name: "Bool",         aliases: &["bool"],         id: Scope(601), subscope_of: &[] },
    ScopeDef { name: "Flag",         aliases: &["flag"],         id: Scope(602), subscope_of: &[] },
    ScopeDef { name: "Color",        aliases: &["color"],        id: Scope(603), subscope_of: &[] },
    ScopeDef { name: "Country",      aliases: &["country"],      id: Scope(604), subscope_of: &[] },
    ScopeDef { name: "Character",    aliases: &["character"],    id: Scope(605), subscope_of: &[] },
    ScopeDef { name: "Province",     aliases: &["province"],     id: Scope(606), subscope_of: &[] },
    ScopeDef { name: "Combat",       aliases: &["combat"],       id: Scope(607), subscope_of: &[] },
    ScopeDef { name: "Unit",         aliases: &["unit"],         id: Scope(608), subscope_of: &[] },
    ScopeDef { name: "Pop",          aliases: &["pop"],          id: Scope(609), subscope_of: &[] },
    ScopeDef { name: "Family",       aliases: &["family"],       id: Scope(610), subscope_of: &[] },
    ScopeDef { name: "Party",        aliases: &["party"],        id: Scope(611), subscope_of: &[] },
    ScopeDef { name: "Religion",     aliases: &["religion"],     id: Scope(612), subscope_of: &[] },
    ScopeDef { name: "Culture",      aliases: &["culture"],      id: Scope(613), subscope_of: &[] },
    ScopeDef { name: "Job",          aliases: &["job"],          id: Scope(614), subscope_of: &[] },
    ScopeDef { name: "CultureGroup", aliases: &["culture group"],id: Scope(615), subscope_of: &[] },
    ScopeDef { name: "Area",         aliases: &["area"],         id: Scope(616), subscope_of: &[] },
    ScopeDef { name: "State",        aliases: &["state"],        id: Scope(617), subscope_of: &[] },
    ScopeDef { name: "Subunit",      aliases: &["subunit"],      id: Scope(618), subscope_of: &[] },
    ScopeDef { name: "Governorship", aliases: &["governorship"], id: Scope(619), subscope_of: &[] },
    ScopeDef { name: "Region",       aliases: &["region"],       id: Scope(620), subscope_of: &[] },
];

const VIC2_MODIFIERS: &[ModifierCategory] = &[
    ModifierCategory { name: "Province",  scopes: &[Scope(606), Scope(604)] },
    ModifierCategory { name: "Country",   scopes: &[Scope(604)] },
    ModifierCategory { name: "Unit",      scopes: &[Scope(608), Scope(604)] },
    ModifierCategory { name: "Character", scopes: &[Scope(605), Scope(604)] },
];

// ── VIC3 ──────────────────────────────────────────────────────────────────────
// F# VIC3Constants.fs is currently minimal (commented-out or stub).
// Keeping as-is pending upstream F# work.

const VIC3_FOLDERS: &[&str] = &[
    "common", "events", "gfx", "interface", "localization", "history", "map", "music", "sound",
];
const VIC3_SCOPES: &[ScopeDef] = &[];
const VIC3_MODIFIERS: &[ModifierCategory] = &[];

// ── IR (Imperator: Rome) ──────────────────────────────────────────────────────
// F# IRConstants.fs — same scope list as CK3 / VIC2.
// Using IDs 700-720.

const IR_FOLDERS: &[&str] = &[
    "common", "events", "localization", "map_data",
];

const IR_SCOPES: &[ScopeDef] = &[
    ScopeDef { name: "Value",        aliases: &["value"],        id: Scope(700), subscope_of: &[] },
    ScopeDef { name: "Bool",         aliases: &["bool"],         id: Scope(701), subscope_of: &[] },
    ScopeDef { name: "Flag",         aliases: &["flag"],         id: Scope(702), subscope_of: &[] },
    ScopeDef { name: "Color",        aliases: &["color"],        id: Scope(703), subscope_of: &[] },
    ScopeDef { name: "Country",      aliases: &["country"],      id: Scope(704), subscope_of: &[] },
    ScopeDef { name: "Character",    aliases: &["character"],    id: Scope(705), subscope_of: &[] },
    ScopeDef { name: "Province",     aliases: &["province"],     id: Scope(706), subscope_of: &[] },
    ScopeDef { name: "Combat",       aliases: &["combat"],       id: Scope(707), subscope_of: &[] },
    ScopeDef { name: "Unit",         aliases: &["unit"],         id: Scope(708), subscope_of: &[] },
    ScopeDef { name: "Pop",          aliases: &["pop"],          id: Scope(709), subscope_of: &[] },
    ScopeDef { name: "Family",       aliases: &["family"],       id: Scope(710), subscope_of: &[] },
    ScopeDef { name: "Party",        aliases: &["party"],        id: Scope(711), subscope_of: &[] },
    ScopeDef { name: "Religion",     aliases: &["religion"],     id: Scope(712), subscope_of: &[] },
    ScopeDef { name: "Culture",      aliases: &["culture"],      id: Scope(713), subscope_of: &[] },
    ScopeDef { name: "Job",          aliases: &["job"],          id: Scope(714), subscope_of: &[] },
    ScopeDef { name: "CultureGroup", aliases: &["culture group"],id: Scope(715), subscope_of: &[] },
    ScopeDef { name: "Area",         aliases: &["area"],         id: Scope(716), subscope_of: &[] },
    ScopeDef { name: "State",        aliases: &["state"],        id: Scope(717), subscope_of: &[] },
    ScopeDef { name: "Subunit",      aliases: &["subunit"],      id: Scope(718), subscope_of: &[] },
    ScopeDef { name: "Governorship", aliases: &["governorship"], id: Scope(719), subscope_of: &[] },
    ScopeDef { name: "Region",       aliases: &["region"],       id: Scope(720), subscope_of: &[] },
];

const IR_MODIFIERS: &[ModifierCategory] = &[
    ModifierCategory { name: "Province",  scopes: &[Scope(706), Scope(704)] },
    ModifierCategory { name: "Country",   scopes: &[Scope(704)] },
    ModifierCategory { name: "Unit",      scopes: &[Scope(708), Scope(704)] },
    ModifierCategory { name: "Character", scopes: &[Scope(705), Scope(704)] },
];

// ── EU5 ───────────────────────────────────────────────────────────────────────
// F# EU5Constants.fs is a stub in the upstream codebase.

const EU5_FOLDERS: &[&str] = &[
    "common", "events", "gfx", "interface", "localization", "history", "map", "music", "sound",
];
const EU5_SCOPES: &[ScopeDef] = &[];
const EU5_MODIFIERS: &[ModifierCategory] = &[];

// ── Custom ────────────────────────────────────────────────────────────────────

const CUSTOM_FOLDERS: &[&str] = &[
    "common", "events", "gfx", "interface", "localisation", "map", "music", "sound",
];
const CUSTOM_SCOPES: &[ScopeDef] = &[];
const CUSTOM_MODIFIERS: &[ModifierCategory] = &[];
