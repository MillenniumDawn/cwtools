//! Variable-tracking emissions:
//! - CW271 (int-only) / CW270 (3-decimal precision): local numeric checks on a
//!   `variable_field`, on regardless of any gate.
//! - CW246 (variable has not been set): a non-numeric `variable_field` value
//!   that names no defined variable. Gated behind var_checks and driven
//!   by the project variable index.

use cwtools_game::constants::Game;
use cwtools_index::TypeIndex;
use cwtools_parser::parser::parse_string;
use cwtools_rules::rules_converter::ast_to_ruleset;
use cwtools_string_table::string_table::StringTable;
use cwtools_validation::{Prepared, build_scope_registry_arc, validate_prepared};

const RULES: &str = r#"
types = { type[foo] = { path = "game/common/foo" } }
foo = {
    add = int_variable_field
    prec = variable_field_32
    ref = variable_field
    get = value[variable]
}
"#;

fn codes(script: &str, vars: &[&str]) -> Vec<String> {
    let table = StringTable::new();
    let parsed_cwt = parse_string(RULES, &table).unwrap();
    let ruleset = ast_to_ruleset(&parsed_cwt, &table);
    let parsed = parse_string(script, &table).unwrap();
    let mut idx = TypeIndex::new();
    for v in vars {
        idx.var_index.add_name(v);
    }
    let registry = build_scope_registry_arc(&ruleset, Some(Game::Hoi4));
    let errors = validate_prepared(
        &parsed,
        "game/common/foo/test.txt",
        &Prepared {
            ruleset: &ruleset,
            table: &table,
            game: Some(Game::Hoi4),
            type_index: Some(&idx),
            modifier_keys: None,
            loc_index: None,
            registry: registry.as_ref(),
            scope_checks: true,
            var_checks: true,
        },
    );
    errors.into_iter().filter_map(|e| e.code).collect()
}

#[test]
fn int_field_with_fraction_is_cw271() {
    let c = codes("foo = { add = 5.5 }", &[]);
    assert!(c.contains(&"CW271".to_string()), "got: {:?}", c);
}

#[test]
fn int_field_with_integer_is_clean() {
    let c = codes("foo = { add = 5 }", &[]);
    assert!(!c.contains(&"CW271".to_string()), "got: {:?}", c);
}

#[test]
fn precision_field_over_three_decimals_is_cw270() {
    let c = codes("foo = { prec = 0.12345 }", &[]);
    assert!(c.contains(&"CW270".to_string()), "got: {:?}", c);
}

#[test]
fn precision_field_three_decimals_is_clean() {
    let c = codes("foo = { prec = 0.123 }", &[]);
    assert!(!c.contains(&"CW270".to_string()), "got: {:?}", c);
}

#[test]
fn undefined_variable_is_cw246() {
    // Index has a different variable, so it is non-empty but lacks `mystery`.
    let c = codes("foo = { ref = mystery }", &["something_else"]);
    assert!(c.contains(&"CW246".to_string()), "got: {:?}", c);
}

#[test]
fn defined_variable_is_clean() {
    let c = codes("foo = { ref = my_var }", &["my_var"]);
    assert!(!c.contains(&"CW246".to_string()), "got: {:?}", c);
}

#[test]
fn numeric_value_is_never_cw246() {
    let c = codes("foo = { ref = 3 }", &["something_else"]);
    assert!(!c.contains(&"CW246".to_string()), "got: {:?}", c);
}

#[test]
fn at_variable_is_never_cw246() {
    let c = codes("foo = { ref = @my_const }", &["something_else"]);
    assert!(!c.contains(&"CW246".to_string()), "got: {:?}", c);
}

#[test]
fn variable_get_field_defined_is_clean() {
    let c = codes("foo = { get = my_var }", &["my_var"]);
    assert!(!c.contains(&"CW246".to_string()), "got: {:?}", c);
}

#[test]
fn variable_get_field_undefined_is_cw246() {
    let c = codes("foo = { get = mystery }", &["something_else"]);
    assert!(c.contains(&"CW246".to_string()), "got: {:?}", c);
}
