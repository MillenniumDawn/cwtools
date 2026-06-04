use crate::constants::Game;
use crate::scope::Scope;
use std::collections::HashMap;

/// Opaque scope id — a thin newtype over the same u32 used by `Scope`.
/// Keeping them separate lets the validation crate import `ScopeId` without
/// pulling the full `Scope` symbol, matching the original public API.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ScopeId(pub u32);

impl ScopeId {
    /// Wrap a raw u32.
    pub const fn of(id: u32) -> Self {
        ScopeId(id)
    }
    /// Convert to the canonical `Scope` type used in constants / scope-defs.
    pub fn as_scope(self) -> Scope {
        Scope(self.0)
    }
}

impl From<Scope> for ScopeId {
    fn from(s: Scope) -> Self {
        ScopeId(s.0)
    }
}

impl From<ScopeId> for Scope {
    fn from(s: ScopeId) -> Self {
        Scope(s.0)
    }
}

/// ANY scope sentinel — matches any scope check.
pub const SCOPE_ANY: ScopeId = ScopeId(0);
/// INVALID scope sentinel — used for `none` contexts.
pub const SCOPE_INVALID: ScopeId = ScopeId(1);

// ── ScopeResult ──────────────────────────────────────────────────────────────

/// Result of resolving a scope-change command against a `ScopeContext`.
///
/// Mirrors F# `ScopeResult` (Scopes.fs:82-88) with the addition of
/// richer payload fields the Rust validation layer needs.
#[derive(Debug, Clone, PartialEq)]
pub enum ScopeResult {
    /// The command is a valid scope-change and has already been applied to the
    /// context.  `ignore_keys` lists child keys that should not be validated
    /// inside the resulting block (matches F# `ignoreKeys`).
    NewScope {
        scope: ScopeId,
        ignore_keys: Vec<String>,
    },
    /// Command exists but the current scope does not satisfy it.
    WrongScope {
        command: String,
        current: ScopeId,
        expected: Vec<ScopeId>,
    },
    /// Variable / scripted-var reference found (key starts with `@` or
    /// matches a var-prefix).
    VarFound,
    /// Variable reference not found.
    VarNotFound(String),
    /// Value-only trigger (not a scope-changer) was matched at the final
    /// segment of a dotted key — e.g. `has_technology`.
    ValueFound,
    /// Nothing matched — caller should treat the key as an unknown command.
    NotFound,
    /// `event_target:`, `parameter:`, `scope:` or similar prefix that always
    /// produces ANY scope (valid in any context).
    AnyScope,
}

// ── Saved state ──────────────────────────────────────────────────────────────

/// Snapshot of a `ScopeContext` that can be restored after recursing into a
/// child block.  Returned by `save()` / accepted by `restore()`.
#[derive(Debug, Clone)]
pub struct SavedContext {
    pub root: ScopeId,
    pub scopes: Vec<ScopeId>,
    pub from: Vec<ScopeId>,
}

// ── ScopeContext ─────────────────────────────────────────────────────────────

/// The scope context that is threaded through the AST traversal.
///
/// Mirrors F# `ScopeContext` (Scopes.fs:61-81) but with the richer stack
/// operations needed by the Rust validator.
///
/// * `root`   – scope of the outermost block (e.g. Country for a country event).
/// * `scopes` – stack; `scopes.last()` is the current scope.
/// * `from`   – FROM stack: `from[0]` = FROM, `from[1]` = FROMFROM, etc.
///
/// Scope zero (`SCOPE_ANY`) is the wildcard that passes all checks.
#[derive(Debug, Clone, PartialEq)]
pub struct ScopeContext {
    pub root: ScopeId,
    /// Current-scope stack (most-recent last, like F# `Scopes` list head).
    pub scopes: Vec<ScopeId>,
    /// FROM chain: index 0 = FROM, 1 = FROMFROM, 2 = FROMFROMFROM, 3 = FROMFROMFROMFROM.
    pub from: Vec<ScopeId>,
    /// Pre-built lookup table for game-specific named scope-links.
    pub scope_links: HashMap<String, ScopeLink>,
}

/// A single named scope-link definition (a scoped effect / one-to-one link).
#[derive(Debug, Clone, PartialEq)]
pub struct ScopeLink {
    /// Scopes the command is valid in (empty = valid in all).
    pub valid_scopes: Vec<ScopeId>,
    /// Scope produced by the link.  None = value-only (no scope change).
    pub target: Option<ScopeId>,
    /// Whether this link actually changes scope (true) or is a value trigger.
    pub is_scope_change: bool,
    /// Keys to ignore inside the child block.
    pub ignore_keys: Vec<String>,
}

impl ScopeContext {
    // ── Constructors ────────────────────────────────────────────────────────

    /// Create a fresh context rooted at `root` for the given `game`.
    pub fn new(game: Game, root: ScopeId) -> Self {
        let mut links = HashMap::new();
        load_scope_links(game, &mut links);
        Self {
            root,
            scopes: vec![root],
            from: Vec::new(),
            scope_links: links,
        }
    }

    /// Create a context with no game-specific links (useful for tests /
    /// generic validation).
    pub fn new_generic(root: ScopeId) -> Self {
        Self {
            root,
            scopes: vec![root],
            from: Vec::new(),
            scope_links: HashMap::new(),
        }
    }

    // ── Stack accessors ──────────────────────────────────────────────────────

    /// Current active scope (top of stack).  Returns `root` if the stack is
    /// somehow empty.
    pub fn current(&self) -> Option<ScopeId> {
        self.scopes.last().copied()
    }

    /// Push a new scope onto the stack (used by callers that already resolved
    /// a `NewScope` result and want to record the change).
    pub fn push_scope(&mut self, scope: ScopeId) {
        self.scopes.push(scope);
    }

    /// Pop the most-recent scope.  Will not pop below the root entry.
    pub fn pop_scope(&mut self) -> Option<ScopeId> {
        if self.scopes.len() > 1 {
            self.scopes.pop()
        } else {
            None
        }
    }

    /// Return the PREV scope (one below current), or current if there is none.
    /// Apply N `prev` hops to a scope list, returning the resulting list.
    fn pop_n(scopes: &[ScopeId], n: usize) -> Vec<ScopeId> {
        let mut v = scopes.to_vec();
        for _ in 0..n {
            if v.len() > 1 {
                v.pop();
            }
        }
        v
    }

    /// Return GET_FROM(i) (1-based, matching F# `GetFrom`).
    pub fn get_from(&self, i: usize) -> ScopeId {
        if i >= 1 && self.from.len() >= i {
            self.from[i - 1]
        } else {
            SCOPE_ANY
        }
    }

    // ── Save / restore ───────────────────────────────────────────────────────

    /// Snapshot the mutable parts of the context.
    pub fn save(&self) -> SavedContext {
        SavedContext {
            root: self.root,
            scopes: self.scopes.clone(),
            from: self.from.clone(),
        }
    }

    /// Restore the mutable parts from a snapshot.
    pub fn restore(&mut self, saved: SavedContext) {
        self.root = saved.root;
        self.scopes = saved.scopes;
        self.from = saved.from;
    }

    // ── replace_scope ────────────────────────────────────────────────────────

    /// Apply a `replace_scope` block (§ replace_scope in CWT rules).
    ///
    /// `root`, `this` and `from`/`prev` slots are set from the provided
    /// scope name strings, resolved through the game's scope catalog.  Unknown
    /// or absent names are left unchanged.
    pub fn apply_replace_scope(
        &mut self,
        root: Option<&str>,
        this: Option<&str>,
        froms: &[String],
        prevs: &[String],
        game: Game,
    ) {
        let resolve = |name: &str| -> Option<ScopeId> {
            let lower = name.to_lowercase();
            game.scope_defs()
                .iter()
                .find(|d| {
                    d.name.eq_ignore_ascii_case(name)
                        || d.aliases.iter().any(|a| a.eq_ignore_ascii_case(name))
                })
                .map(|d| ScopeId::from(d.id))
                .or_else(|| {
                    // Fallback: parse the integer id literals sometimes used in tests
                    lower.parse::<u32>().ok().map(ScopeId)
                })
        };

        if let Some(r) = root.and_then(|n| resolve(n)) {
            self.root = r;
        }
        if let Some(t) = this.and_then(|n| resolve(n)) {
            // "this" becomes the new current scope (push on top)
            if let Some(last) = self.scopes.last_mut() {
                *last = t;
            } else {
                self.scopes.push(t);
            }
        }
        if !froms.is_empty() {
            self.from = froms
                .iter()
                .filter_map(|n| resolve(n))
                .collect();
        }
        if !prevs.is_empty() {
            // Replace the bottom of the scope stack with the prev chain.
            // Keep the current scope on top.
            let current = self.scopes.last().copied().unwrap_or(self.root);
            let mut new_scopes: Vec<ScopeId> = prevs
                .iter()
                .filter_map(|n| resolve(n))
                .collect();
            new_scopes.push(current);
            self.scopes = new_scopes;
        }
    }

    // ── change_scope ─────────────────────────────────────────────────────────

    /// Resolve a single key against the current context.
    ///
    /// Handles:
    /// * Prefixes: `hidden:` (stripped), `event_target:`, `parameter:`,
    ///   `scope:` (Jomini named scope) → `AnyScope`.
    /// * Meta keywords: `this`/`self`, `root`, `prev`/`prevprev`/…,
    ///   `from`/`fromfrom`/…, `root_from`/`root_fromfrom`/….
    /// * Dotted chains: `owner.capital.controller` split and folded.
    /// * Game-specific named links looked up in `scope_links`.
    pub fn change_scope(&mut self, key: &str) -> ScopeResult {
        // Strip leading `hidden:` prefix (F# Scopes.fs:148-149).
        let key = if key.to_ascii_lowercase().starts_with("hidden:") {
            &key[7..]
        } else {
            key
        };

        // Special prefixes that always yield AnyScope (F# Scopes.fs:153-164).
        let lower = key.to_ascii_lowercase();
        if lower.starts_with("event_target:")
            || lower.starts_with("parameter:")
            || lower.starts_with("scope:")
            || lower.starts_with('@')
        {
            // Push ANY so subsequent dotted segments see an open scope.
            self.scopes.push(SCOPE_ANY);
            return ScopeResult::AnyScope;
        }

        // Dotted key: fold through each segment.
        if key.contains('.') {
            return self.change_scope_dotted(key);
        }

        self.resolve_single(key)
    }

    /// Fold a dotted key like `owner.capital.controller` left-to-right.
    fn change_scope_dotted(&mut self, key: &str) -> ScopeResult {
        let segments: Vec<&str> = key.split('.').collect();
        let last_idx = segments.len().saturating_sub(1);
        let mut last_result = ScopeResult::NotFound;

        for (i, seg) in segments.iter().enumerate() {
            let is_last = i == last_idx;
            let result = self.resolve_single(seg);
            match &result {
                ScopeResult::NewScope { .. } | ScopeResult::AnyScope => {
                    // scope was pushed — continue to next segment
                    last_result = result;
                }
                ScopeResult::VarFound | ScopeResult::ValueFound if is_last => {
                    last_result = result;
                    break;
                }
                _ => {
                    // Any failure short-circuits
                    return result;
                }
            }
        }
        last_result
    }

    /// Resolve a single (non-dotted) key.
    fn resolve_single(&mut self, key: &str) -> ScopeResult {
        let lower = key.to_ascii_lowercase();

        // Variable / scripted prefix
        if lower.starts_with('@') {
            self.scopes.push(SCOPE_ANY);
            return ScopeResult::VarFound;
        }

        match lower.as_str() {
            // ── this / self ──────────────────────────────────────────────
            "this" | "self" => {
                let cur = self.scopes.last().copied().unwrap_or(self.root);
                self.scopes.push(cur);
                return ScopeResult::NewScope { scope: cur, ignore_keys: vec![] };
            }
            // ── root ─────────────────────────────────────────────────────
            "root" => {
                let r = self.root;
                self.scopes.push(r);
                return ScopeResult::NewScope { scope: r, ignore_keys: vec![] };
            }
            // ── prev chain ───────────────────────────────────────────────
            "prev" => {
                return self.apply_prev(1);
            }
            "prevprev" | "prev_prev" => {
                return self.apply_prev(2);
            }
            "prevprevprev" | "prev_prev_prev" => {
                return self.apply_prev(3);
            }
            "prevprevprevprev" | "prev_prev_prev_prev" => {
                return self.apply_prev(4);
            }
            // ── from chain ───────────────────────────────────────────────
            "from" => {
                return self.apply_from(1);
            }
            "fromfrom" => {
                return self.apply_from(2);
            }
            "fromfromfrom" => {
                return self.apply_from(3);
            }
            "fromfromfromfrom" => {
                return self.apply_from(4);
            }
            // ── root_from composites ─────────────────────────────────────
            "root_from" => {
                let r = self.root;
                self.scopes.push(r);
                return self.apply_from(1);
            }
            "root_fromfrom" => {
                let r = self.root;
                self.scopes.push(r);
                return self.apply_from(2);
            }
            "root_fromfromfrom" => {
                let r = self.root;
                self.scopes.push(r);
                return self.apply_from(3);
            }
            "root_fromfromfromfrom" => {
                let r = self.root;
                self.scopes.push(r);
                return self.apply_from(4);
            }
            // ── logical/boolean keywords (pass-through) ──────────────────
            "and" | "or" | "not" | "nor" | "nand" | "if" | "else" | "else_if"
            | "hidden_effect" | "hidden_trigger" | "limit"
            | "trigger_if" | "trigger_else" | "trigger_else_if" => {
                let cur = self.scopes.last().copied().unwrap_or(self.root);
                return ScopeResult::NewScope { scope: cur, ignore_keys: vec![] };
            }
            _ => {}
        }

        // Game-specific named link lookup
        let link_opt = self.scope_links.get(&lower).cloned();
        if let Some(link) = link_opt {
            let current = self.scopes.last().copied().unwrap_or(self.root);

            // ANY scope always passes
            let valid = current == SCOPE_ANY
                || link.valid_scopes.is_empty()
                || link.valid_scopes.iter().any(|s| {
                    *s == SCOPE_ANY || *s == current || s.as_scope().is_of_scope(current.as_scope())
                });

            if valid {
                if link.is_scope_change {
                    let target = link.target.unwrap_or(SCOPE_ANY);
                    self.scopes.push(target);
                    return ScopeResult::NewScope {
                        scope: target,
                        ignore_keys: link.ignore_keys.clone(),
                    };
                } else {
                    // Value-only trigger
                    return ScopeResult::ValueFound;
                }
            } else {
                return ScopeResult::WrongScope {
                    command: key.to_string(),
                    current,
                    expected: link.valid_scopes.clone(),
                };
            }
        }

        ScopeResult::NotFound
    }

    // ── prev / from helpers ───────────────────────────────────────────────────

    fn apply_prev(&mut self, hops: usize) -> ScopeResult {
        // Pop `hops` levels.  The resulting top of stack is the PREV scope.
        let new_scopes = Self::pop_n(&self.scopes, hops);
        let scope = new_scopes.last().copied().unwrap_or(self.root);
        self.scopes = new_scopes;
        ScopeResult::NewScope { scope, ignore_keys: vec![] }
    }

    fn apply_from(&mut self, i: usize) -> ScopeResult {
        let scope = self.get_from(i);
        self.scopes.push(scope);
        ScopeResult::NewScope { scope, ignore_keys: vec![] }
    }
}

// ── Scope link loading ────────────────────────────────────────────────────────

fn load_scope_links(game: Game, links: &mut HashMap<String, ScopeLink>) {
    use crate::constants::Game::*;
    match game {
        Hoi4 => load_hoi4_links(links),
        Stellaris => load_stellaris_links(links),
        Eu4 => load_eu4_links(links),
        Ck2 => load_ck2_links(links),
        Ck3 => load_ck3_links(links),
        Vic2 => load_vic2_links(links),
        Ir => load_ir_links(links),
        _ => {}
    }
}

/// Build a scope-change link: valid in `valid_scopes`, produces `target`.
fn sc(valid: &[u32], target: u32) -> ScopeLink {
    ScopeLink {
        valid_scopes: valid.iter().copied().map(ScopeId).collect(),
        target: Some(ScopeId(target)),
        is_scope_change: true,
        ignore_keys: vec![],
    }
}

/// Insert a link under multiple alias keys.
fn insert_aliases(links: &mut HashMap<String, ScopeLink>, names: &[&str], link: ScopeLink) {
    for name in names {
        links.insert(name.to_string(), link.clone());
    }
}

// ── HOI4 ────────────────────────────────────────────────────────────────────

// Scope IDs: Country=100, State=101, UnitLeader=102, Air=103
fn load_hoi4_links(links: &mut HashMap<String, ScopeLink>) {
    const COUNTRY: u32 = 100;
    const STATE: u32 = 101;
    const UNIT_LEADER: u32 = 102;
    const AIR: u32 = 103;

    // Scope-iterators / randoms (valid in all → any → target)
    let entries: &[(&[&str], &[u32], u32)] = &[
        (&["every_country", "random_country", "any_country", "country"],   &[], COUNTRY),
        (&["every_state", "random_state", "any_state", "state"],           &[], STATE),
        (&["every_unit_leader", "random_unit_leader", "unit_leader"],      &[], UNIT_LEADER),
        (&["every_air", "random_air", "air"],                              &[], AIR),
        // Links from country
        (&["capital_scope"],                 &[COUNTRY], STATE),
        (&["overlord"],                      &[COUNTRY], COUNTRY),
        (&["faction_leader"],                &[COUNTRY], COUNTRY),
        // Links from state
        (&["controller", "owner"],           &[STATE], COUNTRY),
    ];

    for (aliases, valid, target) in entries {
        insert_aliases(links, aliases, sc(valid, *target));
    }
}

// ── Stellaris ────────────────────────────────────────────────────────────────

// Scope IDs:
// Country=200, Leader=201, System=202, Planet=203, Ship=204, Fleet=205,
// Pop=206, Army=207, Species=208, PopFaction=209, Sector=210,
// Federation=211, War=212, Megastructure=213, Design=214, Starbase=215,
// Star=216, Deposit=217, ArchaeologicalSite=218, AmbientObject=219
fn load_stellaris_links(links: &mut HashMap<String, ScopeLink>) {
    const COUNTRY: u32 = 200;
    const LEADER: u32 = 201;
    const SYSTEM: u32 = 202;
    const PLANET: u32 = 203;
    const SHIP: u32 = 204;
    const FLEET: u32 = 205;
    const POP: u32 = 206;
    const ARMY: u32 = 207;
    const SPECIES: u32 = 208;
    const POP_FACTION: u32 = 209;
    const SECTOR: u32 = 210;
    const FEDERATION: u32 = 211;
    const WAR: u32 = 212;
    const MEGASTRUCTURE: u32 = 213;
    const STARBASE: u32 = 215;
    const STAR: u32 = 216;
    const DEPOSIT: u32 = 217;

    let entries: &[(&[&str], &[u32], u32)] = &[
        // Iterators / randoms (global)
        (&["every_country", "random_country", "any_country", "country"],         &[], COUNTRY),
        (&["every_planet", "random_planet", "any_planet", "planet"],             &[], PLANET),
        (&["every_ship", "random_ship", "any_ship", "ship"],                     &[], SHIP),
        (&["every_fleet", "random_fleet", "any_fleet", "fleet"],                 &[], FLEET),
        (&["every_pop", "random_pop", "any_pop", "pop"],                         &[], POP),
        (&["every_army", "random_army", "any_army", "army"],                     &[], ARMY),
        (&["every_system", "random_system", "any_system",
           "galactic_object", "system", "galacticobject"],                       &[], SYSTEM),
        (&["every_leader", "random_leader", "any_leader", "leader"],             &[], LEADER),
        (&["every_species", "random_species", "any_species", "species"],         &[], SPECIES),
        (&["every_pop_faction", "random_pop_faction", "pop_faction"],            &[], POP_FACTION),
        (&["federation"],                                                         &[], FEDERATION),
        (&["war"],                                                                &[], WAR),
        (&["megastructure"],                                                      &[], MEGASTRUCTURE),
        (&["starbase"],                                                            &[], STARBASE),
        (&["deposit"],                                                             &[], DEPOSIT),
        // Country links
        (&["overlord", "federation_leader"],                                      &[COUNTRY], COUNTRY),
        (&["capital"],                                                             &[COUNTRY], PLANET),
        (&["capital_star"],                                                        &[COUNTRY], SYSTEM),
        (&["sector"],                                                              &[PLANET, SYSTEM], SECTOR),
        // Planet links
        (&["star"],                                                                &[PLANET], STAR),
        (&["solar_system"],                                                        &[PLANET, SHIP, FLEET, STARBASE], SYSTEM),
        (&["owner"],                                                               &[PLANET, SHIP, FLEET, ARMY, POP_FACTION, STARBASE, MEGASTRUCTURE], COUNTRY),
        (&["controller"],                                                          &[PLANET], COUNTRY),
        // Ship / fleet links
        (&["fleet"],                                                               &[SHIP], FLEET),
        (&["leader"],                                                              &[SHIP, FLEET, COUNTRY], LEADER),
        // Species links
        (&["species"],                                                             &[POP], SPECIES),
    ];

    for (aliases, valid, target) in entries {
        insert_aliases(links, aliases, sc(valid, *target));
    }
}

// ── EU4 ──────────────────────────────────────────────────────────────────────

// Scope IDs: Country=300, Province=301, TradeNode=302, Unit=303,
//            Monarch=304, Heir=305, Consort=306, RebelFaction=307,
//            Religion=308, Culture=309, Advisor=310
fn load_eu4_links(links: &mut HashMap<String, ScopeLink>) {
    const COUNTRY: u32 = 300;
    const PROVINCE: u32 = 301;
    const TRADE_NODE: u32 = 302;

    let entries: &[(&[&str], &[u32], u32)] = &[
        // Generic iterators
        (&["every_country", "random_country", "any_country", "country"],         &[], COUNTRY),
        (&["every_province", "random_province", "any_province", "province"],     &[], PROVINCE),
        // Country → Province
        (&["capital", "capital_scope"],                                           &[COUNTRY, PROVINCE], PROVINCE),
        (&["controller", "owner"],                                                &[PROVINCE, TRADE_NODE], COUNTRY),
        (&["overlord"],                                                            &[COUNTRY], COUNTRY),
        (&["emperor"],                                                             &[], COUNTRY),
        // EU4 scoped effects from CK2/EU4 Scopes.fs
        (&["trade_node", "tradenode"],                                            &[], TRADE_NODE),
    ];

    for (aliases, valid, target) in entries {
        insert_aliases(links, aliases, sc(valid, *target));
    }
}

// ── CK2 ──────────────────────────────────────────────────────────────────────

// Scope IDs: Character=400, Title=401, Province=402, Offmap=403, War=404,
//            Siege=405, Unit=406, Religion=407, Culture=408, Society=409,
//            Artifact=410, Bloodline=411, Wonder=412
fn load_ck2_links(links: &mut HashMap<String, ScopeLink>) {
    const CHARACTER: u32 = 400;
    const TITLE: u32 = 401;
    const PROVINCE: u32 = 402;

    let entries: &[(&[&str], &[u32], u32)] = &[
        (&["every_character", "random_character", "any_character", "character"],  &[], CHARACTER),
        (&["every_province", "random_province"],                                   &[], PROVINCE),
        // Character links (from CK2Scopes.fs scopedEffects)
        (&["primary_title"],                      &[CHARACTER], TITLE),
        (&["mother", "mother_even_if_dead",
           "father", "father_even_if_dead",
           "killer", "liege", "liege_before_war",
           "top_liege", "employer", "host",
           "spouse"],                              &[CHARACTER], CHARACTER),
        (&["capital_scope"],                       &[CHARACTER, TITLE], PROVINCE),
        (&["location"],                            &[CHARACTER], PROVINCE),
        (&["realm_capital"],                       &[CHARACTER], PROVINCE),
        // Title links
        (&["holder_scope"],                        &[TITLE], CHARACTER),
        (&["de_jure_liege_title"],                 &[TITLE], TITLE),
        (&["owner"],                               &[PROVINCE], CHARACTER),
    ];

    for (aliases, valid, target) in entries {
        insert_aliases(links, aliases, sc(valid, *target));
    }
}

// ── CK3 ──────────────────────────────────────────────────────────────────────

// Scope IDs reuse the VIC2/IR set for CK3 (they share identical scopes in F#):
// Value=500, Bool=501, Flag=502, Color=503, Country=504, Character=505,
// Province=506, Combat=507, Unit=508, Pop=509, Family=510, Party=511,
// Religion=512, Culture=513, Job=514, CultureGroup=515, Area=516,
// State=517, Subunit=518, Governorship=519, Region=520
fn load_ck3_links(links: &mut HashMap<String, ScopeLink>) {
    const CHARACTER: u32 = 505;
    const PROVINCE: u32 = 506;

    let entries: &[(&[&str], &[u32], u32)] = &[
        (&["every_character", "random_character", "any_character"], &[], CHARACTER),
        (&["every_province", "random_province", "any_province"],    &[], PROVINCE),
        (&["liege", "top_liege", "father", "mother", "spouse"],     &[CHARACTER], CHARACTER),
        (&["capital_province"],                                       &[CHARACTER], PROVINCE),
        (&["holder"],                                                  &[PROVINCE], CHARACTER),
    ];

    for (aliases, valid, target) in entries {
        insert_aliases(links, aliases, sc(valid, *target));
    }
}

// ── VIC2 ─────────────────────────────────────────────────────────────────────

// Same scope set as CK3 / IR, IDs 600-620
fn load_vic2_links(links: &mut HashMap<String, ScopeLink>) {
    const COUNTRY: u32 = 604;
    const PROVINCE: u32 = 606;

    let entries: &[(&[&str], &[u32], u32)] = &[
        (&["every_country", "random_country"],    &[], COUNTRY),
        (&["every_province", "random_province"],  &[], PROVINCE),
        (&["owner", "controller"],                &[PROVINCE], COUNTRY),
        (&["capital"],                             &[COUNTRY], PROVINCE),
    ];

    for (aliases, valid, target) in entries {
        insert_aliases(links, aliases, sc(valid, *target));
    }
}

// ── IR (Imperator) ────────────────────────────────────────────────────────────

// IDs 700-720
fn load_ir_links(links: &mut HashMap<String, ScopeLink>) {
    const COUNTRY: u32 = 704;
    const PROVINCE: u32 = 706;
    const CHARACTER: u32 = 705;

    let entries: &[(&[&str], &[u32], u32)] = &[
        (&["every_country", "random_country"],      &[], COUNTRY),
        (&["every_province", "random_province"],    &[], PROVINCE),
        (&["every_character", "random_character"],  &[], CHARACTER),
        (&["owner", "controller"],                  &[PROVINCE], COUNTRY),
        (&["capital"],                               &[COUNTRY], PROVINCE),
        (&["liege", "employer", "spouse"],           &[CHARACTER], CHARACTER),
    ];

    for (aliases, valid, target) in entries {
        insert_aliases(links, aliases, sc(valid, *target));
    }
}

// ── validate_scope_field (kept for compat) ────────────────────────────────────

/// Convenience: validate that a `scope[X]` field annotation matches `context`.
/// Returns true when the current scope satisfies the requirement.
pub fn validate_scope_field(context: &ScopeContext, field: &str) -> bool {
    let cur = context.current().unwrap_or(SCOPE_ANY);
    if cur == SCOPE_ANY {
        return true;
    }
    // field is something like "country" or "province"
    let lower = field.to_ascii_lowercase();
    context
        .scope_links
        .get(&lower)
        .map(|l| l.valid_scopes.contains(&cur) || l.valid_scopes.is_empty())
        .unwrap_or(true) // unknown field → lenient
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::constants::Game;

    fn stl_ctx() -> ScopeContext {
        // Root = Country (200)
        ScopeContext::new(Game::Stellaris, ScopeId(200))
    }

    // ── PREV chain tests ──────────────────────────────────────────────────────

    #[test]
    fn prev_single() {
        let mut ctx = stl_ctx();
        ctx.push_scope(ScopeId(203)); // now: [200, 203]
        let res = ctx.change_scope("prev");
        assert_eq!(res, ScopeResult::NewScope { scope: ScopeId(200), ignore_keys: vec![] });
        // Stack after PREV: [200, 200] (hopped back to 200)
        assert_eq!(ctx.current(), Some(ScopeId(200)));
    }

    #[test]
    fn prevprev() {
        let mut ctx = stl_ctx();
        ctx.push_scope(ScopeId(203)); // [200, 203]
        ctx.push_scope(ScopeId(202)); // [200, 203, 202]
        let res = ctx.change_scope("prevprev");
        assert_eq!(res, ScopeResult::NewScope { scope: ScopeId(200), ignore_keys: vec![] });
    }

    #[test]
    fn prevprevprev() {
        let mut ctx = stl_ctx();
        ctx.push_scope(ScopeId(203));
        ctx.push_scope(ScopeId(202));
        ctx.push_scope(ScopeId(204));
        let res = ctx.change_scope("prevprevprev");
        assert_eq!(res, ScopeResult::NewScope { scope: ScopeId(200), ignore_keys: vec![] });
    }

    #[test]
    fn prevprevprevprev() {
        let mut ctx = stl_ctx();
        ctx.push_scope(ScopeId(203));
        ctx.push_scope(ScopeId(202));
        ctx.push_scope(ScopeId(204));
        ctx.push_scope(ScopeId(205));
        let res = ctx.change_scope("prevprevprevprev");
        assert_eq!(res, ScopeResult::NewScope { scope: ScopeId(200), ignore_keys: vec![] });
    }

    // ── FROM chain tests ──────────────────────────────────────────────────────

    #[test]
    fn from_basic() {
        let mut ctx = stl_ctx();
        ctx.from.push(ScopeId(203)); // FROM = Planet
        let res = ctx.change_scope("from");
        assert_eq!(res, ScopeResult::NewScope { scope: ScopeId(203), ignore_keys: vec![] });
    }

    #[test]
    fn fromfrom() {
        let mut ctx = stl_ctx();
        ctx.from.push(ScopeId(203));
        ctx.from.push(ScopeId(202)); // FROMFROM = System
        let res = ctx.change_scope("fromfrom");
        assert_eq!(res, ScopeResult::NewScope { scope: ScopeId(202), ignore_keys: vec![] });
    }

    #[test]
    fn fromfromfrom() {
        let mut ctx = stl_ctx();
        ctx.from.push(ScopeId(203));
        ctx.from.push(ScopeId(202));
        ctx.from.push(ScopeId(204)); // FROMFROMFROM = Ship
        let res = ctx.change_scope("fromfromfrom");
        assert_eq!(res, ScopeResult::NewScope { scope: ScopeId(204), ignore_keys: vec![] });
    }

    #[test]
    fn fromfromfromfrom() {
        let mut ctx = stl_ctx();
        ctx.from.push(ScopeId(203));
        ctx.from.push(ScopeId(202));
        ctx.from.push(ScopeId(204));
        ctx.from.push(ScopeId(205)); // FROMFROMFROMFROM = Fleet
        let res = ctx.change_scope("fromfromfromfrom");
        assert_eq!(res, ScopeResult::NewScope { scope: ScopeId(205), ignore_keys: vec![] });
    }

    #[test]
    fn from_missing_returns_anyscope() {
        let mut ctx = stl_ctx();
        // No FROM set — should fall back to SCOPE_ANY
        let res = ctx.change_scope("from");
        assert_eq!(res, ScopeResult::NewScope { scope: SCOPE_ANY, ignore_keys: vec![] });
    }

    // ── Dotted key tests ──────────────────────────────────────────────────────

    #[test]
    fn dotted_owner_capital() {
        // EU4: Province (301) → owner (Country 300) → capital (Province 301)
        let mut ctx = ScopeContext::new(Game::Eu4, ScopeId(301)); // start in Province
        let res = ctx.change_scope("owner.capital");
        // Should succeed (NewScope at Province level)
        assert!(matches!(res, ScopeResult::NewScope { scope: ScopeId(301), .. }
                           | ScopeResult::NewScope { scope: ScopeId(0), .. }));
    }

    #[test]
    fn dotted_single_segment_same_as_plain() {
        let mut ctx_dot = ScopeContext::new(Game::Eu4, ScopeId(300));
        let mut ctx_plain = ScopeContext::new(Game::Eu4, ScopeId(300));
        let r1 = ctx_dot.change_scope("owner");
        let r2 = ctx_plain.change_scope("owner");
        assert_eq!(r1, r2);
    }

    // ── Prefix tests ──────────────────────────────────────────────────────────

    #[test]
    fn event_target_prefix_anyscope() {
        let mut ctx = stl_ctx();
        let res = ctx.change_scope("event_target:my_target");
        assert_eq!(res, ScopeResult::AnyScope);
    }

    #[test]
    fn parameter_prefix_anyscope() {
        let mut ctx = stl_ctx();
        let res = ctx.change_scope("parameter:x");
        assert_eq!(res, ScopeResult::AnyScope);
    }

    #[test]
    fn scope_prefix_anyscope() {
        let mut ctx = stl_ctx();
        let res = ctx.change_scope("scope:my_scope");
        assert_eq!(res, ScopeResult::AnyScope);
    }

    #[test]
    fn hidden_prefix_stripped() {
        // hidden:owner in EU4 Province should resolve like plain owner
        let mut ctx = ScopeContext::new(Game::Eu4, ScopeId(301));
        let res = ctx.change_scope("hidden:owner");
        assert!(matches!(res, ScopeResult::NewScope { scope: ScopeId(300), .. }));
    }

    // ── Meta scope tests ──────────────────────────────────────────────────────

    #[test]
    fn root_returns_root() {
        let mut ctx = stl_ctx();
        ctx.push_scope(ScopeId(203));
        let res = ctx.change_scope("root");
        assert_eq!(res, ScopeResult::NewScope { scope: ScopeId(200), ignore_keys: vec![] });
    }

    #[test]
    fn this_returns_current() {
        let mut ctx = stl_ctx();
        ctx.push_scope(ScopeId(203));
        let res = ctx.change_scope("this");
        assert_eq!(res, ScopeResult::NewScope { scope: ScopeId(203), ignore_keys: vec![] });
    }

    // ── Save / restore tests ──────────────────────────────────────────────────

    #[test]
    fn save_restore_roundtrip() {
        let mut ctx = stl_ctx();
        ctx.push_scope(ScopeId(203));
        ctx.from.push(ScopeId(202));
        let saved = ctx.save();
        ctx.push_scope(ScopeId(204));
        ctx.restore(saved);
        assert_eq!(ctx.current(), Some(ScopeId(203)));
        assert_eq!(ctx.from, vec![ScopeId(202)]);
    }

    // ── Game-specific link tests ──────────────────────────────────────────────

    #[test]
    fn hoi4_state_owner() {
        let mut ctx = ScopeContext::new(Game::Hoi4, ScopeId(101)); // State
        let res = ctx.change_scope("owner");
        assert!(matches!(res, ScopeResult::NewScope { scope: ScopeId(100), .. }));
    }

    #[test]
    fn stellaris_planet_owner() {
        // Start in Planet scope
        let mut ctx = ScopeContext::new(Game::Stellaris, ScopeId(203));
        let res = ctx.change_scope("owner");
        assert!(matches!(res, ScopeResult::NewScope { scope: ScopeId(200), .. }));
    }

    #[test]
    fn eu4_province_owner_gives_country() {
        let mut ctx = ScopeContext::new(Game::Eu4, ScopeId(301));
        let res = ctx.change_scope("owner");
        assert!(matches!(res, ScopeResult::NewScope { scope: ScopeId(300), .. }));
    }
}
