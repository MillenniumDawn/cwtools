//! Localisation-field checks (CW100/CW122 existence, CW260/CW266 loc commands)
//! and modifier-key set construction.

use cwtools_game::scope_engine::{ScopeContext, ScopeId};
use cwtools_game::scope_registry::ScopeRegistry;
use cwtools_parser::ast::Value;
use cwtools_rules::rules_types::*;

use crate::common::{ValidationError, with_leaf_value_str};
use crate::ctx::ValidationCtx;
use cwtools_error_codes as error_codes;

/// Build the set of valid modifier names for `alias_name[modifier]` slots from
/// the ruleset's `modifiers = { ... }` block. Templated entries like
/// `production_speed_<building>_factor` / `<ideology>_drift` are expanded against
/// the type index, one instance each. Single source of truth so the CLI and LSP
/// agree on what counts as a modifier.
pub fn build_modifier_keys(
    ruleset: &RuleSet,
    type_index: &cwtools_index::TypeIndex,
) -> std::collections::HashSet<String> {
    let mut mk = std::collections::HashSet::new();
    // One expansion buffer for the whole (template x instance) product; only the
    // lowercased result is handed to the set.
    let mut expanded = String::new();
    for (m, _category) in &ruleset.modifiers {
        match (m.find('<'), m.find('>')) {
            (Some(open), Some(close)) if open < close => {
                let tn = &m[open + 1..close];
                let pre = &m[..open];
                let suf = &m[close + 1..];
                for (_uri, inst) in type_index.instances(tn) {
                    expanded.clear();
                    expanded.push_str(pre);
                    expanded.push_str(&inst.name);
                    expanded.push_str(suf);
                    mk.insert(expanded.to_lowercase());
                }
            }
            _ => {
                mk.insert(m.to_lowercase());
            }
        }
    }
    mk
}

/// Validate a `LocalisationField` leaf: that the referenced loc key exists
/// (CW100 / CW122) and, when the scope is known, that the loc string's commands
/// are valid in that scope (CW260 / CW262). Mirrors F# `checkLocKey*` plus the
/// scope-aware loc-command checks.
pub(crate) fn validate_localisation_field(
    ctx: &ValidationCtx,
    leaf: &cwtools_parser::ast::Leaf,
    synced: bool,
    is_inline: bool,
    scope_context: Option<&ScopeContext>,
    errors: &mut Vec<ValidationError>,
) {
    // The meta-localisation block form `{ localization_key = X PARAM = ... }` is
    // accepted unconditionally (its inner key is validated as its own leaf).
    if let Value::Clause(_) = &leaf.value {
        return;
    }
    let was_quoted = matches!(leaf.value, Value::QString(_));
    // Borrow the reference text rather than copying it out of the string table:
    // every loc-bearing field in the corpus comes through here.
    with_leaf_value_str(&leaf.value, ctx.table, |raw| {
        check_loc_key(
            ctx,
            leaf,
            raw.trim_matches('"'),
            was_quoted,
            synced,
            is_inline,
            scope_context,
            errors,
        )
    });
}

/// The body of [`validate_localisation_field`], with the reference text already
/// unquoted and borrowed.
#[allow(clippy::too_many_arguments)]
fn check_loc_key(
    ctx: &ValidationCtx,
    leaf: &cwtools_parser::ast::Leaf,
    key_raw: &str,
    was_quoted: bool,
    synced: bool,
    is_inline: bool,
    scope_context: Option<&ScopeContext>,
    errors: &mut Vec<ValidationError>,
) {
    let file_path = ctx.file_path;
    let game = ctx.game;
    let loc_index = ctx.loc_index;

    // F# skip rules: empty keys, keys with spaces (prose / compound), `[...]`
    // inline command blocks, `$VAR$` scripted references, and `@`-vars are not
    // plain key references and are accepted. The `[...]` command may be embedded
    // with a literal suffix/prefix (e.g. a meta_effect variable
    // `"[?ROOT...GetTokenKey]_subtype"`), so test for brackets anywhere — a real
    // loc key never contains them.
    if key_raw.is_empty()
        || key_raw.contains(' ')
        || (key_raw.contains('[') && key_raw.contains(']'))
        || key_raw.contains('$')
        || key_raw.starts_with('@')
    {
        return;
    }

    // No loc data loaded → accept leniently (e.g. vanilla loc absent).
    let Some(idx) = loc_index else {
        return;
    };
    // Loc keys are ASCII in practice, so fold on the stack; anything else falls
    // back to `to_lowercase` so the Unicode special cases still hold.
    let mut lower_buf: smallvec::SmallVec<[u8; 64]> = smallvec::SmallVec::new();
    let lower_owned;
    let key_lower: &str = if key_raw.is_ascii() {
        lower_buf.extend_from_slice(key_raw.as_bytes());
        lower_buf.make_ascii_lowercase();
        std::str::from_utf8(&lower_buf).unwrap_or_default()
    } else {
        lower_owned = key_raw.to_lowercase();
        &lower_owned
    };
    // A key present in the live overlay (just typed into an open `.yml`, not yet
    // rescanned) counts as existing — keeps live editing from flagging a key the
    // user already added. (#36)
    let in_overlay = ctx.extra_loc_keys.is_some_and(|e| e.contains(key_lower));
    let exists = idx.exists_any(key_lower) || in_overlay;

    let push_missing = |errors: &mut Vec<ValidationError>, lang: &str| {
        let code = &error_codes::CW100_MISSING_LOCALISATION;
        errors.push(
            ValidationError::from_code(
                code,
                file_path,
                leaf.pos.start.line,
                leaf.pos.start.col,
                &[key_raw, lang],
            )
            .with_end(leaf.pos.end),
        );
    };

    if is_inline {
        // F# four-way logic for inline loc keys.
        match (was_quoted, exists) {
            (true, true) if cwtools_parser::parser::is_bare_string_value(key_raw) => {
                let code = &error_codes::CW122_LOC_KEY_IN_INLINE;
                let fix = cwtools_parser::fix::SuggestedFix::replace(
                    "Remove unnecessary quotes",
                    leaf.value_pos,
                    key_raw,
                );
                errors.push(
                    ValidationError::from_code(
                        code,
                        file_path,
                        leaf.pos.start.line,
                        leaf.pos.start.col,
                        &[key_raw],
                    )
                    .with_fix(fix)
                    .with_end(leaf.pos.end),
                );
            }
            (true, _) => {} // Quoted values that cannot be safely unquoted stay quoted.
            (false, true) => {} // unquoted + exists → ok
            (false, false) => push_missing(errors, "any language"),
        }
    } else if synced && !in_overlay {
        // Must exist in every language the project ships loc data for. A key in
        // the live overlay is accepted leniently (no per-language data there).
        for lang in idx.missing_synced_languages(key_lower) {
            push_missing(errors, &lang.to_string());
        }
    } else if !exists {
        push_missing(errors, "any language");
    }

    // Scope-aware loc-command validation at the reference site: validate the
    // referenced loc string's `[command]` chains against the scope of THIS field.
    if exists && let Some(entry) = idx.entry(key_lower) {
        let initial = scope_context
            .map(|c| c.current())
            .unwrap_or(cwtools_game::scope_engine::SCOPE_ANY);
        let data = cwtools_localization::LocScopeData {
            game,
            registry: scope_context.map(|c| c.registry.clone()),
            ..Default::default()
        };
        for diag in cwtools_localization::validate_loc_commands(entry, initial, &data) {
            push_loc_command_diagnostic(
                &diag,
                key_raw,
                leaf,
                file_path,
                scope_context.map(|c| c.registry.as_ref()),
                errors,
            );
        }
    }
}

/// Convert a `LocCommandDiagnostic` (from the loc scope engine) into a
/// `ValidationError` with the matching F# numeric code.
fn push_loc_command_diagnostic(
    diag: &cwtools_localization::LocCommandDiagnostic,
    loc_key: &str,
    leaf: &cwtools_parser::ast::Leaf,
    file_path: &crate::FilePath,
    registry: Option<&ScopeRegistry>,
    errors: &mut Vec<ValidationError>,
) {
    use cwtools_localization::LocCommandDiagnostic as D;
    let scope_name = |id: u32| -> String {
        match registry {
            Some(reg) => reg.name_of(ScopeId(id)),
            None => id.to_string(),
        }
    };
    let (code, message) = match diag {
        D::WrongScope {
            command,
            current_scope,
            expected_scopes,
        } => {
            let expected = expected_scopes
                .iter()
                .map(|s| scope_name(*s))
                .collect::<Vec<_>>()
                .join(", ");
            let code = &error_codes::CW260_LOC_COMMAND_WRONG_SCOPE;
            (
                code,
                code.format(&[command, &scope_name(*current_scope), &expected]),
            )
        }
        D::ChainEndsInScope { command } => {
            let code = &error_codes::CW266_LOC_COMMAND_NOT_IN_DATA_TYPE;
            (code, code.format(&[loc_key, command.as_str(), "scope"]))
        }
        D::NotFound { command } => {
            let code = &error_codes::CW226_INVALID_LOC_COMMAND;
            (code, code.format(&[loc_key, command.as_str()]))
        }
    };
    errors.push(
        ValidationError::from_code_with(
            code,
            code.severity,
            file_path,
            leaf.pos.start.line,
            leaf.pos.start.col,
            message,
        )
        .with_end(leaf.pos.end),
    );
}
