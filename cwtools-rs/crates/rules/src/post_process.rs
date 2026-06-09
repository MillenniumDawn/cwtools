/// Post-processing passes over a fully merged RuleSet.
///
/// Mirrors the F# pipeline in RulesParser.fs:1326-1493:
///   replaceValueMarkerFields -> replaceSingleAliases -> replaceColourField -> replaceIgnoreMarkerFields
///
/// Run after all .cwt files have been parsed and merged so that single_alias
/// definitions referenced in one file but defined in another are all present.
use crate::rules_types::*;

/// Run all four post-processing passes over `ruleset` in the same order as F#.
#[tracing::instrument(skip_all)]
pub fn post_process(ruleset: &mut RuleSet) {
    replace_value_marker_fields(ruleset);
    replace_single_aliases(ruleset);
    replace_colour_field(ruleset);
    replace_ignore_marker_fields(ruleset);
}

// ---------------------------------------------------------------------------
// Pass 1: replaceSingleAliases
// ---------------------------------------------------------------------------

/// Inline `SingleAliasField(name)` references by substituting the body of the
/// matching `single_aliases` entry.  Iterates to fixpoint (up to 10 rounds) so
/// that single_alias bodies that themselves contain single_alias refs are fully
/// expanded.
fn replace_single_aliases(ruleset: &mut RuleSet) {
    // First expand any single_alias refs inside single_alias bodies themselves
    // (up to 10 rounds, same as F#). Track remaining unresolved reference count
    // to detect self-referential cycles: if a round makes no progress (count
    // doesn't decrease), bail rather than growing the rule tree exponentially.
    let mut prev_unresolved = usize::MAX;
    for _ in 0..10 {
        let map: Vec<(String, NewRule)> = ruleset.single_aliases.clone();
        let mut changed = false;
        for (_, rule) in ruleset.single_aliases.iter_mut() {
            inline_single_alias_rule(rule, &map, &mut changed);
        }
        if !changed {
            break;
        }
        let unresolved = count_single_alias_refs(&ruleset.single_aliases);
        if unresolved >= prev_unresolved {
            tracing::warn!(
                unresolved,
                "single_alias expansion cycle detected — bailing to avoid exponential growth"
            );
            break;
        }
        prev_unresolved = unresolved;
    }

    // Now expand all root_rules and aliases
    let map: Vec<(String, NewRule)> = ruleset.single_aliases.clone();
    for root in ruleset.root_rules.iter_mut() {
        match root {
            RootRule::TypeRule(_, rule) => inline_single_alias_rule(rule, &map, &mut true),
            RootRule::AliasRule(_, rule) => inline_single_alias_rule(rule, &map, &mut true),
            RootRule::SingleAliasRule(_, rule) => inline_single_alias_rule(rule, &map, &mut true),
        }
    }
    for (_, rule) in ruleset.aliases.iter_mut() {
        inline_single_alias_rule(rule, &map, &mut true);
    }
    for (_, rule) in ruleset.single_aliases.iter_mut() {
        inline_single_alias_rule(rule, &map, &mut true);
    }
}

/// Recursively walk `rule` and replace any `SingleAliasField` / `SingleAliasClauseField`
/// references with the body from `map`. Sets `changed` to true if any rewrite occurs.
fn inline_single_alias_rule(rule: &mut NewRule, map: &[(String, NewRule)], changed: &mut bool) {
    // A body that is *itself* a single_alias reference, e.g.
    // `alias[effect:every_country] = single_alias_right[every_effect_clause]`.
    // Resolve it in place so the alias body becomes the referenced rules and is
    // deep-validated like an inline body, instead of staying an opaque
    // SingleAliasField that validates permissively.
    if let RuleType::LeafRule {
        right: NewField::SingleAliasField(name),
        ..
    } = &rule.0
    {
        let name = name.clone();
        if let Some(resolved) = lookup_single_alias(&name, map) {
            let left = extract_leaf_left(&rule.0);
            match resolved.0 {
                RuleType::LeafRule { right: ar, .. } => {
                    rule.0 = RuleType::LeafRule { left, right: ar }
                }
                RuleType::NodeRule { rules: ar, .. } => {
                    rule.0 = RuleType::NodeRule { left, rules: ar }
                }
                _ => {}
            }
            *changed = true;
        }
        return;
    }
    // SingleAliasClauseField removed (variant deleted; was never constructed).
    match &mut rule.0 {
        RuleType::NodeRule { rules, .. } => {
            inline_rules_list(rules, map, changed);
        }
        RuleType::ValueClauseRule { rules } => {
            inline_rules_list(rules, map, changed);
        }
        RuleType::SubtypeRule { rules, .. } => {
            inline_rules_list(rules, map, changed);
        }
        _ => {}
    }
}

/// Walk a `Vec<NewRule>` in place, replacing SingleAliasField entries by
/// substituting the resolved body and recursing into nested rules.
fn inline_rules_list(rules: &mut Vec<NewRule>, map: &[(String, NewRule)], changed: &mut bool) {
    let needs_rewrite = rules.iter().any(|r| {
        matches!(
            r.0,
            RuleType::LeafRule {
                right: NewField::SingleAliasField(..),
                ..
            }
        )
    });
    if !needs_rewrite {
        for rule in rules.iter_mut() {
            inline_single_alias_rule(rule, map, changed);
        }
        return;
    }
    let original = std::mem::take(rules);
    for mut rule in original {
        match &rule.0 {
            // LeafRule whose right-hand side is a SingleAliasField
            RuleType::LeafRule {
                left: _,
                right: NewField::SingleAliasField(name),
            } => {
                let name = name.clone();
                let opts = rule.1.clone();
                if let Some(resolved) = lookup_single_alias(&name, map) {
                    *changed = true;
                    match resolved.0 {
                        RuleType::LeafRule { right: ar, .. } => {
                            rules.push((
                                RuleType::LeafRule {
                                    left: extract_leaf_left(&rule.0),
                                    right: ar,
                                },
                                opts,
                            ));
                        }
                        RuleType::NodeRule { rules: ar, .. } => {
                            rules.push((
                                RuleType::NodeRule {
                                    left: extract_leaf_left(&rule.0),
                                    rules: ar,
                                },
                                opts,
                            ));
                        }
                        other => {
                            // Fallback: keep original
                            rules.push((other, opts));
                        }
                    }
                } else {
                    // Not found — keep original
                    rules.push(rule);
                }
            }
            // Recurse into nested rule containers
            RuleType::NodeRule { .. } => {
                inline_single_alias_rule(&mut rule, map, changed);
                rules.push(rule);
            }
            RuleType::ValueClauseRule { .. } => {
                inline_single_alias_rule(&mut rule, map, changed);
                rules.push(rule);
            }
            RuleType::SubtypeRule { .. } => {
                inline_single_alias_rule(&mut rule, map, changed);
                rules.push(rule);
            }
            _ => {
                rules.push(rule);
            }
        }
    }
}

fn lookup_single_alias(name: &str, map: &[(String, NewRule)]) -> Option<NewRule> {
    map.iter().find(|(n, _)| n == name).map(|(_, r)| r.clone())
}

/// Count the total number of `SingleAliasField` / `SingleAliasClauseField` leaf
/// references remaining in the single_aliases bodies. Used for cycle detection:
/// if a fixpoint round didn't reduce this count, expansion has stalled.
fn count_single_alias_refs(single_aliases: &[(String, NewRule)]) -> usize {
    fn count_rule(rule: &NewRule) -> usize {
        match &rule.0 {
            RuleType::LeafRule {
                right: NewField::SingleAliasField(_),
                ..
            } => 1,
            RuleType::NodeRule { rules, .. }
            | RuleType::ValueClauseRule { rules }
            | RuleType::SubtypeRule { rules, .. } => rules.iter().map(count_rule).sum(),
            _ => 0,
        }
    }
    single_aliases.iter().map(|(_, r)| count_rule(r)).sum()
}

/// Extract the `left` field from a `LeafRule` variant.
fn extract_leaf_left(rt: &RuleType) -> NewField {
    match rt {
        RuleType::LeafRule { left, .. } => left.clone(),
        _ => NewField::ScalarField,
    }
}

// ---------------------------------------------------------------------------
// Pass 2: replaceColourField
// ---------------------------------------------------------------------------

/// Expand `LeafRule(l, MarkerField(ColourField))` into a `NodeRule` with a
/// single `LeafValueRule(Float(-256..256))` at cardinality 3..3.
/// Also expand `IrCountryTag` into parallel enum + variable rules.
/// Expand `colour_field` markers into a colour block rule (float -256..256,
/// exactly 3 values). Distinct from the inline `colour[rgb]`/`colour[hsv]` RHS
/// syntax handled at conversion time in `rules_converter::build_colour_rules`
/// (different ranges by design, not a duplicate).
fn replace_colour_field(ruleset: &mut RuleSet) {
    for root in ruleset.root_rules.iter_mut() {
        match root {
            RootRule::TypeRule(_, rule) => expand_colour_in_rule(rule),
            RootRule::AliasRule(_, rule) => expand_colour_in_rule(rule),
            RootRule::SingleAliasRule(_, rule) => expand_colour_in_rule(rule),
        }
    }
    for (_, rule) in ruleset.aliases.iter_mut() {
        expand_colour_in_rule(rule);
    }
    for (_, rule) in ruleset.single_aliases.iter_mut() {
        expand_colour_in_rule(rule);
    }
}

fn expand_colour_in_rule(rule: &mut NewRule) {
    let (rt, _) = rule;
    match rt {
        RuleType::NodeRule { rules, .. } => expand_colour_in_list(rules),
        RuleType::ValueClauseRule { rules } => expand_colour_in_list(rules),
        RuleType::SubtypeRule { rules, .. } => expand_colour_in_list(rules),
        _ => {}
    }
}

fn expand_colour_in_list(rules: &mut Vec<NewRule>) {
    let needs_expand = rules.iter().any(|r| {
        matches!(
            &r.0,
            RuleType::LeafRule {
                right: NewField::MarkerField(Marker::ColourField),
                ..
            } | RuleType::LeafRule {
                left: NewField::MarkerField(Marker::IrCountryTag),
                ..
            } | RuleType::LeafRule {
                right: NewField::MarkerField(Marker::IrCountryTag),
                ..
            } | RuleType::NodeRule {
                left: NewField::MarkerField(Marker::IrCountryTag),
                ..
            }
        )
    });
    if !needs_expand {
        for rule in rules.iter_mut() {
            expand_colour_in_rule(rule);
        }
        return;
    }
    let original = std::mem::take(rules);
    for rule in original {
        rules.extend(expand_colour_rule(rule));
    }
}

fn expand_colour_rule(mut rule: NewRule) -> Vec<NewRule> {
    match &rule.0 {
        // LeafRule(l, MarkerField(ColourField)) -> NodeRule(l, [LeafValue(Float(-256..256)) @ 3..3])
        RuleType::LeafRule {
            right: NewField::MarkerField(Marker::ColourField),
            ..
        } => {
            let left = extract_leaf_left(&rule.0);
            let opts = rule.1.clone();
            let inner_rule = (
                RuleType::LeafValueRule {
                    right: NewField::ValueField(ValueType::Float {
                        min: -256.0,
                        max: 256.0,
                    }),
                },
                Options {
                    min: 3,
                    max: 3,
                    strict_min: true,
                    leafvalue: true,
                    ..Options::default()
                },
            );
            vec![(
                RuleType::NodeRule {
                    left,
                    rules: vec![inner_rule],
                },
                opts,
            )]
        }
        // LeafRule(l, MarkerField(IrCountryTag)) -> two parallel LeafRules
        RuleType::LeafRule {
            right: NewField::MarkerField(Marker::IrCountryTag),
            ..
        } => {
            let left = extract_leaf_left(&rule.0);
            let opts = rule.1.clone();
            vec![
                (
                    RuleType::LeafRule {
                        left: left.clone(),
                        right: NewField::ValueField(ValueType::Enum("country_tags".to_string())),
                    },
                    opts.clone(),
                ),
                (
                    RuleType::LeafRule {
                        left,
                        right: NewField::VariableGetField("dynamic_country_tag".to_string()),
                    },
                    opts,
                ),
            ]
        }
        // LeafRule(MarkerField(IrCountryTag), r) -> two parallel LeafRules
        RuleType::LeafRule {
            left: NewField::MarkerField(Marker::IrCountryTag),
            ..
        } => {
            let right = extract_leaf_right(&rule.0);
            let opts = rule.1.clone();
            vec![
                (
                    RuleType::LeafRule {
                        left: NewField::ValueField(ValueType::Enum("country_tags".to_string())),
                        right: right.clone(),
                    },
                    opts.clone(),
                ),
                (
                    RuleType::LeafRule {
                        left: NewField::VariableGetField("dynamic_country_tag".to_string()),
                        right,
                    },
                    opts,
                ),
            ]
        }
        // NodeRule(MarkerField(IrCountryTag), r) -> two parallel NodeRules
        RuleType::NodeRule {
            left: NewField::MarkerField(Marker::IrCountryTag),
            ..
        } => {
            if let RuleType::NodeRule { rules, .. } = rule.0 {
                let opts = rule.1.clone();
                let mut rules_a = rules.clone();
                expand_colour_in_list(&mut rules_a);
                let mut rules_b = rules;
                expand_colour_in_list(&mut rules_b);
                vec![
                    (
                        RuleType::NodeRule {
                            left: NewField::ValueField(ValueType::Enum("country_tags".to_string())),
                            rules: rules_a,
                        },
                        opts.clone(),
                    ),
                    (
                        RuleType::NodeRule {
                            left: NewField::VariableGetField("dynamic_country_tag".to_string()),
                            rules: rules_b,
                        },
                        opts,
                    ),
                ]
            } else {
                vec![rule]
            }
        }
        // Recurse into nested containers
        RuleType::NodeRule { .. }
        | RuleType::ValueClauseRule { .. }
        | RuleType::SubtypeRule { .. } => {
            expand_colour_in_rule(&mut rule);
            vec![rule]
        }
        _ => vec![rule],
    }
}

fn extract_leaf_right(rt: &RuleType) -> NewField {
    match rt {
        RuleType::LeafRule { right, .. } => right.clone(),
        _ => NewField::ScalarField,
    }
}

// ---------------------------------------------------------------------------
// Pass 3: replaceValueMarkerFields
// ---------------------------------------------------------------------------

/// Rewrite `ValueScopeMarkerField { is_int, min, max }` to
/// `ValueScopeField { is_int, min, max }` everywhere it appears.
/// This is the base rewrite without the optional formula/range expansion.
fn replace_value_marker_fields(ruleset: &mut RuleSet) {
    for root in ruleset.root_rules.iter_mut() {
        match root {
            RootRule::TypeRule(_, rule) => rewrite_vsm_in_rule(rule),
            RootRule::AliasRule(_, rule) => rewrite_vsm_in_rule(rule),
            RootRule::SingleAliasRule(_, rule) => rewrite_vsm_in_rule(rule),
        }
    }
    for (_, rule) in ruleset.aliases.iter_mut() {
        rewrite_vsm_in_rule(rule);
    }
    for (_, rule) in ruleset.single_aliases.iter_mut() {
        rewrite_vsm_in_rule(rule);
    }
}

fn rewrite_vsm_in_rule(rule: &mut NewRule) {
    let (rt, _) = rule;
    match rt {
        RuleType::LeafRule { left, right } => {
            rewrite_vsm_field(left);
            rewrite_vsm_field(right);
        }
        RuleType::LeafValueRule { right } => {
            rewrite_vsm_field(right);
        }
        RuleType::NodeRule { left, rules } => {
            rewrite_vsm_field(left);
            rewrite_vsm_in_list(rules);
        }
        RuleType::ValueClauseRule { rules } => {
            rewrite_vsm_in_list(rules);
        }
        RuleType::SubtypeRule { rules, .. } => {
            rewrite_vsm_in_list(rules);
        }
    }
}

fn rewrite_vsm_in_list(rules: &mut [NewRule]) {
    for rule in rules.iter_mut() {
        rewrite_vsm_in_rule(rule);
    }
}

fn rewrite_vsm_field(field: &mut NewField) {
    if let NewField::ValueScopeMarkerField { is_int, min, max } = field {
        *field = NewField::ValueScopeField {
            is_int: *is_int,
            min: *min,
            max: *max,
        };
    }
}

// ---------------------------------------------------------------------------
// Pass 4: replaceIgnoreMarkerFields
// ---------------------------------------------------------------------------

/// Replace `LeafRule(field, IgnoreMarkerField)` with
/// `NodeRule(IgnoreField(Box::new(field)), [])`.
fn replace_ignore_marker_fields(ruleset: &mut RuleSet) {
    for root in ruleset.root_rules.iter_mut() {
        match root {
            RootRule::TypeRule(_, rule) => expand_ignore_in_rule(rule),
            RootRule::AliasRule(_, rule) => expand_ignore_in_rule(rule),
            RootRule::SingleAliasRule(_, rule) => expand_ignore_in_rule(rule),
        }
    }
    for (_, rule) in ruleset.aliases.iter_mut() {
        expand_ignore_in_rule(rule);
    }
    for (_, rule) in ruleset.single_aliases.iter_mut() {
        expand_ignore_in_rule(rule);
    }
}

fn expand_ignore_in_rule(rule: &mut NewRule) {
    let (rt, _) = rule;
    match rt {
        RuleType::NodeRule { rules, .. } => expand_ignore_in_list(rules),
        RuleType::ValueClauseRule { rules } => expand_ignore_in_list(rules),
        RuleType::SubtypeRule { rules, .. } => expand_ignore_in_list(rules),
        _ => {}
    }
}

fn expand_ignore_in_list(rules: &mut [NewRule]) {
    for rule in rules.iter_mut() {
        if matches!(
            rule.0,
            RuleType::LeafRule {
                right: NewField::IgnoreMarkerField,
                ..
            }
        ) {
            let left = extract_leaf_left(&rule.0);
            let opts = rule.1.clone();
            *rule = (
                RuleType::NodeRule {
                    left: NewField::IgnoreField(Box::new(left)),
                    rules: vec![],
                },
                opts,
            );
        } else {
            match &rule.0 {
                RuleType::NodeRule { .. }
                | RuleType::ValueClauseRule { .. }
                | RuleType::SubtypeRule { .. } => {
                    expand_ignore_in_rule(rule);
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::rules_converter::ast_to_ruleset;
    use cwtools_parser::parser::parse_string;
    use cwtools_string_table::string_table::StringTable;

    fn parse_and_post(input: &str) -> RuleSet {
        let table = StringTable::new();
        let parsed = parse_string(input, &table).unwrap();
        let mut ruleset = ast_to_ruleset(&parsed, &table);
        post_process(&mut ruleset);
        ruleset
    }

    // -----------------------------------------------------------------------
    // Pass 1: single_alias inlining
    // -----------------------------------------------------------------------

    #[test]
    fn test_single_alias_inline_leaf() {
        // A single_alias whose body is a simple leaf rule (scalar right).
        // Any rule that references single_alias_right[my_sa] should have its
        // right-hand side replaced with the body's right-hand side (scalar).
        let input = r#"
single_alias[my_sa] = scalar

alias[effect:test] = {
    ## cardinality = 0..inf
    my_field = single_alias_right[my_sa]
}
"#;
        let rs = parse_and_post(input);
        let (_, (rule, _)) = rs.aliases.iter().find(|(n, _)| n == "effect:test").unwrap();
        if let RuleType::NodeRule { rules, .. } = rule {
            let (inner_rule, _) = &rules[0];
            match inner_rule {
                RuleType::LeafRule { right, .. } => {
                    assert!(
                        matches!(right, NewField::ScalarField),
                        "expected ScalarField after single_alias inline, got {:?}",
                        right
                    );
                }
                other => panic!("expected LeafRule, got {:?}", other),
            }
        } else {
            panic!("expected NodeRule");
        }
    }

    #[test]
    fn test_single_alias_inline_node() {
        // A single_alias whose body is a node (block).
        // Referencing it via single_alias_right[my_node_sa] should yield a NodeRule
        // with the body's inner rules.
        let input = r#"
single_alias[my_node_sa] = {
    ## cardinality = 0..1
    inner_key = scalar
}

alias[effect:node_test] = {
    ## cardinality = 0..inf
    block_ref = single_alias_right[my_node_sa]
}
"#;
        let rs = parse_and_post(input);
        let (_, (rule, _)) = rs
            .aliases
            .iter()
            .find(|(n, _)| n == "effect:node_test")
            .unwrap();
        if let RuleType::NodeRule { rules, .. } = rule {
            let (inner_rule, _) = &rules[0];
            // Should have been promoted to a NodeRule whose rules contain inner_key=scalar
            match inner_rule {
                RuleType::NodeRule {
                    rules: inner_rules, ..
                } => {
                    assert!(
                        !inner_rules.is_empty(),
                        "inlined node alias should have inner rules"
                    );
                }
                other => panic!("expected NodeRule after node-alias inline, got {:?}", other),
            }
        } else {
            panic!("expected outer NodeRule");
        }
    }

    #[test]
    fn test_single_alias_inline_whole_body() {
        // An alias whose ENTIRE body is a single_alias_right reference, e.g.
        // `alias[effect:every_country] = single_alias_right[every_effect_clause]`.
        // Must inline to the referenced node's rules so the body deep-validates
        // (otherwise every_*/random_* scope-effect bodies validate permissively).
        let input = r#"
single_alias[every_clause] = {
    ## cardinality = 0..1
    limit = scalar
}

alias[effect:every_country] = single_alias_right[every_clause]
"#;
        let rs = parse_and_post(input);
        let (_, (rule, _)) = rs
            .aliases
            .iter()
            .find(|(n, _)| n == "effect:every_country")
            .unwrap();
        match rule {
            RuleType::NodeRule { rules, .. } => {
                assert!(
                    rules.iter().any(|(rt, _)| matches!(rt,
                        RuleType::LeafRule { left: NewField::SpecificField(s), .. } if s == "limit")),
                    "expected 'limit' rule from inlined every_clause, got {:?}", rules
                );
            }
            other => panic!(
                "expected NodeRule after whole-body single_alias inline, got {:?}",
                other
            ),
        }
    }

    // -----------------------------------------------------------------------
    // Pass 2: colour field expansion
    // -----------------------------------------------------------------------

    #[test]
    fn test_colour_field_expands_to_node_rule() {
        let input = r#"
alias[effect:colour_test] = {
    ## cardinality = 0..1
    colour = colour_field
}
"#;
        let rs = parse_and_post(input);
        let (_, (rule, _)) = rs
            .aliases
            .iter()
            .find(|(n, _)| n == "effect:colour_test")
            .unwrap();
        if let RuleType::NodeRule { rules, .. } = rule {
            let (inner, _) = &rules[0];
            match inner {
                RuleType::NodeRule {
                    rules: colour_inner,
                    ..
                } => {
                    assert_eq!(
                        colour_inner.len(),
                        1,
                        "colour NodeRule should have 1 LeafValue child"
                    );
                    let (lv, lv_opts) = &colour_inner[0];
                    match lv {
                        RuleType::LeafValueRule {
                            right: NewField::ValueField(ValueType::Float { min, max }),
                        } => {
                            assert_eq!(*min, -256.0, "colour float min");
                            assert_eq!(*max, 256.0, "colour float max");
                        }
                        other => panic!("colour child should be Float(-256..256), got {:?}", other),
                    }
                    assert_eq!(lv_opts.min, 3);
                    assert_eq!(lv_opts.max, 3);
                }
                other => panic!(
                    "expected NodeRule from colour_field expansion, got {:?}",
                    other
                ),
            }
        } else {
            panic!("expected outer NodeRule");
        }
    }

    // -----------------------------------------------------------------------
    // Pass 3: ValueScopeMarkerField -> ValueScopeField
    // -----------------------------------------------------------------------

    #[test]
    fn test_value_scope_marker_rewrite() {
        let input = r#"
alias[trigger:val_test] = {
    ## cardinality = 0..inf
    amount = value_field
}
"#;
        let rs = parse_and_post(input);
        let (_, (rule, _)) = rs
            .aliases
            .iter()
            .find(|(n, _)| n == "trigger:val_test")
            .unwrap();
        if let RuleType::NodeRule { rules, .. } = rule {
            let (inner, _) = &rules[0];
            match inner {
                RuleType::LeafRule { right, .. } => {
                    assert!(
                        matches!(right, NewField::ValueScopeField { is_int: false, .. }),
                        "expected ValueScopeField, got {:?}",
                        right
                    );
                }
                other => panic!("expected LeafRule, got {:?}", other),
            }
        } else {
            panic!("expected outer NodeRule");
        }
    }

    // -----------------------------------------------------------------------
    // Pass 4: IgnoreMarkerField expansion
    // -----------------------------------------------------------------------

    #[test]
    fn test_ignore_marker_expands() {
        let input = r#"
alias[effect:ignore_test] = {
    ## cardinality = 0..inf
    some_key = ignore_field
}
"#;
        let rs = parse_and_post(input);
        let (_, (rule, _)) = rs
            .aliases
            .iter()
            .find(|(n, _)| n == "effect:ignore_test")
            .unwrap();
        if let RuleType::NodeRule { rules, .. } = rule {
            let (inner, _) = &rules[0];
            match inner {
                RuleType::NodeRule {
                    left: NewField::IgnoreField(boxed),
                    rules: inner_rules,
                } => {
                    assert!(
                        matches!(boxed.as_ref(), NewField::SpecificField(_)),
                        "IgnoreField should wrap the original key field"
                    );
                    assert!(
                        inner_rules.is_empty(),
                        "IgnoreField NodeRule should have no children"
                    );
                }
                other => panic!("expected NodeRule(IgnoreField(..), []), got {:?}", other),
            }
        } else {
            panic!("expected outer NodeRule");
        }
    }
}
