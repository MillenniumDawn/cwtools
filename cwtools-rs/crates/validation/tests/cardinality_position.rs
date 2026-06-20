use cwtools_parser::parser::parse_string;
use cwtools_rules::rules_converter::ast_to_ruleset;
use cwtools_string_table::string_table::StringTable;
use cwtools_validation::validate_ast;

// An over-count cardinality warning (CW242, "appears N time(s), expected at
// most M") must be anchored on the offending field's own line, not on the
// block's first child. Previously the diagnostic landed on whatever the first
// child happened to be (e.g. `icon`), which is misleading — the squiggle was
// nowhere near the field actually being flagged.
#[test]
fn over_count_cardinality_points_at_field_not_first_child() {
    let cwt = r#"
my_decision = {
    icon = scalar
    ## cardinality = 0..1
    custom_cost_text = scalar
}

types = {
    type[my_decision] = {
        path = "game/common/decisions"
    }
}
"#;
    let table = StringTable::new();
    let parsed_cwt = parse_string(cwt, &table).unwrap();
    let ruleset = ast_to_ruleset(&parsed_cwt, &table);

    let script = r#"
my_decision = {
    icon = test_icon
    custom_cost_text = a
    custom_cost_text = b
}
"#;
    let parsed = parse_string(script, &table).unwrap();
    let errors = validate_ast(&parsed, &ruleset, &table, "test.txt", None, None, None);

    let card = errors
        .iter()
        .find(|e| {
            e.code == Some("CW242")
                && e.message.contains("custom_cost_text")
                && e.message.contains("at most")
        })
        .unwrap_or_else(|| {
            panic!(
                "expected a CW242 over-count for custom_cost_text. Errors: {:?}",
                errors
                    .iter()
                    .map(|e| (e.code, e.message.clone(), e.line))
                    .collect::<Vec<_>>()
            )
        });

    // `.line` is 1-based; index the source to see what line the squiggle covers.
    let line_text = script.lines().nth((card.line - 1) as usize).unwrap_or("");
    assert!(
        line_text.contains("custom_cost_text"),
        "CW242 squiggle should sit on a custom_cost_text line, but landed on line {}: {:?}",
        card.line,
        line_text
    );
    assert!(
        !line_text.contains("icon"),
        "CW242 squiggle landed on the icon line ({}), not the offending field",
        card.line
    );
}
