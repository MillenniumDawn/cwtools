use std::collections::HashSet;

use tower_lsp::lsp_types::*;

use cwtools_game::scope_engine::{SCOPE_ANY, ScopeId};
use cwtools_game::scope_registry::ScopeRegistry;
use cwtools_info::InfoService;
use cwtools_rules::rules_types::{
    NewField, ParsedAliasPattern, PatternKind, RootRule, RuleSet, RuleType, TypeType, ValueType,
};
use cwtools_validation::scope_matches_required;

use super::generate_node_snippet;
use super::scope_names::scope_completion_names;
use super::snippets::{alias_completion_snippet, choice_list, quote_if_needed};
use super::sort_for_kind;

/// The current scope at the cursor, paired with the registry that resolves it,
/// when scope-aware completion can act. `None` when no scope is known or the
/// scope is the open wildcard (`SCOPE_ANY`) — in which case completion offers the
/// full set unchanged.
type ScopeCtx<'a> = Option<(ScopeId, &'a ScopeRegistry)>;

/// Whether a `## scope` requirement list is genuinely scope-specific (narrows the
/// set) rather than the scope-agnostic `any`/`all`.
fn is_specific_requirement(required: &[String]) -> bool {
    !required.is_empty()
        && !required
            .iter()
            .any(|s| s.eq_ignore_ascii_case("any") || s.eq_ignore_ascii_case("all"))
}

/// How a completion item ranks against the current scope. A mismatch is never
/// dropped (scope tracking is imperfect, so hiding a valid item is worse than a
/// stale one at the bottom of the list); it only sinks below the normal buckets.
#[derive(Clone, Copy)]
enum ScopeRank {
    /// Matches a scope-specific `## scope` — leads the list (bucket `0`).
    SpecificMatch,
    /// No scope info, or a scope-agnostic match — keeps the kind bucket.
    Neutral,
    /// No overload satisfies the current scope — bottom bucket (`z`).
    Mismatch,
}

impl ScopeRank {
    fn sort_text(self, kind: Option<CompletionItemKind>, label: &str) -> Option<String> {
        match self {
            ScopeRank::SpecificMatch => Some(format!("0_{}", label)),
            ScopeRank::Neutral => sort_for_kind(kind, label),
            ScopeRank::Mismatch => Some(format!("z_{}", label)),
        }
    }
}

/// Build context-aware completion items from the child rules at the cursor's
/// position (the rules come from `position::rules_at_pos`, which resolves
/// aliases, typed keys, and subtypes the same way validation does).
#[tracing::instrument(skip_all, fields(rules = rules.len()))]
pub(crate) fn completions_from_rules(
    rules: &[(RuleType, cwtools_rules::rules_types::Options)],
    ruleset: &RuleSet,
    info: &InfoService,
    language: &str,
    modifier_keys: &HashSet<String>,
    registry: Option<&cwtools_game::scope_registry::ScopeRegistry>,
    current_scope: Option<ScopeId>,
) -> Vec<CompletionItem> {
    let mut items: Vec<CompletionItem> = Vec::new();
    // Per-request memo so a repeated enum is only collected/sorted once (#46).
    let mut enum_cache: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    // Built (sort + clone) at most once per call even if several scope rules
    // appear in this block (#44).
    let mut scope_names: Option<Vec<String>> = None;
    // Scope-aware ranking/filtering acts only on a definitely-known scope; the
    // open wildcard (`SCOPE_ANY`) means "unknown", so leave the set unchanged.
    let scope_ctx: ScopeCtx = match (current_scope, registry) {
        (Some(s), Some(reg)) if s != SCOPE_ANY => Some((s, reg)),
        _ => None,
    };
    // Scope-link keys (`mio:ORG = { … }`) are the same regardless of which alias
    // category triggered them, so emit them at most once per block (#76).
    let mut scope_links_emitted = false;

    for (rule_type, opts) in rules {
        match rule_type {
            // A concrete key in the block
            RuleType::LeafRule {
                left: NewField::SpecificField(k),
                right,
            } => push_specific_leaf_key(&mut items, k, right, opts, ruleset),
            // A node block key — generate snippet with required child fields pre-populated
            RuleType::NodeRule {
                left: NewField::SpecificField(k),
                rules: inner,
            } => push_specific_node_key(&mut items, k, inner, opts, ruleset),
            // An enum-keyed field: every member of the enum is a valid key here
            // (e.g. MIO `equipment_bonus = { enum[equipment_stat] = variable_field }`).
            RuleType::LeafRule {
                left: NewField::ValueField(ValueType::Enum(e)),
                right,
            } => push_enum_keyed_leaf(&mut items, &mut enum_cache, ruleset, info, e, right),
            RuleType::NodeRule {
                left: NewField::ValueField(ValueType::Enum(e)),
                ..
            } => push_enum_keyed_node(&mut items, &mut enum_cache, ruleset, info, e),
            // A typed key: every instance of the type is a valid key here
            // (e.g. `equipment_type = { <equipment_group> }` blocks, or
            // `<equipment> = { ... }` entries).
            RuleType::LeafRule {
                left: NewField::TypeField(TypeType::Simple(t)),
                right: NewField::AliasField(_) | NewField::ValueField(_) | NewField::ScalarField,
            }
            | RuleType::NodeRule {
                left: NewField::TypeField(TypeType::Simple(t)),
                ..
            } => {
                let style = if matches!(rule_type, RuleType::NodeRule { .. }) {
                    TypeInstanceStyle::NodeKey
                } else {
                    TypeInstanceStyle::LeafKey
                };
                push_type_instances(&mut items, info, t, style);
            }
            // An enum value at the leaf level
            RuleType::LeafValueRule {
                right: NewField::ValueField(ValueType::Enum(e)),
            } => push_enum_leaf_values(&mut items, &mut enum_cache, ruleset, info, e),
            // A bare type reference value
            RuleType::LeafValueRule {
                right: NewField::TypeField(TypeType::Simple(t)),
            }
            | RuleType::LeafRule {
                right: NewField::TypeField(TypeType::Simple(t)),
                ..
            } => push_type_instances(&mut items, info, t, TypeInstanceStyle::Reference),
            // An alias expansion
            RuleType::LeafRule {
                right: NewField::AliasField(cat),
                ..
            }
            | RuleType::LeafValueRule {
                right: NewField::AliasField(cat),
            }
            | RuleType::NodeRule {
                left: NewField::AliasField(cat),
                ..
            } => {
                push_alias_keys(&mut items, ruleset, info, modifier_keys, cat, scope_ctx);
                // A category with a `scope_field` alias (effect/trigger) accepts a
                // scope-switch key here (`mio:ORG = { … }`), so offer those keys too
                // (#76). The resolution machinery already backs goto/hover.
                if !scope_links_emitted
                    && ruleset
                        .alias_categories
                        .get(cat)
                        .is_some_and(|c| c.scope_field_idx.is_some())
                {
                    push_scope_link_keys(&mut items, ruleset, info);
                    scope_links_emitted = true;
                }
            }
            // alias_keys_field[cat]: the KEY of this leaf must be one of the alias
            // names in category `cat` (e.g. `alias_keys_field[modifier]` in a
            // dynamic_modifier block). Offer the same set as alias_name[cat] would
            // (cwtools-vscode#65).
            RuleType::LeafRule {
                left: NewField::AliasValueKeysField(cat),
                ..
            } => push_alias_keys(&mut items, ruleset, info, modifier_keys, cat, scope_ctx),
            // Scope names
            RuleType::LeafRule {
                right: NewField::ScopeField(_),
                ..
            }
            | RuleType::LeafValueRule {
                right: NewField::ScopeField(_),
            } => {
                let names =
                    scope_names.get_or_insert_with(|| scope_completion_names(language, registry));
                push_scope_names(&mut items, names);
            }
            // Boolean field at leaf value level
            RuleType::LeafValueRule {
                right: NewField::ValueField(ValueType::Bool),
            } => push_bool_leaf_values(&mut items),
            _ => {}
        }
    }

    // Dedup by label: subtype-flattening can produce duplicate rules when the same
    // field appears in multiple subtypes. Keep the first occurrence (which carries
    // the most specific snippet) (cwtools-vscode#66).
    let mut seen_labels: HashSet<String> = HashSet::new();
    items.retain(|item| seen_labels.insert(item.label.clone()));

    items
}

/// A concrete leaf key (`key = <value>`): one `FIELD` item completing to
/// `key = ` with a value-shaped placeholder (yes/no for bools, inline choices
/// for short enums, a bare tab stop otherwise — cwtools-vscode#16).
fn push_specific_leaf_key(
    items: &mut Vec<CompletionItem>,
    k: &str,
    right: &NewField,
    opts: &cwtools_rules::rules_types::Options,
    ruleset: &RuleSet,
) {
    let snippet = match right {
        NewField::ValueField(ValueType::Bool) => {
            // Insert a yes/no placeholder
            Some(format!("{} = ${{1|yes,no|}}", k))
        }
        NewField::ValueField(ValueType::Enum(e)) => {
            // Inline enum values if the list is short enough
            let vals = enum_values_for(ruleset, e);
            if !vals.is_empty() && vals.len() <= 20 {
                Some(format!("{} = ${{1|{}|}}", k, choice_list(vals)))
            } else {
                // Long enum: still complete the `key = ` and let the
                // value be typed/triggered.
                Some(format!("{} = $0", k))
            }
        }
        // Any other value kind (scalar, int, float, type ref, …):
        // complete `key = ` with the cursor after the `=`, rather than
        // a bare `key` with no operator (cwtools-vscode#16).
        _ => Some(format!("{} = $0", k)),
    };
    items.push(CompletionItem {
        label: k.to_string(),
        kind: Some(CompletionItemKind::FIELD),
        detail: opts.description.clone(),
        insert_text: snippet.clone(),
        insert_text_format: if snippet.is_some() {
            Some(InsertTextFormat::SNIPPET)
        } else {
            None
        },
        sort_text: sort_for_kind(Some(CompletionItemKind::FIELD), k),
        ..Default::default()
    });
}

/// A node block key (`key = { … }`): one `STRUCT` item whose snippet pre-fills
/// the block's required child fields. Rules carrying `required_scopes` sort
/// ahead of the rest.
fn push_specific_node_key(
    items: &mut Vec<CompletionItem>,
    k: &str,
    inner: &[(RuleType, cwtools_rules::rules_types::Options)],
    opts: &cwtools_rules::rules_types::Options,
    ruleset: &RuleSet,
) {
    let snippet = generate_node_snippet(k, inner, ruleset);
    // Scope-aware sortText: if rule has required_scopes push it earlier (lower sort key).
    let sort = if !opts.required_scopes.is_empty() {
        format!("0_{}", k)
    } else {
        format!("1_{}", k)
    };
    items.push(CompletionItem {
        label: k.to_string(),
        kind: Some(CompletionItemKind::STRUCT),
        detail: opts.description.clone(),
        insert_text: Some(snippet),
        insert_text_format: Some(InsertTextFormat::SNIPPET),
        sort_text: Some(sort),
        ..Default::default()
    });
}

/// Enum members as leaf keys (`enum[stat] = variable_field`): each member is a
/// valid key completing to `member = <value>`, the value placeholder shaped by
/// the rule's right-hand side.
fn push_enum_keyed_leaf(
    items: &mut Vec<CompletionItem>,
    enum_cache: &mut std::collections::HashMap<String, Vec<String>>,
    ruleset: &RuleSet,
    info: &InfoService,
    e: &str,
    right: &NewField,
) {
    let snippet_value = match right {
        NewField::ValueField(ValueType::Bool) => "${1|yes,no|}".to_string(),
        _ => "${1}".to_string(),
    };
    for v in all_enum_values_cached(enum_cache, ruleset, info, e) {
        items.push(CompletionItem {
            label: v.clone(),
            kind: Some(CompletionItemKind::FIELD),
            detail: Some(format!("enum {}", e)),
            insert_text: Some(format!("{} = {}", v, snippet_value)),
            insert_text_format: Some(InsertTextFormat::SNIPPET),
            sort_text: sort_for_kind(Some(CompletionItemKind::FIELD), v),
            ..Default::default()
        });
    }
}

/// Enum members as node-block keys (`enum[x] = { … }`): each member completes to
/// `member = { $0 }`.
fn push_enum_keyed_node(
    items: &mut Vec<CompletionItem>,
    enum_cache: &mut std::collections::HashMap<String, Vec<String>>,
    ruleset: &RuleSet,
    info: &InfoService,
    e: &str,
) {
    for v in all_enum_values_cached(enum_cache, ruleset, info, e) {
        items.push(CompletionItem {
            label: v.clone(),
            kind: Some(CompletionItemKind::STRUCT),
            detail: Some(format!("enum {}", e)),
            insert_text: Some(format!("{} = {{\n\t$0\n}}", v)),
            insert_text_format: Some(InsertTextFormat::SNIPPET),
            sort_text: sort_for_kind(Some(CompletionItemKind::STRUCT), v),
            ..Default::default()
        });
    }
}

/// Enum members as bare leaf values: one `ENUM_MEMBER` item per member, no
/// insert-text (the value is the label itself).
fn push_enum_leaf_values(
    items: &mut Vec<CompletionItem>,
    enum_cache: &mut std::collections::HashMap<String, Vec<String>>,
    ruleset: &RuleSet,
    info: &InfoService,
    e: &str,
) {
    for v in all_enum_values_cached(enum_cache, ruleset, info, e) {
        items.push(CompletionItem {
            label: v.clone(),
            kind: Some(CompletionItemKind::ENUM_MEMBER),
            detail: Some(format!("enum {}", e)),
            sort_text: sort_for_kind(Some(CompletionItemKind::ENUM_MEMBER), v),
            ..Default::default()
        });
    }
}

/// How a `<type>` reference is offered: as a key (with a `key = …` snippet) or
/// as a bare value reference (just the label).
enum TypeInstanceStyle {
    /// `<type> = { … }` key — completes to `name = { $0 }`.
    NodeKey,
    /// `<type> = value` key — completes to `name = ${1}`.
    LeafKey,
    /// A bare `<type>` value reference — the label is the instance name.
    Reference,
}

/// Emit one completion item per known instance of `t`. Shared by the typed-key
/// arms (which complete to `name = …` snippets) and the bare type-reference
/// value arm (which offers the instance name directly).
#[tracing::instrument(skip_all, fields(type_name = %t))]
fn push_type_instances(
    items: &mut Vec<CompletionItem>,
    info: &InfoService,
    t: &str,
    style: TypeInstanceStyle,
) {
    for (_, inst) in info.type_index.instances(t) {
        let (kind, insert_text) = match style {
            TypeInstanceStyle::NodeKey => (
                CompletionItemKind::STRUCT,
                Some(format!("{} = {{\n\t$0\n}}", inst.name)),
            ),
            TypeInstanceStyle::LeafKey => (
                CompletionItemKind::STRUCT,
                Some(format!("{} = ${{1}}", inst.name)),
            ),
            TypeInstanceStyle::Reference => (CompletionItemKind::REFERENCE, None),
        };
        items.push(CompletionItem {
            label: inst.name.clone(),
            kind: Some(kind),
            detail: Some(format!("{} instance", t)),
            insert_text_format: insert_text.as_ref().map(|_| InsertTextFormat::SNIPPET),
            insert_text,
            sort_text: sort_for_kind(Some(kind), &inst.name),
            ..Default::default()
        });
    }
}

/// Emit the keys of all `alias:<cat>` entries, labelled with the category
/// (trigger/effect/…) and carrying the alias's ### docs. Overloads collapse
/// onto one item (first description and first resolvable snippet win). The
/// `modifier` category has no alias entries, so its keys come from the expanded
/// modifier-key set instead.
///
/// Type-pattern aliases like `alias[effect:<scripted_effect>] = yes` are expanded
/// here: instead of emitting the raw `<scripted_effect>` placeholder, we look up
/// every instance of the `scripted_effect` type in the index and offer each as a
/// KEYWORD item (cwtools-vscode#64).
///
/// When `scope` is known, an alias/modifier is filtered out if every overload
/// declares a `## scope` the current scope can't satisfy, and one that matches a
/// scope-specific requirement is ranked into the top bucket (#78). The scope test
/// reuses the validator's `scope_matches_required`, so completion and validation
/// agree on what is in scope.
fn push_alias_keys(
    items: &mut Vec<CompletionItem>,
    ruleset: &RuleSet,
    info: &InfoService,
    modifier_keys: &HashSet<String>,
    cat: &str,
    scope: ScopeCtx,
) {
    let prefix = format!("{}:", cat);
    // Pre-pass: aggregate a scope verdict per exact-alias key across all its
    // overloads (`(any_match, any_specific_match)`). A key survives if ANY overload
    // is in scope; it ranks top if ANY overload matches a scope-specific `## scope`.
    // Mirrors the validator's per-key `.any(...)` scope check (rule_core/alias.rs).
    let verdicts: Option<std::collections::HashMap<&str, (bool, bool)>> =
        scope.map(|(current, reg)| {
            let mut m: std::collections::HashMap<&str, (bool, bool)> =
                std::collections::HashMap::new();
            for (alias_name, (_, opts)) in &ruleset.aliases {
                let Some(k) = alias_name.strip_prefix(&prefix) else {
                    continue;
                };
                if k == "scope_field" || ParsedAliasPattern::parse(k, 0).is_some() {
                    continue;
                }
                let matches = scope_matches_required(current, reg, &opts.required_scopes);
                let specific = matches && is_specific_requirement(&opts.required_scopes);
                let e = m.entry(k).or_insert((false, false));
                e.0 |= matches;
                e.1 |= specific;
            }
            m
        });
    // Own the keys so that instance names (borrowed from the type index, not from
    // `ruleset.aliases`) can also participate in the seen-check below.
    let mut seen: std::collections::HashMap<String, usize> = std::collections::HashMap::new();
    for (alias_name, (rule, opts)) in &ruleset.aliases {
        let Some(k) = alias_name.strip_prefix(&prefix) else {
            continue;
        };
        if k == "scope_field" {
            continue;
        }
        // Skip type/enum/value pattern aliases (e.g. `<scripted_effect>`,
        // `enum[idea_name]`). They are expanded from the type index below.
        if ParsedAliasPattern::parse(k, 0).is_some() {
            continue;
        }
        // Scope ranking (never a drop): a scope-specific match leads the list,
        // a scope mismatch sinks to the bottom bucket, everything else keeps its
        // kind bucket. Scope tracking is imperfect (nested/event_target/half-typed
        // contexts), so a valid key must never silently vanish (#78).
        let scope_rank = match verdicts.as_ref().and_then(|v| v.get(k)) {
            Some(&(false, _)) => ScopeRank::Mismatch,
            Some(&(true, true)) => ScopeRank::SpecificMatch,
            _ => ScopeRank::Neutral,
        };
        if let Some(&idx) = seen.get(k) {
            let item: &mut CompletionItem = &mut items[idx];
            if item.documentation.is_none()
                && let Some(d) = &opts.description
            {
                item.documentation = Some(Documentation::String(d.clone()));
            }
            // First overload wins the snippet; adopt a later one only
            // if the first had no resolvable shape.
            if item.insert_text.is_none()
                && let Some(snip) = alias_completion_snippet(k, rule, ruleset)
            {
                item.insert_text = Some(snip);
                item.insert_text_format = Some(InsertTextFormat::SNIPPET);
            }
            continue;
        }
        seen.insert(k.to_string(), items.len());
        // A block effect/trigger (`if`, `random`, …) completes to
        // `key = { …required fields… }`; a value one
        // (`add_political_power`) to `key = <placeholder>` so the
        // cursor lands after the `=`, ready for the value.
        let snippet = alias_completion_snippet(k, rule, ruleset);
        let sort_text = scope_rank.sort_text(Some(CompletionItemKind::KEYWORD), k);
        items.push(CompletionItem {
            label: k.to_string(),
            kind: Some(CompletionItemKind::KEYWORD),
            detail: Some(cat.to_string()),
            documentation: opts.description.clone().map(Documentation::String),
            insert_text_format: snippet.as_ref().map(|_| InsertTextFormat::SNIPPET),
            insert_text: snippet,
            sort_text,
            ..Default::default()
        });
    }

    // Expand pure type-pattern aliases (e.g. `alias[effect:<scripted_effect>] = yes`):
    // emit one KEYWORD item per instance of the referenced type. Composite patterns
    // like `production_speed_<building>_factor` (non-empty prefix/suffix) are too
    // complex to expand here and are skipped.
    if let Some(cat_idx) = ruleset.alias_categories.get(cat) {
        for pattern in &cat_idx.parsed_patterns {
            if !pattern.prefix.is_empty() || !pattern.suffix.is_empty() {
                continue;
            }
            let (_, (rule, _)) = &ruleset.aliases[pattern.alias_idx];
            match pattern.kind {
                PatternKind::Type => {
                    for (_, inst) in info.type_index.instances(&pattern.placeholder_name) {
                        if seen.contains_key(&inst.name) {
                            continue;
                        }
                        let snippet = alias_completion_snippet(&inst.name, rule, ruleset);
                        items.push(CompletionItem {
                            label: inst.name.clone(),
                            kind: Some(CompletionItemKind::KEYWORD),
                            detail: Some(cat.to_string()),
                            insert_text_format: snippet.as_ref().map(|_| InsertTextFormat::SNIPPET),
                            insert_text: snippet,
                            sort_text: sort_for_kind(Some(CompletionItemKind::KEYWORD), &inst.name),
                            ..Default::default()
                        });
                    }
                }
                PatternKind::Enum => {
                    for v in all_enum_values(ruleset, info, &pattern.placeholder_name) {
                        if seen.contains_key(&v) {
                            continue;
                        }
                        let snippet = alias_completion_snippet(&v, rule, ruleset);
                        items.push(CompletionItem {
                            label: v.clone(),
                            kind: Some(CompletionItemKind::KEYWORD),
                            detail: Some(cat.to_string()),
                            insert_text_format: snippet.as_ref().map(|_| InsertTextFormat::SNIPPET),
                            insert_text: snippet,
                            sort_text: sort_for_kind(Some(CompletionItemKind::KEYWORD), &v),
                            ..Default::default()
                        });
                    }
                }
                PatternKind::Value => {
                    for v in info
                        .type_index
                        .value_set_values
                        .values(&pattern.placeholder_name)
                    {
                        if seen.contains_key(v) {
                            continue;
                        }
                        let snippet = alias_completion_snippet(v, rule, ruleset);
                        items.push(CompletionItem {
                            label: v.to_string(),
                            kind: Some(CompletionItemKind::KEYWORD),
                            detail: Some(cat.to_string()),
                            insert_text_format: snippet.as_ref().map(|_| InsertTextFormat::SNIPPET),
                            insert_text: snippet,
                            sort_text: sort_for_kind(Some(CompletionItemKind::KEYWORD), v),
                            ..Default::default()
                        });
                    }
                }
            }
        }
    }

    // The `modifier` category has no alias entries — modifiers live
    // in the expanded modifier-key set (modifiers.cwt + templated
    // names like production_speed_<building>_factor). This is the
    // MIO `equipment_bonus` / idea `modifier` block case.
    if cat == "modifier" {
        // When the scope is known, resolve each modifier to its category's
        // `supported_scopes` (modifier_categories.cwt) so a scope-specific modifier
        // ranks ahead and a scope-mismatched one sinks to the bottom bucket (never
        // dropped — see the alias path). The map is keyed by the SAME
        // expanded/lowercased names as `modifier_keys`, so nothing shifts when the
        // scope is unknown (#78).
        let modifier_scopes = scope.map(|_| expanded_modifier_scopes(ruleset, info));
        for m in modifier_keys {
            let scope_rank = match scope {
                Some((current, reg)) => {
                    let scopes = modifier_scopes
                        .as_ref()
                        .and_then(|map| map.get(m).copied())
                        .unwrap_or(&[]);
                    if !scope_matches_required(current, reg, scopes) {
                        ScopeRank::Mismatch
                    } else if is_specific_requirement(scopes) {
                        ScopeRank::SpecificMatch
                    } else {
                        ScopeRank::Neutral
                    }
                }
                None => ScopeRank::Neutral,
            };
            let sort_text = scope_rank.sort_text(Some(CompletionItemKind::FIELD), m);
            items.push(CompletionItem {
                label: m.clone(),
                kind: Some(CompletionItemKind::FIELD),
                detail: Some("modifier".to_string()),
                insert_text: Some(format!("{} = $0", m)),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                sort_text,
                ..Default::default()
            });
        }
    }
}

/// Map each (expanded, lowercased) modifier name to its category's
/// `supported_scopes`. Templated modifiers (`production_speed_<building>_factor`)
/// expand against the type index one instance each — the same expansion
/// `build_modifier_keys` performs, so the keys line up with the modifier-key set.
/// Names whose category has no `modifier_categories.cwt` entry map to an empty
/// (unrestricted) scope list.
fn expanded_modifier_scopes<'a>(
    ruleset: &'a RuleSet,
    info: &InfoService,
) -> std::collections::HashMap<String, &'a [String]> {
    let mut map: std::collections::HashMap<String, &'a [String]> = std::collections::HashMap::new();
    for (name, category) in &ruleset.modifiers {
        let scopes = ruleset
            .modifier_categories
            .get(category)
            .map(Vec::as_slice)
            .unwrap_or(&[]);
        match (name.find('<'), name.find('>')) {
            (Some(open), Some(close)) if open < close => {
                let tn = &name[open + 1..close];
                let pre = &name[..open];
                let suf = &name[close + 1..];
                for (_, inst) in info.type_index.instances(tn) {
                    map.insert(
                        format!("{}{}{}", pre, inst.name, suf).to_lowercase(),
                        scopes,
                    );
                }
            }
            _ => {
                map.insert(name.to_lowercase(), scopes);
            }
        }
    }
    map
}

/// Emit scope-switch keys for PREFIXED `from_data` scope links
/// (`mio:ORG = { … }`, `sp:PROJ = { … }`, `token:… = { … }`): one block-opening
/// item per type instance the link's `data_source` names (#76). Only prefixed
/// links are offered — the prefix-less bare links (`<country>`, `<state>`, …)
/// are high-cardinality and would flood every effect/trigger block on every
/// keystroke (the response is always incomplete, so they rebuild each time). A
/// scope switch is rarely completed by typing a raw country tag / state id, and
/// goto/hover still resolve those. Capped so a mod with thousands of MIOs can't
/// bury the block's real effects.
fn push_scope_link_keys(items: &mut Vec<CompletionItem>, ruleset: &RuleSet, info: &InfoService) {
    const SCOPE_LINK_KEY_CAP: usize = 2000;
    let mut count = 0usize;
    for li in &ruleset.link_inputs {
        if !li.from_data {
            continue;
        }
        let Some(prefix) = li.prefix.as_deref().filter(|p| !p.is_empty()) else {
            continue;
        };
        for ds in &li.data_source {
            let Some(t) = ds.strip_prefix('<').and_then(|s| s.strip_suffix('>')) else {
                continue;
            };
            for (_, inst) in info.type_index.instances(t) {
                if count >= SCOPE_LINK_KEY_CAP {
                    return;
                }
                let label = format!("{}{}", prefix, inst.name);
                items.push(CompletionItem {
                    label: label.clone(),
                    kind: Some(CompletionItemKind::REFERENCE),
                    detail: Some(format!("{} scope", li.name)),
                    insert_text: Some(format!("{} = {{\n\t$0\n}}", label)),
                    insert_text_format: Some(InsertTextFormat::SNIPPET),
                    sort_text: sort_for_kind(Some(CompletionItemKind::REFERENCE), &label),
                    ..Default::default()
                });
                count += 1;
            }
        }
    }
}

/// Scope names (`scope[country]` positions): one `VALUE` item per name.
fn push_scope_names(items: &mut Vec<CompletionItem>, names: &[String]) {
    for name in names {
        items.push(CompletionItem {
            label: name.clone(),
            kind: Some(CompletionItemKind::VALUE),
            detail: Some("scope".to_string()),
            sort_text: sort_for_kind(Some(CompletionItemKind::VALUE), name),
            ..Default::default()
        });
    }
}

/// Boolean leaf value: the `yes`/`no` keywords.
fn push_bool_leaf_values(items: &mut Vec<CompletionItem>) {
    for v in &["yes", "no"] {
        items.push(CompletionItem {
            label: v.to_string(),
            kind: Some(CompletionItemKind::KEYWORD),
            detail: Some("bool".to_string()),
            sort_text: sort_for_kind(Some(CompletionItemKind::KEYWORD), v),
            ..Default::default()
        });
    }
}

pub(crate) fn enum_values_for<'a>(ruleset: &'a RuleSet, enum_name: &str) -> &'a [String] {
    if let Some(&idx) = ruleset.enum_by_name.get(enum_name) {
        return &ruleset.enums[idx].values;
    }
    &[]
}

/// Enum members from the static definition AND the collected complex-enum
/// values (equipment_stat, country_tags, idea_name, … extracted from game
/// files). The completion-item paths use this; snippet placeholders stick to
/// the static list (dynamic enums are too large for inline choices).
pub(crate) fn all_enum_values(
    ruleset: &RuleSet,
    info: &InfoService,
    enum_name: &str,
) -> Vec<String> {
    let mut vals = enum_values_for(ruleset, enum_name).to_vec();
    vals.extend(
        info.type_index
            .complex_enum_values
            .values(enum_name)
            .map(str::to_string),
    );
    vals.sort_unstable();
    vals.dedup();
    vals
}

/// Per-request memo for [`all_enum_values`]: one completion request can hit the
/// same enum across several match arms (e.g. multiple `LeafValueRule`s sharing
/// `equipment_stat`), and `all_enum_values` re-collects + sorts + dedups each
/// time. Cache by enum name within a single call so it only happens once.
fn all_enum_values_cached<'c>(
    cache: &'c mut std::collections::HashMap<String, Vec<String>>,
    ruleset: &RuleSet,
    info: &InfoService,
    enum_name: &str,
) -> &'c [String] {
    cache
        .entry(enum_name.to_string())
        .or_insert_with(|| all_enum_values(ruleset, info, enum_name))
}

/// Completion items for a leaf VALUE position: enumerate what the matched
/// rules' right-hand sides accept. `value_rules` comes from
/// `position::rules_at_pos` (alias usages already expanded to their overloads,
/// so `has_completed_focus = |` arrives here as a `TypeField("focus")` rule).
#[tracing::instrument(skip_all, fields(value_rules = value_rules.len()))]
pub(crate) fn value_completions(
    value_rules: &[(RuleType, cwtools_rules::rules_types::Options)],
    ruleset: &RuleSet,
    info: &InfoService,
    registry: Option<&cwtools_game::scope_registry::ScopeRegistry>,
    language: &str,
) -> Vec<CompletionItem> {
    let mut items: Vec<CompletionItem> = Vec::new();
    let mut seen: HashSet<String> = HashSet::new();
    // Per-request memo so a repeated enum is only collected/sorted once (#46).
    let mut enum_cache: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    // Built (sort + clone) at most once per call even if several scope-typed
    // value rules arrive here (#44).
    let mut scope_names: Option<Vec<String>> = None;
    let mut push = |label: String,
                    kind: CompletionItemKind,
                    detail: String,
                    insert_text: Option<String>,
                    items: &mut Vec<CompletionItem>| {
        if seen.insert(label.clone()) {
            let sort_label = label.clone();
            items.push(CompletionItem {
                label,
                kind: Some(kind),
                detail: Some(detail),
                insert_text,
                sort_text: sort_for_kind(Some(kind), &sort_label),
                ..Default::default()
            });
        }
    };

    for (rule_type, _opts) in value_rules {
        let right = match rule_type {
            RuleType::LeafRule { right, .. } => right,
            RuleType::LeafValueRule { right } => right,
            _ => continue,
        };
        match right {
            NewField::TypeField(TypeType::Simple(t)) => {
                for (_, inst) in info.type_index.instances(t) {
                    push(
                        inst.name.clone(),
                        CompletionItemKind::REFERENCE,
                        format!("{} instance", t),
                        None,
                        &mut items,
                    );
                }
            }
            NewField::TypeField(TypeType::Complex {
                prefix,
                name,
                suffix,
            }) => {
                for (_, inst) in info.type_index.instances(name) {
                    push(
                        format!("{}{}{}", prefix, inst.name, suffix),
                        CompletionItemKind::REFERENCE,
                        format!("{} instance", name),
                        None,
                        &mut items,
                    );
                }
            }
            NewField::ValueField(ValueType::Enum(e)) => {
                for v in all_enum_values_cached(&mut enum_cache, ruleset, info, e) {
                    // A value with whitespace/special chars must insert quoted so
                    // it parses as one token (`"No Compromise, No Surrender"`); a
                    // bare identifier inserts as its own label.
                    let quoted = quote_if_needed(v);
                    let insert_text = (quoted != *v).then_some(quoted);
                    push(
                        v.clone(),
                        CompletionItemKind::ENUM_MEMBER,
                        format!("enum {}", e),
                        insert_text,
                        &mut items,
                    );
                }
            }
            // A free localisation name (`name = localisation`): offer known loc
            // keys (workspace entities) rather than falling through to the flat
            // variable dump (cwtools-vscode#74).
            NewField::LocalisationField { .. } => {
                for k in super::scope_names::loc_key_names(info) {
                    push(
                        k.to_string(),
                        CompletionItemKind::TEXT,
                        "loc key".to_string(),
                        None,
                        &mut items,
                    );
                }
            }
            NewField::ValueField(ValueType::Bool) => {
                for v in ["yes", "no"] {
                    push(
                        v.to_string(),
                        CompletionItemKind::KEYWORD,
                        "bool".to_string(),
                        None,
                        &mut items,
                    );
                }
            }
            NewField::ScopeField(_)
            | NewField::ValueScopeField { .. }
            | NewField::ValueScopeMarkerField { .. } => {
                let names =
                    scope_names.get_or_insert_with(|| scope_completion_names(language, registry));
                for name in names.iter() {
                    push(
                        name.clone(),
                        CompletionItemKind::VALUE,
                        "scope".to_string(),
                        None,
                        &mut items,
                    );
                }
            }
            // `value[x]` reads and writes both offer the already-collected set
            // members. A write (`set_country_flag = |`) names a new member, so the
            // list is a "did you mean an existing one" hint rather than a closed
            // set; reads (`value[x]`) want exactly these. Same source either way.
            NewField::VariableGetField(ns) | NewField::VariableSetField(ns) => {
                let source: Vec<String> = match ns.as_str() {
                    "event_target" => info.event_target_counts.keys().cloned().collect(),
                    "variable" => info.variable_counts.keys().cloned().collect(),
                    // Flags/tokens/…: config-declared values plus the members
                    // collected from mod+vanilla effects (set_country_flag etc.).
                    other => {
                        let mut vals: Vec<String> =
                            ruleset.values.get(other).cloned().unwrap_or_default();
                        vals.extend(
                            info.type_index
                                .value_set_values
                                .values(other)
                                .map(str::to_string),
                        );
                        vals
                    }
                };
                for v in source {
                    push(
                        v,
                        CompletionItemKind::CONSTANT,
                        format!("value[{}]", ns),
                        None,
                        &mut items,
                    );
                }
            }
            NewField::VariableField { .. } => {
                for v in info.variable_counts.keys() {
                    push(
                        v.clone(),
                        CompletionItemKind::CONSTANT,
                        "variable".to_string(),
                        None,
                        &mut items,
                    );
                }
            }
            NewField::ValueField(ValueType::MathExpr) => {
                for v in info.variable_counts.keys() {
                    push(
                        v.clone(),
                        CompletionItemKind::CONSTANT,
                        "variable".to_string(),
                        None,
                        &mut items,
                    );
                }
                for et in info.event_target_counts.keys() {
                    push(
                        format!("event_target:{}", et),
                        CompletionItemKind::VARIABLE,
                        "event target".to_string(),
                        None,
                        &mut items,
                    );
                }
            }
            _ => {}
        }
    }

    items
}

/// Build root-level type snippets for types whose path matches `logical_path`.
///
/// When the cursor is at the top level of a file, offer a snippet for each
/// matching type.  Mirrors F# rootTypeItems:1077-1097: uses typeKeyFilter keys
/// as the block opener if set, otherwise the type name itself; also adds
/// subtype.typeKeyField alternatives.
#[tracing::instrument(skip_all, fields(logical_path = %logical_path))]
pub(crate) fn root_type_snippets(ruleset: &RuleSet, logical_path: &str) -> Vec<CompletionItem> {
    let mut items = Vec::new();

    for td in &ruleset.types {
        if !cwtools_info::check_path_dir(&td.path_options, logical_path) {
            continue;
        }

        // Determine which keys to offer as block openers.
        let mut openers: Vec<String> = match &td.type_key_filter {
            Some((keys, false)) if !keys.is_empty() => keys.clone(),
            _ => vec![td.name.clone()],
        };

        // Add subtype typeKeyField alternatives.
        for st in &td.subtypes {
            if let Some(tkf) = &st.type_key_field
                && !openers.contains(tkf)
            {
                openers.push(tkf.clone());
            }
        }

        // Find the TypeRule for this type to get child rules for snippet body.
        let child_rules: Option<&[(RuleType, cwtools_rules::rules_types::Options)]> =
            ruleset.root_rules.iter().find_map(|r| {
                if let RootRule::TypeRule(name, (RuleType::NodeRule { rules, .. }, _)) = r {
                    if name == &td.name {
                        Some(rules.as_slice())
                    } else {
                        None
                    }
                } else {
                    None
                }
            });

        for opener in openers {
            let snippet = if let Some(cr) = child_rules {
                generate_node_snippet(&opener, cr, ruleset)
            } else {
                format!("{} = {{\n\t$0\n}}", opener)
            };
            items.push(CompletionItem {
                label: opener.clone(),
                kind: Some(CompletionItemKind::STRUCT),
                detail: Some(format!("type {} instance", td.name)),
                insert_text: Some(snippet),
                insert_text_format: Some(InsertTextFormat::SNIPPET),
                sort_text: Some(format!("0_{}", opener)),
                ..Default::default()
            });
        }
    }

    items
}
