//! Runtime scope/link registry — the data-driven replacement for the hardcoded
//! per-game scope tables. Built from `scopes.cwt` + `links.cwt` (see the builder
//! in the validation crate) and held by every [`crate::scope_engine::ScopeContext`].
//!
//! Lives in the game crate so `ScopeContext` can hold it without a dependency
//! cycle; it is *populated* by the validation crate (which sees the rules
//! `RuleSet`). The struct fields are public so that builder can fill them.

use crate::constants::Game;
use crate::scope_engine::{SCOPE_ANY, SCOPE_INVALID, ScopeId, ScopeLink};
use std::collections::HashMap;

/// An owned scope definition (config-driven equivalent of the const `ScopeDef`).
#[derive(Debug, Clone, PartialEq)]
pub struct ScopeDefOwned {
    pub name: String,
    pub aliases: Vec<String>,
    pub subscope_of: Vec<ScopeId>,
}

/// The scopes and links available to scope resolution, built from config.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct ScopeRegistry {
    /// id -> definition (name, aliases, parents).
    pub by_id: HashMap<ScopeId, ScopeDefOwned>,
    /// lowercased name AND every alias -> id.
    pub by_name: HashMap<String, ScopeId>,
    /// Named links / iterators (`owner`, `every_state`, …) keyed by lowercase name.
    pub links: HashMap<String, ScopeLink>,
    /// Prefix links (`var:`, `sp:`, `event_target:`), matched by key prefix.
    pub prefix_links: Vec<(String, ScopeLink)>,
}

impl ScopeRegistry {
    /// Canonical short name for a scope id (first alias, else `scope_N`). The
    /// sentinels resolve to `any` / `invalid`.
    pub fn name_of(&self, id: ScopeId) -> String {
        if id == SCOPE_ANY {
            return "any".to_string();
        }
        if id == SCOPE_INVALID {
            return "invalid".to_string();
        }
        match self.by_id.get(&id) {
            Some(d) => d.aliases.first().cloned().unwrap_or_else(|| d.name.clone()),
            None => format!("scope_{}", id.0),
        }
    }

    /// Resolve a scope name/alias (`country`, `Special Project`, `any`, `none`) to an id.
    /// `none` maps to `SCOPE_ANY` (F# anyScope semantics: unrestricted).
    pub fn id_of(&self, name: &str) -> Option<ScopeId> {
        let lower = name.trim().to_ascii_lowercase();
        match lower.as_str() {
            "any" | "all" | "none" => Some(SCOPE_ANY),
            "invalid" => Some(SCOPE_INVALID),
            _ => self.by_name.get(&lower).copied(),
        }
    }

    /// True when no config scopes were loaded (callers then fall back / stay lenient).
    pub fn is_empty(&self) -> bool {
        self.by_id.is_empty()
    }

    /// Whether `current` satisfies `target`: the same scope, the wildcard, or a
    /// subscope of it (`is_subscope_of`, walked transitively). E.g. a `character`
    /// scope satisfies a `country` requirement because `Character is_subscope_of
    /// { country }` in scopes.cwt.
    pub fn is_subscope_or_eq(&self, current: ScopeId, target: ScopeId) -> bool {
        if current == target || current == SCOPE_ANY || target == SCOPE_ANY {
            return true;
        }
        let mut stack = vec![current];
        let mut seen = std::collections::HashSet::new();
        while let Some(c) = stack.pop() {
            if c == target {
                return true;
            }
            if !seen.insert(c) {
                continue;
            }
            if let Some(def) = self.by_id.get(&c) {
                stack.extend(def.subscope_of.iter().copied());
            }
        }
        false
    }

    /// Build a registry from the hardcoded `const` scope tables for games that
    /// don't (yet) load their scopes from config — Stellaris/EU4/etc. and unit
    /// tests. HOI4 is config-driven and returns an empty registry here.
    pub fn from_hardcoded(game: Game) -> Self {
        let mut reg = ScopeRegistry::default();
        for def in game.scope_defs() {
            let id = ScopeId(def.id.0);
            reg.by_name.insert(def.name.to_ascii_lowercase(), id);
            for a in def.aliases {
                reg.by_name.insert(a.to_ascii_lowercase(), id);
            }
            reg.by_id.insert(
                id,
                ScopeDefOwned {
                    name: def.name.to_string(),
                    aliases: def.aliases.iter().map(|s| s.to_string()).collect(),
                    subscope_of: def.subscope_of.iter().map(|s| ScopeId(s.0)).collect(),
                },
            );
        }
        crate::scope_engine::load_scope_links(game, &mut reg.links);
        reg
    }
}
