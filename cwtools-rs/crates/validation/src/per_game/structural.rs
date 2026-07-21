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
use cwtools_parser::fix::SuggestedFix;
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

/// The reserved keywords' interned ids, resolved once per file so the walk
/// compares token ids instead of doing string-table lookups per block. This
/// walk visits every block of every file and the per-block lookups dominated
/// its cost (~25% of the whole MD validate phase before; integer compares now).
struct Keywords {
    not: StringId,
    if_: StringId,
    else_if: StringId,
    and: StringId,
    or: StringId,
    nor: StringId,
    limit: StringId,
    count_triggers: StringId,
}

impl Keywords {
    fn new(table: &StringTable) -> Self {
        Self {
            not: table.intern("NOT").normal,
            if_: table.intern("if").normal,
            else_if: table.intern("else_if").normal,
            and: table.intern("AND").normal,
            or: table.intern("OR").normal,
            nor: table.intern("NOR").normal,
            limit: table.intern("limit").normal,
            count_triggers: table.intern("count_triggers").normal,
        }
    }
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
                if l.key.normal != kw.limit {
                    return false;
                }
            }
            Child::LeafValue(_) => return false,
            Child::Comment(_) => {}
        }
    }
    true
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
    for child in children {
        let Some(block) = as_block(child, ast) else {
            continue;
        };
        let key = block.key;

        // CW223 — NOT with more than one child. The remediation differs by game
        // (HOI4 has no NOR/NAND triggers), so the message is chosen by the caller.
        if key == kw.not && non_comment_count(block.children) > 1 {
            push(
                errors,
                &error_codes::CW223_INCORRECT_NOT_USAGE,
                cw223_msg.to_string(),
                block.range,
                file_path,
            );
        }

        // CW121 — empty if/else_if. Fix: delete the empty block.
        if (key == kw.if_ || key == kw.else_if) && is_empty_if(block.children, ast, kw) {
            push_fix(
                errors,
                &error_codes::CW121_EMPTY_IF,
                error_codes::CW121_EMPTY_IF.message_template.to_string(),
                block.range,
                file_path,
                SuggestedFix::delete("Remove empty if", block.range),
            );
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
        let state = if key == kw.and {
            if parent == BoolState::And {
                push(
                    errors,
                    &error_codes::CW251_UNNECESSARY_BOOLEAN,
                    error_codes::CW251_UNNECESSARY_BOOLEAN.format(&["AND"]),
                    block.range,
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
                    block.range,
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

    // Task 18: a real emit carries the offending block's own SourceRange end, so
    // the LSP can publish a precise squiggle. Locks the population wiring: the end
    // must equal the NOT block's `pos.end`, not a re-derived or absent position.
    #[test]
    fn diagnostic_carries_block_range_end() {
        let src = "x = {\n    NOT = { a = 1 b = 2 }\n}\n";
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
        assert_eq!(
            err.end,
            Some((not_block.range.end.line, not_block.range.end.col)),
            "CW223 must carry the NOT block's exclusive range end"
        );
    }

    #[test]
    fn cw121_fix_deletes_empty_if() {
        assert_fix("CW121", "x = { if = { } }\n", "x = { }\n");
    }

    #[test]
    fn cw281_fix_deletes_empty_limit() {
        assert_fix("CW281", "x = { limit = { } }\n", "x = { }\n");
    }
}
