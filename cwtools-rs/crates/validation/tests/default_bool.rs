//! `## default_bool = yes|no` (cwtools-vscode #26): a bool field annotated with
//! its engine default emits an info-level hint (CW282) when it is explicitly set
//! to that default value, so the redundant line can be omitted.

use cwtools_parser::parser::parse_string;
use cwtools_rules::rules_converter::ast_to_ruleset;
use cwtools_string_table::string_table::StringTable;
use cwtools_validation::{ErrorSeverity, validate_ast};

const RULES: &str = r#"
types = { type[foo] = { path = "game/common/foo" } }
foo = {
    name = scalar
    ## default_bool = yes
    some_bool_field = bool
}
"#;

fn validate(script: &str) -> Vec<cwtools_validation::ValidationError> {
    let table = StringTable::new();
    let parsed_cwt = parse_string(RULES, &table).unwrap();
    let ruleset = ast_to_ruleset(&parsed_cwt, &table);
    let parsed = parse_string(script, &table).unwrap();
    validate_ast(&parsed, &ruleset, &table, "test.txt", None, None, None)
}

fn codes(script: &str) -> Vec<String> {
    validate(script)
        .into_iter()
        .filter_map(|e| e.code.map(String::from))
        .collect()
}

#[test]
fn emits_cw282_when_set_to_default() {
    assert!(
        codes("foo = { name = x some_bool_field = yes }").contains(&"CW282".to_string()),
        "expected CW282 when bool field is set to its default"
    );
}

#[test]
fn cw282_is_information_severity() {
    let err = validate("foo = { name = x some_bool_field = yes }")
        .into_iter()
        .find(|e| e.code == Some("CW282"))
        .expect("CW282 emitted");
    assert_eq!(err.severity, ErrorSeverity::Information);
}

#[test]
fn no_hint_when_set_to_non_default() {
    assert!(
        !codes("foo = { name = x some_bool_field = no }").contains(&"CW282".to_string()),
        "CW282 must not fire when the field is set to a non-default value"
    );
}

#[test]
fn no_hint_without_directive() {
    // A bool field with no `## default_bool` annotation never emits CW282.
    const PLAIN: &str = r#"
types = { type[foo] = { path = "game/common/foo" } }
foo = {
    name = scalar
    some_bool_field = bool
}
"#;
    let table = StringTable::new();
    let parsed_cwt = parse_string(PLAIN, &table).unwrap();
    let ruleset = ast_to_ruleset(&parsed_cwt, &table);
    let parsed = parse_string("foo = { name = x some_bool_field = yes }", &table).unwrap();
    let errors = validate_ast(&parsed, &ruleset, &table, "test.txt", None, None, None);
    assert!(
        !errors.iter().any(|e| e.code == Some("CW282")),
        "CW282 must not fire without the directive"
    );
}
