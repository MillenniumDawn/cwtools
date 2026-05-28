use crate::constants::Game;
use std::collections::HashMap;

/// A scope identifier (opaque u32).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct ScopeId(pub u32);

/// Result of resolving a scope change.
#[derive(Debug, Clone, PartialEq)]
pub enum ScopeResult {
    /// Scope changed successfully to the given scope.
    NewScope { scope: ScopeId },
    /// Scope is a logical join (e.g., every_state = { ... } means "execute in every state scope").
    ScopeJoin,
    /// Scope is a logical change that resets the stack.
    ScopeClear,
    /// Scope change failed (e.g., from_root from a non-root scope).
    WrongScope,
    /// The key doesn't change scope at all.
    NotFound,
}

/// A single scope change definition (e.g., `every_state` → push `State`).
#[derive(Debug, Clone)]
pub struct ScopeChange {
    pub name: &'static str,
    pub result: ScopeResult,
    pub description: &'static str,
}

/// The scope context tracks current scope during AST traversal.
/// Mirrors the F# `ScopeContext` from `Scopes.fs`.
#[derive(Debug, Clone)]
pub struct ScopeContext {
    /// The root scope (e.g., Country for country events).
    pub root: ScopeId,
    /// The immediate `from` scope.
    pub from: Option<ScopeId>,
    /// The scope stack (current scope is last element).
    pub scopes: Vec<ScopeId>,
    /// Pre-built scope changes for this game.
    pub scope_changes: HashMap<String, ScopeChange>,
}

impl ScopeContext {
    pub fn new(game: Game, root: ScopeId) -> Self {
        let mut changes = HashMap::new();
        load_scope_changes(game, &mut changes);

        Self {
            root,
            from: None,
            scopes: vec![root],
            scope_changes: changes,
        }
    }

    /// Current active scope (top of stack).
    pub fn current(&self) -> Option<ScopeId> {
        self.scopes.last().copied()
    }

    /// Push a new scope onto the stack.
    pub fn push(&mut self, scope: ScopeId) {
        self.scopes.push(scope);
    }

    /// Pop the current scope, restoring the previous.
    pub fn pop(&mut self) -> Option<ScopeId> {
        if self.scopes.len() > 1 {
            self.scopes.pop()
        } else {
            None // Can't pop below root
        }
    }

    /// Restore the scope stack to a saved state.
    pub fn restore(&mut self, saved: Vec<ScopeId>) {
        self.scopes = saved;
    }

    /// Save current scope stack.
    pub fn save(&self) -> Vec<ScopeId> {
        self.scopes.clone()
    }

    /// Apply a scope change by key (e.g., "every_state", "random_country", "prev").
    pub fn change_scope(&mut self, key: &str) -> ScopeResult {
        let normalized = key.to_lowercase();

        // Special meta scopes
        match normalized.as_str() {
            "this" | "self" => {
                return ScopeResult::NewScope {
                    scope: self.current().unwrap_or(self.root),
                }
            }
            "root" => {
                return ScopeResult::NewScope { scope: self.root }
            }
            "from" => {
                if let Some(from) = self.from {
                    return ScopeResult::NewScope { scope: from };
                }
                return ScopeResult::WrongScope;
            }
            "prev" => {
                if self.scopes.len() >= 2 {
                    let prev = self.scopes[self.scopes.len() - 2];
                    return ScopeResult::NewScope { scope: prev };
                }
                return ScopeResult::WrongScope;
            }
            _ => {}
        }

        // Look up in scope changes table (clone result to avoid borrow conflict)
        let result = self.scope_changes.get(&normalized).map(|c| c.result.clone());
        if let Some(change_result) = result {
            match change_result {
                ScopeResult::NewScope { scope } => {
                    self.push(scope);
                    ScopeResult::NewScope { scope }
                }
                ScopeResult::ScopeJoin => {
                    // Join scopes don't push; they indicate iteration
                    ScopeResult::ScopeJoin
                }
                ScopeResult::ScopeClear => {
                    self.scopes.clear();
                    self.scopes.push(self.root);
                    ScopeResult::ScopeClear
                }
                other => other,
            }
        } else {
            ScopeResult::NotFound
        }
    }

    /// Set the `from` scope (used when entering a new block that has a `from` context).
    pub fn set_from(&mut self, scope: ScopeId) {
        self.from = Some(scope);
    }
}

fn make_change(name: &'static str, scope_id: u32, desc: &'static str) -> ScopeChange {
    ScopeChange {
        name,
        result: ScopeResult::NewScope {
            scope: ScopeId(scope_id),
        },
        description: desc,
    }
}

fn load_scope_changes(game: Game, changes: &mut HashMap<String, ScopeChange>) {
    use crate::constants::Game::*;

    match game {
        Hoi4 => load_hoi4_scope_changes(changes),
        Stellaris => load_stellaris_scope_changes(changes),
        _ => {} // stub for other games
    }
}

fn load_hoi4_scope_changes(changes: &mut HashMap<String, ScopeChange>) {
    // Scope IDs from constants.rs:
    // Country = 100, State = 101, Unit Leader = 102, Air = 103

    let defs: &[(&str, u32, &str)] = &[
        ("country", 100, "Change scope to Country"),
        ("state", 101, "Change scope to State"),
        ("unit_leader", 102, "Change scope to Unit Leader"),
        ("air", 103, "Change scope to Air"),
        ("every_country", 100, "Iterate over all countries"),
        ("random_country", 100, "Pick random country"),
        ("every_state", 101, "Iterate over all states"),
        ("random_state", 101, "Pick random state"),
        ("every_unit_leader", 102, "Iterate over all unit leaders"),
        ("random_unit_leader", 102, "Pick random unit leader"),
        ("every_air", 103, "Iterate over all air wings"),
        ("random_air", 103, "Pick random air wing"),
        ("hidden_effect", 100, "Hidden effect block"),
        ("hidden_trigger", 100, "Hidden trigger block"),
    ];

    for (name, scope_id, desc) in defs {
        changes.insert(name.to_string(), make_change(name, *scope_id, desc));
    }
}

fn load_stellaris_scope_changes(changes: &mut HashMap<String, ScopeChange>) {
    let defs: &[(&str, u32, &str)] = &[
        ("country", 200, "Change scope to Country"),
        ("leader", 201, "Change scope to Leader"),
        ("system", 202, "Change scope to System"),
        ("planet", 203, "Change scope to Planet"),
        ("ship", 204, "Change scope to Ship"),
        ("fleet", 205, "Change scope to Fleet"),
        ("pop", 206, "Change scope to Pop"),
        ("army", 207, "Change scope to Army"),
        ("species", 208, "Change scope to Species"),
        ("pop_faction", 209, "Change scope to Pop Faction"),
        ("sector", 210, "Change scope to Sector"),
        ("federation", 211, "Change scope to Federation"),
        ("war", 212, "Change scope to War"),
        ("megastructure", 213, "Change scope to Megastructure"),
        ("design", 214, "Change scope to Design"),
        ("starbase", 215, "Change scope to Starbase"),
        ("star", 216, "Change scope to Star"),
        ("deposit", 217, "Change scope to Deposit"),
        ("archaeological_site", 218, "Change scope to Archaeological Site"),
        ("every_country", 200, "Iterate over all countries"),
        ("random_country", 200, "Pick random country"),
        ("every_planet", 203, "Iterate over all planets"),
        ("random_planet", 203, "Pick random planet"),
        ("every_ship", 204, "Iterate over all ships"),
        ("random_ship", 204, "Pick random ship"),
        ("every_fleet", 205, "Iterate over all fleets"),
        ("random_fleet", 205, "Pick random fleet"),
        ("every_pop", 206, "Iterate over all pops"),
        ("random_pop", 206, "Pick random pop"),
        ("every_army", 207, "Iterate over all armies"),
        ("random_army", 207, "Pick random army"),
        ("every_system", 202, "Iterate over all systems"),
        ("random_system", 202, "Pick random system"),
    ];

    for (name, scope_id, desc) in defs {
        changes.insert(name.to_string(), make_change(name, *scope_id, desc));
    }
}

/// Validate that a scope field (e.g., `scope[country]`) matches current context.
pub fn validate_scope_field(
    _context: &ScopeContext,
    _field: &str,
) -> bool {
    // TODO: strict scope validation against ScopeContext.current()
    // For now, accept all scope fields (mirrors F# lenient behavior)
    true
}
