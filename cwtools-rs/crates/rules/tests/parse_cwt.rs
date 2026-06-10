use cwtools_parser::parser::parse_string;
use cwtools_rules::rules_converter::ast_to_ruleset;
use cwtools_string_table::string_table::StringTable;

#[test]
fn test_parse_test_cwt() {
    let input = r#"
alias[effect:create_starbase] = {
    ## cardinality = 1..1
    owner = scalar

    ## cardinality = 1..1
    size = scalar
}

types = {
    type[ship_size] = {
        path = "game/common/ship_sizes"
        subtype[starbase] = {
            class = shipclass_starbase
        }
    }
}

enums = {
    enum[shipsize_class] = {
        shipclass_military
        shipclass_starbase
    }
}
"#;

    let table = StringTable::new();
    let parsed = parse_string(input, &table).unwrap();
    let ruleset = ast_to_ruleset(&parsed, &table);

    assert_eq!(ruleset.aliases.len(), 1); // alias[effect:create_starbase] extracted
    assert_eq!(ruleset.root_rules.len(), 0); // no top-level type rules in this snippet
    assert_eq!(ruleset.types.len(), 1);
    assert!(ruleset.root_rules.is_empty());
    assert_eq!(ruleset.types[0].name, "ship_size");
    assert_eq!(
        ruleset.types[0].path_options.paths,
        vec!["common/ship_sizes"]
    );
    assert_eq!(ruleset.types[0].subtypes.len(), 1);
    assert_eq!(ruleset.types[0].subtypes[0].name, "starbase");

    assert_eq!(ruleset.enums.len(), 1);
    assert_eq!(ruleset.enums[0].key, "shipsize_class");
    assert_eq!(
        ruleset.enums[0].values,
        vec!["shipclass_military", "shipclass_starbase"]
    );
}

#[test]
fn test_parse_real_cwt() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../testfiles/configtests/test.cwt"
    );
    let input = std::fs::read_to_string(path).unwrap();
    let table = StringTable::new();
    let parsed = parse_string(&input, &table).unwrap();
    let ruleset = ast_to_ruleset(&parsed, &table);

    assert!(!ruleset.types.is_empty());
    assert!(!ruleset.enums.is_empty());

    // Check that we got the ship_size type
    let ship_size = ruleset.types.iter().find(|t| t.name == "ship_size");
    assert!(ship_size.is_some());

    // Check that we got the shipsize_class enum
    let shipsize_class = ruleset.enums.iter().find(|e| e.key == "shipsize_class");
    assert!(shipsize_class.is_some());
}

#[test]
fn test_parse_stellaris_ethics() {
    let path = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../testfiles/stellarisconfig/ethics.cwt"
    );
    let input = std::fs::read_to_string(path).unwrap();
    let table = StringTable::new();
    let parsed = parse_string(&input, &table).unwrap();
    let ruleset = ast_to_ruleset(&parsed, &table);

    assert!(!ruleset.types.is_empty());
    let ethos = ruleset.types.iter().find(|t| t.name == "ethos");
    assert!(ethos.is_some());

    if let Some(e) = ethos {
        assert_eq!(e.path_options.paths, vec!["common/ethics"]);
        assert_eq!(e.subtypes.len(), 2);
    }
}
