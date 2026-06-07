//! Tier S/G scope emissions (gated behind CWTOOLS_SCOPE_CHECKS):
//! - CW235 zero-modifier (a known modifier set to 0)
//! - CW247 rule-wrong-scope (reconciled from the Rust-invented CW400)
//! - CW104 trigger-wrong-scope (alias scope check)
//! - root scope seeding via `## replace_scope` (state-history `state` object)

use cwtools_game::constants::Game;
use cwtools_parser::parser::parse_string;
use cwtools_rules::rules_converter::ast_to_ruleset;
use cwtools_string_table::string_table::StringTable;
use cwtools_validation::validate_ast;
use std::collections::HashSet;

/// Scope checks are on by default now; ensure the escape-hatch isn't set so the
/// LazyLock resolves to enabled regardless of environment.
fn enable_scope_checks() {
    // SAFETY: tests are the sole writer; set before any validation runs.
    unsafe { std::env::remove_var("CWTOOLS_NO_SCOPE_CHECKS") };
}

fn codes_hoi4(cwt: &str, script: &str) -> Vec<String> {
    enable_scope_checks();
    let table = StringTable::new();
    let parsed_cwt = parse_string(cwt, &table).unwrap();
    let ruleset = ast_to_ruleset(&parsed_cwt, &table);
    let parsed = parse_string(script, &table).unwrap();
    let errors = validate_ast(
        &parsed,
        &ruleset,
        &table,
        "game/common/foo/test.txt",
        Some(Game::Hoi4),
        None,
        None,
    );
    errors.into_iter().filter_map(|e| e.code).collect()
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
    enable_scope_checks();
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
    let errors = validate_ast(
        &parsed,
        &ruleset,
        &table,
        "game/common/foo/test.txt",
        Some(Game::Hoi4),
        None,
        Some(&modifiers),
    );
    let codes: Vec<String> = errors.into_iter().filter_map(|e| e.code).collect();
    assert!(codes.contains(&"CW235".to_string()), "got: {:?}", codes);
}

#[test]
fn nonzero_known_modifier_is_clean() {
    enable_scope_checks();
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
    let errors = validate_ast(
        &parsed,
        &ruleset,
        &table,
        "game/common/foo/test.txt",
        Some(Game::Hoi4),
        None,
        Some(&modifiers),
    );
    let codes: Vec<String> = errors.into_iter().filter_map(|e| e.code).collect();
    assert!(!codes.contains(&"CW235".to_string()), "got: {:?}", codes);
}
