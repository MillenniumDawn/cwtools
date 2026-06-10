pub mod constants;
pub mod docs_parser;
pub mod scope;
pub mod scope_engine;
pub mod scope_registry;

// Re-export the most-used public types at the crate root for convenience.
pub use constants::Game;
pub use docs_parser::{
    ActualModifier, DataTypeDump, DocEntry, DocKind, ModifierDef, RawDoc,
    modifier_definitions_from_docs, parse_data_type_dump, parse_jomini_effects,
    parse_jomini_triggers, parse_legacy_docs, parse_modifier_log, parse_setup_log,
};
pub use scope::{Scope, ScopeDef};
pub use scope_engine::{SCOPE_ANY, SCOPE_INVALID, ScopeId, ScopeLink};
pub use scope_registry::{ScopeDefOwned, ScopeRegistry};
