//! Localisation-field checks (CW100/CW122 existence, CW260/CW266 loc commands)
//! and modifier-key set construction.

use cwtools_game::scope_engine::{ScopeContext, ScopeId};
use cwtools_game::scope_registry::ScopeRegistry;
use cwtools_parser::ast::Value;
use cwtools_rules::rules_types::*;

use crate::common::{ValidationError, leaf_value_to_string};
use crate::ctx::ValidationCtx;
use crate::error_codes;

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
    for m in &ruleset.modifiers {
        match (m.find('<'), m.find('>')) {
            (Some(open), Some(close)) if open < close => {
                let tn = &m[open + 1..close];
                let pre = &m[..open];
                let suf = &m[close + 1..];
                for (_uri, inst) in type_index.instances(tn) {
                    mk.insert(format!("{}{}{}", pre, inst.name, suf));
                }
            }
            _ => {
                mk.insert(m.clone());
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
    let table = ctx.table;
    let file_path = ctx.file_path;
    let game = ctx.game;
    let loc_index = ctx.loc_index;
    // The meta-localisation block form `{ localization_key = X PARAM = ... }` is
    // accepted unconditionally (its inner key is validated as its own leaf).
    if let Value::Clause(_) = &leaf.value {
        return;
    }

    let was_quoted = matches!(leaf.value, Value::QString(_));
    let raw = leaf_value_to_string(&leaf.value, table);
    let key_raw = raw.trim_matches('"');

    // F# skip rules: empty keys, keys with spaces (prose / compound), `[...]`
    // inline command blocks, `$VAR$` scripted references, and `@`-vars are not
    // plain key references and are accepted.
    if key_raw.is_empty()
        || key_raw.contains(' ')
        || (key_raw.starts_with('[') && key_raw.ends_with(']'))
        || key_raw.contains('$')
        || key_raw.starts_with('@')
    {
        return;
    }

    // No loc data loaded → accept leniently (e.g. vanilla loc absent).
    let Some(idx) = loc_index else {
        return;
    };
    let key_lower = key_raw.to_lowercase();
    let exists = idx.exists_any(&key_lower);

    let push_missing = |errors: &mut Vec<ValidationError>, lang: &str| {
        let code = &error_codes::CW100_MISSING_LOCALISATION;
        errors.push(ValidationError::from_code(
            code,
            file_path,
            leaf.pos.start.line,
            leaf.pos.start.col,
            &[key_raw, lang],
        ));
    };

    if is_inline {
        // F# four-way logic for inline loc keys.
        match (was_quoted, exists) {
            (true, true) => {
                let code = &error_codes::CW122_LOC_KEY_IN_INLINE;
                errors.push(ValidationError::from_code(
                    code,
                    file_path,
                    leaf.pos.start.line,
                    leaf.pos.start.col,
                    &[key_raw],
                ));
            }
            (true, false) => {} // quoted + missing → skip (lenient, matches F#)
            (false, true) => {} // unquoted + exists → ok
            (false, false) => push_missing(errors, "any language"),
        }
    } else if synced {
        // Must exist in every language the project ships loc data for.
        for lang in idx.missing_synced_languages(&key_lower) {
            push_missing(errors, &lang.to_string());
        }
    } else if !exists {
        push_missing(errors, "any language");
    }

    // Scope-aware loc-command validation at the reference site: validate the
    // referenced loc string's `[command]` chains against the scope of THIS field.
    if exists && let Some(entry) = idx.entry(&key_lower) {
        let initial = scope_context
            .and_then(|c| c.current())
            .unwrap_or(cwtools_game::scope_engine::SCOPE_ANY);
        let data = cwtools_localization::LocScopeData {
            game: cwtools_localization::Game::from_engine(game),
            registry: scope_context.map(|c| c.registry.clone()),
            ..Default::default()
        };
        for diag in cwtools_localization::validate_loc_commands(entry, initial, &data) {
            push_loc_command_diagnostic(
                &diag,
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
    leaf: &cwtools_parser::ast::Leaf,
    file_path: &str,
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
            (
                code,
                code.format(&[command.as_str(), command.as_str(), "scope"]),
            )
        }
    };
    errors.push(ValidationError {
        message,
        severity: code.severity,
        line: leaf.pos.start.line,
        col: leaf.pos.start.col,
        file: file_path.to_string(),
        code: Some(code.id.to_string()),
    });
}
