/// A scope represents the game entity context in which a script block operates.
/// e.g., "country", "state", "planet", "leader", etc.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Scope(pub u32);

impl Scope {
    /// Wildcard: matches any scope check.
    pub const ANY: Scope = Scope(0);
    /// Sentinel for invalid / unresolved scopes.
    pub const INVALID: Scope = Scope(1);

    /// Returns true if `self` satisfies a requirement of `other`.
    ///
    /// Rules (matching F# `IsOfScope`):
    /// * ANY (0) passes everything in either position.
    /// * Exact match always passes.
    /// * Subscope relations are checked via the global registry below, but
    ///   since the Rust port uses a flat scope ID space, we currently only
    ///   support the EU4 "Trade Node isSubscopeOf province" relation explicitly.
    pub fn is_of_scope(self, other: Scope) -> bool {
        if self == other {
            return true;
        }
        if self == Scope::ANY || other == Scope::ANY {
            return true;
        }
        // EU4: Trade Node (302) isSubscopeOf Province (301)
        if self == Scope(302) && other == Scope(301) {
            return true;
        }
        false
    }
}

/// A scope context tracks the current scope stack during script evaluation.
/// Scopes are pushed/popped as the evaluator traverses nested blocks.
///
/// NOTE: This type is kept for use in `constants.rs` (ScopeDef) and for
/// code that imports `cwtools_game::scope::ScopeContext` directly.
/// The live validation engine uses `cwtools_game::scope_engine::ScopeContext`.
#[derive(Debug, Clone, PartialEq)]
pub struct ScopeContext {
    pub root: Scope,
    /// FROM chain (index 0 = FROM, 1 = FROMFROM, …).
    pub from: Vec<Scope>,
    /// Scope stack; first element is the current scope (F# style list-head).
    pub scopes: Vec<Scope>,
}

impl ScopeContext {
    pub fn new(root: Scope) -> Self {
        Self {
            root,
            from: Vec::new(),
            scopes: Vec::new(),
        }
    }

    pub fn current_scope(&self) -> Scope {
        self.scopes.first().copied().unwrap_or(self.root)
    }

    /// Push a new scope (inserts at head, matching F# `x :: scopes`).
    pub fn push_scope(&mut self, scope: Scope) {
        self.scopes.insert(0, scope);
    }

    pub fn pop_scope(&mut self) -> Option<Scope> {
        if self.scopes.is_empty() {
            None
        } else {
            Some(self.scopes.remove(0))
        }
    }

    /// GET_FROM(i) — 1-based, matching F# `GetFrom`.
    pub fn get_from(&self, index: usize) -> Scope {
        if index >= 1 && self.from.len() >= index {
            self.from[index - 1]
        } else {
            self.root
        }
    }
}

/// The result of attempting to resolve a scope change via a command
/// (effect / trigger).
///
/// This richer variant is kept for use by code that imports
/// `cwtools_game::scope::ScopeResult`.  The validation crate uses the
/// leaner `scope_engine::ScopeResult`.
#[derive(Debug, Clone, PartialEq)]
pub enum ScopeResult {
    /// Scope changed successfully; new context + ignored keys.
    NewScope {
        new_scope: ScopeContext,
        ignore_keys: Vec<String>,
    },
    /// Command exists but scope doesn't match.
    WrongScope {
        command: String,
        current: Scope,
        expected: Vec<Scope>,
    },
    /// Command not found.
    NotFound,
    /// Variable reference found (e.g., `@var`).
    VarFound,
    /// Variable reference not found.
    VarNotFound(String),
    /// Value-only trigger matched (no scope change).
    ValueFound,
}

/// A game-specific scope definition (name + aliases + subscope relations).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScopeDef {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub id: Scope,
    /// IDs of scopes this scope is a sub-scope of.
    pub subscope_of: &'static [Scope],
}

/// A game-specific modifier category.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModifierCategory {
    pub name: &'static str,
    pub scopes: &'static [Scope],
}
