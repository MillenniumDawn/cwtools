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
use std::cell::RefCell;
use std::collections::HashSet;

/// Immutable shared context for one file's validation pass. Holds only borrows,
/// so it is cheap to copy a `&ValidationCtx` into every recursive call.
pub(crate) struct ValidationCtx<'a> {
    pub(crate) ast: &'a ParsedFile,
    pub(crate) ruleset: &'a RuleSet,
    pub(crate) table: &'a StringTable,
    pub(crate) file_path: &'a crate::common::FilePath,
    pub(crate) game: Option<Game>,
    pub(crate) type_index: Option<&'a cwtools_index::TypeIndex>,
    pub(crate) modifier_keys: Option<&'a HashSet<String>>,
    pub(crate) loc_index: Option<&'a LocIndex>,
    /// Extra loc keys to treat as existing, on top of `loc_index` — the LSP's
    /// live overlay of unsaved keys in open `.yml` files, so a key just typed
    /// resolves immediately without waiting for a full rescan (#36). Lowercased,
    /// like the keys the existence checks compare against.
    pub(crate) extra_loc_keys: Option<&'a HashSet<String>>,
    pub(crate) scope_checks: bool,
    pub(crate) var_checks: bool,
    /// Stack of implicit/explicit loop-variable names (normalized) in scope for
    /// the block currently being validated. Loop effects (`for_each_loop`, …)
    /// expose `value`/`index`/`break` temp variables their body can read bare;
    /// entering such a block pushes the names here and leaving truncates them, so
    /// a bare read in the body isn't flagged CW246 without leaking the names to
    /// sibling/parent blocks. The single `ValidationCtx` is shared by `&`, so
    /// this uses interior mutability.
    pub(crate) loop_vars: RefCell<Vec<String>>,
    /// Sink for the project-wide unused check (CW239/CW231): the instances of
    /// reference-tracked types this file uses. `None` on every path that didn't
    /// ask for the tracking, which is all of them but the batch driver's, so the
    /// recording sites cost one branch. Shared by `&` like the rest of the
    /// context, hence the `RefCell`.
    pub(crate) type_uses: Option<&'a RefCell<crate::references::UsedInstances>>,
}

impl ValidationCtx<'_> {
    /// Whether uses of `type_name`'s instances are being recorded this run.
    /// Checked before the affix forms a complex `<type>` reference expands to,
    /// so a run that tracks nothing never builds them.
    pub(crate) fn tracks_type_uses(&self, type_name: &str) -> bool {
        self.type_uses.is_some()
            && crate::references::is_tracked(self.ruleset, self.game, type_name)
    }

    /// Record `instance` as a use of a `type_name` instance.
    pub(crate) fn mark_type_use(&self, type_name: &str, instance: &str) {
        if let Some(sink) = self.type_uses {
            sink.borrow_mut().mark(type_name, instance);
        }
    }

    /// Whether `name`, normalized the same way the variable index is, currently
    /// names a loop-local variable in scope. Normalizes into a reusable
    /// thread-local buffer (like `VarIndex::contains`) instead of allocating a
    /// fresh `String` on every checked variable read.
    pub(crate) fn is_loop_var(&self, name: &str) -> bool {
        thread_local! {
            static NORM_BUF: std::cell::RefCell<String> = const { std::cell::RefCell::new(String::new()) };
        }
        NORM_BUF.with(|buf| {
            let mut buf = buf.borrow_mut();
            cwtools_index::VarIndex::normalize_into(name, &mut buf);
            self.loop_vars.borrow().iter().any(|v| v == buf.as_str())
        })
    }
}
