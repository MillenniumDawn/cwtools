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
//!
//! Note: this parser stores `key = { ... }` as a `Node` OR as a `Leaf` whose
//! value is a `Clause`, so [`as_block`] normalises both.

use crate::{ValidationError, error_codes};
use cwtools_parser::ast::{Child, ParsedFile, SourceRange, Value};
use cwtools_string_table::string_table::{StringId, StringTable};

/// The implicit boolean context a node sits in, mirroring F#'s `BoolState`.
#[derive(Clone, Copy, PartialEq)]
enum BoolState {
    And,
    Or,
}

/// A reserved boolean keyword that opens a block (`AND`/`OR`/`NOR`).
#[derive(Clone, Copy, PartialEq)]
enum BoolKeyword {
    And,
    Or,
    Nor,
}

impl BoolKeyword {
    fn from_str(s: &str) -> Option<Self> {
        match s {
            "AND" => Some(Self::And),
            "OR" => Some(Self::Or),
            "NOR" => Some(Self::Nor),
            _ => None,
        }
    }
}

/// A `key = { ... }` block, normalised from either a `Node` or a `Leaf`-with-`Clause`.
/// The key is kept as a `StringId` so the per-block keyword checks compare the
/// borrowed text (via [`StringTable::with_string`]) without an owned `String`.
struct Block<'a> {
    key: StringId,
    children: &'a [Child],
    range: SourceRange,
}

impl Block<'_> {
    /// True if the block's key equals `kw` exactly (case-sensitive, matching the
    /// reserved keyword spellings `NOT`/`AND`/`OR`/`if`/...).
    fn key_is(&self, table: &StringTable, kw: &str) -> bool {
        table.with_string(self.key, |s| s == kw).unwrap_or(false)
    }
}

fn as_block<'a>(child: &Child, ast: &'a ParsedFile) -> Option<Block<'a>> {
    match child {
        Child::Node(idx) => {
            let n = &ast.arena.nodes[*idx as usize];
            Some(Block {
                key: n.key.normal,
                children: &n.children,
                range: n.pos,
            })
        }
        Child::Leaf(idx) => {
            let l = &ast.arena.leaves[*idx as usize];
            if let Value::Clause(children) = &l.value {
                Some(Block {
                    key: l.key.normal,
                    children,
                    range: l.pos,
                })
            } else {
                None
            }
        }
        _ => None,
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
fn is_empty_if(children: &[Child], ast: &ParsedFile, table: &StringTable) -> bool {
    for child in children {
        match child {
            // A bare `key = value` leaf counts as an effect -> not empty.
            Child::Leaf(idx) => {
                let l = &ast.arena.leaves[*idx as usize];
                if !matches!(l.value, Value::Clause(_)) {
                    return false;
                }
                // A `key = { ... }` leaf-clause: only `limit` is allowed.
                if !table.with_string(l.key.normal, |s| s == "limit").unwrap_or(false) {
                    return false;
                }
            }
            Child::Node(idx) => {
                let n = &ast.arena.nodes[*idx as usize];
                if !table.with_string(n.key.normal, |s| s == "limit").unwrap_or(false) {
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

fn walk(
    children: &[Child],
    ast: &ParsedFile,
    table: &StringTable,
    file_path: &str,
    parent: BoolState,
    errors: &mut Vec<ValidationError>,
) {
    for child in children {
        let Some(block) = as_block(child, ast) else {
            continue;
        };

        // CW223 — NOT with more than one child.
        if block.key_is(table, "NOT") && non_comment_count(block.children) > 1 {
            push(
                errors,
                &error_codes::CW223_INCORRECT_NOT_USAGE,
                error_codes::CW223_INCORRECT_NOT_USAGE
                    .message_template
                    .to_string(),
                block.range,
                file_path,
            );
        }

        // CW121 — empty if/else_if.
        if (block.key_is(table, "if") || block.key_is(table, "else_if"))
            && is_empty_if(block.children, ast, table)
        {
            push(
                errors,
                &error_codes::CW121_EMPTY_IF,
                error_codes::CW121_EMPTY_IF.message_template.to_string(),
                block.range,
                file_path,
            );
        }

        // CW251 — redundant boolean nesting; also compute the child context.
        // Resolve the key once into the boolean keyword (if any), comparing the
        // borrowed text rather than cloning it.
        let kw = table
            .with_string(block.key, BoolKeyword::from_str)
            .flatten();
        let state = match (parent, kw) {
            (BoolState::And, Some(BoolKeyword::And)) => {
                push(
                    errors,
                    &error_codes::CW251_UNNECESSARY_BOOLEAN,
                    error_codes::CW251_UNNECESSARY_BOOLEAN.format(&["AND"]),
                    block.range,
                    file_path,
                );
                BoolState::And
            }
            (BoolState::Or, Some(BoolKeyword::Or)) => {
                push(
                    errors,
                    &error_codes::CW251_UNNECESSARY_BOOLEAN,
                    error_codes::CW251_UNNECESSARY_BOOLEAN.format(&["OR"]),
                    block.range,
                    file_path,
                );
                BoolState::Or
            }
            // OR and NOR both put their children in an Or context (NOR never
            // pushes CW251, matching F#).
            (_, Some(BoolKeyword::Or)) | (_, Some(BoolKeyword::Nor)) => BoolState::Or,
            _ => BoolState::And,
        };

        walk(block.children, ast, table, file_path, state, errors);
    }
}

/// Run the cross-game structural hints over a whole file.
pub fn validate_structural(
    ast: &ParsedFile,
    table: &StringTable,
    file_path: &str,
    errors: &mut Vec<ValidationError>,
) {
    walk(
        &ast.root_children,
        ast,
        table,
        file_path,
        BoolState::And,
        errors,
    );
}
