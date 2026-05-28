/// Parsed result from a .cwt file or set of files.
#[derive(Debug, Clone, PartialEq)]
pub struct RuleSet {
    pub types: Vec<TypeDefinition>,
    pub aliases: Vec<(String, NewRule)>,
    pub single_aliases: Vec<(String, NewRule)>,
    pub enums: Vec<EnumDefinition>,
    pub complex_enums: Vec<ComplexEnumDef>,
    pub root_rules: Vec<RootRule>,
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
    NodeRule { left: NewField, rules: Vec<NewRule> },
    LeafRule { left: NewField, right: NewField },
    LeafValueRule { right: NewField },
    ValueClauseRule { rules: Vec<NewRule> },
    SubtypeRule { name: String, positive: bool, rules: Vec<NewRule> },
}

#[derive(Debug, Clone, PartialEq)]
pub enum NewField {
    ValueField(ValueType),
    SpecificField(String),
    ScalarField,
    TypeField(TypeType),
    ScopeField(Vec<String>),
    LocalisationField { synced: bool, is_inline: bool },
    FilepathField { prefix: Option<String>, extension: Option<String> },
    IconField(String),
    AliasValueKeysField(String),
    AliasField(String),
    SingleAliasField(String),
    SingleAliasClauseField(String, String),
    SubtypeField(String, bool, Vec<NewRule>),
    VariableSetField(String),
    VariableGetField(String),
    VariableField { is_int: bool, is_32bit: bool, min: f64, max: f64 },
    ValueScopeMarkerField { is_int: bool, min: f64, max: f64 },
    ValueScopeField { is_int: bool, min: f64, max: f64 },
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
    Complex { prefix: String, name: String, suffix: String },
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
    pub name_tree: String, // placeholder: would be AST node
    pub start_from_root: bool,
}

/// Root-level rule from a .cwt file.
#[derive(Debug, Clone, PartialEq)]
pub enum RootRule {
    AliasRule(String, NewRule),
    SingleAliasRule(String, NewRule),
    TypeRule(String, NewRule),
}
