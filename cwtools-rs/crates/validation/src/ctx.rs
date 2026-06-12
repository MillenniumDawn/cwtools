//! Shared immutable validation context.
//!
//! The rule-vs-AST walkers all thread the same bag of per-file context (the
//! parsed AST, the ruleset, the string table, the game, and the optional
//! type/modifier/loc indexes). Bundling it into one borrow struct keeps the
//! recursive signatures small: each call passes `&ValidationCtx` plus only the
//! genuinely per-call varying args (the current node/rules, the mutable
//! `scope_context`, and the `errors` sink).

use cwtools_game::constants::Game;
use cwtools_localization::LocIndex;
use cwtools_parser::ast::ParsedFile;
use cwtools_rules::rules_types::RuleSet;
use cwtools_string_table::string_table::StringTable;
use std::collections::HashSet;

/// Immutable shared context for one file's validation pass. Holds only borrows,
/// so it is cheap to copy a `&ValidationCtx` into every recursive call.
pub(crate) struct ValidationCtx<'a> {
    pub(crate) ast: &'a ParsedFile,
    pub(crate) ruleset: &'a RuleSet,
    pub(crate) table: &'a StringTable,
    pub(crate) file_path: &'a str,
    pub(crate) game: Option<Game>,
    pub(crate) type_index: Option<&'a cwtools_index::TypeIndex>,
    pub(crate) modifier_keys: Option<&'a HashSet<String>>,
    pub(crate) loc_index: Option<&'a LocIndex>,
    pub(crate) scope_checks: bool,
    pub(crate) var_checks: bool,
}
