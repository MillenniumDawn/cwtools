//! Tier R reconciliation: the rules engine must emit F#'s node-kind-specific
//! codes for structural/value/cardinality mismatches, not the Rust-invented
//! CW200-205.
//!
//! Mapping (F# CWTools/Rules/RuleValidationService.fs + FieldValidators.fs):
//! - unexpected `key = {...}` node      -> CW262
//! - unexpected `key = value` leaf      -> CW263
//! - unexpected bare value / leafvalue  -> CW264
//! - unexpected `{...}` value clause    -> CW265
//! - cardinality (too few / too many)   -> CW242
//! - wrong value type                   -> CW240

use cwtools_parser::parser::parse_string;
use cwtools_rules::rules_converter::ast_to_ruleset;
use cwtools_string_table::string_table::StringTable;
use cwtools_validation::validate_ast;

fn codes_for(cwt: &str, script: &str) -> Vec<String> {
    let table = StringTable::new();
    let parsed_cwt = parse_string(cwt, &table).unwrap();
    let ruleset = ast_to_ruleset(&parsed_cwt, &table);
    let parsed = parse_string(script, &table).unwrap();
    let errors = validate_ast(&parsed, &ruleset, &table, "test.txt", None, None, None);
    errors.into_iter().filter_map(|e| e.code).collect()
}

const RULES: &str = r#"
types = {
    type[foo] = {
        path = "game/common/foo"
    }
}
foo = {
    name = scalar
    count = int
    ## cardinality = 1..1
    required_field = scalar
}
"#;

#[test]
fn unexpected_leaf_is_cw263() {
    let codes = codes_for(
        RULES,
        r#"
foo = {
    required_field = ok
    bogus_leaf = 3
}
"#,
    );
    assert!(codes.contains(&"CW263".to_string()), "got: {:?}", codes);
    assert!(!codes.iter().any(|c| c == "CW201"), "CW201 retired");
}

#[test]
fn unexpected_node_is_cw262() {
    let codes = codes_for(
        RULES,
        r#"
foo = {
    required_field = ok
    bogus_block = { x = 1 }
}
"#,
    );
    assert!(codes.contains(&"CW262".to_string()), "got: {:?}", codes);
}

#[test]
fn wrong_value_type_is_cw240() {
    let codes = codes_for(
        RULES,
        r#"
foo = {
    required_field = ok
    count = notaninteger
}
"#,
    );
    assert!(codes.contains(&"CW240".to_string()), "got: {:?}", codes);
    assert!(!codes.iter().any(|c| c == "CW202"), "CW202 retired");
}

#[test]
fn missing_required_is_cw242() {
    // `required_field` has `## cardinality = 1..1` and is omitted.
    let codes = codes_for(
        RULES,
        r#"
foo = {
    name = something
}
"#,
    );
    assert!(codes.contains(&"CW242".to_string()), "got: {:?}", codes);
    assert!(
        !codes.iter().any(|c| c == "CW203" || c == "CW204"),
        "CW203/CW204 retired"
    );
}
