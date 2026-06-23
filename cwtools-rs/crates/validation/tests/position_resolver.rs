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
use cwtools_validation::{Prepared, build_scope_registry_arc};

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
                primary_loc_key: None,
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
    let registry = build_scope_registry_arc(&ruleset, None);
    let prepared = Prepared {
        ruleset: &ruleset,
        table: &table,
        game: None,
        type_index: idx,
        modifier_keys: None,
        loc_index: None,
        extra_loc_keys: None,
        registry: registry.as_ref(),
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

/// Two trigger overloads match `oil`: the `<resource>` type pattern (scope
/// country/state) and an empty game-derived `enum[equipment_category]` that only
/// matches via the permissive fallback. When the resource IS indexed, the
/// resource overload must come first so hover shows its description/scopes rather
/// than the unrelated equipment_category one (cwtools-vscode resource-trigger
/// hover bug — the real fix is indexing `common/resources`, this guards the
/// resolver ordering it relies on).
const RESOURCE_OVERLOAD_RULES: &str = r#"
types = {
    type[scripted_trigger] = { path = "game/common/scripted_triggers" }
    type[resource] = { path = "game/common/resources" }
}
enums = { enum[equipment_category] = { } }
scripted_trigger = {
    alias_name[trigger] = alias_match_left[trigger]
}
### Check amount of resource state or country has.
## scope = { country state }
alias[trigger:<resource>] = int
### Check ratio of this type of unit for commander.
## scope = { unit_leader combat }
alias[trigger:enum[equipment_category]] = variable_field
"#;

#[test]
fn indexed_resource_trigger_resolves_to_resource_overload() {
    let script = "my_trig = {\n\toil > 5\n}\n";
    let idx = type_index(&[("resource", "oil")]);
    let ctx = resolve(
        RESOURCE_OVERLOAD_RULES,
        script,
        "game/common/scripted_triggers/test.txt",
        "oil > 5",
        Some(&idx),
    )
    .expect("context");
    // The first matched overload (what hover surfaces) must be the resource one.
    let (_, opts) = ctx.value_rules.first().expect("a matched overload");
    assert_eq!(
        opts.description.as_deref(),
        Some("Check amount of resource state or country has."),
        "resource overload should win; got value_rules: {:?}",
        ctx.value_rules
            .iter()
            .map(|(_, o)| (&o.description, &o.required_scopes))
            .collect::<Vec<_>>()
    );
    assert_eq!(opts.required_scopes, vec!["country", "state"]);
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
    let prepared = Prepared {
        ruleset: &ruleset,
        table: &table,
        game: None,
        type_index: None,
        modifier_keys: None,
        loc_index: None,
        extra_loc_keys: None,
        registry: None,
        scope_checks: true,
        var_checks: false,
    };
    let ctx =
        rules_at_pos(&parsed, "game/common/decisions/test.txt", &prepared, 2, 6).expect("context");
    assert!(ctx.leaf.is_none(), "insert position has no leaf");
    assert!(has_specific_key(&ctx.child_rules, "cost"));
    assert!(has_specific_key(&ctx.child_rules, "visible"));
}

const FOCUS_RULES: &str = r#"
types = {
    ## unique = yes
    ## type_key_filter = style
    type[focus_style] = { path = "game/common/national_focus" name_field = "name" }
    ## unique = yes
    ## type_key_filter = focus_tree
    type[focus_tree] = { path = "game/common/national_focus" name_field = "id" }
    ## unique = yes
    ## type_key_filter = focus
    type[focus] = { path = "game/common/national_focus" skip_root_key = focus_tree name_field = "id" }
    ## unique = yes
    ## type_key_filter = { joint_focus shared_focus }
    type[shared_focus] = {
        path = "game/common/national_focus"
        name_field = "id"
        ## only_if_not = { joint_focus }
        ## type_key_filter = shared_focus
        subtype[shared] = { }
        ## only_if_not = { shared_focus }
        ## type_key_filter = joint_focus
        subtype[joint_focus] = { }
    }
    ## type_key_filter = search_filter_prios
    type[search_filter_prios] = { path = "game/common/national_focus" }
    type[spriteType] = { path = "game/interface" }
}
focus_tree = {
    id = scalar
}
focus = {
    id = scalar
}
alias[trigger:always] = bool
shared_focus = {
    id = localisation
    ## cardinality = 0..1
    text = localisation
    ## cardinality = 0..inf
    icon = <spriteType>
    ## cardinality = 0..inf
    icon = {
        <spriteType> = {
            alias_name[trigger] = alias_match_left[trigger]
        }
    }
    cost = float
    x = int
    y = int
    ## cardinality = 0..1
    relative_position_id = <shared_focus>
    ## cardinality = 0..1
    relative_position_id = <focus>
}
"#;

#[test]
fn shared_focus_block_resolves_child_rules() {
    // A top-level `shared_focus = { … }` in a national_focus file must resolve to
    // the shared_focus rules so the editor offers its fields, not a flat fallback
    // (cwtools-vscode#20). The type is keyed by a multi-key type_key_filter.
    let script = "shared_focus = {\n    id = test_focus\n    HERE\n}\n";
    let ctx = resolve(
        FOCUS_RULES,
        script,
        "common/national_focus/test.txt",
        "HERE",
        None,
    )
    .expect("should resolve a context inside shared_focus");
    let keys: Vec<&str> = ctx
        .child_rules
        .iter()
        .filter_map(|(rt, _)| match rt {
            RuleType::LeafRule {
                left: NewField::SpecificField(k),
                ..
            }
            | RuleType::NodeRule {
                left: NewField::SpecificField(k),
                ..
            } => Some(k.as_str()),
            _ => None,
        })
        .collect();
    assert!(
        has_specific_key(&ctx.child_rules, "cost"),
        "expected shared_focus fields (cost/icon), got: {:?}",
        keys
    );
}

#[test]
fn cursor_on_blank_line_after_field_is_insert_position() {
    // The parser's leaf range absorbs trailing whitespace, so a cursor on a blank
    // line after `icon = GFX_x` previously resolved to that leaf's VALUE (offering
    // value completions, usually empty) instead of the block's fields. It must be
    // an insert position (cwtools-vscode#20).
    let table = StringTable::new();
    let parsed_cwt = parse_string(FOCUS_RULES, &table).unwrap();
    let ruleset = ast_to_ruleset(&parsed_cwt, &table);
    let script = "shared_focus = {\n\tid = my_shared\n\ticon = GFX_x\n\t\n}\n";
    let parsed = parse_string(script, &table).unwrap();
    let registry = build_scope_registry_arc(&ruleset, None);
    let prepared = Prepared {
        ruleset: &ruleset,
        table: &table,
        game: None,
        type_index: None,
        modifier_keys: None,
        loc_index: None,
        extra_loc_keys: None,
        registry: registry.as_ref(),
        scope_checks: true,
        var_checks: false,
    };
    // The blank line is parser line 4 (1-based), col 1 (after the tab).
    let ctx = rules_at_pos(&parsed, "common/national_focus/test.txt", &prepared, 4, 1)
        .expect("should resolve a context on the blank line");
    assert!(
        ctx.leaf.as_ref().is_none_or(|l| !l.in_value),
        "blank line must not be an in-value position, got leaf: {:?}",
        ctx.leaf.as_ref().map(|l| (l.key.clone(), l.in_value))
    );
    assert!(
        has_specific_key(&ctx.child_rules, "cost"),
        "blank line should offer the block's fields"
    );
}
