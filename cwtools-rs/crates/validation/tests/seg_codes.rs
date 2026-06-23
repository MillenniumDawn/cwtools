//! Tier S/G scope emissions (gated behind CWTOOLS_SCOPE_CHECKS):
//! - CW235 zero-modifier (a known modifier set to 0)
//! - CW247 rule-wrong-scope (reconciled from the Rust-invented CW400)
//! - CW104 trigger-wrong-scope (alias scope check)
//! - root scope seeding via `## replace_scope` (state-history `state` object)

use cwtools_game::constants::Game;
use cwtools_parser::parser::parse_string;
use cwtools_rules::rules_converter::ast_to_ruleset;
use cwtools_string_table::string_table::StringTable;
use cwtools_validation::{Prepared, build_scope_registry_arc, validate_prepared};
use std::collections::HashSet;

fn codes_hoi4(cwt: &str, script: &str) -> Vec<String> {
    let table = StringTable::new();
    let parsed_cwt = parse_string(cwt, &table).unwrap();
    let ruleset = ast_to_ruleset(&parsed_cwt, &table);
    let parsed = parse_string(script, &table).unwrap();
    let registry = build_scope_registry_arc(&ruleset, Some(Game::Hoi4));
    let errors = validate_prepared(
        &parsed,
        "game/common/foo/test.txt",
        &Prepared {
            ruleset: &ruleset,
            table: &table,
            game: Some(Game::Hoi4),
            type_index: None,
            modifier_keys: None,
            loc_index: None,
            extra_loc_keys: None,
            registry: registry.as_ref(),
            scope_checks: true,
            var_checks: false,
        },
    );
    errors
        .into_iter()
        .filter_map(|e| e.code.map(String::from))
        .collect()
}

/// `foo` validates at the default country scope. A `## scope = state` trigger
/// used directly in it must produce CW104; a `## scope = country` one stays clean.
const SCOPE_RULES: &str = r#"
scopes = {
    Country = { aliases = { country } }
    State = { aliases = { state } }
}
types = { type[foo] = { path = "game/common/foo" } }
foo = {
    alias_name[trigger] = alias_match_left[trigger]
}
## scope = country
alias[trigger:country_only] = bool
## scope = state
alias[trigger:state_only] = bool
"#;

#[test]
fn state_trigger_in_country_scope_is_cw104() {
    let c = codes_hoi4(SCOPE_RULES, "foo = { state_only = yes }");
    assert!(c.contains(&"CW104".to_string()), "got: {:?}", c);
}

#[test]
fn country_trigger_in_country_scope_is_clean() {
    let c = codes_hoi4(SCOPE_RULES, "foo = { country_only = yes }");
    assert!(!c.contains(&"CW104".to_string()), "got: {:?}", c);
}

/// A type whose root rule seeds state scope via `## replace_scope` should make a
/// state-only effect inside it clean (mirrors history/states `state` object).
const REPLACE_SCOPE_RULES: &str = r#"
scopes = {
    Country = { aliases = { country } }
    State = { aliases = { state } }
}
types = { type[st] = { path = "game/common/foo" } }
## replace_scope = { this = state root = state }
st = {
    inner = {
        alias_name[effect] = alias_match_left[effect]
    }
}
## scope = state
alias[effect:state_fx] = bool
"#;

#[test]
fn replace_scope_seeds_root_state_scope() {
    // state_fx (## scope = state) inside a replace_scope=state type: no CW105.
    let c = codes_hoi4(REPLACE_SCOPE_RULES, "st = { inner = { state_fx = yes } }");
    assert!(!c.contains(&"CW105".to_string()), "got: {:?}", c);
}

/// A `scope[country]` target field. Resolving the chain from the default country
/// scope catches a target that lands in the wrong scope (CW243) or uses a link in
/// the wrong scope (CW245).
const TARGET_RULES: &str = r#"
scopes = {
    Country = { aliases = { country } }
    State = { aliases = { state } }
}
links = {
    capital_scope = { output_scope = state input_scopes = country }
    controller = { output_scope = country input_scopes = state }
    faction_leader = { output_scope = country input_scopes = country }
}
types = { type[foo] = { path = "game/common/foo" } }
foo = {
    tgt = scope[country]
}
"#;

#[test]
fn target_resolves_to_wrong_scope_is_cw243() {
    // capital_scope: country -> state, but the field wants country.
    let c = codes_hoi4(TARGET_RULES, "foo = { tgt = capital_scope }");
    assert!(c.contains(&"CW243".to_string()), "got: {:?}", c);
}

#[test]
fn link_used_in_wrong_scope_is_cw245() {
    // controller is only valid in state scope; used here from country.
    let c = codes_hoi4(TARGET_RULES, "foo = { tgt = controller }");
    assert!(c.contains(&"CW245".to_string()), "got: {:?}", c);
}

#[test]
fn target_resolving_to_country_is_clean() {
    // faction_leader: country -> country, matches the field. No target error.
    let c = codes_hoi4(TARGET_RULES, "foo = { tgt = faction_leader }");
    assert!(!c.contains(&"CW243".to_string()), "got: {:?}", c);
    assert!(!c.contains(&"CW245".to_string()), "got: {:?}", c);
}

/// `Character is_subscope_of { country }`, so a `## scope = country` trigger is
/// valid inside a character scope and must NOT produce CW104.
const SUBSCOPE_RULES: &str = r#"
scopes = {
    Country = { aliases = { country } }
    Character = { aliases = { character } is_subscope_of = { country } }
}
links = {
    character = { output_scope = character input_scopes = country }
}
types = { type[foo] = { path = "game/common/foo" } }
foo = {
    ## push_scope = character
    char_block = { alias_name[trigger] = alias_match_left[trigger] }
}
## scope = country
alias[trigger:country_only] = bool
"#;

#[test]
fn country_trigger_in_character_subscope_is_clean() {
    let c = codes_hoi4(
        SUBSCOPE_RULES,
        "foo = { char_block = { country_only = yes } }",
    );
    assert!(!c.contains(&"CW104".to_string()), "got: {:?}", c);
}

#[test]
fn zero_known_modifier_is_cw235() {
    let table = StringTable::new();
    // A rules file with a foo type whose body takes no fixed fields, so the
    // modifier key falls through to the dynamic-modifier accept path.
    let cwt = r#"
types = { type[foo] = { path = "game/common/foo" } }
foo = {
    dummy = scalar
}
"#;
    let parsed_cwt = parse_string(cwt, &table).unwrap();
    let ruleset = ast_to_ruleset(&parsed_cwt, &table);

    let mut modifiers = HashSet::new();
    modifiers.insert("attack_factor".to_string());

    let script = r#"
foo = {
    attack_factor = 0
}
"#;
    let parsed = parse_string(script, &table).unwrap();
    let registry = build_scope_registry_arc(&ruleset, Some(Game::Hoi4));
    let errors = validate_prepared(
        &parsed,
        "game/common/foo/test.txt",
        &Prepared {
            ruleset: &ruleset,
            table: &table,
            game: Some(Game::Hoi4),
            type_index: None,
            modifier_keys: Some(&modifiers),
            loc_index: None,
            extra_loc_keys: None,
            registry: registry.as_ref(),
            scope_checks: true,
            var_checks: false,
        },
    );
    let codes: Vec<String> = errors
        .into_iter()
        .filter_map(|e| e.code.map(String::from))
        .collect();
    assert!(codes.contains(&"CW235".to_string()), "got: {:?}", codes);
}

#[test]
fn nonzero_known_modifier_is_clean() {
    let table = StringTable::new();
    let cwt = r#"
types = { type[foo] = { path = "game/common/foo" } }
foo = {
    dummy = scalar
}
"#;
    let parsed_cwt = parse_string(cwt, &table).unwrap();
    let ruleset = ast_to_ruleset(&parsed_cwt, &table);

    let mut modifiers = HashSet::new();
    modifiers.insert("attack_factor".to_string());

    let script = r#"
foo = {
    attack_factor = 0.05
}
"#;
    let parsed = parse_string(script, &table).unwrap();
    let registry = build_scope_registry_arc(&ruleset, Some(Game::Hoi4));
    let errors = validate_prepared(
        &parsed,
        "game/common/foo/test.txt",
        &Prepared {
            ruleset: &ruleset,
            table: &table,
            game: Some(Game::Hoi4),
            type_index: None,
            modifier_keys: Some(&modifiers),
            loc_index: None,
            extra_loc_keys: None,
            registry: registry.as_ref(),
            scope_checks: true,
            var_checks: false,
        },
    );
    let codes: Vec<String> = errors
        .into_iter()
        .filter_map(|e| e.code.map(String::from))
        .collect();
    assert!(!codes.contains(&"CW235".to_string()), "got: {:?}", codes);
}

/// A trigger that matches a key ONLY via an unpopulated game-derived enum must
/// not inherit that alias's `## scope`. Regression for the resource-in-state
/// false positive: `oil` matched an empty `enum[equipment_category]` (scope
/// unit_leader/combat) when resources weren't indexed, flagging a bogus CW104.
const EMPTY_ENUM_RULES: &str = r#"
scopes = {
    Country = { aliases = { country } }
    State = { aliases = { state } }
    "Unit Leader" = { aliases = { unit_leader } }
}
enums = { enum[empty_e] = { } }
types = { type[foo] = { path = "game/common/foo" } }
foo = {
    alias_name[trigger] = alias_match_left[trigger]
}
## scope = { unit_leader }
alias[trigger:enum[empty_e]] = int
## scope = { unit_leader }
alias[trigger:skill] = int
"#;

#[test]
fn empty_enum_only_match_does_not_cw104() {
    // `oil` matches only the empty enum (permissively) -> no confident overload
    // -> no scope check -> no false CW104 in country scope.
    let c = codes_hoi4(EMPTY_ENUM_RULES, "foo = { oil = 1 }");
    assert!(!c.contains(&"CW104".to_string()), "got: {:?}", c);
}

#[test]
fn confident_literal_trigger_still_cw104() {
    // `skill` is an exact (confident) unit_leader trigger; in country scope it
    // must still fire CW104 — the fix only suppresses uncertain matches.
    let c = codes_hoi4(EMPTY_ENUM_RULES, "foo = { skill = 1 }");
    assert!(c.contains(&"CW104".to_string()), "got: {:?}", c);
}

/// A bare integer scope block (`129 = { ... }`) is a HOI4 state scope, so a
/// state-only trigger inside it is clean and a country-only one is CW104. A
/// numeric key matched as an explicit `int` field (random_list weight) keeps the
/// current scope instead.
const NUMERIC_STATE_RULES: &str = r#"
scopes = {
    Country = { aliases = { country } }
    State = { aliases = { state } }
}
types = { type[foo] = { path = "game/common/foo" } }
foo = {
    alias_name[effect] = alias_match_left[effect]
}
alias[effect:scope_field] = {
    alias_name[trigger] = alias_match_left[trigger]
    alias_name[effect] = alias_match_left[effect]
}
alias[effect:random_list] = {
    int = {
        alias_name[trigger] = alias_match_left[trigger]
        alias_name[effect] = alias_match_left[effect]
    }
}
## scope = state
alias[trigger:state_only] = bool
## scope = country
alias[trigger:country_only] = bool
"#;

#[test]
fn numeric_block_is_state_scope() {
    // 129 -> state, so a state-only trigger inside is clean.
    let c = codes_hoi4(NUMERIC_STATE_RULES, "foo = { 129 = { state_only = yes } }");
    assert!(
        !c.contains(&"CW104".to_string()),
        "state_only in 129 should be clean: {:?}",
        c
    );
}

#[test]
fn country_trigger_in_numeric_state_block_is_cw104() {
    // 129 -> state, so a country-only trigger inside is wrong-scope.
    let c = codes_hoi4(
        NUMERIC_STATE_RULES,
        "foo = { 129 = { country_only = yes } }",
    );
    assert!(
        c.contains(&"CW104".to_string()),
        "country_only in state 129 should be CW104: {:?}",
        c
    );
}

#[test]
fn random_list_weight_keeps_current_scope() {
    // The int weight bucket is NOT a state scope; a country-only trigger inside
    // stays valid at the country root.
    let c = codes_hoi4(
        NUMERIC_STATE_RULES,
        "foo = { random_list = { 10 = { country_only = yes } } }",
    );
    assert!(
        !c.contains(&"CW104".to_string()),
        "country_only in random_list weight should be clean: {:?}",
        c
    );
}
