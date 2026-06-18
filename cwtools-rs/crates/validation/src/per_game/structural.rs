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
            Child::LeafValue(_) | Child::ValueClause(_) => return false,
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
    errors.push(ValidationError {
        message: msg,
        severity: code.severity,
        line: r.start.line,
        col: r.start.col,
        file: file.to_string(),
        code: Some(code.id.to_string()),
    });
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

        // CW121 — empty if/else_if.
        if (key == kw.if_ || key == kw.else_if) && is_empty_if(block.children, ast, kw) {
            push(
                errors,
                &error_codes::CW121_EMPTY_IF,
                error_codes::CW121_EMPTY_IF.message_template.to_string(),
                block.range,
                file_path,
            );
        }

        // CW281 — a `limit = { }` with no trigger conditions.
        if key == kw.limit && non_comment_count(block.children) == 0 {
            push(
                errors,
                &error_codes::CW281_EMPTY_LIMIT,
                error_codes::CW281_EMPTY_LIMIT.message_template.to_string(),
                block.range,
                file_path,
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
