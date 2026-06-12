//! Tests for the position-targeted rule resolver (`position::rules_at_pos`):
//! alias descent (trigger blocks), typed keys (MIO equipment_bonus), subtype
//! exactness, multi-level skip_root_key, and key-vs-value position detection.

use std::collections::HashMap;

use cwtools_index::{SourceLocation, TypeIndex, TypeInstance};
use cwtools_parser::parser::parse_string;
use cwtools_rules::rules_converter::ast_to_ruleset;
use cwtools_rules::rules_types::*;
use cwtools_string_table::string_table::StringTable;
use cwtools_validation::position::{RuleContext, rules_at_pos};
use cwtools_validation::{Prepared, build_enum_map, build_scope_registry_arc};

/// Position of `marker`'s first char in `script`: 1-based line, 0-based col
/// (the parser/LSP convention).
fn pos_of(script: &str, marker: &str) -> (u32, u16) {
    let off = script
        .find(marker)
        .unwrap_or_else(|| panic!("marker {:?} not in script", marker));
    let before = &script[..off];
    let line = before.matches('\n').count() as u32 + 1;
    let col = before.rsplit('\n').next().unwrap().len() as u16;
    (line, col)
}

fn type_index(entries: &[(&str, &str)]) -> TypeIndex {
    let mut idx = TypeIndex::new();
    let mut per_type: HashMap<String, Vec<TypeInstance>> = HashMap::new();
    for (type_name, instance) in entries {
        per_type
            .entry(type_name.to_string())
            .or_default()
            .push(TypeInstance {
                name: instance.to_string(),
                location: SourceLocation { line: 1, col: 0 },
            });
    }
    idx.merge("test_defs.txt", per_type);
    idx
}

/// Resolve the context at `marker` (first occurrence) in `script`.
fn resolve(
    cwt: &str,
    script: &str,
    file_path: &str,
    marker: &str,
    idx: Option<&TypeIndex>,
) -> Option<RuleContext> {
    let table = StringTable::new();
    let parsed_cwt = parse_string(cwt, &table).unwrap();
    let ruleset = ast_to_ruleset(&parsed_cwt, &table);
    let parsed = parse_string(script, &table).unwrap();
    let enum_map = build_enum_map(&ruleset);
    let registry = build_scope_registry_arc(&ruleset, None);
    let prepared = Prepared {
        ruleset: &ruleset,
        table: &table,
        game: None,
        type_index: idx,
        modifier_keys: None,
        loc_index: None,
        registry: registry.as_ref(),
        enum_map: &enum_map,
        scope_checks: true,
        var_checks: false,
    };
    let (line, col) = pos_of(script, marker);
    rules_at_pos(&parsed, file_path, &prepared, line, col)
}

fn has_alias_left(rules: &[(RuleType, Options)], category: &str) -> bool {
    rules.iter().any(|(rt, _)| {
        matches!(rt,
            RuleType::LeafRule { left: NewField::AliasField(c), .. }
            | RuleType::NodeRule { left: NewField::AliasField(c), .. } if c == category)
    })
}

fn has_specific_key(rules: &[(RuleType, Options)], key: &str) -> bool {
    rules.iter().any(|(rt, _)| {
        matches!(rt,
            RuleType::LeafRule { left: NewField::SpecificField(k), .. }
            | RuleType::NodeRule { left: NewField::SpecificField(k), .. } if k == key)
    })
}

const TRIGGER_RULES: &str = r#"
types = {
    type[focus] = {
        path = "game/common/national_focus"
    }
    type[decision] = {
        path = "game/common/decisions"
    }
}
decision = {
    allowed = {
        alias_name[trigger] = alias_match_left[trigger]
    }
    cost = int
}
alias[trigger:has_completed_focus] = <focus>
alias[trigger:always] = bool
alias[trigger:NOT] = {
    alias_name[trigger] = alias_match_left[trigger]
}
"#;

#[test]
fn trigger_alias_value_resolves_to_focus_typefield() {
    // Cursor on the VALUE of has_completed_focus inside a trigger block: the
    // alias must expand to its LeafRule with right = TypeField(focus).
    let script = r#"
decision = {
    allowed = {
        has_completed_focus = my_focus
    }
    cost = 5
}
"#;
    let idx = type_index(&[("focus", "my_focus")]);
    let ctx = resolve(
        TRIGGER_RULES,
        script,
        "game/common/decisions/test.txt",
        "my_focus\n",
        Some(&idx),
    )
    .expect("context");
    let leaf = ctx.leaf.as_ref().expect("leaf at pos");
    assert_eq!(leaf.key, "has_completed_focus");
    assert!(leaf.in_value, "cursor is on the value side");
    let has_focus_typefield = ctx.value_rules.iter().any(|(rt, _)| {
        matches!(rt,
            RuleType::LeafRule { right: NewField::TypeField(TypeType::Simple(t)), .. } if t == "focus")
    });
    assert!(
        has_focus_typefield,
        "value_rules should contain TypeField(focus), got: {:?}",
        ctx.value_rules
    );
}

#[test]
fn trigger_block_insert_position_offers_trigger_alias() {
    // Cursor at an empty insert position inside `allowed = { ... }`: the block's
    // rules contain the alias_name[trigger] rule (the item generator expands it).
    let script = r#"
decision = {
    allowed = {
        always = yes
    }
    cost = 5
}
"#;
    let ctx = resolve(
        TRIGGER_RULES,
        script,
        "game/common/decisions/test.txt",
        "always",
        None,
    )
    .expect("context");
    // Cursor is ON the `always` leaf key — child_rules are still the block's.
    assert!(
        has_alias_left(&ctx.child_rules, "trigger"),
        "child_rules should contain alias_name[trigger], got: {:?}",
        ctx.child_rules
    );
    let leaf = ctx.leaf.as_ref().expect("leaf");
    assert_eq!(leaf.key, "always");
    assert!(!leaf.in_value, "cursor is on the key");
}

#[test]
fn nested_alias_block_descends() {
    // Cursor inside `NOT = { ... }`: descend through the alias[trigger:NOT]
    // body, which again exposes alias_name[trigger].
    let script = r#"
decision = {
    allowed = {
        NOT = {
            always = yes
        }
    }
    cost = 5
}
"#;
    let ctx = resolve(
        TRIGGER_RULES,
        script,
        "game/common/decisions/test.txt",
        "always",
        None,
    )
    .expect("context");
    assert!(
        has_alias_left(&ctx.child_rules, "trigger"),
        "inside NOT the trigger aliases apply again, got: {:?}",
        ctx.child_rules
    );
}

#[test]
fn mio_typed_key_descends_to_modifier_rules() {
    // `equipment_bonus = { <equipment> = { alias_name[modifier] } }`: the
    // concrete equipment key matches the TypeField rule and the descent reaches
    // the modifier alias context.
    let cwt = r#"
types = {
    type[equipment] = {
        path = "game/common/units/equipment"
    }
    type[mio] = {
        path = "game/common/military_industrial_organization/organizations"
    }
}
mio = {
    name = scalar
    equipment_bonus = {
        <equipment> = {
            alias_name[modifier] = alias_match_left[modifier]
        }
    }
}
"#;
    let script = r#"
mio = {
    name = my_org
    equipment_bonus = {
        some_equip = {
            build_cost_ic = 0.5
        }
    }
}
"#;
    let idx = type_index(&[("equipment", "some_equip")]);
    let ctx = resolve(
        cwt,
        script,
        "game/common/military_industrial_organization/organizations/test.txt",
        "build_cost_ic",
        Some(&idx),
    )
    .expect("context");
    assert!(
        has_alias_left(&ctx.child_rules, "modifier"),
        "inside the equipment block the modifier alias rules apply, got: {:?}",
        ctx.child_rules
    );
}

#[test]
fn subtype_rules_apply_exactly() {
    // An entity matching subtype[a] exposes a's fields; one matching subtype[b]
    // exposes b's, not a's.
    let cwt = r#"
types = {
    type[thing] = {
        path = "game/common/things"
        subtype[a] = {
            kind = kind_a
        }
        subtype[b] = {
            kind = kind_b
        }
    }
}
thing = {
    kind = scalar
    subtype[a] = {
        a_field = scalar
    }
    subtype[b] = {
        b_field = scalar
    }
}
"#;
    let script = r#"
thing_one = {
    kind = kind_a
    a_field = x
}
"#;
    let ctx =
        resolve(cwt, script, "game/common/things/test.txt", "a_field", None).expect("context");
    assert!(
        has_specific_key(&ctx.child_rules, "a_field"),
        "subtype[a] fields apply, got: {:?}",
        ctx.child_rules
    );
    assert!(
        !has_specific_key(&ctx.child_rules, "b_field"),
        "subtype[b] fields must NOT apply to a kind_a entity, got: {:?}",
        ctx.child_rules
    );
}

#[test]
fn multi_level_skip_root_key_descends_to_instance() {
    // HOI4 ideas shape: skip_root_key = { ideas any } — the cursor inside
    // `ideas = { country = { my_idea = { | } } }` resolves to the idea rules.
    let cwt = r#"
types = {
    type[idea] = {
        path = "game/common/ideas"
        skip_root_key = { ideas any }
    }
}
idea = {
    cost = int
    removal_cost = int
}
"#;
    let script = r#"
ideas = {
    country = {
        my_idea = {
            cost = 100
        }
    }
}
"#;
    let ctx = resolve(cwt, script, "game/common/ideas/test.txt", "cost", None).expect("context");
    assert!(
        has_specific_key(&ctx.child_rules, "cost"),
        "idea rules apply at the instance level, got: {:?}",
        ctx.child_rules
    );
    assert!(has_specific_key(&ctx.child_rules, "removal_cost"));
}

#[test]
fn value_vs_key_position_on_same_leaf() {
    let cwt = r#"
types = {
    type[decision] = {
        path = "game/common/decisions"
    }
}
decision = {
    kind = enum[kinds]
}
enums = {
    enum[kinds] = {
        alpha
        beta
    }
}
"#;
    let script = r#"
my_decision = {
    kind = alpha
}
"#;
    // Cursor on the key.
    let ctx =
        resolve(cwt, script, "game/common/decisions/test.txt", "kind", None).expect("context");
    let leaf = ctx.leaf.as_ref().expect("leaf");
    assert!(!leaf.in_value);

    // Cursor on the value: the enum rule appears in value_rules.
    let ctx = resolve(
        cwt,
        script,
        "game/common/decisions/test.txt",
        "alpha\n",
        None,
    )
    .expect("context");
    let leaf = ctx.leaf.as_ref().expect("leaf");
    assert!(leaf.in_value);
    let has_enum = ctx.value_rules.iter().any(|(rt, _)| {
        matches!(rt,
            RuleType::LeafRule { right: NewField::ValueField(ValueType::Enum(e)), .. } if e == "kinds")
    });
    assert!(
        has_enum,
        "value_rules should contain enum[kinds], got: {:?}",
        ctx.value_rules
    );
}

#[test]
fn insert_position_in_entity_returns_entity_rules() {
    // Cursor on the blank line inside the entity body (no containing child).
    let cwt = r#"
types = {
    type[decision] = {
        path = "game/common/decisions"
    }
}
decision = {
    cost = int
    visible = { alias_name[trigger] = alias_match_left[trigger] }
}
alias[trigger:always] = bool
"#;
    let script = "my_decision = {\n    MARKER\n}\n";
    // Use a script with a marker leaf removed: place cursor mid-block where no
    // child exists. Build it by replacing the marker with spaces.
    let script = script.replace("MARKER", "      ");
    let table = StringTable::new();
    let parsed_cwt = parse_string(cwt, &table).unwrap();
    let ruleset = ast_to_ruleset(&parsed_cwt, &table);
    let parsed = parse_string(&script, &table).unwrap();
    let enum_map = build_enum_map(&ruleset);
    let prepared = Prepared {
        ruleset: &ruleset,
        table: &table,
        game: None,
        type_index: None,
        modifier_keys: None,
        loc_index: None,
        registry: None,
        enum_map: &enum_map,
        scope_checks: true,
        var_checks: false,
    };
    let ctx =
        rules_at_pos(&parsed, "game/common/decisions/test.txt", &prepared, 2, 6).expect("context");
    assert!(ctx.leaf.is_none(), "insert position has no leaf");
    assert!(has_specific_key(&ctx.child_rules, "cost"));
    assert!(has_specific_key(&ctx.child_rules, "visible"));
}
