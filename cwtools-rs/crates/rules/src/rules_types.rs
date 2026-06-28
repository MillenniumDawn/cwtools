/// Parsed result from a .cwt file or set of files.
#[derive(Debug, Clone, PartialEq)]
pub struct RuleSet {
    pub types: Vec<TypeDefinition>,
    pub aliases: Vec<(String, NewRule)>,
    pub single_aliases: Vec<(String, NewRule)>,
    pub enums: Vec<EnumDefinition>,
    pub complex_enums: Vec<ComplexEnumDef>,
    pub root_rules: Vec<RootRule>,
    /// Parsed `values = { value[name] = { ... } }` blocks (item G).
    /// Keyed by name; sets from multiple .cwt files are unioned at merge.
    pub values: std::collections::HashMap<String, Vec<String>>,
    /// Names from a top-level `modifiers = { name = category ... }` block. These
    /// are the valid keys for `alias_name[modifier]` slots (modifier contexts).
    pub modifiers: Vec<String>,
    /// Link names from a top-level `links = { name = { ... } }` block (links.cwt).
    /// A from-data scope link (e.g. `character`, `state`, `owner`) can appear as a
    /// scope-switching key, so these are the valid keys for an `[cat:scope_field]`
    /// slot alongside scope commands and type instances. See [`crate`] consumers.
    /// Derived from `link_inputs` (names + prefixes) during reindex.
    pub scope_links: std::collections::HashSet<String>,
    /// Scope definitions from a top-level `scopes = { Name = { aliases = {..} } }`
    /// block (scopes.cwt). Used to build the runtime scope registry. Empty when no
    /// scopes.cwt is loaded (the engine then falls back to the hardcoded table).
    pub scope_inputs: Vec<ScopeInput>,
    /// Full link definitions from `links = { name = { ... } }` (links.cwt), with
    /// every field the scope engine needs (output/input scopes, prefix, from_data).
    pub link_inputs: Vec<LinkInput>,
    /// Top-level script folder names from `folders.cwt` (one per line). Drives
    /// which subdirectories of a mod/vanilla root are discovered; empty when the
    /// config ships no folders.cwt (discovery then falls back to the engine's
    /// built-in folder list).
    pub folders: Vec<String>,
    /// Lookup index over `aliases`, built by `reindex()`. Two-level map:
    /// `category → key → indices of every matching overload`. Lookups require
    /// only two borrowed-str probes with zero allocation on the hot path.
    pub alias_exact:
        std::collections::HashMap<String, std::collections::HashMap<String, Vec<usize>>>,
    /// Per-category alias metadata (the `<type>` patterns and `scope_field`),
    /// also built by `reindex()`.
    pub alias_categories: std::collections::HashMap<String, AliasCategoryIndex>,
    /// Lookup index over `types`, built by `reindex()`. Maps a type name to its
    /// index in `types`, so name lookups are O(1) instead of a linear scan.
    pub type_by_name: std::collections::HashMap<String, usize>,
    /// Lookup index over `enums`, built by `reindex()`. Maps an enum key to its
    /// index in `enums` for O(1) lookups.
    pub enum_by_name: std::collections::HashMap<String, usize>,
    /// Lookup index over `root_rules`, built by `reindex()`. Maps a type-rule
    /// name to its index in `root_rules`, so `find_rules_by_name` is O(1)
    /// instead of a linear scan per root child.
    pub type_rules_idx: std::collections::HashMap<String, usize>,
    /// Built by `reindex()`: lowercased effect/trigger alias key -> the
    /// `value_set[...]` namespace its body declares (e.g. `set_country_flag` ->
    /// `country_flag`). Used to collect dynamically-defined set members (flags,
    /// tokens, …) for completion. Aliases declaring multiple namespaces keep
    /// the first found.
    pub value_set_effects: std::collections::HashMap<String, String>,
    /// Built by `reindex()`: lowercased effect/trigger alias key -> the
    /// `(binding_field_key, namespace)` pairs declared by a NESTED field in its
    /// block body (e.g. `generate_character` -> `[("token_base", "character_token")]`,
    /// `set_country_flag` -> `[("flag", "country_flag")]`). Lets the value-set
    /// collector capture the value of the exact field bound to `value_set[ns]`
    /// instead of guessing from a fixed key list, so members under non-obvious keys
    /// (`token_base`, `id`, `legacy_id`, `array`, …) are still collected.
    pub value_set_effect_fields: std::collections::HashMap<String, Vec<(String, String)>>,
}

/// Scope/link config inputs (`scopes.cwt` / `links.cwt`). The types live in the
/// game crate next to `ScopeRegistry::from_config` (the scope graph's single
/// source of truth); re-exported here because the converter produces them and
/// `RuleSet` carries them.
pub use cwtools_game::scope_registry::{LinkInput, ScopeInput};

/// What kind of placeholder a parsed alias pattern contains.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum PatternKind {
    /// `<type>` or `<type.subtype>` — an instance of that type (subtype
    /// is advisory; only the base name is checked against the type index).
    Type,
    /// `enum[name]` or `complex_enum[name]` — a member of a named enum.
    Enum,
    /// `value[name]` or `value_set[name]` — a member of a named value set.
    Value,
}

/// Alias name pattern pre-parsed at ruleset build time.
///
/// An alias name like `modifier:production_speed_<building>_factor` or
/// `effect:set_country_flag_value[country_flag]` is split once into its
/// structural parts so the per-call `alias_pattern_matches` can skip the
/// string scanning.
#[derive(Debug, Clone, PartialEq)]
pub struct ParsedAliasPattern {
    /// Index into `RuleSet::aliases` for the corresponding rule.
    pub alias_idx: usize,
    /// Text before the placeholder (may be empty).
    pub prefix: String,
    /// Text after the placeholder (may be empty).
    pub suffix: String,
    /// What the placeholder represents.
    pub kind: PatternKind,
    /// The type/enum/value-set name inside the placeholder brackets.
    ///
    /// For `<type.subtype>` this stores the full `type.subtype` string; the
    /// base-type extraction (splitting on `.`) happens at match time.
    pub placeholder_name: String,
}

impl ParsedAliasPattern {
    /// Parse the `rest` portion of an alias name (the part after `category:`)
    /// into a `ParsedAliasPattern`. Returns `None` for patterns without a
    /// recognised placeholder (those go into the exact-match index instead).
    pub fn parse(rest: &str, alias_idx: usize) -> Option<Self> {
        if let Some(open) = rest.find('<') {
            let close = open + rest[open..].find('>')?;
            return Some(ParsedAliasPattern {
                alias_idx,
                prefix: rest[..open].to_string(),
                suffix: rest[close + 1..].to_string(),
                kind: PatternKind::Type,
                placeholder_name: rest[open + 1..close].to_string(),
            });
        }
        // Bracketed forms — check longer markers first so `enum[` does not
        // match inside `complex_enum[`. Pick the earliest match.
        // Store (open, inner, close, after) offsets only; resolve kind after
        // picking the earliest match so we never move the PatternKind during
        // the comparison loop.
        let markers: &[(&str, PatternKind)] = &[
            ("value_set[", PatternKind::Value),
            ("complex_enum[", PatternKind::Enum),
            ("value[", PatternKind::Value),
            ("enum[", PatternKind::Enum),
        ];
        let mut found: Option<(usize, usize, usize, usize, PatternKind)> = None;
        for (marker, kind) in markers {
            if let Some(open) = rest.find(marker) {
                let inner = open + marker.len();
                let close = inner + rest[inner..].find(']')?;
                let earlier = found.as_ref().is_none_or(|&(o, ..)| open < o);
                if earlier {
                    found = Some((open, inner, close, close + 1, *kind));
                }
            }
        }
        let (open, inner, close, after, kind) = found?;
        Some(ParsedAliasPattern {
            alias_idx,
            prefix: rest[..open].to_string(),
            suffix: rest[after..].to_string(),
            kind,
            placeholder_name: rest[inner..close].to_string(),
        })
    }
}

/// Per-category alias index entry (see `RuleSet::alias_categories`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AliasCategoryIndex {
    /// Aliases in this category whose name embeds a placeholder pattern.
    /// Pre-parsed at `reindex()` time so match loops skip per-call string scanning.
    pub parsed_patterns: Vec<ParsedAliasPattern>,
    /// Index of this category's `scope_field` alias, if any.
    pub scope_field_idx: Option<usize>,
}

/// Normalize a `.cwt` path pattern to its lowercase lookup key:
/// `\` -> `/`, strip surrounding `/`, lowercase. Produces exactly the same string
/// as `p.replace('\\', "/").trim_matches('/').to_lowercase()` but skips the
/// `replace` allocation on the common (Linux) no-backslash case.
fn normalize_path_lower(p: &str) -> String {
    if p.contains('\\') {
        p.replace('\\', "/").trim_matches('/').to_lowercase()
    } else {
        p.trim_matches('/').to_lowercase()
    }
}

impl Default for RuleSet {
    fn default() -> Self {
        Self::new()
    }
}

impl RuleSet {
    pub fn new() -> Self {
        Self {
            types: Vec::new(),
            aliases: Vec::new(),
            single_aliases: Vec::new(),
            enums: Vec::new(),
            complex_enums: Vec::new(),
            root_rules: Vec::new(),
            values: std::collections::HashMap::new(),
            modifiers: Vec::new(),
            scope_links: std::collections::HashSet::new(),
            scope_inputs: Vec::new(),
            link_inputs: Vec::new(),
            folders: Vec::new(),
            alias_exact: std::collections::HashMap::default(),
            alias_categories: std::collections::HashMap::new(),
            type_by_name: std::collections::HashMap::new(),
            enum_by_name: std::collections::HashMap::new(),
            type_rules_idx: std::collections::HashMap::new(),
            value_set_effects: std::collections::HashMap::new(),
            value_set_effect_fields: std::collections::HashMap::new(),
        }
    }

    /// Build the alias lookup indexes from `aliases`. Call once after all aliases
    /// are loaded and post-processed (names/order are stable after that).
    pub fn reindex(&mut self) {
        self.alias_exact.clear();
        self.alias_categories.clear();
        self.value_set_effects.clear();
        self.value_set_effect_fields.clear();
        // Which value_set namespace (if any) a rule tree declares.
        fn first_value_set_ns(rule: &RuleType) -> Option<&str> {
            fn of_field(f: &NewField) -> Option<&str> {
                match f {
                    NewField::VariableSetField(ns) => Some(ns.as_str()),
                    _ => None,
                }
            }
            match rule {
                RuleType::LeafRule { left, right } => of_field(left).or_else(|| of_field(right)),
                RuleType::LeafValueRule { right } => of_field(right),
                RuleType::NodeRule { left, rules } => of_field(left)
                    .or_else(|| rules.iter().find_map(|(rt, _)| first_value_set_ns(rt))),
                RuleType::ValueClauseRule { rules } | RuleType::SubtypeRule { rules, .. } => {
                    rules.iter().find_map(|(rt, _)| first_value_set_ns(rt))
                }
            }
        }
        // Every `<specific_key> = value_set[ns]` binding reachable in a rule tree,
        // as `(key, ns)` pairs (see `value_set_effect_fields`).
        fn collect_binding_fields(rule: &RuleType, out: &mut Vec<(String, String)>) {
            match rule {
                RuleType::LeafRule {
                    left: NewField::SpecificField(key),
                    right: NewField::VariableSetField(ns),
                } => out.push((key.to_ascii_lowercase(), ns.clone())),
                RuleType::NodeRule { left, rules } => {
                    if let NewField::SpecificField(key) = left {
                        for (rt, _) in rules {
                            if let RuleType::LeafValueRule {
                                right: NewField::VariableSetField(ns),
                            } = rt
                            {
                                out.push((key.to_ascii_lowercase(), ns.clone()));
                            }
                        }
                    }
                    for (rt, _) in rules {
                        collect_binding_fields(rt, out);
                    }
                }
                RuleType::ValueClauseRule { rules } | RuleType::SubtypeRule { rules, .. } => {
                    for (rt, _) in rules {
                        collect_binding_fields(rt, out);
                    }
                }
                _ => {}
            }
        }
        for (i, (name, (rule, _))) in self.aliases.iter().enumerate() {
            if let Some((cat, key)) = name.split_once(':')
                && (cat == "effect" || cat == "trigger")
            {
                if let Some(ns) = first_value_set_ns(rule) {
                    self.value_set_effects
                        .entry(key.to_ascii_lowercase())
                        .or_insert_with(|| ns.to_string());
                }
                let mut fields = Vec::new();
                collect_binding_fields(rule, &mut fields);
                if !fields.is_empty() {
                    self.value_set_effect_fields
                        .entry(key.to_ascii_lowercase())
                        .or_default()
                        .extend(fields);
                }
            }
            // Store under the original category+key AND the all-lowercase variant
            // so that game-file keys like `instantTextboxType` (mixed case) match
            // rule alias keys like `instantTextBoxType` (camelCase). Paradox
            // script keys are case-insensitive; aliases are no different.
            if let Some((cat, key)) = name.split_once(':') {
                self.alias_exact
                    .entry(cat.to_string())
                    .or_default()
                    .entry(key.to_string())
                    .or_default()
                    .push(i);
                let lower_cat = cat.to_ascii_lowercase();
                let lower_key = key.to_ascii_lowercase();
                if lower_cat != cat || lower_key != key {
                    self.alias_exact
                        .entry(lower_cat)
                        .or_default()
                        .entry(lower_key)
                        .or_default()
                        .push(i);
                }
            }
            if let Some((cat, rest)) = name.split_once(':') {
                let entry = self.alias_categories.entry(cat.to_string()).or_default();
                if rest == "scope_field" {
                    entry.scope_field_idx = Some(i);
                } else if let Some(parsed) = ParsedAliasPattern::parse(rest, i) {
                    entry.parsed_patterns.push(parsed);
                }
            }
        }
        for td in &mut self.types {
            td.path_options.paths_lower = td
                .path_options
                .paths
                .iter()
                .map(|p| normalize_path_lower(p))
                .collect();
            td.path_options.path_file_lower = td
                .path_options
                .path_file
                .as_deref()
                .map(|s| s.to_lowercase());
            td.path_options.path_ext_lower = td.path_options.path_extension.as_deref().map(|s| {
                let s = s.to_lowercase();
                s.strip_prefix('.').map(|t| t.to_string()).unwrap_or(s)
            });
        }
        for ce in &mut self.complex_enums {
            ce.path_options.paths_lower = ce
                .path_options
                .paths
                .iter()
                .map(|p| normalize_path_lower(p))
                .collect();
            ce.path_options.path_file_lower = ce
                .path_options
                .path_file
                .as_deref()
                .map(|s| s.to_lowercase());
            ce.path_options.path_ext_lower = ce.path_options.path_extension.as_deref().map(|s| {
                let s = s.to_lowercase();
                s.strip_prefix('.').map(|t| t.to_string()).unwrap_or(s)
            });
        }
        self.type_by_name.clear();
        for (i, td) in self.types.iter().enumerate() {
            self.type_by_name.insert(td.name.clone(), i);
        }
        self.enum_by_name.clear();
        for (i, e) in self.enums.iter().enumerate() {
            self.enum_by_name.insert(e.key.clone(), i);
        }
        self.type_rules_idx.clear();
        for (i, rr) in self.root_rules.iter().enumerate() {
            if let RootRule::TypeRule(name, _) = rr {
                // First writer wins — mirrors find_rules_by_name returning the
                // first TypeRule with a given name.
                self.type_rules_idx.entry(name.clone()).or_insert(i);
            }
        }
    }
}

/// A rule definition for a type (e.g. `ethos = { ... }`).
#[derive(Debug, Clone, PartialEq)]
pub struct TypeDefinition {
    pub name: String,
    pub name_field: Option<String>,
    pub path_options: PathOptions,
    pub subtypes: Vec<SubTypeDefinition>,
    pub type_key_filter: Option<(Vec<String>, bool)>,
    pub skip_root_key: Vec<SkipRootKey>,
    pub starts_with: Option<String>,
    pub type_per_file: bool,
    pub key_prefix: Option<String>,
    pub warning_only: bool,
    pub unique: bool,
    pub should_be_referenced: bool,
    pub localisation: Vec<TypeLocalisation>,
    pub graph_related_types: Vec<String>,
    pub modifiers: Vec<TypeModifier>,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct PathOptions {
    pub paths: Vec<String>,
    pub path_strict: bool,
    pub path_file: Option<String>,
    pub path_extension: Option<String>,
    /// Pre-computed lowercased path patterns, built by `RuleSet::reindex()`.
    pub paths_lower: Vec<String>,
    /// Pre-computed lowercased `path_file`, built by `RuleSet::reindex()`.
    pub path_file_lower: Option<String>,
    /// Pre-computed lowercased `path_extension` with leading `.` stripped, built by `RuleSet::reindex()`.
    pub path_ext_lower: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct SubTypeDefinition {
    pub name: String,
    pub display_name: Option<String>,
    pub abbreviation: Option<String>,
    pub rules: Vec<NewRule>,
    pub type_key_field: Option<String>,
    pub starts_with: Option<String>,
    pub push_scope: Option<String>,
    pub localisation: Vec<TypeLocalisation>,
    pub only_if_not: Vec<String>,
    pub modifiers: Vec<TypeModifier>,
    /// `## type_key_filter = X` (or `= { a b }`): the subtype is active when the
    /// instance's own node key is one of these values.
    pub type_key_filter: Vec<String>,
}

/// Whether a `SkipRootKey::MultipleKeys` rule matches when the root key IS one
/// of the listed keys (`Equals`, from a `==` directive) or is NOT (`NotEquals`,
/// from `<>`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MatchKind {
    Equals,
    NotEquals,
}

impl MatchKind {
    /// Build from the old "should_match" bool: `==` (true) -> Equals, else NotEquals.
    pub fn from_equals(is_equals: bool) -> Self {
        if is_equals {
            MatchKind::Equals
        } else {
            MatchKind::NotEquals
        }
    }

    /// Whether this is the `Equals` (`==`) kind.
    pub fn is_equals(self) -> bool {
        matches!(self, MatchKind::Equals)
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum SkipRootKey {
    SpecificKey(String),
    AnyKey,
    MultipleKeys(Vec<String>, MatchKind),
}

#[derive(Debug, Clone, PartialEq)]
pub struct TypeLocalisation {
    pub name: String,
    pub prefix: String,
    pub suffix: String,
    pub required: bool,
    pub optional: bool,
    pub explicit_field: Option<String>,
    pub replace_scopes: Option<ReplaceScopes>,
    pub primary: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct TypeModifier {
    pub prefix: String,
    pub suffix: String,
    pub category: String, // ModifierCategory simplified
    pub documentation: Option<String>,
    pub explicit: bool,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ReplaceScopes {
    pub root: Option<String>,
    pub this: Option<String>,
    pub froms: Vec<String>,
    pub prevs: Vec<String>,
}

/// A rule is a (RuleType, Options) pair.
pub type NewRule = (RuleType, Options);

#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, PartialEq)]
pub enum RuleType {
    NodeRule {
        left: NewField,
        rules: Vec<NewRule>,
    },
    LeafRule {
        left: NewField,
        right: NewField,
    },
    LeafValueRule {
        right: NewField,
    },
    ValueClauseRule {
        rules: Vec<NewRule>,
    },
    SubtypeRule {
        name: String,
        positive: bool,
        rules: Vec<NewRule>,
    },
}

#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, PartialEq)]
pub enum NewField {
    ValueField(ValueType),
    SpecificField(String),
    ScalarField,
    TypeField(TypeType),
    ScopeField(Vec<String>),
    LocalisationField {
        synced: bool,
        is_inline: bool,
    },
    FilepathField {
        prefix: Option<String>,
        extension: Option<String>,
    },
    IconField(String),
    AliasValueKeysField(String),
    AliasField(String),
    SingleAliasField(String),
    // SingleAliasClauseField removed: never constructed by the converter.
    VariableSetField(String),
    VariableGetField(String),
    VariableField {
        is_int: bool,
        is_32bit: bool,
        min: f64,
        max: f64,
    },
    ValueScopeMarkerField {
        is_int: bool,
        min: f64,
        max: f64,
    },
    ValueScopeField {
        is_int: bool,
        min: f64,
        max: f64,
    },
    MarkerField(Marker),
    // JominiGuiField removed: never constructed.
    IgnoreMarkerField,
    IgnoreField(Box<NewField>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum ValueType {
    Enum(String),
    Float {
        min: f64,
        max: f64,
    },
    Bool,
    Int {
        min: i32,
        max: i32,
    },
    Percent,
    Date,
    DateTime,
    Ck2Dna,
    Ck2DnaProperty,
    IrFamilyName,
    StlNameFormat(String),
    /// A recursive math-expression operand (HOI4 `set_variable` math blocks).
    /// As a leaf it is a number or variable reference; as a `{block}` it is a
    /// `value` base plus `mathexpr` operator keys, validated strictly so a
    /// mis-typed operator is flagged rather than silently treated as a new
    /// variable assignment. See `rule_core::validate_math_clause`.
    MathExpr,
}

#[derive(Debug, Clone, PartialEq)]
pub enum TypeType {
    Simple(String),
    Complex {
        prefix: String,
        name: String,
        suffix: String,
    },
}

#[derive(Debug, Clone, PartialEq)]
pub enum Marker {
    ColourField,
    IrCountryTag,
}

/// The label and direction of a reference declared via `## outgoingReferenceLabel`
/// (`Outgoing`) or `## incomingReferenceLabel` (`Incoming`).
#[derive(Debug, Clone, PartialEq)]
pub enum ReferenceDetail {
    Outgoing(String),
    Incoming(String),
}

#[derive(Debug, Clone, PartialEq)]
pub struct Options {
    pub min: i32,
    pub max: i32,
    pub strict_min: bool,
    pub leafvalue: bool,
    pub description: Option<String>,
    pub push_scope: Option<String>,
    pub replace_scopes: Option<ReplaceScopes>,
    pub severity: Option<Severity>,
    pub required_scopes: Vec<String>,
    pub comparison: bool,
    pub reference_details: Option<ReferenceDetail>,
    // key_required_quotes, value_required_quotes, type_hint removed:
    // always default-valued, no readers (quoted-key enforcement unimplemented).
    pub error_if_only_match: Option<String>,
    /// `## default_bool = yes|no`: the field's engine default. When the field is
    /// set to this value an info-level hint (CW282) notes it can be omitted.
    pub default_bool: Option<bool>,
}

impl Default for Options {
    fn default() -> Self {
        Self {
            min: 0,
            max: 1000,
            strict_min: true,
            leafvalue: false,
            description: None,
            push_scope: None,
            replace_scopes: None,
            severity: None,
            required_scopes: Vec::new(),
            comparison: false,
            reference_details: None,
            error_if_only_match: None,
            default_bool: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub enum Severity {
    Error,
    Warning,
    Information,
    Hint,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EnumDefinition {
    pub key: String,
    pub description: String,
    pub values: Vec<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub struct ComplexEnumDef {
    pub name: String,
    pub description: String,
    pub path_options: PathOptions,
    pub name_tree: ComplexEnumNameTree,
    pub start_from_root: bool,
}

/// Represents the `name = { ... }` subtree inside a complex_enum definition.
/// This captures the key-path structure used to extract enum member names from files.
#[derive(Debug, Clone, PartialEq)]
pub enum ComplexEnumNameTree {
    /// No name block was present.
    Empty,
    /// A list of leaf/node entries describing the name-extraction path.
    Entries(Vec<ComplexEnumNameTreeEntry>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum ComplexEnumNameTreeEntry {
    /// A leaf entry: the key under which the enum name lives.
    /// `is_name` is true when the value is `enum_name`/`this`.
    Leaf { key: String, is_name: bool },
    /// A nested node entry: descend into `key` then recurse.
    Node {
        key: String,
        children: ComplexEnumNameTree,
    },
    /// A bare `enum_name` value inside a block (`stats = { enum_name }`):
    /// every bare value at this level of the target file is an enum member.
    BareName,
}

/// Root-level rule from a .cwt file.
#[allow(clippy::enum_variant_names)]
#[derive(Debug, Clone, PartialEq)]
pub enum RootRule {
    AliasRule(String, NewRule),
    SingleAliasRule(String, NewRule),
    TypeRule(String, NewRule),
}
