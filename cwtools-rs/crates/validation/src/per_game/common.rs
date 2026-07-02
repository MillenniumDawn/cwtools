use crate::{ValidationError, error_codes};
use cwtools_parser::ast::{Child, ParsedFile, SourceRange, Value};
use cwtools_rules::rules_types::{RuleSet, TypeDefinition};
use cwtools_string_table::string_table::{StringId, StringTable};
use rustc_hash::FxHashMap;

/// A `key = { ... }` block (a `Leaf` whose value is a `Clause`), normalised so
/// the per-game structural walkers share one `Value::Clause` extraction. The key
/// is kept as a `StringId` so callers that only compare it avoid an owned
/// `String`.
pub(crate) struct Block<'a> {
    pub key: StringId,
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

/// Normalise a `key = { ... }` child (a Leaf with a Clause value) into a
/// [`Block`]. Returns `None` for leaves whose value isn't a clause, and for
/// comments / bare values.
pub(crate) fn as_block<'a>(child: &Child, ast: &'a ParsedFile) -> Option<Block<'a>> {
    match child {
        Child::Leaf(idx) => {
            let l = &ast.arena.leaves[*idx as usize];
            if let Value::Clause(children) = &l.value {
                Some(Block {
                    key: l.key.normal,
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
    file_path: &str,
    errors: &mut Vec<ValidationError>,
) {
    let mut type_counts: FxHashMap<String, usize> = FxHashMap::default();

    for child in &ast.root_children {
        let (key, line, col) = match child {
            Child::Leaf(idx) => {
                let leaf = &ast.arena.leaves[*idx as usize];
                let k = table.get_string(leaf.key.normal).unwrap_or_default();
                (k, leaf.pos.start.line, leaf.pos.start.col)
            }
            Child::LeafValue(_) | Child::Comment(_) => continue,
        };
        *type_counts.entry(key.clone()).or_insert(0) += 1;

        // Check if this type is defined with unique=true
        if let Some(type_def) = find_matching_type(&key, ruleset)
            && type_def.unique
        {
            let count = type_counts.get(&key).copied().unwrap_or(0);
            // Emit exactly once, at the second occurrence, so the error anchors
            // at the duplicate rather than at 0,0.
            if count == 2 {
                // CW261 (DuplicateTypeDef). F#'s message is
                // "Key {id} of type {typename} is defined multiple times";
                // this per-file detection keys off the type name appearing
                // as repeated sibling keys, so `id` and `typename` collapse
                // to the same token. F#'s check is project-wide and grouped
                // by extracted instance id — a known refinement gap.
                let code = &error_codes::CW261_DUPLICATE_TYPE_DEF;
                errors.push(ValidationError::from_code(
                    code,
                    file_path,
                    line,
                    col,
                    &[&key, &key],
                ));
            }
        }
    }
}

fn find_matching_type<'a>(key: &str, ruleset: &'a RuleSet) -> Option<&'a TypeDefinition> {
    ruleset.type_by_name.get(key).map(|&i| &ruleset.types[i])
}
