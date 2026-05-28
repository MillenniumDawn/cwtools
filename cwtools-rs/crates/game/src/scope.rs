/// A scope represents the game entity context in which a script block operates.
/// e.g., "country", "state", "planet", "leader", etc.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Scope(pub u32);

impl Scope {
    pub const ANY: Scope = Scope(0);
    pub const INVALID: Scope = Scope(1);

    pub fn is_of_scope(self, other: Scope) -> bool {
        // In the full implementation, this checks the scope hierarchy.
        // For now, exact match or ANY.
        self == other || other == Scope::ANY
    }
}

/// A scope context tracks the current scope stack during script evaluation.
/// Scopes are pushed/popped as the evaluator traverses nested blocks.
#[derive(Debug, Clone, PartialEq)]
pub struct ScopeContext {
    pub root: Scope,
    pub from: Vec<Scope>,
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

    pub fn push_scope(&mut self, scope: Scope) {
        self.scopes.insert(0, scope);
    }

    pub fn pop_scope(&mut self) -> Option<Scope> {
        self.scopes.pop()
    }

    pub fn get_from(&self, index: usize) -> Scope {
        self.from.get(index).copied().unwrap_or(self.root)
    }
}

/// The result of attempting to resolve a scope change via a command (effect/trigger).
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
}

/// A game-specific scope definition (name + aliases).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScopeDef {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub id: Scope,
}

/// A game-specific modifier category.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ModifierCategory {
    pub name: &'static str,
    pub scopes: &'static [Scope],
}
