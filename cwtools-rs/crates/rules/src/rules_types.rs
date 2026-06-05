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
    pub values: Vec<(String, Vec<String>)>,
    /// Names from a top-level `modifiers = { name = category ... }` block. These
    /// are the valid keys for `alias_name[modifier]` slots (modifier contexts).
    pub modifiers: Vec<String>,
    /// Link names from a top-level `links = { name = { ... } }` block (links.cwt).
    /// A from-data scope link (e.g. `character`, `state`, `owner`) can appear as a
    /// scope-switching key, so these are the valid keys for an `[cat:scope_field]`
    /// slot alongside scope commands and type instances. See [`crate`] consumers.
    pub scope_links: std::collections::HashSet<String>,
    /// Lookup index over `aliases`, built by `reindex()`. Maps a full alias name
    /// (`"cat:key"`) to the indices of every matching overload, so alias
    /// resolution is O(1) instead of a linear scan over all aliases per key.
    pub alias_exact: std::collections::HashMap<String, Vec<usize>>,
    /// Per-category alias metadata (the `<type>` patterns and `scope_field`),
    /// also built by `reindex()`.
    pub alias_categories: std::collections::HashMap<String, AliasCategoryIndex>,
    /// Lookup index over `types`, built by `reindex()`. Maps a type name to its
    /// index in `types`, so name lookups are O(1) instead of a linear scan.
    pub type_by_name: std::collections::HashMap<String, usize>,
    /// Lookup index over `enums`, built by `reindex()`. Maps an enum key to its
    /// index in `enums` for O(1) lookups.
    pub enum_by_name: std::collections::HashMap<String, usize>,
}

/// Per-category alias index entry (see `RuleSet::alias_categories`).
#[derive(Debug, Clone, PartialEq, Default)]
pub struct AliasCategoryIndex {
    /// Indices of aliases in this category whose name embeds a `<type>` pattern
    /// (e.g. `trigger:<scripted_trigger>`, `modifier:production_speed_<building>_factor`).
    pub type_pattern_idxs: Vec<usize>,
    /// Index of this category's `scope_field` alias, if any.
    pub scope_field_idx: Option<usize>,
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
            values: Vec::new(),
            modifiers: Vec::new(),
            scope_links: std::collections::HashSet::new(),
            alias_exact: std::collections::HashMap::new(),
            alias_categories: std::collections::HashMap::new(),
            type_by_name: std::collections::HashMap::new(),
            enum_by_name: std::collections::HashMap::new(),
        }
    }

    /// Build the alias lookup indexes from `aliases`. Call once after all aliases
    /// are loaded and post-processed (names/order are stable after that).
    pub fn reindex(&mut self) {
        self.alias_exact.clear();
        self.alias_categories.clear();
        for (i, (name, _)) in self.aliases.iter().enumerate() {
            // Store under the original name AND the all-lowercase variant so
            // that game-file keys like `instantTextboxType` (mixed case) match
            // rule alias keys like `instantTextBoxType` (camelCase). Paradox
            // script keys are case-insensitive; aliases are no different.
            self.alias_exact.entry(name.clone()).or_default().push(i);
            let lower = name.to_ascii_lowercase();
            if lower != *name {
                self.alias_exact.entry(lower).or_default().push(i);
            }
            if let Some((cat, rest)) = name.split_once(':') {
                let entry = self.alias_categories.entry(cat.to_string()).or_default();
                if rest == "scope_field" {
                    entry.scope_field_idx = Some(i);
                } else if rest.contains('<') || rest.contains('[') {
                    // A placeholder pattern: `<type>`, `<type.subtype>`,
                    // `value[set]`, `enum[name]` embedded in the alias name.
                    entry.type_pattern_idxs.push(i);
                }
            }
        }
        for td in &mut self.types {
            td.path_options.paths_lower = td.path_options.paths.iter().map(|p| {
                p.replace('\\', "/").trim_matches('/').to_lowercase()
            }).collect();
        }
        for ce in &mut self.complex_enums {
            ce.path_options.paths_lower = ce.path_options.paths.iter().map(|p| {
                p.replace('\\', "/").trim_matches('/').to_lowercase()
            }).collect();
        }
        self.type_by_name.clear();
        for (i, td) in self.types.iter().enumerate() {
            self.type_by_name.insert(td.name.clone(), i);
        }
        self.enum_by_name.clear();
        for (i, e) in self.enums.iter().enumerate() {
            self.enum_by_name.insert(e.key.clone(), i);
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

#[derive(Debug, Clone, PartialEq)]
pub struct PathOptions {
    pub paths: Vec<String>,
    pub path_strict: bool,
    pub path_file: Option<String>,
    pub path_extension: Option<String>,
    /// Pre-computed lowercased path patterns, built by `RuleSet::reindex()`.
    pub paths_lower: Vec<String>,
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

#[derive(Debug, Clone, PartialEq)]
pub enum SkipRootKey {
    SpecificKey(String),
    AnyKey,
    MultipleKeys(Vec<String>, bool),
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
    SingleAliasClauseField(String, String),
    SubtypeField(String, bool, Vec<NewRule>),
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
    JominiGuiField,
    IgnoreMarkerField,
    IgnoreField(Box<NewField>),
}

#[derive(Debug, Clone, PartialEq)]
pub enum ValueType {
    Enum(String),
    Float { min: f64, max: f64 },
    Bool,
    Int { min: i32, max: i32 },
    Percent,
    Date,
    DateTime,
    Ck2Dna,
    Ck2DnaProperty,
    IrFamilyName,
    StlNameFormat(String),
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
    pub reference_details: Option<(bool, String)>,
    pub key_required_quotes: bool,
    pub value_required_quotes: bool,
    pub type_hint: Option<(String, bool)>,
    pub error_if_only_match: Option<String>,
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
            key_required_quotes: false,
            value_required_quotes: false,
            type_hint: None,
            error_if_only_match: None,
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
}

/// Root-level rule from a .cwt file.
#[derive(Debug, Clone, PartialEq)]
pub enum RootRule {
    AliasRule(String, NewRule),
    SingleAliasRule(String, NewRule),
    TypeRule(String, NewRule),
}
