//! A subtype whose only discriminator on a *variant* is a `<type.subtype>`
//! reference (`archetype = <equipment.naval_equip>`) must still activate when the
//! referenced archetype is itself a member of that subtype. The archetype
//! self-determines its subtype from a direct discriminator (`type = enum[...]`);
//! the variant resolves through the archetype via the subtype-membership index.
//!
//! Regression: before subtype-aware indexing, the index carried no
//! `equipment.naval_equip` key, so the variant never activated `naval_equip` and
//! its `model =` field fell through to the catch-all alias (CW267).

use cwtools_index::TypeIndex;
use cwtools_parser::parser::parse_string;
use cwtools_rules::rules_converter::ast_to_ruleset;
use cwtools_string_table::string_table::StringTable;
use cwtools_validation::{collect_subtype_instances, validate_ast};

const CWT: &str = r#"
types = {
    type[equipment] = {
        skip_root_key = equipments
        path = "game/common/units/equipment"
        subtype[archetype_equip] = {
            ## cardinality = 0..1
            is_archetype = yes
        }
        subtype[naval_equip] = {
            ## cardinality = 0..1
            type = enum[ship_units]
            ## cardinality = 0..1
            archetype = <equipment.naval_equip>
        }
    }
}

equipment = {
    ## cardinality = 0..1
    is_archetype = bool
    ## cardinality = 0..1
    archetype = <equipment>
    ## cardinality = 0..1
    type = enum[ship_units]
    alias_name[unit_stat] = alias_match_left[unit_stat]
    subtype[naval_equip] = {
        ## cardinality = 0..1
        model = scalar
    }
}

alias[unit_stat:build_cost_ic] = float

enums = {
    enum[ship_units] = {
        submarine
        destroyer
    }
}
"#;

const SCRIPT: &str = r#"
equipments = {
    ship_hull_submarine = {
        is_archetype = yes
        type = submarine
        model = base_sub_model
    }
    ship_hull_cruiser_submarine = {
        archetype = ship_hull_submarine
        model = cruiser_sub_model
    }
}
"#;

fn build_index(ruleset: &cwtools_rules::rules_types::RuleSet, table: &StringTable) -> TypeIndex {
    let parsed = parse_string(SCRIPT, table).unwrap();
    let logical = "common/units/equipment/ships.txt";
    let mut idx = TypeIndex::new();
    idx.merge(
        "file://ships.txt",
        cwtools_index::collect_type_instances(ruleset, &parsed, logical, table),
    );
    idx.merge(
        "file://ships.txt",
        collect_subtype_instances(ruleset, &parsed, logical, table),
    );
    idx
}

#[test]
fn naval_variant_with_archetype_ref_activates_naval_equip() {
    let table = StringTable::new();
    let ruleset = ast_to_ruleset(&parse_string(CWT, &table).unwrap(), &table);

    // The archetype is tagged naval_equip from its own `type = submarine`.
    let idx = build_index(&ruleset, &table);
    assert!(
        idx.contains("equipment.naval_equip", "ship_hull_submarine"),
        "archetype should be a naval_equip member"
    );

    let parsed = parse_string(SCRIPT, &table).unwrap();
    let errors = validate_ast(
        &parsed,
        &ruleset,
        &table,
        "common/units/equipment/ships.txt",
        Some(cwtools_validation::Game::Hoi4),
        Some(&idx),
        None,
    );
    let msgs: Vec<&String> = errors.iter().map(|e| &e.message).collect();
    assert!(
        !msgs.iter().any(|m| m.contains("model")),
        "model should be accepted on the naval variant, got: {:?}",
        msgs
    );
    assert!(
        errors.is_empty(),
        "expected no diagnostics, got: {:?}",
        msgs
    );
}
