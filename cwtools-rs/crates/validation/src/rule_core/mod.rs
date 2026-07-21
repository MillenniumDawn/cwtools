//! The rule-vs-AST core: matching children against rules, cardinality,
//! alias-usage resolution, and per-field value checks.

mod alias;
mod children;
mod leaf;
mod matching;
mod subtype_merge;
mod suggest;

pub(crate) use alias::alias_overloads;
pub(crate) use children::{math_clause_rules, rule_right_is_math_expr};
pub(crate) use leaf::field_matches_value;
pub(crate) use matching::{matching_candidates, rule_matches_leaf_key};
pub(crate) use subtype_merge::{
    flatten_nested_subtype_rules, merged_rules_for_type, validate_with_type,
};
