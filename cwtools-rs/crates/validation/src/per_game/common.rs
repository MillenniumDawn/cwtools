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
    pub children: &'a [Child],
    pub range: SourceRange,
}

impl Block<'_> {
    /// The block's key as an owned `String` (empty if interning lost it).
    pub fn key_string(&self, table: &StringTable) -> String {
        table.get_string(self.key).unwrap_or_default()
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
