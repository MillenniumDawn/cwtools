//! Structural validation of the loaded `.cwt` rule config.
//!
//! Re-walks each parsed `.cwt` AST and flags references to undefined types,
//! enums and single-aliases — a broken schema otherwise silently degrades every
//! downstream check (see `referenced_name` for why alias categories are out).
//! Reuses the converter's
//! `field_from_string` so the reference classification can't drift from how the
//! rules are actually compiled. Definitions self-resolve (a `type[foo]`
//! definition's own name is in `type_by_name`), so this permissive whole-AST
//! walk never false-flags a definition; it only fires on a *referenced* name
//! that no definition provides.

use std::path::{Path, PathBuf};

use cwtools_parser::ast::{Child, ParsedFile, Value};
use cwtools_string_table::string_table::StringTable;

use crate::rules_converter::field_parser::field_from_string;
use crate::rules_converter::value_to_string;
use crate::rules_types::{NewField, RuleSet, TypeType, ValueType};
use crate::ruleset_loader::RuleParseError;

/// Validate every loaded `.cwt` AST against the fully-merged `RuleSet`, returning
/// one `RuleParseError` per undefined reference (positioned at the referencing
/// leaf). Run after all files are merged so cross-file definitions resolve.
pub fn validate_ruleset_references(
    files: &[(PathBuf, ParsedFile)],
    ruleset: &RuleSet,
    table: &StringTable,
) -> Vec<RuleParseError> {
    let mut errors = Vec::new();
    for (path, ast) in files {
        for child in &ast.root_children {
            walk_child(child, ast, table, ruleset, path, &mut errors);
        }
    }
    errors
}

fn walk_child(
    child: &Child,
    ast: &ParsedFile,
    table: &StringTable,
    ruleset: &RuleSet,
    path: &Path,
    errors: &mut Vec<RuleParseError>,
) {
    match child {
        Child::Leaf(idx) => {
            let leaf = &ast.arena.leaves[*idx as usize];
            let pos = &leaf.pos.start;
            // The key may itself be a reference (`<character> = { … }`).
            let key = table.get_string(leaf.key.normal).unwrap_or_default();
            check_field(&key, pos.line, pos.col, ruleset, path, errors);
            match &leaf.value {
                Value::Clause(children) => {
                    for ch in children {
                        walk_child(ch, ast, table, ruleset, path, errors);
                    }
                }
                other => check_field(
                    &value_to_string(other, table),
                    pos.line,
                    pos.col,
                    ruleset,
                    path,
                    errors,
                ),
            }
        }
        Child::LeafValue(idx) => {
            let lv = &ast.arena.leaf_values[*idx as usize];
            let pos = &lv.pos.start;
            match &lv.value {
                Value::Clause(children) => {
                    for ch in children {
                        walk_child(ch, ast, table, ruleset, path, errors);
                    }
                }
                other => check_field(
                    &value_to_string(other, table),
                    pos.line,
                    pos.col,
                    ruleset,
                    path,
                    errors,
                ),
            }
        }
        Child::ValueClause(idx) => {
            for ch in &ast.arena.value_clauses[*idx as usize].children {
                walk_child(ch, ast, table, ruleset, path, errors);
            }
        }
        Child::Comment(_) => {}
    }
}

fn check_field(
    s: &str,
    line: u32,
    col: u16,
    ruleset: &RuleSet,
    path: &Path,
    errors: &mut Vec<RuleParseError>,
) {
    if let Some((kind, name)) = referenced_name(&field_from_string(s))
        && !is_defined(ruleset, kind, &name)
    {
        errors.push(RuleParseError {
            file: path.to_path_buf(),
            line,
            col,
            message: format!("rule references undefined {} `{}`", kind.label(), name),
        });
    }
}

#[derive(Clone, Copy)]
enum RefKind {
    Type,
    Enum,
    SingleAlias,
}

impl RefKind {
    fn label(self) -> &'static str {
        match self {
            RefKind::Type => "type",
            RefKind::Enum => "enum",
            RefKind::SingleAlias => "single_alias",
        }
    }
}

/// The referenced name a field carries, if it points at a definition the config
/// must provide.
///
/// Path / scope / value-set fields are intentionally omitted: they resolve
/// leniently (engine-provided or defined by use). Alias categories are omitted
/// too — an `alias[cat:name]` *definition* key parses to the same `AliasField`
/// as an `alias_name[cat]` *reference*, so a whole-AST walk can't tell them apart
/// and would false-flag the definitions. Types, enums and single-aliases use
/// distinct definition syntax (`type[x]`, `enum[x]` under `enums`,
/// `single_alias[x]`) that does NOT parse to a reference field, so their
/// definitions self-resolve and only genuine dangling references fire.
fn referenced_name(field: &NewField) -> Option<(RefKind, String)> {
    match field {
        // `<type>` / `<type.subtype>`: the subtype qualifier constrains the
        // match but the definition is keyed by the base type, so check that.
        NewField::TypeField(TypeType::Simple(n)) => Some((RefKind::Type, base_type(n).to_string())),
        NewField::TypeField(TypeType::Complex { name, .. }) => {
            Some((RefKind::Type, base_type(name).to_string()))
        }
        NewField::ValueField(ValueType::Enum(n)) => Some((RefKind::Enum, n.clone())),
        NewField::SingleAliasField(n) => Some((RefKind::SingleAlias, n.clone())),
        _ => None,
    }
}

/// The base type of a `type.subtype` reference (everything before the first `.`).
fn base_type(name: &str) -> &str {
    name.split('.').next().unwrap_or(name)
}

fn is_defined(ruleset: &RuleSet, kind: RefKind, name: &str) -> bool {
    match kind {
        RefKind::Type => ruleset.type_by_name.contains_key(name),
        RefKind::Enum => {
            ruleset.enum_by_name.contains_key(name)
                || ruleset.complex_enums.iter().any(|c| c.name == name)
        }
        RefKind::SingleAlias => ruleset.single_aliases.iter().any(|(k, _)| k == name),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules_converter::ast_to_ruleset;
    use cwtools_parser::parser::parse_string;

    fn check(src: &str) -> Vec<RuleParseError> {
        let table = StringTable::new();
        let parsed = parse_string(src, &table).unwrap();
        let ruleset = ast_to_ruleset(&parsed, &table);
        let files = vec![(PathBuf::from("test.cwt"), parsed)];
        validate_ruleset_references(&files, &ruleset, &table)
    }

    #[test]
    fn flags_undefined_type_reference_but_not_defined_one() {
        let src = "types = {\n    type[foo] = { path = \"common/foo\" }\n}\n\
                   some_rule = {\n    a = <foo>\n    b = <undefined_type>\n}\n";
        let errors = check(src);
        assert!(
            errors.iter().any(|e| e.message.contains("undefined_type")),
            "should flag undefined type, got: {:?}",
            errors
        );
        assert!(
            !errors.iter().any(|e| e.message.contains("`foo`")),
            "must NOT flag the defined type `foo`, got: {:?}",
            errors
        );
    }

    #[test]
    fn defined_type_reference_is_clean() {
        let src = "types = {\n    type[foo] = { path = \"common/foo\" }\n}\n\
                   r = { a = <foo> }\n";
        assert!(check(src).is_empty(), "got: {:?}", check(src));
    }

    #[test]
    fn type_subtype_reference_resolves_to_base_type() {
        // `<decision.timed>` constrains to a subtype but is defined by the base
        // type `decision`, so it must not flag.
        let src = "types = {\n    type[decision] = { path = \"common/decisions\" }\n}\n\
                   r = { a = <decision.timed> }\n";
        assert!(check(src).is_empty(), "got: {:?}", check(src));
    }
}
