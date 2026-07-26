//! Cross-game structural (boolean/syntax) hints.
//!
//! Ported from F# `CWTools/Validation/Common/CommonValidation.fs`:
//! - `validateNOTMultiple`      -> CW223 (NOT with multiple children)
//! - `validateIfWithNoEffect`   -> CW121 (empty if/else_if)
//! - `validateRedundantANDWithNOR` -> CW251 (AND-in-AND / OR-in-OR)
//!
//! F# scopes these to the rules engine's classified effect/trigger blocks. This
//! parser has no such classification, so the walk instead keys off the reserved
//! logic keywords (`NOT`/`AND`/`OR`/`NOR`/`if`/`else_if`), which only appear in
//! trigger/effect script — running it file-wide matches F# in practice.

use super::common::as_block;
use crate::{ValidationError, error_codes};
use cwtools_game::constants::Game;
use cwtools_parser::ast::{Child, ParsedFile, SourceRange, Value};
use cwtools_parser::fix::{SuggestedFix, key_token_range};
use cwtools_string_table::string_table::{StringId, StringTable};

/// The implicit boolean context a node sits in, mirroring F#'s `BoolState`.
#[derive(Clone, Copy, PartialEq)]
enum BoolState {
    And,
    Or,
    /// Inside a `NOT`: neither an explicit `AND` nor `OR` is redundant here.
    /// `NOT = { a b }` means "none true", so `NOT = { AND = {…} }` (not-all) and
    /// `NOT = { OR = {…} }` (none, the standard HOI4 idiom) are both meaningful.
    Neutral,
}

/// The reserved keywords' interned *lowercase* ids, resolved once per file so
/// the walk compares token ids instead of doing string-table lookups per block.
/// This walk visits every block of every file and the per-block lookups
/// dominated its cost (~25% of the whole MD validate phase before; integer
/// compares now). Paradox script keys are case-insensitive, so every comparison
/// is against a block's `key_lower` — `NOT`/`not`/`Not` are one keyword.
struct Keywords {
    not: StringId,
    if_: StringId,
    else_if: StringId,
    else_: StringId,
    and: StringId,
    or: StringId,
    nor: StringId,
    limit: StringId,
    count_triggers: StringId,
    value: StringId,
}

impl Keywords {
    fn new(table: &StringTable) -> Self {
        Self {
            not: table.intern("not").lower,
            if_: table.intern("if").lower,
            else_if: table.intern("else_if").lower,
            else_: table.intern("else").lower,
            and: table.intern("and").lower,
            or: table.intern("or").lower,
            nor: table.intern("nor").lower,
            limit: table.intern("limit").lower,
            count_triggers: table.intern("count_triggers").lower,
            value: table.intern("value").lower,
        }
    }
}

/// Whether a key is one of the boolean operators the checks below reason about.
fn is_bool_operator(key: StringId, kw: &Keywords) -> bool {
    key == kw.not || key == kw.and || key == kw.or || key == kw.nor
}

/// Whether an operator block is a dynamic-value (math) expression rather than a
/// boolean one. HOI4 reuses `and`/`or`/`not` as *value* operators inside
/// `check_expr`, `set_variable` and friends — `and = { value = x less_than = 3 }`
/// combines two computed values and is not the `AND` trigger this walk reasons
/// about. A direct `value = …` child tells the two apart without a rules lookup.
fn is_math_expression(children: &[Child], ast: &ParsedFile, kw: &Keywords) -> bool {
    children.iter().any(|c| match c {
        Child::Leaf(idx) => ast.arena.leaves[*idx as usize].key.lower == kw.value,
        _ => false,
    })
}

/// Number of children that are not comments.
fn non_comment_count(children: &[Child]) -> usize {
    children
        .iter()
        .filter(|c| !matches!(c, Child::Comment(_)))
        .count()
}

/// F# `validateIfWithNoEffect`: an `if`/`else_if` with no leaf assignments and
/// no block children other than `limit`.
fn is_empty_if(children: &[Child], ast: &ParsedFile, kw: &Keywords) -> bool {
    for child in children {
        match child {
            // A bare `key = value` leaf counts as an effect -> not empty.
            Child::Leaf(idx) => {
                let l = &ast.arena.leaves[*idx as usize];
                if !matches!(l.value, Value::Clause(_)) {
                    return false;
                }
                // A `key = { ... }` leaf-clause: only `limit` is allowed.
                if l.key.lower != kw.limit {
                    return false;
                }
            }
            Child::LeafValue(_) => return false,
            Child::Comment(_) => {}
        }
    }
    true
}

/// Deleting a block an `else_if`/`else` hangs off leaves the follower with no
/// antecedent, which the game rejects.
fn chain_follows(children: &[Child], idx: usize, ast: &ParsedFile, kw: &Keywords) -> bool {
    for child in &children[idx + 1..] {
        match child {
            Child::Comment(_) => {}
            Child::Leaf(i) => {
                let key = ast.arena.leaves[*i as usize].key.lower;
                return key == kw.else_if || key == kw.else_;
            }
            Child::LeafValue(_) => return false,
        }
    }
    false
}

fn push(
    errors: &mut Vec<ValidationError>,
    code: &error_codes::ErrorCode,
    msg: String,
    r: SourceRange,
    file: &str,
) {
    errors.push(
        ValidationError::from_code_with(code, code.severity, file, r.start.line, r.start.col, msg)
            .with_end(r.end),
    );
}

/// As [`push`], but carries a fix. Used by the delete-the-empty-block hints
/// (CW121/CW281) whose block range is the deletion span.
fn push_fix(
    errors: &mut Vec<ValidationError>,
    code: &error_codes::ErrorCode,
    msg: String,
    r: SourceRange,
    file: &str,
    fix: SuggestedFix,
) {
    errors.push(
        ValidationError::from_code_with(code, code.severity, file, r.start.line, r.start.col, msg)
            .with_fix(fix)
            .with_end(r.end),
    );
}

#[allow(clippy::too_many_arguments)]
fn walk(
    children: &[Child],
    ast: &ParsedFile,
    kw: &Keywords,
    file_path: &str,
    parent: BoolState,
    cw223_msg: &str,
    errors: &mut Vec<ValidationError>,
) {
    for (idx, child) in children.iter().enumerate() {
        let Some(block) = as_block(child, ast) else {
            continue;
        };
        let key = block.key_lower;

        // Value arithmetic, not boolean logic: none of the checks below apply to
        // it, and its children are operands, so descend with a neutral context.
        if is_bool_operator(key, kw) && is_math_expression(block.children, ast, kw) {
            walk(
                block.children,
                ast,
                kw,
                file_path,
                BoolState::Neutral,
                cw223_msg,
                errors,
            );
            continue;
        }

        // CW223 — NOT with more than one child. The remediation differs by game
        // (HOI4 has no NOR/NAND triggers), so the message is chosen by the caller.
        // A quoted key interns with its quotes and can never match `kw.not`, so
        // the source token is always exactly `NOT`, 3 chars.
        if key == kw.not && non_comment_count(block.children) > 1 {
            push(
                errors,
                &error_codes::CW223_INCORRECT_NOT_USAGE,
                cw223_msg.to_string(),
                key_token_range(block.range.start, 3),
                file_path,
            );
        }

        // CW121 — empty if/else_if. Fix: delete the empty block.
        if (key == kw.if_ || key == kw.else_if) && is_empty_if(block.children, ast, kw) {
            let msg = error_codes::CW121_EMPTY_IF.message_template.to_string();
            if chain_follows(children, idx, ast, kw) {
                push(
                    errors,
                    &error_codes::CW121_EMPTY_IF,
                    msg,
                    block.range,
                    file_path,
                );
            } else {
                push_fix(
                    errors,
                    &error_codes::CW121_EMPTY_IF,
                    msg,
                    block.range,
                    file_path,
                    SuggestedFix::delete("Remove empty if", block.range),
                );
            }
        }

        // CW281 — a `limit = { }` with no trigger conditions. Fix: delete it.
        if key == kw.limit && non_comment_count(block.children) == 0 {
            push_fix(
                errors,
                &error_codes::CW281_EMPTY_LIMIT,
                error_codes::CW281_EMPTY_LIMIT.message_template.to_string(),
                block.range,
                file_path,
                SuggestedFix::delete("Remove empty limit", block.range),
            );
        }

        // CW251 — redundant boolean nesting; also compute the child context.
        // Advice about the operator keyword, so the range covers it alone; see
        // the CW223 note above on why the source token length is known.
        let state = if key == kw.and {
            if parent == BoolState::And {
                push(
                    errors,
                    &error_codes::CW251_UNNECESSARY_BOOLEAN,
                    error_codes::CW251_UNNECESSARY_BOOLEAN.format(&["AND"]),
                    key_token_range(block.range.start, 3),
                    file_path,
                );
            }
            BoolState::And
        } else if key == kw.or {
            if parent == BoolState::Or {
                push(
                    errors,
                    &error_codes::CW251_UNNECESSARY_BOOLEAN,
                    error_codes::CW251_UNNECESSARY_BOOLEAN.format(&["OR"]),
                    key_token_range(block.range.start, 2),
                    file_path,
                );
            }
            BoolState::Or
        } else if key == kw.nor {
            // NOR puts its children in an Or context (an OR directly inside is
            // redundant), and never pushes CW251 itself. Matches F#.
            BoolState::Or
        } else if key == kw.not {
            // NOT is a neutral context: HOI4 `NOT = { a b }` means "none true",
            // so a wrapping AND (not-all) or OR (none, the common HOI4 idiom)
            // both change/clarify intent and must not flag CW251.
            BoolState::Neutral
        } else if key == kw.count_triggers {
            // count_triggers counts how many direct children are true, so its
            // children are independent (not implicitly ANDed). An AND that groups
            // several into one counted unit is meaningful, not redundant.
            BoolState::Neutral
        } else {
            BoolState::And
        };

        walk(block.children, ast, kw, file_path, state, cw223_msg, errors);
    }
}

/// Run the cross-game structural hints over a whole file.
pub fn validate_structural(
    ast: &ParsedFile,
    table: &StringTable,
    file_path: &str,
    game: Game,
    errors: &mut Vec<ValidationError>,
) {
    // HOI4 has no NOR/NAND triggers, so the default CW223 advice is invalid there.
    let cw223_msg = match game {
        Game::Hoi4 => error_codes::CW223_INCORRECT_NOT_USAGE_HOI4_MSG,
        _ => error_codes::CW223_INCORRECT_NOT_USAGE.message_template,
    };
    let kw = Keywords::new(table);
    walk(
        &ast.root_children,
        ast,
        &kw,
        file_path,
        BoolState::And,
        cw223_msg,
        errors,
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use cwtools_parser::fix::apply_edits;
    use cwtools_parser::parser::parse_string;

    /// The codes emitted for `src`, in emit order.
    fn codes(src: &str) -> Vec<&'static str> {
        let table = StringTable::new();
        let ast = parse_string(src, &table).unwrap();
        let mut errors = Vec::new();
        validate_structural(&ast, &table, "test.txt", Game::Hoi4, &mut errors);
        errors.iter().filter_map(|e| e.code).collect()
    }

    /// Validate `src`, apply the fix on the first diagnostic with `code`, and
    /// assert the result equals `expected` and no longer emits `code`.
    fn assert_fix(code: &str, src: &str, expected: &str) {
        let table = StringTable::new();
        let ast = parse_string(src, &table).unwrap();
        let mut errors = Vec::new();
        validate_structural(&ast, &table, "test.txt", Game::Hoi4, &mut errors);

        let err = errors
            .iter()
            .find(|e| e.code == Some(code))
            .unwrap_or_else(|| panic!("{code} emitted for {src:?}, got {errors:?}"));
        let fix = err.fix.as_ref().expect("diagnostic carries a fix");
        let fixed = apply_edits(src, &fix.edits);
        assert_eq!(fixed, expected, "{code} fix output");

        let ast2 = parse_string(&fixed, &table).unwrap();
        let mut errors2 = Vec::new();
        validate_structural(&ast2, &table, "test.txt", Game::Hoi4, &mut errors2);
        assert!(
            !errors2.iter().any(|e| e.code == Some(code)),
            "{code} must be gone after applying the fix"
        );
    }

    // Issue #107: carrying the block's own range buried every line of the body
    // under one squiggle.
    #[test]
    fn cw223_underlines_only_the_not_key() {
        let src = "x = {\n    NOT = {\n        a = 1\n        b = 2\n    }\n}\n";
        let table = StringTable::new();
        let ast = parse_string(src, &table).unwrap();
        let mut errors = Vec::new();
        validate_structural(&ast, &table, "test.txt", Game::Hoi4, &mut errors);

        let err = errors
            .iter()
            .find(|e| e.code == Some("CW223"))
            .expect("CW223 emitted");

        // Recover the NOT block's range from the AST and compare.
        let x_block = as_block(&ast.root_children[0], &ast).expect("x is a block");
        let not_block = x_block
            .children
            .iter()
            .find_map(|c| as_block(c, &ast))
            .expect("NOT block present");
        assert_eq!(err.line, not_block.range.start.line);
        assert_eq!(err.col, not_block.range.start.col);

        let (end_line, end_col) = err.end.expect("CW223 carries an end");
        assert_eq!(
            end_line, not_block.range.start.line,
            "stays on the NOT line"
        );
        assert_eq!(
            end_col,
            not_block.range.start.col + 3,
            "spans `NOT` and nothing else"
        );
        assert!(
            end_line < not_block.range.end.line,
            "must not reach the block body"
        );
    }

    // Same shape as CW223: advice about the operator keyword, so spanning the
    // block buried the body under a squiggle.
    #[test]
    fn cw251_underlines_only_the_operator_key() {
        // The root context is already AND, so the outer AND is the redundant one.
        let and_src = "AND = {\n    tag = GER\n    has_war = no\n}\n";
        // Inside an OR, the nested OR is the redundant one.
        let or_src = "OR = {\n    OR = {\n        tag = GER\n        tag = FRA\n    }\n}\n";

        for (src, line, col, len) in [(and_src, 1, 0, 3), (or_src, 2, 4, 2)] {
            let table = StringTable::new();
            let ast = parse_string(src, &table).unwrap();
            let mut errors = Vec::new();
            validate_structural(&ast, &table, "test.txt", Game::Hoi4, &mut errors);

            let err = errors
                .iter()
                .find(|e| e.code == Some("CW251"))
                .unwrap_or_else(|| panic!("CW251 emitted for {src:?}, got {errors:?}"));
            assert_eq!((err.line, err.col), (line, col), "{src:?}");
            assert_eq!(
                err.end,
                Some((line, col + len)),
                "CW251 must span only the operator key in {src:?}"
            );
        }
    }

    #[test]
    fn cw121_fix_deletes_empty_if() {
        assert_fix("CW121", "x = { if = { } }\n", "x = { }\n");
    }

    // The diagnostic still reports; only the chain-breaking edit is withheld.
    #[test]
    fn cw121_offers_no_fix_when_a_chain_follows() {
        for src in [
            "x = { if = { } else_if = { a = 1 } }\n",
            "x = { if = { } else = { a = 1 } }\n",
            "x = { if = { a = 1 } else_if = { } else = { b = 2 } }\n",
        ] {
            let table = StringTable::new();
            let ast = parse_string(src, &table).unwrap();
            let mut errors = Vec::new();
            validate_structural(&ast, &table, "test.txt", Game::Hoi4, &mut errors);

            let err = errors
                .iter()
                .find(|e| e.code == Some("CW121"))
                .unwrap_or_else(|| panic!("CW121 emitted for {src:?}, got {errors:?}"));
            assert!(
                err.fix.is_none(),
                "CW121 must not offer a chain-breaking delete for {src:?}"
            );
        }
    }

    #[test]
    fn cw121_still_fixes_a_trailing_empty_else_if() {
        assert_fix(
            "CW121",
            "x = { if = { a = 1 } else_if = { } }\n",
            "x = { if = { a = 1 } }\n",
        );
    }

    #[test]
    fn cw281_fix_deletes_empty_limit() {
        assert_fix("CW281", "x = { limit = { } }\n", "x = { }\n");
    }

    // Paradox script keys are case-insensitive, so every spelling of a reserved
    // logic keyword must reach the same check.

    #[test]
    fn not_flagged_in_every_casing() {
        for key in ["NOT", "not", "Not"] {
            let src = format!("x = {{ {key} = {{ has_war = yes tag = GER }} }}\n");
            assert_eq!(codes(&src), ["CW223"], "{key}");
        }
    }

    #[test]
    fn empty_if_flagged_in_every_casing() {
        for key in ["if", "IF", "If", "else_if", "ELSE_IF", "Else_if"] {
            let src = format!("x = {{ {key} = {{ }} }}\n");
            assert_eq!(codes(&src), ["CW121"], "{key}");
        }
    }

    #[test]
    fn empty_limit_flagged_in_every_casing() {
        for key in ["limit", "LIMIT", "Limit"] {
            let src = format!("x = {{ {key} = {{ }} }}\n");
            assert_eq!(codes(&src), ["CW281"], "{key}");
        }
    }

    #[test]
    fn if_with_only_a_limit_is_empty_in_every_casing() {
        // The limit doesn't count as an effect, so the `if` is still empty.
        let src = "x = { IF = { LIMIT = { tag = GER } } }\n";
        assert_eq!(codes(src), ["CW121"]);
    }

    #[test]
    fn redundant_and_flagged_in_every_casing() {
        for key in ["AND", "and", "And"] {
            // The root context is already AND, so a top-level AND is redundant.
            let src = format!("{key} = {{ tag = GER }}\n");
            assert_eq!(codes(&src), ["CW251"], "{key}");
        }
    }

    #[test]
    fn redundant_or_flagged_in_every_casing() {
        for (outer, inner) in [("OR", "OR"), ("or", "OR"), ("OR", "or"), ("or", "or")] {
            let src = format!("{outer} = {{ {inner} = {{ tag = GER }} }}\n");
            assert_eq!(codes(&src), ["CW251"], "{outer} / {inner}");
        }
    }

    #[test]
    fn nor_opens_an_or_context_in_every_casing() {
        for key in ["NOR", "nor"] {
            // NOR is never redundant itself, but an OR directly inside it is.
            let src = format!("{key} = {{ OR = {{ tag = GER }} }}\n");
            assert_eq!(codes(&src), ["CW251"], "{key}");
        }
    }

    // Regression: a lowercase `or` fell through to the default AND context, so
    // the AND grouping inside it wrongly read as redundant (false CW251).
    #[test]
    fn and_inside_or_is_not_redundant_in_every_casing() {
        for key in ["OR", "or"] {
            let src = format!(
                "{key} = {{ AND = {{ has_war = yes tag = GER }} has_capitulated = yes }}\n"
            );
            assert!(codes(&src).is_empty(), "{key}: {:?}", codes(&src));
        }
    }

    #[test]
    fn and_inside_not_is_not_redundant_in_every_casing() {
        for key in ["NOT", "not"] {
            let src = format!("{key} = {{ AND = {{ has_war = yes tag = GER }} }}\n");
            assert!(codes(&src).is_empty(), "{key}: {:?}", codes(&src));
        }
    }

    // HOI4's dynamic-value syntax reuses the operator names on values, where the
    // boolean rules don't hold: `and = { value = … }` inside a `check_expr` is
    // arithmetic, not a redundant AND.
    #[test]
    fn math_expression_operators_are_not_boolean() {
        let src = "x = { check_expr = {\n\
                   value = { value = global.num_days mod = 365 greater_than = 90 }\n\
                   and = { value = global.num_days mod = 365 less_than = 300 }\n\
                   } }\n";
        assert!(codes(src).is_empty(), "{:?}", codes(src));
    }

    #[test]
    fn math_expression_not_is_not_a_trigger() {
        let src = "x = { not = { value = current_level equals = 3 } }\n";
        assert!(codes(src).is_empty(), "{:?}", codes(src));
    }

    #[test]
    fn triggers_below_a_math_block_are_still_checked() {
        // The `limit` of a math `if` holds real triggers, so the walk must keep
        // descending rather than write off the whole subtree.
        let src = "x = { set_temp_variable = { v = { value = 0\n\
                   if = { limit = { NOT = { has_war = yes tag = GER } } add = 1 }\n\
                   } } }\n";
        assert_eq!(codes(src), ["CW223"]);
    }

    #[test]
    fn count_triggers_is_neutral_in_every_casing() {
        for key in ["count_triggers", "COUNT_TRIGGERS"] {
            let src = format!("{key} = {{ amount = 2 AND = {{ has_war = yes tag = GER }} }}\n");
            assert!(codes(&src).is_empty(), "{key}: {:?}", codes(&src));
        }
    }
}
