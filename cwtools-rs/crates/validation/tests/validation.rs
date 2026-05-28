use cwtools_parser::parser::parse_string;
use cwtools_rules::rules_converter::ast_to_ruleset;
use cwtools_string_table::string_table::StringTable;
use cwtools_validation::{validate_ast, ErrorSeverity};

#[test]
fn test_validate_simple_type() {
    let cwt = r#"
ethos = {
    cost = int
    category = scalar
}

types = {
    type[ethos] = {
        path = "game/common/ethics"
        subtype[actual_ethics] = {
            cost = int
            category = scalar
        }
    }
}
"#;

    let table = StringTable::new();
    let parsed_cwt = parse_string(cwt, &table).unwrap();
    let ruleset = ast_to_ruleset(&parsed_cwt, &table);

    // Valid file matching the ethos type
    let script = r#"
ethos = {
    cost = 2
    category = "materialist"
}
"#;
    let parsed = parse_string(script, &table).unwrap();
    let errors = validate_ast(&parsed, &ruleset, &table);
    assert!(errors.is_empty(), "Expected no errors but got: {:?}", errors);

    // Invalid file: wrong value type for int field
    let bad_script = r#"
ethos = {
    cost = "not_a_number"
    category = "materialist"
}
"#;
    let parsed_bad = parse_string(bad_script, &table).unwrap();
    let errors = validate_ast(&parsed_bad, &ruleset, &table);
    assert!(
        !errors.is_empty(),
        "Expected validation error for wrong type"
    );
    assert_eq!(errors[0].severity, ErrorSeverity::Error);
}

#[test]
fn test_validate_cardinality() {
    let cwt = r#"
types = {
    type[ship_size] = {
        path = "game/common/ship_sizes"
        subtype[ship] = {
            ## cardinality = 0..1
            max_speed = float
            ## cardinality = 0..1
            is_civilian = bool
        }
    }
}
"#;

    let table = StringTable::new();
    let parsed_cwt = parse_string(cwt, &table).unwrap();
    let ruleset = ast_to_ruleset(&parsed_cwt, &table);

    // Valid: max_speed appears once
    let script = r#"
ship_size = {
    max_speed = 5.5
    is_civilian = no
}
"#;
    let parsed = parse_string(script, &table).unwrap();
    let errors = validate_ast(&parsed, &ruleset, &table);
    assert!(errors.is_empty(), "Expected no errors but got: {:?}", errors);

    // TODO: cardinality enforcement needs more work - the rules currently
    // count root-level children per type, not per subtype. This is a known
    // limitation that requires scope tracking.
}
