use cwtools_rules::rules_types::{NewField, RuleSet, RuleType, ValueType};

use super::builders::enum_values_for;

/// Build an LSP snippet body for a NodeRule, pre-populating required child fields
/// (those with cardinality min >= 1 and a SpecificField left-side).
///
/// Mirrors F# createSnippetForClause:346-390. Tab-stop numbering starts at 1.
pub(crate) fn generate_node_snippet(
    key: &str,
    child_rules: &[(RuleType, cwtools_rules::rules_types::Options)],
    ruleset: &RuleSet,
) -> String {
    // Collect required SpecificField leaves/nodes (min >= 1).
    let mut required_parts: Vec<String> = Vec::new();
    let mut tab_stop = 1u32;

    // Use a seen-set so duplicate keys (e.g. from subtype rules) don't repeat.
    let mut seen: std::collections::HashSet<String> = std::collections::HashSet::new();

    for (rule_type, opts) in child_rules {
        if opts.min < 1 {
            continue;
        }
        match rule_type {
            RuleType::LeafRule {
                left: NewField::SpecificField(k),
                right,
            } => {
                if seen.contains(k) {
                    continue;
                }
                seen.insert(k.clone());
                let placeholder = leaf_right_placeholder(right, tab_stop, ruleset);
                required_parts.push(format!("\t{} = {}", k, placeholder));
                tab_stop += 1;
            }
            RuleType::NodeRule {
                left: NewField::SpecificField(k),
                ..
            } => {
                if seen.contains(k) {
                    continue;
                }
                seen.insert(k.clone());
                required_parts.push(format!("\t{} = ${{{}:{{ }}}}", k, tab_stop));
                tab_stop += 1;
            }
            _ => {}
        }
    }

    if required_parts.is_empty() {
        // No required fields — just a block with cursor inside.
        format!("{} = {{\n\t$0\n}}", key)
    } else {
        let body = required_parts.join("\n");
        format!("{} = {{\n{}\n}}", key, body)
    }
}

/// Build the snippet body for an alias (effect/trigger) completion item from the
/// alias's rule shape. A block alias (`if`, `random`, `every_state`) expands to
/// `key = { …required fields… }` via [`generate_node_snippet`] (so e.g. `if`
/// pre-fills its required `limit = { }`); a value alias (`add_political_power`,
/// `set_country_flag`) expands to `key = <placeholder>` so the cursor lands after
/// the `=`, ready for the value. Returns `None` for shapes that have no snippet.
pub(super) fn alias_completion_snippet(
    key: &str,
    rule: &RuleType,
    ruleset: &RuleSet,
) -> Option<String> {
    match rule {
        RuleType::NodeRule { rules, .. } => Some(generate_node_snippet(key, rules, ruleset)),
        RuleType::LeafRule { right, .. } => Some(format!(
            "{} = {}",
            key,
            leaf_right_placeholder(right, 0, ruleset)
        )),
        _ => None,
    }
}

/// Produce a snippet placeholder string for the right-hand side of a leaf rule.
pub(crate) fn leaf_right_placeholder(right: &NewField, tab_stop: u32, ruleset: &RuleSet) -> String {
    match right {
        NewField::ValueField(ValueType::Bool) => {
            format!("${{{}|yes,no|}}", tab_stop)
        }
        NewField::ValueField(ValueType::Enum(e)) => {
            let vals = enum_values_for(ruleset, e);
            if !vals.is_empty() && vals.len() <= 20 {
                format!("${{{}|{}|}}", tab_stop, vals.join(","))
            } else {
                format!("${{{}}}", tab_stop)
            }
        }
        // A concrete literal value (e.g. `alias[effect:<se>] = yes`): insert it
        // directly so the snippet reads `my_se = yes` rather than `my_se = ${0}`.
        NewField::SpecificField(s) if !s.is_empty() => s.clone(),
        _ => format!("${{{}}}", tab_stop),
    }
}
