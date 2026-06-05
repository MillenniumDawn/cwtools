use cwtools_info::{TypeIndex, TypeInstance, SourceLocation};
use cwtools_parser::parser::parse_string;
use cwtools_rules::rules_converter::ast_to_ruleset;
use cwtools_string_table::string_table::StringTable;
use cwtools_validation::{validate_ast, ErrorSeverity, error_hash};
use std::collections::HashMap;

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
    let errors = validate_ast(&parsed, &ruleset, &table, "test.txt", None, None, None);
    assert!(errors.is_empty(), "Expected no errors but got: {:?}", errors);

    // Invalid file: wrong value type for int field
    let bad_script = r#"
ethos = {
    cost = "not_a_number"
    category = "materialist"
}
"#;
    let parsed_bad = parse_string(bad_script, &table).unwrap();
    let errors = validate_ast(&parsed_bad, &ruleset, &table, "test.txt", None, None, None);
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
    let errors = validate_ast(&parsed, &ruleset, &table, "test.txt", None, None, None);
    assert!(errors.is_empty(), "Expected no errors but got: {:?}", errors);
}

#[test]
fn test_error_hash() {
    let error = cwtools_validation::ValidationError {
        message: "Field 'cost' has value 'not_a_number', expected Int".to_string(),
        severity: ErrorSeverity::Error,
        line: 3,
        col: 10,
        file: "test.txt".to_string(),
        code: None,
    };
    let hash = error_hash(&error);
    assert_eq!(hash, "error|test.txt|3|Field 'cost' has value 'not_a_number', expected Int");
}

#[test]
fn test_type_key_filter_matching() {
    let cwt = r#"
types = {
    type[event] = {
        path = "game/events"
        subtype[country_event] = {
            type_key_field = is_triggered_only
            is_triggered_only = bool
            id = scalar
        }
        subtype[news_event] = {
            type_key_field = major
            major = bool
            id = scalar
        }
    }
}
"#;

    let table = StringTable::new();
    let parsed_cwt = parse_string(cwt, &table).unwrap();
    let ruleset = ast_to_ruleset(&parsed_cwt, &table);

    // country_event has is_triggered_only - should match country_event subtype
    let script = r#"
event = {
    is_triggered_only = yes
    id = my_event
}
"#;
    let parsed = parse_string(script, &table).unwrap();
    let errors = validate_ast(&parsed, &ruleset, &table, "test.txt", None, None, None);
    assert!(errors.is_empty(), "Expected no errors but got: {:?}", errors);

    // news_event has major - should match news_event subtype
    let script2 = r#"
event = {
    major = yes
    id = news_event
}
"#;
    let parsed2 = parse_string(script2, &table).unwrap();
    let errors2 = validate_ast(&parsed2, &ruleset, &table, "test.txt", None, None, None);
    assert!(errors2.is_empty(), "Expected no errors but got: {:?}", errors2);

    // Generic event without subtype key - should not get subtype-specific errors
    let script3 = r#"
event = {
    id = generic_event
}
"#;
    let parsed3 = parse_string(script3, &table).unwrap();
    let errors3 = validate_ast(&parsed3, &ruleset, &table, "test.txt", None, None, None);
    // No subtype matches, so no subtype rules apply, no errors expected
    assert!(errors3.is_empty(), "Expected no errors for generic event: {:?}", errors3);
}

#[test]
fn test_type_index_checking() {
    // Rules: simple type reference field
    let cwt = r#"
event = {
    ## cardinality = 0..inf
    requires_technology = <technology>
}
types = {
    type[event] = {
        path = "game/events"
    }
    type[technology] = {
        path = "game/common/technology"
    }
}
"#;
    let table = StringTable::new();
    let parsed_cwt = parse_string(cwt, &table).unwrap();
    let ruleset = ast_to_ruleset(&parsed_cwt, &table);

    let script_valid = r#"
event = {
    requires_technology = my_tech_alpha
}
"#;
    let script_bogus = r#"
event = {
    requires_technology = bogus_tech_xyz
}
"#;

    // Build a TypeIndex with one known technology instance
    let mut idx = TypeIndex::new();
    let mut map = HashMap::new();
    map.insert(
        "technology".to_string(),
        vec![TypeInstance {
            name: "my_tech_alpha".to_string(),
            location: SourceLocation { line: 1, col: 0 },
        }],
    );
    idx.merge("file://tech.txt", map);

    // Valid reference: no error
    let parsed_v = parse_string(script_valid, &table).unwrap();
    let errs_v = validate_ast(&parsed_v, &ruleset, &table, "game/events/test.txt", None, Some(&idx), None);
    assert!(errs_v.is_empty(), "Expected no errors for valid ref, got: {:?}", errs_v);

    // Bogus reference: should produce CW500
    let parsed_b = parse_string(script_bogus, &table).unwrap();
    let errs_b = validate_ast(&parsed_b, &ruleset, &table, "game/events/test.txt", None, Some(&idx), None);
    let type_errs: Vec<_> = errs_b.iter().filter(|e| e.code.as_deref() == Some("CW500")).collect();
    assert!(!type_errs.is_empty(), "Expected CW500 for bogus type ref, got: {:?}", errs_b);

    // Without type_index: no type-ref errors even for bogus reference
    let errs_no_idx = validate_ast(&parsed_b, &ruleset, &table, "game/events/test.txt", None, None, None);
    assert!(errs_no_idx.iter().all(|e| e.code.as_deref() != Some("CW500")),
        "Expected no CW500 without index, got: {:?}", errs_no_idx);
}

#[test]
fn test_alias_overload_disjunction() {
    // Regression: an aliased usage must be accepted if it matches ANY
    // `alias[cat:key]` overload, not just the first one declared.
    // Mirrors HOI4 `alias[trigger:original_tag]` (scope OR tag) and the ~40
    // `alias[ai_strategy_rule:ai_strategy]` blocks keyed by `type`.
    let cwt = r#"
types = {
    type[ai_strategy] = {
        path = "game/common/ai_strategy"
    }
}

ai_strategy = {
    ## cardinality = 0..1
    enable = {
        alias_name[trigger] = alias_match_left[trigger]
    }
    alias_name[ai_strategy_rule] = alias_match_left[ai_strategy_rule]
}

alias[trigger:original_tag] = scope[country]
alias[trigger:original_tag] = enum[country_tags]

alias[ai_strategy_rule:ai_strategy] = {
    type = enum[ai_role_strats]
    id = scalar
    value = int
}
alias[ai_strategy_rule:ai_strategy] = {
    type = enum[building_strats]
    id = scalar
    target = int
    value = int
}

enums = {
    enum[country_tags] = { AFG TAL }
    enum[ai_role_strats] = { role_ratio }
    enum[building_strats] = { building_target }
}
"#;
    let table = StringTable::new();
    let parsed_cwt = parse_string(cwt, &table).unwrap();
    let ruleset = ast_to_ruleset(&parsed_cwt, &table);

    // original_tag = AFG matches the 2nd trigger overload (enum[country_tags]).
    // The ai_strategy block matches the 2nd rule overload (building_strats, which
    // is the only one that allows `target`). Both fail against the FIRST overload.
    let script = r#"
my_strat = {
    enable = {
        original_tag = AFG
    }
    ai_strategy = {
        type = building_target
        id = foo
        target = 5
        value = 10
    }
}
"#;
    let parsed = parse_string(script, &table).unwrap();
    let errors = validate_ast(
        &parsed, &ruleset, &table, "game/common/ai_strategy/test.txt",
        Some(cwtools_game::constants::Game::Hoi4), None, None,
    );
    assert!(errors.is_empty(), "Expected no errors but got: {:?}", errors);
}

#[test]
fn test_enum_keyed_rule_matches_key() {
    // Regression: a rule keyed by `enum[x] = value` must match keys that are
    // members of enum x (HOI4 `research = { enum[tech_category] = float }`).
    let cwt = r#"
types = {
    type[ai_strategy_plan] = {
        path = "game/common/ai_strategy_plans"
    }
}

ai_strategy_plan = {
    ## cardinality = 0..1
    research = {
        enum[tech_category] = float
    }
}

enums = {
    enum[tech_category] = { CAT_inf }
}
"#;
    let table = StringTable::new();
    let parsed_cwt = parse_string(cwt, &table).unwrap();
    let ruleset = ast_to_ruleset(&parsed_cwt, &table);

    let script = r#"
my_plan = {
    research = {
        CAT_inf = 10.0
    }
}
"#;
    let parsed = parse_string(script, &table).unwrap();
    let errors = validate_ast(
        &parsed, &ruleset, &table, "game/common/ai_strategy_plans/test.txt",
        Some(cwtools_game::constants::Game::Hoi4), None, None,
    );
    assert!(errors.is_empty(), "Expected no errors but got: {:?}", errors);
}

#[test]
fn test_quoted_key_matches_same_rule_as_unquoted() {
    // Regression: a quoted key like `"AST" = { ... }` is just an alternate spelling
    // of the bare key and must match the same rule. The parser keeps the quotes in
    // `key.normal`, so the validator unquotes the key before matching (mirroring how
    // values are unquoted). Real case: `"LOG" = { has_war_with = CAR }` inside a
    // trigger block was flagged "Unexpected field" while bare `LOG` was accepted.
    let cwt = r#"
types = {
    type[diplo] = {
        path = "game/common/diplo"
    }
}

diplo = {
    ## cardinality = 0..inf
    enum[country_tags] = {
        value = int
    }
}

enums = {
    enum[country_tags] = { AST LOG USA }
}
"#;
    let table = StringTable::new();
    let parsed_cwt = parse_string(cwt, &table).unwrap();
    let ruleset = ast_to_ruleset(&parsed_cwt, &table);

    // Both quoted and unquoted tag keys must validate cleanly.
    let script = r#"
diplo = {
    "AST" = { value = 1 }
    LOG = { value = 2 }
}
"#;
    let parsed = parse_string(script, &table).unwrap();
    let errors = validate_ast(
        &parsed, &ruleset, &table, "game/common/diplo/test.txt",
        Some(cwtools_game::constants::Game::Hoi4), None, None,
    );
    assert!(errors.is_empty(), "Expected no errors but got: {:?}", errors);

    // Unquoting must not make matching blanket-permissive: a key that is not an
    // enum member (quoted or not) is still unexpected.
    let bad = r#"
diplo = {
    "XYZ" = { value = 1 }
}
"#;
    let parsed_bad = parse_string(bad, &table).unwrap();
    let errors = validate_ast(
        &parsed_bad, &ruleset, &table, "game/common/diplo/test.txt",
        Some(cwtools_game::constants::Game::Hoi4), None, None,
    );
    assert!(
        errors.iter().any(|e| e.message.contains("Unexpected")),
        "Expected an unexpected-field error for a non-member tag, got: {:?}", errors
    );
}

#[test]
fn test_named_scope_link_is_valid_scope_key() {
    // Regression: a from-data scope link declared in links.cwt (e.g. `character`)
    // can appear as a scope-switching key in an effect block. The validator reads
    // the `links = { ... }` block so `character = { ... }` matches the
    // `alias[effect:scope_field]` overload instead of reading as "Unexpected field".
    // Real case: HOI4 on_actions effect bodies with `character = { ... }`.
    let cwt = r#"
links = {
    character = {
        output_scope = character
        input_scopes = country
        from_data = yes
        data_source = <character>
    }
}

types = {
    type[evt] = {
        path = "game/events"
    }
}

evt = {
    effect = {
        alias_name[effect] = alias_match_left[effect]
    }
}

alias[effect:scope_field] = {
    alias_name[effect] = alias_match_left[effect]
}

alias[effect:add_attack] = int
"#;
    let table = StringTable::new();
    let parsed_cwt = parse_string(cwt, &table).unwrap();
    let ruleset = ast_to_ruleset(&parsed_cwt, &table);
    assert!(
        ruleset.scope_links.contains("character"),
        "expected `character` to be collected as a scope link"
    );

    let script = r#"
evt = {
    effect = {
        character = {
            add_attack = 1
        }
    }
}
"#;
    let parsed = parse_string(script, &table).unwrap();
    let errors = validate_ast(
        &parsed, &ruleset, &table, "game/events/test.txt",
        Some(cwtools_game::constants::Game::Hoi4), None, None,
    );
    assert!(
        !errors.iter().any(|e| e.message.contains("character")),
        "Expected no 'Unexpected field character' error, got: {:?}", errors
    );
}
