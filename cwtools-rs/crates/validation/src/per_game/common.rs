use crate::{ValidationError, error_codes};
use cwtools_parser::ast::{Child, ParsedFile, SourceRange, Value};
use cwtools_parser::fix::key_token_range;
use cwtools_rules::rules_types::{RuleSet, TypeDefinition};
use cwtools_string_table::string_table::{StringId, StringTable};
use rustc_hash::FxHashMap;

/// True when any directory segment of `file_path` equals `segment`
/// (case-insensitive). Mods sometimes nest `events/` into subfolders.
pub(crate) fn under_dir_segment(file_path: &str, segment: &str) -> bool {
    let norm = file_path.replace('\\', "/");
    norm.rsplit_once('/')
        .is_some_and(|(dir, _)| dir.split('/').any(|s| s.eq_ignore_ascii_case(segment)))
}

/// Whether a child's key matches `expected` (case-insensitive).
pub(crate) fn child_key_eq(
    child: &Child,
    ast: &ParsedFile,
    table: &StringTable,
    expected: &str,
) -> bool {
    match child {
        Child::Leaf(idx) => {
            let leaf = &ast.arena.leaves[*idx as usize];
            table
                .with_string(leaf.key.normal, |k| k.eq_ignore_ascii_case(expected))
                .unwrap_or(false)
        }
        _ => false,
    }
}

/// Whether a child is a block containing an `always = no` leaf.
pub(crate) fn child_is_always_no(child: &Child, ast: &ParsedFile, table: &StringTable) -> bool {
    as_block(child, ast).is_some_and(|block| {
        block.children.iter().any(|c| {
            if !child_key_eq(c, ast, table, "always") {
                return false;
            }
            let Child::Leaf(idx) = c else { return false };
            match &ast.arena.leaves[*idx as usize].value {
                Value::Bool(b) => !*b,
                Value::String(t) | Value::QString(t) => table
                    .with_string(t.normal, |s| s.eq_ignore_ascii_case("no"))
                    .unwrap_or(false),
                _ => false,
            }
        })
    })
}

/// A `key = { ... }` block (a `Leaf` whose value is a `Clause`), normalised so
/// the per-game structural walkers share one `Value::Clause` extraction. The key
/// is kept as a lowercased `StringId` so callers that only compare it avoid an
/// owned `String`, and so comparisons are case-insensitive like the game.
pub(crate) struct Block<'a> {
    pub key_lower: StringId,
    pub children: &'a [Child],
    pub range: SourceRange,
}

impl Block<'_> {
    /// The block's key lowercased, for case-insensitive Paradox key dispatch.
    pub fn key_string_lower(&self, table: &StringTable) -> String {
        table.get_string(self.key_lower).unwrap_or_default()
    }
}

/// Character length of an interned key, for the `key_token_range` spans the
/// structural hints squiggle with. Borrowed rather than materialised: the
/// walkers only ever need the count.
pub(crate) fn key_len(table: &StringTable, key: StringId) -> usize {
    table.with_string(key, |k| k.chars().count()).unwrap_or(0)
}

/// Normalise a `key = { ... }` child (a Leaf with a Clause value) into a
/// [`Block`]. Returns `None` for leaves whose value isn't a clause, and for
/// comments / bare values.
pub(crate) fn as_block<'a>(child: &Child, ast: &'a ParsedFile) -> Option<Block<'a>> {
    match child {
        Child::Leaf(idx) => {
            let l = &ast.arena.leaves[*idx as usize];
            if let Value::Clause(children) = &l.value {
                Some(Block {
                    key_lower: l.key.lower,
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

/// Depth-first pre-order walk over every `key = { ... }` block under
/// `children`, calling `f` on each block before descending into it. Shared
/// skeleton for the stateless per-game walkers; walkers that thread state down
/// the recursion (structural's CW223 fold) keep their own.
pub(crate) fn walk_blocks(children: &[Child], ast: &ParsedFile, f: &mut impl FnMut(&Block<'_>)) {
    for child in children {
        let Some(block) = as_block(child, ast) else {
            continue;
        };
        f(&block);
        walk_blocks(block.children, ast, f);
    }
}

/// Validate common features across all games.
pub fn validate_common(
    ast: &ParsedFile,
    ruleset: &RuleSet,
    table: &StringTable,
    file_path: &crate::FilePath,
    errors: &mut Vec<ValidationError>,
) {
    // Root keys repeat (a file is many instances of one type), so the owned key
    // is only materialised the first time each distinct one is seen.
    let mut type_counts: FxHashMap<StringId, usize> = FxHashMap::default();

    for child in &ast.root_children {
        let (leaf, key_id) = match child {
            Child::Leaf(idx) => {
                let leaf = &ast.arena.leaves[*idx as usize];
                (leaf, leaf.key.normal)
            }
            Child::LeafValue(_) | Child::Comment(_) => continue,
        };
        let count = type_counts.entry(key_id).or_insert(0);
        *count += 1;
        let count = *count;

        // Check if this type is defined with unique=true, and emit exactly once,
        // at the second occurrence, so the error anchors at the duplicate rather
        // than at 0,0.
        let unique_dup = count == 2
            && table
                .with_string(key_id, |k| {
                    find_matching_type(k, ruleset).is_some_and(|td| td.unique)
                })
                .unwrap_or(false);
        if unique_dup {
            let key = table.get_string(key_id).unwrap_or_default();
            // The complaint is the duplicated key, so the squiggle covers
            // the key token, not the whole entity definition.
            let end = key_token_range(leaf.pos.start, key.chars().count()).end;
            let (line, col) = (leaf.pos.start.line, leaf.pos.start.col);
            // CW261 (DuplicateTypeDef). F#'s message is
            // "Key {id} of type {typename} is defined multiple times";
            // this per-file detection keys off the type name appearing
            // as repeated sibling keys, so `id` and `typename` collapse
            // to the same token. F#'s check is project-wide and grouped
            // by extracted instance id — a known refinement gap.
            let code = &error_codes::CW261_DUPLICATE_TYPE_DEF;
            errors.push(
                ValidationError::from_code(code, file_path, line, col, &[&key, &key]).with_end(end),
            );
        }
    }
}

fn find_matching_type<'a>(key: &str, ruleset: &'a RuleSet) -> Option<&'a TypeDefinition> {
    ruleset.type_by_name.get(key).map(|&i| &ruleset.types[i])
}
