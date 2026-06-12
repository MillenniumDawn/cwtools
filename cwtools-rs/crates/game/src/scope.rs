/// A scope represents the game entity context in which a script block operates.
/// e.g., "country", "state", "planet", "leader", etc. Used by the const scope
/// tables in `constants.rs`; the live engine uses `scope_engine::ScopeId`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct Scope(pub u32);

/// A game-specific scope definition (name + aliases + subscope relations).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct ScopeDef {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub id: Scope,
    /// IDs of scopes this scope is a sub-scope of.
    pub subscope_of: &'static [Scope],
}
