//! Variable-tracking emissions:
//! - CW271 (int-only) / CW270 (3-decimal precision): local numeric checks on a
//!   `variable_field`, on regardless of any gate.
//! - CW246 (variable has not been set): a non-numeric `variable_field` value
//!   that names no defined variable. Gated behind CWTOOLS_VAR_CHECKS and driven
//!   by the project variable index.

use cwtools_game::constants::Game;
use cwtools_index::TypeIndex;
use cwtools_parser::parser::parse_string;
use cwtools_rules::rules_converter::ast_to_ruleset;
use cwtools_string_table::string_table::StringTable;
use cwtools_validation::validate_ast;

/// The var "has not been set" check is opt-in; turn it on for this binary. Set
/// before any validation runs so the LazyLock resolves to enabled.
fn enable_var_checks() {
    // SAFETY: tests are the sole writer; set before any validation runs.
    unsafe { std::env::set_var("CWTOOLS_VAR_CHECKS", "1") };
}

const RULES: &str = r#"
types = { type[foo] = { path = "game/common/foo" } }
foo = {
    add = int_variable_field
    prec = variable_field_32
    ref = variable_field
}
"#;

fn codes(script: &str, vars: &[&str]) -> Vec<String> {
    enable_var_checks();
    let table = StringTable::new();
    let parsed_cwt = parse_string(RULES, &table).unwrap();
    let ruleset = ast_to_ruleset(&parsed_cwt, &table);
    let parsed = parse_string(script, &table).unwrap();
    let mut idx = TypeIndex::new();
    for v in vars {
        idx.var_index.add_name(v);
    }
    let errors = validate_ast(
        &parsed,
        &ruleset,
        &table,
        "game/common/foo/test.txt",
        Some(Game::Hoi4),
        Some(&idx),
        None,
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
