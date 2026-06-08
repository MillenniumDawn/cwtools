use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};

use cwtools_file_manager::file_manager::{FileManager, FileManagerConfig};
use cwtools_info::TypeIndex;
use cwtools_info::{
    PositionElement, ReferenceHint, TypeInstance, element_at_position, info_at_position,
};
use cwtools_parser::ast::{ParseError, ParsedFile};
use cwtools_parser::parser::parse_string;
use cwtools_rules::rules_types::{NewField, RootRule, RuleSet, RuleType, TypeType, ValueType};
use cwtools_rules::ruleset_loader::load_ruleset_from_dir;
use cwtools_string_table::string_table::StringTable;
use cwtools_validation::{
    ValidationError, build_enum_map, build_scope_registry_arc, validate_ast_with_loc_prebuilt,
};

mod symbols;
mod workspace_cache;

/// Build the set of valid modifier names for `alias_name[modifier]` slots from the
/// ruleset's `modifiers = { ... }` block. Templated entries like
/// `production_speed_<building>_factor` / `<ideology>_drift` are expanded against
/// the type index, one per instance. Mirrors the CLI so the extension and CLI
/// agree on what counts as a modifier.
fn build_modifier_keys(ruleset: &RuleSet, type_index: &TypeIndex) -> HashSet<String> {
    let mut mk = HashSet::new();
    for m in &ruleset.modifiers {
        match (m.find('<'), m.find('>')) {
            (Some(open), Some(close)) if open < close => {
                let tn = &m[open + 1..close];
                let pre = &m[..open];
                let suf = &m[close + 1..];
                for (_uri, inst) in type_index.instances(tn) {
                    mk.insert(format!("{}{}{}", pre, inst.name, suf));
                }
            }
            _ => {
                mk.insert(m.clone());
            }
        }
    }
    mk
}

/// Map the engine `Game` to the localization crate's `Game` enum.
fn engine_to_loc_game(game: Option<cwtools_game::constants::Game>) -> cwtools_localization::Game {
    use cwtools_game::constants::Game as G;
    use cwtools_localization::Game as LG;
    match game {
        Some(G::Hoi4) => LG::HOI4,
        Some(G::Stellaris) => LG::Stellaris,
        Some(G::Eu4) => LG::EU4,
        Some(G::Ck3) => LG::CK3,
        Some(G::Ir) => LG::IR,
        Some(G::Vic3) => LG::VIC3,
        Some(G::Eu5) => LG::EU5,
        _ => LG::Generic,
    }
}

/// Convert a loc-file diagnostic into a `ValidationError` so it shares the
/// `validation_error_to_diagnostic` rendering path. Loc positions are 1-based;
/// `ValidationError.col` is 0-based (used directly by the renderer).
fn loc_diag_to_validation_error(d: &cwtools_localization::LocDiagnostic) -> ValidationError {
    let severity = match d.severity {
        cwtools_localization::LocSeverity::Error => cwtools_validation::ErrorSeverity::Error,
        cwtools_localization::LocSeverity::Warning => cwtools_validation::ErrorSeverity::Warning,
        cwtools_localization::LocSeverity::Information => {
            cwtools_validation::ErrorSeverity::Information
        }
    };
    ValidationError {
        message: d.message.clone(),
        severity,
        line: d.line as u32,
        col: d.col.saturating_sub(1) as u16,
        file: d.file.clone(),
        code: Some(d.code.to_string()),
    }
}

/// Index a base-game ("vanilla") install into per-type instances, ready to merge
/// into the workspace TypeIndex. Mirrors the CLI's `index_game_dir` / `--vanilla`:
/// for a game root, `FileManagerConfig::default()` already covers the standard
/// layout (common/, gfx/, events/, …). The discovered ASTs are used directly (no
/// re-parse) because vanilla files are only indexed, never validated.
fn index_vanilla_dir(
    dir: &std::path::Path,
    ruleset: &RuleSet,
    table: &StringTable,
) -> HashMap<String, Vec<TypeInstance>> {
    let config = FileManagerConfig {
        root: dir.to_path_buf(),
        ..Default::default()
    };
    let mut mgr = FileManager::with_string_table(config, table.clone());
    let index = match mgr.discover_and_parse() {
        Ok(files) => cwtools_info::index_discovered_files(files, ruleset, table, None),
        Err(_) => TypeIndex::new(),
    };
    // Drop the per-instance file_uri; the merge slot only needs the instances.
    index
        .map
        .into_iter()
        .map(|(k, v)| (k, v.into_iter().map(|(_, inst)| inst).collect()))
        .collect()
}

/// Best-effort discovery of a base-game install for `game`, checking the usual
/// OS cache directory for persistent caches, used when the client doesn't pass
/// `cacheDir`. Honors `XDG_CACHE_HOME`/`LOCALAPPDATA`, then `~/.cache` (Linux) or
/// `~/Library/Caches` (macOS), and finally the temp dir.
fn default_cache_dir() -> Option<std::path::PathBuf> {
    use std::path::PathBuf;
    if let Ok(x) = std::env::var("XDG_CACHE_HOME")
        && !x.is_empty() {
            return Some(PathBuf::from(x).join("cwtools"));
        }
    if let Ok(la) = std::env::var("LOCALAPPDATA")
        && !la.is_empty() {
            return Some(PathBuf::from(la).join("cwtools"));
        }
    if let Ok(home) = std::env::var("HOME")
        && !home.is_empty() {
            let home = PathBuf::from(home);
            #[cfg(target_os = "macos")]
            {
                return Some(home.join("Library/Caches/cwtools"));
            }
            #[cfg(not(target_os = "macos"))]
            {
                return Some(home.join(".cache/cwtools"));
            }
        }
    Some(std::env::temp_dir().join("cwtools"))
}

/// Steam library locations across platforms. Returns the first existing dir.
/// Used as a fallback when the client passes neither `vanilla` nor `vanillaCache`.
fn discover_vanilla_dir(game: &str) -> Option<std::path::PathBuf> {
    // Map our game id to the Steam "common" install folder name.
    let folder = match game {
        "hoi4" => "Hearts of Iron IV",
        "stellaris" => "Stellaris",
        "eu4" => "Europa Universalis IV",
        "ck2" => "Crusader Kings II",
        "ck3" => "Crusader Kings III",
        "vic2" => "Victoria 2",
        "vic3" => "Victoria 3",
        "ir" => "ImperatorRome",
        _ => return None,
    };

    let home = std::env::var_os("HOME").map(std::path::PathBuf::from);
    // Steam library roots to probe (Linux, macOS, Windows).
    let mut roots: Vec<std::path::PathBuf> = Vec::new();
    if let Some(h) = &home {
        roots.push(h.join(".steam/steam/steamapps/common"));
        roots.push(h.join(".local/share/Steam/steamapps/common"));
        roots.push(h.join("Library/Application Support/Steam/steamapps/common"));
    }
    roots.push(std::path::PathBuf::from(
        "C:/Program Files (x86)/Steam/steamapps/common",
    ));
    roots.push(std::path::PathBuf::from(
        "C:/Program Files/Steam/steamapps/common",
    ));

    roots
        .into_iter()
        .map(|r| r.join(folder))
        .find(|p| p.is_dir())
}

// ── Custom LSP notification types ─────────────────────────────────────────────

/// `loadingBar` server→client notification (S→C).
/// Payload: `{ "enable": bool, "value": string }`.
/// Used to drive the extension's status-bar progress indicator.
enum LoadingBar {}
impl tower_lsp::lsp_types::notification::Notification for LoadingBar {
    type Params = serde_json::Value;
    const METHOD: &'static str = "loadingBar";
}

/// `updateFileList` server→client notification (S→C).
/// Payload: `{ "fileList": [{ "scope": string, "uri": string, "logicalpath": string }] }`.
/// Used to populate the extension's file explorer tree view.
enum UpdateFileList {}
impl tower_lsp::lsp_types::notification::Notification for UpdateFileList {
    type Params = serde_json::Value;
    const METHOD: &'static str = "updateFileList";
}

/// Server state.
struct DocumentState {
    /// file URI -> parsed document
    documents: Mutex<HashMap<String, ParsedDoc>>,
    /// loaded .cwt ruleset
    ruleset: Mutex<Option<RuleSet>>,
    /// shared string table
    string_table: StringTable,
    /// game language from init options
    language: Mutex<String>,
    /// symbol index for goto-definition and references
    symbol_index: Mutex<symbols::SymbolIndex>,
    /// computed info service for type/references/definitions. `RwLock` so the
    /// full-workspace pass-2 validation can share a single read guard across
    /// rayon threads, and the many read-only consumers (hover, completion,
    /// document-symbol, export fingerprinting, validation) don't serialize.
    info_service: parking_lot::RwLock<cwtools_info::InfoService>,
    /// workspace folder URI captured from initialize params
    workspace_uri: Mutex<Option<String>>,
    /// pre-generated base-game type instances (from a vanilla cache OR a live
    /// index of `vanilla_dir`), merged into the workspace index so the editor
    /// resolves base-game references.
    vanilla_index: Mutex<Option<HashMap<String, Vec<cwtools_info::TypeInstance>>>>,
    /// base-game install dir (from the `vanilla` init option, or auto-discovered).
    /// Indexed lazily into `vanilla_index` on the first full-workspace scan.
    vanilla_dir: Mutex<Option<std::path::PathBuf>>,
    /// cached modifier-key set; rebuilt after ruleset load and after each full
    /// workspace scan when the type index is complete.
    modifier_keys: parking_lot::RwLock<HashSet<String>>,
    /// loc-key index (workspace + vanilla) for CW100/CW122 on config files and
    /// for scope-aware loc-command checks. Rebuilt on each full workspace scan.
    loc_index: parking_lot::RwLock<Option<cwtools_localization::LocIndex>>,
    /// languages to validate loc against, from the `localisationLanguages` init
    /// option. `None` = all languages with data (the default). When set, the
    /// missing-translation check and per-file loc checks are scoped to these,
    /// so an english-targeted mod isn't flagged for every other language vanilla
    /// happens to ship.
    loc_languages: Mutex<Option<Vec<cwtools_localization::Lang>>>,
    /// Writable directory for persistent caches (from the `cacheDir` init
    /// option, else an OS cache dir). The base-game type index is cached here
    /// keyed by game + version, so it isn't re-parsed on every startup.
    cache_dir: Mutex<Option<std::path::PathBuf>>,
    /// Monotonic edit counter, bumped on every `did_change`. A debounced
    /// validation captures the value at spawn time; the cross-file dependent
    /// sweep bails the moment a newer edit lands, so concurrent sweeps collapse
    /// into the latest one instead of stacking up and double-validating.
    edit_generation: AtomicU64,
    /// Extra filename glob patterns to skip during the workspace scan (on top
    /// of the engine baseline like Changelog.txt / README.md). Sourced from
    /// `ignoreFilePatterns` in `initializationOptions` and the
    /// `workspace/didChangeConfiguration` payload.
    ignore_file_patterns: parking_lot::RwLock<Vec<String>>,
    /// Extra directory glob patterns to skip during the workspace scan. Sourced
    /// from `ignoreDirectories` in `initializationOptions` and
    /// `workspace/didChangeConfiguration`.
    ignore_dir_patterns: parking_lot::RwLock<Vec<String>>,
    /// Per open document, the set of lowercased identifier-like tokens it
    /// mentions (keys + string values from its parsed AST). Used by the
    /// dependent sweep to revalidate only the open docs that actually reference a
    /// changed export, instead of every open doc. A SOUND OVER-APPROXIMATION:
    /// when a doc's token set is missing, it's always included. Updated on
    /// did_open / did_change, removed on did_close.
    doc_tokens: parking_lot::RwLock<HashMap<String, HashSet<String>>>,
}

struct ParsedDoc {
    version: i32,
    text: String,
    /// Shared so the cross-file dependent sweep can validate against it without
    /// re-parsing (an `Arc` clone instead of a full re-parse per open file).
    ast: Option<Arc<ParsedFile>>,
}

impl DocumentState {
    fn new() -> Self {
        Self {
            documents: Mutex::new(HashMap::new()),
            ruleset: Mutex::new(None),
            string_table: StringTable::new(),
            language: Mutex::new("paradox".to_string()),
            symbol_index: Mutex::new(symbols::SymbolIndex::new()),
            info_service: parking_lot::RwLock::new(cwtools_info::InfoService::new()),
            workspace_uri: Mutex::new(None),
            vanilla_index: Mutex::new(None),
            vanilla_dir: Mutex::new(None),
            modifier_keys: parking_lot::RwLock::new(HashSet::new()),
            loc_index: parking_lot::RwLock::new(None),
            loc_languages: Mutex::new(None),
            cache_dir: Mutex::new(None),
            edit_generation: AtomicU64::new(0),
            ignore_file_patterns: parking_lot::RwLock::new(Vec::new()),
            ignore_dir_patterns: parking_lot::RwLock::new(Vec::new()),
            doc_tokens: parking_lot::RwLock::new(HashMap::new()),
        }
    }
}

struct Backend {
    client: Client,
    state: Arc<DocumentState>,
}

/// Debounce window for `did_change`: a burst of keystrokes within this window
/// coalesces into a single validation. Short enough to feel live, long enough
/// to skip the per-keystroke re-parse that made large files lag.
const DEBOUNCE_MS: u64 = 250;

// ── Custom notification stubs ─────────────────────────────────────────────────

// NOT PORTED — code-actions, pre-trigger refactor, techGraph / event-graph.
// See the F# LanguageFeatures.fs module if these are needed later.
//   - getEmbeddedMetadata: per-file metadata bundle sent to the extension on
//     open (F# LanguageFeatures.getEmbeddedMetadata).  Low priority until the
//     extension side is ported.

impl Backend {
    /// Called when the VS Code extension tells us the user switched to a file.
    /// We receive it but don't act on it yet.
    async fn on_did_focus_file(&self, _params: Value) {
        // C→S: accept silently.
    }
}

// ── Hover helpers ─────────────────────────────────────────────────────────────

/// Pull `ignoreFilePatterns` and `ignoreDirectories` arrays out of a
/// `serde_json::Value` (the `initializationOptions` payload and the
/// `workspace/didChangeConfiguration` payload share the same shape).
/// Returns the two lists. Filters non-string and empty entries.
fn extract_ignore_patterns(opts: &Value) -> (Vec<String>, Vec<String>) {
    let mut files = Vec::new();
    let mut dirs = Vec::new();
    if let Some(arr) = opts.get("ignoreFilePatterns").and_then(|v| v.as_array()) {
        for v in arr {
            if let Some(s) = v.as_str()
                && !s.is_empty()
            {
                files.push(s.to_string());
            }
        }
    }
    if let Some(arr) = opts.get("ignoreDirectories").and_then(|v| v.as_array()) {
        for v in arr {
            if let Some(s) = v.as_str()
                && !s.is_empty()
            {
                dirs.push(s.to_string());
            }
        }
    }
    (files, dirs)
}

/// Tiny glob match supporting `*` (any chars) and `?` (single char), matching
/// the semantics of `cwtools_file_manager::glob_match`. Kept inline to avoid a
/// new public re-export from the file_manager crate.
fn lsp_glob_match(pattern: &str, text: &str) -> bool {
    if let Some(suffix) = pattern.strip_prefix('*') {
        if suffix.is_empty() {
            return true;
        }
        return text.ends_with(suffix);
    }
    if let Some(prefix) = pattern.strip_suffix('*') {
        return text.starts_with(prefix);
    }
    let p: Vec<char> = pattern.chars().collect();
    let t: Vec<char> = text.chars().collect();
    let (m, n) = (p.len(), t.len());
    let mut dp = vec![vec![false; n + 1]; m + 1];
    dp[0][0] = true;
    for i in 1..=m {
        if p[i - 1] == '*' {
            dp[i][0] = dp[i - 1][0];
        }
    }
    for i in 1..=m {
        for j in 1..=n {
            if p[i - 1] == '*' {
                dp[i][j] = dp[i - 1][j] || dp[i][j - 1];
            } else if p[i - 1] == '?' || p[i - 1] == t[j - 1] {
                dp[i][j] = dp[i - 1][j - 1];
            }
        }
    }
    dp[m][n]
}

/// Derive the logical path (relative to mod root) from a file:// URI and the
/// workspace root URI.  Falls back to the raw path if the workspace prefix
/// cannot be stripped.
fn logical_path_from_uri(uri: &str, workspace_uri: &Option<String>) -> String {
    let path = uri.strip_prefix("file://").unwrap_or(uri);
    if let Some(ws) = workspace_uri {
        let ws_path = ws.strip_prefix("file://").unwrap_or(ws.as_str());
        // Strip leading slash-terminated prefix
        let prefix = ws_path.trim_end_matches('/');
        if let Some(rel) = path.strip_prefix(prefix) {
            return rel.trim_start_matches('/').to_string();
        }
    }
    // Fallback: use just the filename portion
    path.to_string()
}

/// Build a Markdown hover string from a PositionInfo + optional rule options.
fn build_hover_markdown(
    element: &PositionElement,
    hint: &ReferenceHint,
    ruleset: Option<&RuleSet>,
) -> String {
    // Try to find a matching rule's description and scope info for the element key.
    let (rule_desc, rule_scopes) = match (ruleset, element) {
        (Some(rules), PositionElement::Leaf { key, .. })
        | (Some(rules), PositionElement::Node { key }) => find_rule_description(rules, key),
        _ => (None, Vec::new()),
    };

    let mut parts: Vec<String> = Vec::new();

    // Primary classification
    match hint {
        ReferenceHint::TypeRef { type_name, value } => {
            parts.push(format!(
                "**Type reference** — `{}` (`{}`)",
                value, type_name
            ));
        }
        ReferenceHint::EnumRef { enum_name, value } => {
            parts.push(format!(
                "**Enum value** — `{}` (member of `{}`)",
                value, enum_name
            ));
        }
        ReferenceHint::LocRef { key } => {
            parts.push(format!("**Localisation key** — `{}`", key));
        }
        ReferenceHint::FileRef { path } => {
            parts.push(format!("**File path** — `{}`", path));
        }
        ReferenceHint::ScopeName { name } => {
            parts.push(format!("**Scope** — `{}`", name));
        }
        ReferenceHint::Unknown => {
            // Fall back to the raw element description
            match element {
                PositionElement::Node { key } => {
                    parts.push(format!("**Node** — `{}`", key));
                }
                PositionElement::Leaf { key, value } => {
                    parts.push(format!("**Field** — `{} = {}`", key, value));
                }
                PositionElement::LeafValue { value } => {
                    parts.push(format!("**Value** — `{}`", value));
                }
            }
        }
    }

    // Append rule description if found
    if let Some(desc) = &rule_desc {
        parts.push(format!("\n{}", desc));
    }

    // Append required scopes if any
    if !rule_scopes.is_empty() {
        parts.push(format!("\n**Required scopes**: {}", rule_scopes.join(", ")));
    }

    parts.join("\n\n")
}

/// Walk root_rules for a leaf rule whose left field matches `key` and return
/// the Options.description (and required_scopes) if found.
fn find_rule_description(rules: &RuleSet, key: &str) -> (Option<String>, Vec<String>) {
    for root_rule in &rules.root_rules {
        let (_, (rule_type, _)) = match root_rule {
            RootRule::TypeRule(n, r) => (n, r),
            RootRule::AliasRule(n, r) => (n, r),
            RootRule::SingleAliasRule(n, r) => (n, r),
        };
        let child_rules = match rule_type {
            RuleType::NodeRule { rules, .. } => rules.as_slice(),
            _ => continue,
        };
        for (inner_type, opts) in child_rules {
            match inner_type {
                RuleType::LeafRule {
                    left: NewField::SpecificField(k),
                    ..
                }
                | RuleType::NodeRule {
                    left: NewField::SpecificField(k),
                    ..
                }
                    if k.eq_ignore_ascii_case(key) => {
                        return (opts.description.clone(), opts.required_scopes.clone());
                    }
                _ => {}
            }
        }
    }
    (None, Vec::new())
}

// ── Completion context helpers ────────────────────────────────────────────────

/// Walk the AST to find the cursor's enclosing node key path.
/// Returns the list of ancestor node keys from outermost to innermost.
///
/// Limitation: this is a linear scan; aliases and deeply nested rules are not
/// fully resolved.  When context can't be determined we fall back to the flat
/// global list.
fn enclosing_key_path(ast: &ParsedFile, line: u32, col: u16, table: &StringTable) -> Vec<String> {
    let target = cwtools_parser::ast::SourcePos { line, col };
    let mut path = Vec::new();
    collect_enclosing_path(&ast.root_children, &ast.arena, &target, table, &mut path);
    path
}

fn collect_enclosing_path(
    children: &[cwtools_parser::ast::Child],
    arena: &cwtools_parser::ast::Arena,
    target: &cwtools_parser::ast::SourcePos,
    table: &StringTable,
    path: &mut Vec<String>,
) -> bool {
    use cwtools_parser::ast::{Child, Value};

    for child in children {
        match child {
            Child::Node(idx) => {
                let node = &arena.nodes[*idx as usize];
                if pos_in_range_simple(target, &node.pos) {
                    let key = table.get_string(node.key.normal).unwrap_or_default();
                    path.push(key);
                    if collect_enclosing_path(&node.children, arena, target, table, path) {
                        return true;
                    }
                    // cursor is in this node but not a child — we're done
                    return true;
                }
            }
            Child::Leaf(idx) => {
                let leaf = &arena.leaves[*idx as usize];
                if let Value::Clause(ch) = &leaf.value
                    && pos_in_range_simple(target, &leaf.pos) {
                        let key = table.get_string(leaf.key.normal).unwrap_or_default();
                        path.push(key);
                        if collect_enclosing_path(ch, arena, target, table, path) {
                            return true;
                        }
                        return true;
                    }
            }
            _ => {}
        }
    }
    false
}

fn pos_in_range_simple(
    pos: &cwtools_parser::ast::SourcePos,
    range: &cwtools_parser::ast::SourceRange,
) -> bool {
    let s = &range.start;
    let e = &range.end;
    if pos.line < s.line || pos.line > e.line {
        return false;
    }
    if pos.line == s.line && pos.col < s.col {
        return false;
    }
    if pos.line == e.line && pos.col > e.col {
        return false;
    }
    true
}

/// Given an enclosing key path and a ruleset, find the applicable child rules.
///
/// Walks root_rules: for the outermost key (the type block) it finds a
/// TypeRule whose name matches a type definition that covers `logical_path`;
/// then descends the rule tree following the remaining path elements.
///
/// Returns the slice of child (RuleType, Options) pairs that apply at the
/// cursor's level, or None when no match is found.
fn rules_for_context<'a>(
    ruleset: &'a RuleSet,
    key_path: &[String],
    logical_path: &str,
) -> Option<&'a [(RuleType, cwtools_rules::rules_types::Options)]> {
    if key_path.is_empty() {
        // Top-level context: return all rules from all type rules for this path
        // (no single slice, caller handles)
        return None;
    }

    // Find a TypeRule whose corresponding TypeDef covers logical_path and
    // whose top-level key matches key_path[0].
    let _top_key = &key_path[0];
    for root_rule in &ruleset.root_rules {
        let (type_name, (rule_type, _)) = match root_rule {
            RootRule::TypeRule(n, r) => (n, r),
            _ => continue,
        };
        if let Some(&idx) = ruleset.type_by_name.get(type_name) {
            let td = &ruleset.types[idx];
            // Check path
            if !cwtools_info_path_check(&td.path_options, logical_path) {
                continue;
            }
            // Check the key matches (type_key_filter or starts_with)
            // We check the top-level key against the type's skip_root_key stack.
            // Simple case: no skip_root_key, so key_path[0] IS the instance.
            // With skip_root_key we'd need to look for key_path[1] etc.; skip for now.
            // For path[1..] we descend into the NodeRule's child rules.
            if let RuleType::NodeRule { rules, .. } = rule_type {
                // If there's only one level, return these rules directly
                if key_path.len() == 1 {
                    return Some(rules);
                }
                // Descend further into nested rules
                return descend_rules(rules, &key_path[1..]);
            }
        }
    }

    // Also try alias rules (the cursor might be inside an alias block)
    for root_rule in &ruleset.root_rules {
        let (_, (rule_type, _)) = match root_rule {
            RootRule::AliasRule(n, r) => (n, r),
            RootRule::SingleAliasRule(n, r) => (n, r),
            _ => continue,
        };
        if let RuleType::NodeRule { rules, .. } = rule_type {
            if key_path.len() == 1 {
                return Some(rules);
            }
            if let Some(slice) = descend_rules(rules, &key_path[1..]) {
                return Some(slice);
            }
        }
    }

    None
}

fn descend_rules<'a>(
    rules: &'a [(RuleType, cwtools_rules::rules_types::Options)],
    remaining: &[String],
) -> Option<&'a [(RuleType, cwtools_rules::rules_types::Options)]> {
    if remaining.is_empty() {
        return Some(rules);
    }
    let next_key = &remaining[0];
    for (rule_type, _) in rules {
        match rule_type {
            RuleType::NodeRule {
                left: NewField::SpecificField(k),
                rules: inner,
                ..
            }
            | RuleType::NodeRule {
                left: NewField::AliasValueKeysField(k),
                rules: inner,
                ..
            }
                if k.eq_ignore_ascii_case(next_key) => {
                    return descend_rules(inner, &remaining[1..]);
                }
            _ => {}
        }
    }
    None
}

/// Thin wrapper around the info crate's path check (avoids re-exporting it).
fn cwtools_info_path_check(
    opts: &cwtools_rules::rules_types::PathOptions,
    logical_path: &str,
) -> bool {
    if opts.paths.is_empty() {
        return true;
    }
    let norm = logical_path.replace('\\', "/");
    let dir = match norm.rfind('/') {
        Some(idx) => &norm[..idx],
        None => "",
    };
    let dir_lower = dir.to_lowercase();
    for p in &opts.paths {
        let pat = p.replace('\\', "/");
        let pat = pat.trim_matches('/');
        let pat_lower = pat.to_lowercase();
        if opts.path_strict {
            if dir_lower == pat_lower {
                return true;
            }
        } else {
            let after = &dir_lower[std::cmp::min(pat_lower.len(), dir_lower.len())..];
            if dir_lower.starts_with(&pat_lower) && (after.is_empty() || after.starts_with('/')) {
                return true;
            }
        }
    }
    false
}

/// Parse a string into an LSP Url, falling back to a clone of `fallback` on error.
fn parse_uri(uri_str: impl AsRef<str>, fallback: &Url) -> Url {
    uri_str
        .as_ref()
        .parse()
        .unwrap_or_else(|_| fallback.clone())
}

/// Build context-aware completion items from the child rules at the cursor's
/// position.
///
/// Limitation: AliasField expansion is not fully recursive (that requires
/// following the alias chain, which can be large).  TypeField completions use
/// the TypeIndex from the InfoService.  ScopeField completions are placeholder
/// scope names.
fn completions_from_rules(
    rules: &[(RuleType, cwtools_rules::rules_types::Options)],
    ruleset: &RuleSet,
    info: &cwtools_info::InfoService,
    language: &str,
) -> Vec<CompletionItem> {
    let mut items: Vec<CompletionItem> = Vec::new();

    for (rule_type, opts) in rules {
        match rule_type {
            // A concrete key in the block
            RuleType::LeafRule {
                left: NewField::SpecificField(k),
                right,
            } => {
                let snippet = match right {
                    NewField::ValueField(ValueType::Bool) => {
                        // Insert a yes/no placeholder
                        Some(format!("{} = ${{1|yes,no|}}", k))
                    }
                    NewField::ValueField(ValueType::Enum(e)) => {
                        // Inline enum values if the list is short enough
                        let vals = enum_values_for(ruleset, e);
                        if !vals.is_empty() && vals.len() <= 20 {
                            let choices = vals.join(",");
                            Some(format!("{} = ${{1|{}|}}", k, choices))
                        } else {
                            None
                        }
                    }
                    _ => None,
                };
                items.push(CompletionItem {
                    label: k.clone(),
                    kind: Some(CompletionItemKind::FIELD),
                    detail: opts.description.clone(),
                    insert_text: snippet.clone(),
                    insert_text_format: if snippet.is_some() {
                        Some(InsertTextFormat::SNIPPET)
                    } else {
                        None
                    },
                    ..Default::default()
                });
            }
            // A node block key — generate snippet with required child fields pre-populated
            RuleType::NodeRule {
                left: NewField::SpecificField(k),
                rules: inner,
            } => {
                let snippet = generate_node_snippet(k, inner, ruleset);
                // Scope-aware sortText: if rule has required_scopes push it earlier (lower sort key).
                let sort = if !opts.required_scopes.is_empty() {
                    format!("0_{}", k)
                } else {
                    format!("1_{}", k)
                };
                items.push(CompletionItem {
                    label: k.clone(),
                    kind: Some(CompletionItemKind::STRUCT),
                    detail: opts.description.clone(),
                    insert_text: Some(snippet),
                    insert_text_format: Some(InsertTextFormat::SNIPPET),
                    sort_text: Some(sort),
                    ..Default::default()
                });
            }
            // An enum value at the leaf level
            RuleType::LeafValueRule {
                right: NewField::ValueField(ValueType::Enum(e)),
            } => {
                for v in enum_values_for(ruleset, e) {
                    items.push(CompletionItem {
                        label: v,
                        kind: Some(CompletionItemKind::ENUM_MEMBER),
                        detail: Some(format!("enum {}", e)),
                        ..Default::default()
                    });
                }
            }
            // A bare type reference value
            RuleType::LeafValueRule {
                right: NewField::TypeField(TypeType::Simple(t)),
            }
            | RuleType::LeafRule {
                right: NewField::TypeField(TypeType::Simple(t)),
                ..
            } => {
                for (_, inst) in info.type_index.instances(t) {
                    items.push(CompletionItem {
                        label: inst.name.clone(),
                        kind: Some(CompletionItemKind::REFERENCE),
                        detail: Some(format!("{} instance", t)),
                        ..Default::default()
                    });
                }
            }
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
                // Emit the keys of all alias:<cat> entries
                for (alias_name, _) in &ruleset.aliases {
                    if let Some(k) = alias_name.strip_prefix(&format!("{}:", cat)) {
                        items.push(CompletionItem {
                            label: k.to_string(),
                            kind: Some(CompletionItemKind::KEYWORD),
                            detail: Some(format!("alias {}", cat)),
                            ..Default::default()
                        });
                    }
                }
            }
            // Scope names
            RuleType::LeafRule {
                right: NewField::ScopeField(_),
                ..
            }
            | RuleType::LeafValueRule {
                right: NewField::ScopeField(_),
            } => {
                for scope in scope_names_for_game(language) {
                    items.push(CompletionItem {
                        label: scope.to_string(),
                        kind: Some(CompletionItemKind::VALUE),
                        detail: Some("scope".to_string()),
                        ..Default::default()
                    });
                }
            }
            // Boolean field at leaf value level
            RuleType::LeafValueRule {
                right: NewField::ValueField(ValueType::Bool),
            } => {
                for v in &["yes", "no"] {
                    items.push(CompletionItem {
                        label: v.to_string(),
                        kind: Some(CompletionItemKind::KEYWORD),
                        detail: Some("bool".to_string()),
                        ..Default::default()
                    });
                }
            }
            _ => {}
        }
    }

    items
}

fn enum_values_for(ruleset: &RuleSet, enum_name: &str) -> Vec<String> {
    if let Some(&idx) = ruleset.enum_by_name.get(enum_name) {
        return ruleset.enums[idx].values.clone();
    }
    Vec::new()
}

/// Build an LSP snippet body for a NodeRule, pre-populating required child fields
/// (those with cardinality min >= 1 and a SpecificField left-side).
///
/// Mirrors F# createSnippetForClause:346-390. Tab-stop numbering starts at 1.
fn generate_node_snippet(
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

/// Produce a snippet placeholder string for the right-hand side of a leaf rule.
fn leaf_right_placeholder(right: &NewField, tab_stop: u32, ruleset: &RuleSet) -> String {
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
        _ => format!("${{{}}}", tab_stop),
    }
}

/// Build root-level type snippets for types whose path matches `logical_path`.
///
/// When the cursor is at the top level of a file, offer a snippet for each
/// matching type.  Mirrors F# rootTypeItems:1077-1097: uses typeKeyFilter keys
/// as the block opener if set, otherwise the type name itself; also adds
/// subtype.typeKeyField alternatives.
fn root_type_snippets(ruleset: &RuleSet, logical_path: &str) -> Vec<CompletionItem> {
    let mut items = Vec::new();

    for td in &ruleset.types {
        if !cwtools_info_path_check(&td.path_options, logical_path) {
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
                && !openers.contains(tkf) {
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

/// Build best-effort localisation-key completions for .yml files.
///
/// Offers all known loc keys from the InfoService.  Inside a `[...]` data-
/// function block, offers scope/command names instead.  Best-effort only —
/// full CWTools loc completion (F# locComplete:208-243) would need the loc
/// database and scope tracking, which are not yet ported.
fn loc_completions(info: &cwtools_info::InfoService, language: &str) -> Vec<CompletionItem> {
    // Collect all top-level keys from all files as potential loc keys
    let mut items: Vec<CompletionItem> = info
        .files.values().flat_map(|fi| fi.top_level_keys.iter().map(|(k, _)| k.clone()))
        .collect::<std::collections::HashSet<_>>()
        .into_iter()
        .map(|k| CompletionItem {
            label: k.clone(),
            kind: Some(CompletionItemKind::TEXT),
            detail: Some("loc key".to_string()),
            ..Default::default()
        })
        .collect();

    // Offer scope names as data-function completions inside [...]
    for scope in scope_names_for_game(language) {
        items.push(CompletionItem {
            label: scope.to_string(),
            kind: Some(CompletionItemKind::FUNCTION),
            detail: Some("scope command".to_string()),
            ..Default::default()
        });
    }

    items
}

/// Best-effort scope name list for the current game.
fn scope_names_for_game(language: &str) -> &'static [&'static str] {
    match language {
        "hoi4" => &[
            "THIS",
            "ROOT",
            "PREV",
            "FROM",
            "OVERLORD",
            "FACTION_LEADER",
            "capital_scope",
            "owner",
        ],
        "stellaris" => &[
            "this",
            "root",
            "prev",
            "from",
            "owner",
            "controller",
            "space_owner",
            "space_controller",
            "solar_system",
        ],
        "eu4" => &[
            "THIS",
            "ROOT",
            "PREV",
            "FROM",
            "EMPEROR",
            "capital_scope",
            "owner",
            "controller",
        ],
        "ck3" => &["this", "root", "prev", "from", "liege", "employer", "host"],
        _ => &["this", "root", "prev", "from"],
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        // Distinctive banner so it's unmistakable in the Output panel WHICH server
        // is running. If you don't see this line, you're on an old/F# binary.
        self.client
            .log_message(
                MessageType::INFO,
                "★ CWTools RUST LSP server — build: two-pass-index + modifier-keys (rust-2025-06b)",
            )
            .await;
        // Store language from init options
        if let Some(opts) = &params.initialization_options {
            if let Some(lang) = opts.get("language").and_then(|v| v.as_str()) {
                *self.state.language.lock() = lang.to_string();
                self.client
                    .log_message(MessageType::INFO, format!("language: {}", lang))
                    .await;
            }

            // Optional list of loc languages to validate (e.g. ["english"]).
            // Unknown/empty entries are ignored; an empty resulting list leaves
            // scoping off (validate all languages). See `loc_languages`.
            if let Some(arr) = opts.get("localisationLanguages").and_then(|v| v.as_array()) {
                let langs: Vec<cwtools_localization::Lang> = arr
                    .iter()
                    .filter_map(|v| v.as_str())
                    .filter_map(cwtools_localization::Lang::from_name)
                    .collect();
                if !langs.is_empty() {
                    self.client
                        .log_message(
                            MessageType::INFO,
                            format!("localisation languages scoped to: {:?}", langs),
                        )
                        .await;
                    *self.state.loc_languages.lock() = Some(langs);
                }
            }

            // Persistent cache directory for the base-game index (so it isn't
            // re-parsed every startup). The client should pass its global
            // storage path; we fall back to an OS cache dir otherwise.
            if let Some(cd) = opts.get("cacheDir").and_then(|v| v.as_str()) {
                *self.state.cache_dir.lock() = Some(std::path::PathBuf::from(cd));
            }
            self.client
                .log_message(MessageType::INFO, format!("init options: {:?}", opts))
                .await;

            // Load a pre-generated vanilla cache if provided, so the editor
            // resolves base-game references (sprites, operation_tokens, …)
            // without re-parsing the install. Merged into the index in
            // validate_entire_workspace.
            if let Some(vc) = opts.get("vanillaCache").and_then(|v| v.as_str()) {
                match cwtools_info::vanilla_cache::load(std::path::Path::new(vc)) {
                    Ok((game, _fingerprint, per_type)) => {
                        let total: usize = per_type.values().map(|v| v.len()).sum();
                        *self.state.vanilla_index.lock() = Some(per_type);
                        self.client
                            .log_message(
                                MessageType::INFO,
                                format!(
                                    "Loaded {} base-game instances from vanilla cache {} (game {})",
                                    total, vc, game
                                ),
                            )
                            .await;
                    }
                    Err(e) => {
                        self.client
                            .log_message(
                                MessageType::WARNING,
                                format!("Could not load vanilla cache {}: {}", vc, e),
                            )
                            .await;
                    }
                }
            }

            // A raw base-game install dir (like the CLI's `--vanilla`). Stored
            // here and indexed lazily on the first full-workspace scan, so the
            // editor resolves base-game references without a pre-built cache.
            if let Some(vd) = opts.get("vanilla").and_then(|v| v.as_str()) {
                let p = std::path::PathBuf::from(vd);
                if p.is_dir() {
                    *self.state.vanilla_dir.lock() = Some(p);
                    self.client
                        .log_message(MessageType::INFO, format!("Base-game dir set: {}", vd))
                        .await;
                } else {
                    self.client
                        .log_message(
                            MessageType::WARNING,
                            format!("`vanilla` dir does not exist: {}", vd),
                        )
                        .await;
                }
            }

            // Load .cwt rules from rulesCache if provided
            if let Some(cache) = opts.get("rulesCache").and_then(|v| v.as_str()) {
                let cache_path = std::path::Path::new(cache);
                let (combined_ruleset, parse_errors) =
                    load_ruleset_from_dir(cache_path, &self.state.string_table);

                for err in &parse_errors {
                    self.client
                        .log_message(MessageType::WARNING, err.clone())
                        .await;
                }

                let loaded = !combined_ruleset.types.is_empty()
                    || !combined_ruleset.enums.is_empty()
                    || !combined_ruleset.aliases.is_empty()
                    || !combined_ruleset.root_rules.is_empty();

                if loaded {
                    self.client
                        .log_message(
                            MessageType::INFO,
                            format!(
                                "Loaded rules from {} ({} types, {} enums, {} aliases, {} errors)",
                                cache,
                                combined_ruleset.types.len(),
                                combined_ruleset.enums.len(),
                                combined_ruleset.aliases.len(),
                                parse_errors.len(),
                            ),
                        )
                        .await;
                    *self.state.ruleset.lock() = Some(combined_ruleset);
                    // Rebuild modifier_keys now that the ruleset is loaded.
                    // The type index is empty at this point; it will be rebuilt
                    // again after validate_entire_workspace with the full index.
                    self.rebuild_modifier_keys();
                } else {
                    self.client
                        .log_message(
                            MessageType::WARNING,
                            format!("No rules loaded from {}. Errors: {:?}", cache, parse_errors),
                        )
                        .await;
                }
            }
        }

        // Store workspace URI if provided
        if let Some(folders) = &params.workspace_folders
            && let Some(first) = folders.first() {
                *self.state.workspace_uri.lock() = Some(first.uri.to_string());
            }

        // Per-workspace ignore globs from the extension. The extension
        // forwards `cwtools.ignore.filePatterns` and `cwtools.ignore.directories`
        // into initializationOptions on first launch; runtime updates come
        // through `workspace/didChangeConfiguration` and re-apply the same
        // helper. We layer these on top of the engine's hard-coded baseline
        // (Changelog.txt, README.*, LICENSE.*, *.md) — user patterns extend,
        // they don't replace.
        if let Some(opts) = &params.initialization_options {
            let (files, dirs) = extract_ignore_patterns(opts);
            if !files.is_empty() || !dirs.is_empty() {
                *self.state.ignore_file_patterns.write() = files;
                *self.state.ignore_dir_patterns.write() = dirs;
                self.client
                    .log_message(
                        MessageType::INFO,
                        format!(
                            "ignore patterns: {} files, {} dirs (engine defaults still apply)",
                            self.state.ignore_file_patterns.read().len(),
                            self.state.ignore_dir_patterns.read().len(),
                        ),
                    )
                    .await;
            }
        }

        Ok(InitializeResult {
            capabilities: ServerCapabilities {
                text_document_sync: Some(TextDocumentSyncCapability::Options(
                    TextDocumentSyncOptions {
                        open_close: Some(true),
                        change: Some(TextDocumentSyncKind::FULL),
                        will_save: None,
                        will_save_wait_until: None,
                        save: Some(TextDocumentSyncSaveOptions::SaveOptions(SaveOptions {
                            include_text: Some(false),
                        })),
                    },
                )),
                hover_provider: Some(HoverProviderCapability::Simple(true)),
                completion_provider: Some(CompletionOptions {
                    resolve_provider: Some(false),
                    trigger_characters: Some(vec!["=".to_string(), "<".to_string()]),
                    work_done_progress_options: Default::default(),
                    all_commit_characters: None,
                    completion_item: None,
                }),
                definition_provider: Some(OneOf::Left(true)),
                references_provider: Some(OneOf::Left(true)),
                execute_command_provider: Some(ExecuteCommandOptions {
                    commands: vec!["getFileTypes".to_string()],
                    work_done_progress_options: Default::default(),
                }),
                document_symbol_provider: Some(OneOf::Left(true)),
                workspace_symbol_provider: Some(OneOf::Left(true)),
                rename_provider: Some(OneOf::Right(RenameOptions {
                    prepare_provider: Some(true),
                    work_done_progress_options: Default::default(),
                })),
                ..Default::default()
            },
            server_info: Some(ServerInfo {
                name: "cwtools-server".to_string(),
                version: Some(env!("CARGO_PKG_VERSION").to_string()),
            }),
        })
    }

    async fn initialized(&self, _params: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "CWTools server initialized!")
            .await;

        // Workspace-wide initial validation spawned in background so the LSP
        // handshake returns promptly.
        let client = self.client.clone();
        let state = self.state.clone();
        tokio::spawn(async move {
            let backend = Backend { client, state };
            backend.validate_entire_workspace().await;
        });
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    /// Re-read ignore globs when the extension's `cwtools.ignore.*` settings
    /// change. The shape mirrors what we accept in `initializationOptions`:
    /// the payload is the `cwtools` namespace object, with optional
    /// `ignoreFilePatterns` and `ignoreDirectories` arrays. The next
    /// full-workspace scan will pick up the new values; an in-flight scan
    /// finishes with the snapshot it took.
    async fn did_change_configuration(&self, params: DidChangeConfigurationParams) {
        // The client may send either the whole `cwtools` section (when the
        // section is registered via `configurationSection`) or just the
        // changed slice. `extract_ignore_patterns` looks for the same two
        // keys at the top level — works in both cases.
        let (files, dirs) = extract_ignore_patterns(&params.settings);
        *self.state.ignore_file_patterns.write() = files;
        *self.state.ignore_dir_patterns.write() = dirs;
        tracing::info!(
            file_globs = self.state.ignore_file_patterns.read().len(),
            dir_globs = self.state.ignore_dir_patterns.read().len(),
            "ignore patterns updated via didChangeConfiguration"
        );
    }

    // --- Text document sync ---
    #[tracing::instrument(skip_all)]
    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri.to_string();
        let text = params.text_document.text;
        let version = params.text_document.version;
        tracing::debug!(%uri, version, bytes = text.len(), "did_open");

        let (diagnostics, parsed) = self.parse_and_validate(&uri, &text).await;

        {
            let ast = parsed.map(Arc::new);
            self.update_doc_tokens(&uri, ast.as_ref());
            let mut docs = self.state.documents.lock();
            docs.insert(
                uri.clone(),
                ParsedDoc {
                    version,
                    text: text.clone(),
                    ast,
                },
            );
        }

        self.client
            .publish_diagnostics(params.text_document.uri, diagnostics, Some(version))
            .await;
    }

    #[tracing::instrument(skip_all)]
    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri.to_string();
        let version = params.text_document.version;

        let Some(change) = params.content_changes.into_iter().next() else {
            return;
        };
        let text = change.text;
        tracing::debug!(%uri, version, bytes = text.len(), "did_change");

        // Store the new text+version immediately (keep the prior AST until we
        // revalidate). The debounced task checks the version to know whether this
        // is still the latest edit.
        {
            let mut docs = self.state.documents.lock();
            let ast = docs.remove(&uri).and_then(|d| d.ast);
            docs.insert(uri.clone(), ParsedDoc { version, text, ast });
        }

        // Bump the global edit counter so any in-flight dependent sweep from an
        // earlier edit knows it has been superseded and can stop early.
        let generation = self.state.edit_generation.fetch_add(1, Ordering::SeqCst) + 1;

        // Validate in the background after a short debounce so a burst of
        // keystrokes coalesces into one validation and the handler returns
        // immediately (no per-keystroke re-parse lag).
        let client = self.client.clone();
        let state = self.state.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(DEBOUNCE_MS)).await;
            let backend = Backend { client, state };
            backend.debounced_validate(uri, version, generation).await;
        });
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        let uri = params.text_document.uri.to_string();
        if let Some(text) = {
            let docs = self.state.documents.lock();
            docs.get(&uri).map(|d| d.text.clone())
        } {
            let (diagnostics, _) = self.parse_and_validate(&uri, &text).await;
            self.client
                .publish_diagnostics(params.text_document.uri, diagnostics, None)
                .await;
        }
    }

    #[tracing::instrument(skip_all)]
    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri.to_string();
        tracing::debug!(%uri, "did_close");
        {
            let mut docs = self.state.documents.lock();
            docs.remove(&uri);
        }
        // Release the closed file's entries from the global indexes. Without
        // this, opening then closing a file leaves its type instances,
        // variables, event targets, and symbols in memory permanently.
        {
            let mut index = self.state.symbol_index.lock();
            index.clear_document(&uri);
        }
        {
            let mut info = self.state.info_service.write();
            info.clear_file(&uri);
        }
        self.state.doc_tokens.write().remove(&uri);
        cwtools_profiling::trim_memory();
        cwtools_profiling::log_rss("did_close");
        self.client
            .publish_diagnostics(params.text_document.uri, vec![], None)
            .await;
    }

    // --- Language features ---

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        let uri = params
            .text_document_position_params
            .text_document
            .uri
            .to_string();
        let pos = params.text_document_position_params.position;

        let docs = self.state.documents.lock();
        if let Some(doc) = docs.get(&uri)
            && let Some(ast) = &doc.ast {
                let lsp_line = pos.line + 1; // LSP is 0-based; parser is 1-based
                let lsp_col = pos.character as u16;

                let ws_uri = self.state.workspace_uri.lock().clone();
                let logical_path = logical_path_from_uri(&uri, &ws_uri);

                let ruleset_guard = self.state.ruleset.lock();
                let pos_info = if let Some(rs) = ruleset_guard.as_ref() {
                    info_at_position(
                        ast,
                        lsp_line,
                        lsp_col,
                        rs,
                        &logical_path,
                        &self.state.string_table,
                    )
                } else {
                    // No rules: fall back to position-only lookup
                    None
                };
                drop(ruleset_guard);

                if let Some(info) = pos_info {
                    let ruleset_guard2 = self.state.ruleset.lock();
                    let md =
                        build_hover_markdown(&info.element, &info.hint, ruleset_guard2.as_ref());
                    return Ok(Some(Hover {
                        contents: HoverContents::Markup(MarkupContent {
                            kind: MarkupKind::Markdown,
                            value: md,
                        }),
                        range: None,
                    }));
                }

                // Fallback: no-rule position finder
                if let Some(element) = element_at_position(
                    ast,
                    pos.line + 1,
                    pos.character as u16,
                    &self.state.string_table,
                ) {
                    let contents = match element {
                        PositionElement::Node { key } => {
                            format!("**Node**: `{}`", key)
                        }
                        PositionElement::Leaf { key, value } => {
                            format!("**Field**: `{} = {}`", key, value)
                        }
                        PositionElement::LeafValue { value } => {
                            format!("**Value**: `{}`", value)
                        }
                    };
                    return Ok(Some(Hover {
                        contents: HoverContents::Markup(MarkupContent {
                            kind: MarkupKind::Markdown,
                            value: contents,
                        }),
                        range: None,
                    }));
                }
            }
        Ok(None)
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        let uri = params.text_document_position.text_document.uri.to_string();
        let pos = params.text_document_position.position;

        let lsp_line = pos.line + 1;
        let lsp_col = pos.character as u16;

        // Try context-aware completions first.
        //
        // Limitations:
        //  - Alias expansion is one level deep only (full recursive alias
        //    expansion would require following chains, which can be large).
        //  - ScopeField values are a static per-game list; full dynamic scope
        //    resolution is not implemented.
        //  - Deeply nested nodes inside aliases or subtypes may not match.
        let ws_uri = self.state.workspace_uri.lock().clone();
        let logical_path = logical_path_from_uri(&uri, &ws_uri);
        let language = self.state.language.lock().clone();

        // .yml localisation file — offer loc-key / data-function completions.
        if uri.ends_with(".yml") || uri.ends_with(".yaml") {
            let info_guard = self.state.info_service.read();
            let items = loc_completions(&info_guard, &language);
            if !items.is_empty() {
                return Ok(Some(CompletionResponse::Array(items)));
            }
        }

        let context_items: Vec<CompletionItem> = {
            let docs = self.state.documents.lock();
            let ruleset_guard = self.state.ruleset.lock();
            let info_guard = self.state.info_service.read();

            if let (Some(doc), Some(rs)) = (docs.get(&uri), ruleset_guard.as_ref()) {
                if let Some(ast) = &doc.ast {
                    let key_path =
                        enclosing_key_path(ast, lsp_line, lsp_col, &self.state.string_table);
                    if key_path.is_empty() {
                        // Top level — offer root-type snippets for this file's path.
                        root_type_snippets(rs, &logical_path)
                    } else if let Some(rules) = rules_for_context(rs, &key_path, &logical_path) {
                        completions_from_rules(rules, rs, &info_guard, &language)
                    } else {
                        Vec::new()
                    }
                } else {
                    Vec::new()
                }
            } else {
                Vec::new()
            }
        };

        if !context_items.is_empty() {
            return Ok(Some(CompletionResponse::Array(context_items)));
        }

        // Fallback: flat global list (original behavior) when context-aware
        // matching produced nothing (no rules loaded, unrecognised path, etc.)
        let mut items = Vec::new();

        let ruleset = self.state.ruleset.lock();
        if let Some(rules) = ruleset.as_ref() {
            for t in &rules.types {
                items.push(CompletionItem {
                    label: t.name.clone(),
                    kind: Some(CompletionItemKind::STRUCT),
                    detail: Some("Type definition".to_string()),
                    ..Default::default()
                });
            }
            for e in &rules.enums {
                items.push(CompletionItem {
                    label: e.key.clone(),
                    kind: Some(CompletionItemKind::ENUM),
                    detail: Some(format!("Enum ({} values)", e.values.len())),
                    ..Default::default()
                });
            }
        }
        drop(ruleset);

        let info = self.state.info_service.read();
        for var in &info.all_variables {
            items.push(CompletionItem {
                label: var.clone(),
                kind: Some(CompletionItemKind::CONSTANT),
                detail: Some("Variable".to_string()),
                ..Default::default()
            });
        }
        for et in &info.all_event_targets {
            items.push(CompletionItem {
                label: format!("event_target:{}", et),
                kind: Some(CompletionItemKind::VARIABLE),
                detail: Some("Event target".to_string()),
                ..Default::default()
            });
        }
        for (file_uri, file_info) in &info.files {
            for (key, _loc) in &file_info.top_level_keys {
                items.push(CompletionItem {
                    label: key.clone(),
                    kind: Some(CompletionItemKind::KEYWORD),
                    detail: Some(format!("Key in {}", file_uri)),
                    ..Default::default()
                });
            }
        }

        if items.is_empty() {
            Ok(None)
        } else {
            Ok(Some(CompletionResponse::Array(items)))
        }
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        let pos = params.text_document_position_params.position;
        let uri = params
            .text_document_position_params
            .text_document
            .uri
            .to_string();

        let ws_uri = self.state.workspace_uri.lock().clone();
        let logical_path = logical_path_from_uri(&uri, &ws_uri);

        // First try the rule-aware lookup via info_at_position so we get a
        // TypeRef hint and can look up the actual definition location.
        let type_ref: Option<(String, String)> = {
            let docs = self.state.documents.lock();
            let ruleset_guard = self.state.ruleset.lock();
            if let (Some(doc), Some(rs)) = (docs.get(&uri), ruleset_guard.as_ref()) {
                if let Some(ast) = &doc.ast {
                    let info = info_at_position(
                        ast,
                        pos.line + 1,
                        pos.character as u16,
                        rs,
                        &logical_path,
                        &self.state.string_table,
                    );
                    info.and_then(|i| match i.hint {
                        ReferenceHint::TypeRef { type_name, value } => Some((type_name, value)),
                        _ => None,
                    })
                } else {
                    None
                }
            } else {
                None
            }
        };

        if let Some((type_name, instance_name)) = type_ref {
            // Look up in the TypeIndex
            let info = self.state.info_service.read();
            let instances = info.type_index.instances(&type_name);
            let found: Vec<Location> = instances
                .iter()
                .filter(|(_, inst)| inst.name == instance_name)
                .map(|(file_uri, inst)| Location {
                    uri: parse_uri(
                        file_uri,
                        &params.text_document_position_params.text_document.uri,
                    ),
                    range: Range {
                        start: Position {
                            line: inst.location.line.saturating_sub(1),
                            character: inst.location.col as u32,
                        },
                        end: Position {
                            line: inst.location.line.saturating_sub(1),
                            character: inst.location.col as u32 + instance_name.len() as u32,
                        },
                    },
                })
                .collect();
            if !found.is_empty() {
                return Ok(Some(GotoDefinitionResponse::Array(found)));
            }
        }

        // Fallback: heuristic symbol-based lookup
        let docs = self.state.documents.lock();
        if let Some(doc) = docs.get(&uri)
            && let Some(ast) = &doc.ast
                && let Some(element) = element_at_position(
                    ast,
                    pos.line + 1,
                    pos.character as u16,
                    &self.state.string_table,
                ) {
                    let symbol = match &element {
                        PositionElement::Node { key } => key.clone(),
                        PositionElement::Leaf { key, .. } => key.clone(),
                        PositionElement::LeafValue { value } => value.clone(),
                    };
                    drop(docs);
                    let info = self.state.info_service.read();
                    if let Some(defs) = info.find_definitions(&symbol) {
                        let locations: Vec<Location> = defs
                            .iter()
                            .map(|(file_uri, loc)| Location {
                                uri: parse_uri(
                                    file_uri,
                                    &params.text_document_position_params.text_document.uri,
                                ),
                                range: Range {
                                    start: Position {
                                        line: loc.line.saturating_sub(1),
                                        character: loc.col as u32,
                                    },
                                    end: Position {
                                        line: loc.line.saturating_sub(1),
                                        character: (loc.col + symbol.len() as u16) as u32,
                                    },
                                },
                            })
                            .collect();
                        if !locations.is_empty() {
                            return Ok(Some(GotoDefinitionResponse::Array(locations)));
                        }
                    }
                }
        Ok(None)
    }

    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        let pos = params.text_document_position.position;
        let uri = params.text_document_position.text_document.uri.to_string();

        let ws_uri = self.state.workspace_uri.lock().clone();
        let logical_path = logical_path_from_uri(&uri, &ws_uri);

        // Try rule-aware: identify a TypeRef at cursor then scan type_index for
        // all locations where that type's instances are referenced.
        //
        // Limitation: reference scanning walks the TypeIndex for definition
        // locations only.  Tracking every *use* of a type instance across the
        // workspace would require an additional references index that is not yet
        // built.  Full cross-file reference tracking is left as future work.
        let type_ref: Option<(String, String)> = {
            let docs = self.state.documents.lock();
            let ruleset_guard = self.state.ruleset.lock();
            if let (Some(doc), Some(rs)) = (docs.get(&uri), ruleset_guard.as_ref()) {
                if let Some(ast) = &doc.ast {
                    let info = info_at_position(
                        ast,
                        pos.line + 1,
                        pos.character as u16,
                        rs,
                        &logical_path,
                        &self.state.string_table,
                    );
                    info.and_then(|i| match i.hint {
                        ReferenceHint::TypeRef { type_name, value } => Some((type_name, value)),
                        _ => None,
                    })
                } else {
                    None
                }
            } else {
                None
            }
        };

        if let Some((type_name, instance_name)) = type_ref {
            let mut all_locs: Vec<Location> = Vec::new();

            // 1. Definition location(s) from TypeIndex.
            {
                let info = self.state.info_service.read();
                let instances = info.type_index.instances(&type_name);
                for (file_uri, inst) in instances
                    .iter()
                    .filter(|(_, inst)| inst.name == instance_name)
                {
                    all_locs.push(Location {
                        uri: file_uri.parse().unwrap_or_else(|_| {
                            params.text_document_position.text_document.uri.clone()
                        }),
                        range: Range {
                            start: Position {
                                line: inst.location.line.saturating_sub(1),
                                character: inst.location.col as u32,
                            },
                            end: Position {
                                line: inst.location.line.saturating_sub(1),
                                character: inst.location.col as u32 + instance_name.len() as u32,
                            },
                        },
                    });
                }
            }

            // 2. Use-sites: scan all docs for TypeField leaves with the same value.
            {
                let docs = self.state.documents.lock();
                let ruleset_guard = self.state.ruleset.lock();
                let ws_uri = self.state.workspace_uri.lock().clone();
                if let Some(rs) = ruleset_guard.as_ref() {
                    let use_sites = scan_use_sites(
                        &type_name,
                        &instance_name,
                        &docs,
                        rs,
                        &ws_uri,
                        &self.state.string_table,
                    );
                    for (file_uri, loc) in use_sites {
                        all_locs.push(Location {
                            uri: parse_uri(
                                file_uri,
                                &params.text_document_position.text_document.uri,
                            ),
                            range: Range {
                                start: Position {
                                    line: loc.line.saturating_sub(1),
                                    character: loc.col as u32,
                                },
                                end: Position {
                                    line: loc.line.saturating_sub(1),
                                    character: loc.col as u32 + instance_name.len() as u32,
                                },
                            },
                        });
                    }
                }
            }

            if !all_locs.is_empty() {
                return Ok(Some(all_locs));
            }
        }

        // Fallback: heuristic-based approach
        let docs = self.state.documents.lock();
        if let Some(doc) = docs.get(&uri)
            && let Some(ast) = &doc.ast
                && let Some(element) = element_at_position(
                    ast,
                    pos.line + 1,
                    pos.character as u16,
                    &self.state.string_table,
                ) {
                    let symbol = match &element {
                        PositionElement::Node { key } => key.clone(),
                        PositionElement::Leaf { key, .. } => key.clone(),
                        PositionElement::LeafValue { value } => value.clone(),
                    };
                    drop(docs);
                    let info = self.state.info_service.read();
                    let mut all_locs = Vec::new();
                    if let Some(defs) = info.find_definitions(&symbol) {
                        all_locs.extend(defs.iter().map(|(file_uri, loc)| Location {
                            uri: parse_uri(
                                file_uri,
                                &params.text_document_position.text_document.uri,
                            ),
                            range: Range {
                                start: Position {
                                    line: loc.line.saturating_sub(1),
                                    character: loc.col as u32,
                                },
                                end: Position {
                                    line: loc.line.saturating_sub(1),
                                    character: (loc.col + symbol.len() as u16) as u32,
                                },
                            },
                        }));
                    }
                    if let Some(refs) = info.find_references(&symbol) {
                        all_locs.extend(refs.iter().map(|(file_uri, loc)| Location {
                            uri: parse_uri(
                                file_uri,
                                &params.text_document_position.text_document.uri,
                            ),
                            range: Range {
                                start: Position {
                                    line: loc.line.saturating_sub(1),
                                    character: loc.col as u32,
                                },
                                end: Position {
                                    line: loc.line.saturating_sub(1),
                                    character: (loc.col + symbol.len() as u16) as u32,
                                },
                            },
                        }));
                    }
                    let index = self.state.symbol_index.lock();
                    if let Some(locs) = index.find_references(&symbol) {
                        all_locs.extend(locs.iter().map(|l| Location {
                            uri: l.uri.parse().unwrap_or_else(|_| {
                                params.text_document_position.text_document.uri.clone()
                            }),
                            range: Range {
                                start: Position {
                                    line: l.line.saturating_sub(1),
                                    character: l.col as u32,
                                },
                                end: Position {
                                    line: l.line.saturating_sub(1),
                                    character: (l.col + symbol.len() as u16) as u32,
                                },
                            },
                        }));
                    }
                    if !all_locs.is_empty() {
                        return Ok(Some(all_locs));
                    }
                }
        Ok(None)
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        let uri = params.text_document.uri.to_string();
        let info = self.state.info_service.read();

        // Emit type instances as document symbols (one per named instance),
        // derived from the cross-file index — `FileInfo` no longer keeps a
        // per-file copy of these.
        let mut symbols: Vec<SymbolInformation> = Vec::new();
        for (type_name, inst) in info.type_index.instances_in_file(&uri) {
            #[allow(deprecated)]
            symbols.push(SymbolInformation {
                name: inst.name.clone(),
                kind: SymbolKind::STRUCT,
                tags: None,
                deprecated: None,
                location: Location {
                    uri: params.text_document.uri.clone(),
                    range: Range {
                        start: Position {
                            line: inst.location.line.saturating_sub(1),
                            character: inst.location.col as u32,
                        },
                        end: Position {
                            line: inst.location.line.saturating_sub(1),
                            character: inst.location.col as u32 + inst.name.len() as u32,
                        },
                    },
                },
                container_name: Some(type_name.to_string()),
            });
        }

        // Also include @-variables as symbols (still tracked per-file).
        let Some(file_info) = info.files.get(&uri) else {
            return Ok(if symbols.is_empty() {
                None
            } else {
                Some(DocumentSymbolResponse::Flat(symbols))
            });
        };
        for (name, loc) in &file_info.defined_variables {
            #[allow(deprecated)]
            symbols.push(SymbolInformation {
                name: name.clone(),
                kind: SymbolKind::CONSTANT,
                tags: None,
                deprecated: None,
                location: Location {
                    uri: params.text_document.uri.clone(),
                    range: Range {
                        start: Position {
                            line: loc.line.saturating_sub(1),
                            character: loc.col as u32,
                        },
                        end: Position {
                            line: loc.line.saturating_sub(1),
                            character: loc.col as u32 + name.len() as u32,
                        },
                    },
                },
                container_name: None,
            });
        }

        if symbols.is_empty() {
            Ok(None)
        } else {
            Ok(Some(DocumentSymbolResponse::Flat(symbols)))
        }
    }

    async fn symbol(
        &self,
        params: WorkspaceSymbolParams,
    ) -> Result<Option<Vec<SymbolInformation>>> {
        let query = params.query.to_lowercase();
        let info = self.state.info_service.read();
        let mut symbols: Vec<SymbolInformation> = Vec::new();

        for (type_name, instances) in &info.type_index.map {
            for (file_uri, inst) in instances {
                if query.is_empty() || inst.name.to_lowercase().contains(&query) {
                    #[allow(deprecated)]
                    symbols.push(SymbolInformation {
                        name: inst.name.clone(),
                        kind: SymbolKind::STRUCT,
                        tags: None,
                        deprecated: None,
                        location: Location {
                            uri: file_uri
                                .parse()
                                .unwrap_or_else(|_| Url::parse("file:///unknown").unwrap()),
                            range: Range {
                                start: Position {
                                    line: inst.location.line.saturating_sub(1),
                                    character: inst.location.col as u32,
                                },
                                end: Position {
                                    line: inst.location.line.saturating_sub(1),
                                    character: inst.location.col as u32 + inst.name.len() as u32,
                                },
                            },
                        },
                        container_name: Some(type_name.clone()),
                    });
                }
                // Cap at 500 to avoid flooding the client.
                if symbols.len() >= 500 {
                    break;
                }
            }
            if symbols.len() >= 500 {
                break;
            }
        }

        if symbols.is_empty() {
            Ok(None)
        } else {
            Ok(Some(symbols))
        }
    }

    async fn prepare_rename(
        &self,
        params: TextDocumentPositionParams,
    ) -> Result<Option<PrepareRenameResponse>> {
        let uri = params.text_document.uri.to_string();
        let pos = params.position;
        let ws_uri = self.state.workspace_uri.lock().clone();
        let logical_path = logical_path_from_uri(&uri, &ws_uri);

        let type_ref: Option<(String, String)> = {
            let docs = self.state.documents.lock();
            let ruleset_guard = self.state.ruleset.lock();
            if let (Some(doc), Some(rs)) = (docs.get(&uri), ruleset_guard.as_ref()) {
                if let Some(ast) = &doc.ast {
                    let info = info_at_position(
                        ast,
                        pos.line + 1,
                        pos.character as u16,
                        rs,
                        &logical_path,
                        &self.state.string_table,
                    );
                    info.and_then(|i| match i.hint {
                        ReferenceHint::TypeRef { type_name, value } => Some((type_name, value)),
                        _ => None,
                    })
                } else {
                    None
                }
            } else {
                None
            }
        };

        if let Some((_, instance_name)) = type_ref {
            // Return a range covering the instance name at cursor.
            let range = Range {
                start: Position {
                    line: pos.line,
                    character: pos.character,
                },
                end: Position {
                    line: pos.line,
                    character: pos.character + instance_name.len() as u32,
                },
            };
            return Ok(Some(PrepareRenameResponse::Range(range)));
        }
        Ok(None)
    }

    async fn rename(&self, params: RenameParams) -> Result<Option<WorkspaceEdit>> {
        let uri = params.text_document_position.text_document.uri.to_string();
        let pos = params.text_document_position.position;
        let new_name = params.new_name.clone();
        let ws_uri = self.state.workspace_uri.lock().clone();
        let logical_path = logical_path_from_uri(&uri, &ws_uri);

        // Identify what's under the cursor
        let type_ref: Option<(String, String)> = {
            let docs = self.state.documents.lock();
            let ruleset_guard = self.state.ruleset.lock();
            if let (Some(doc), Some(rs)) = (docs.get(&uri), ruleset_guard.as_ref()) {
                if let Some(ast) = &doc.ast {
                    let info = info_at_position(
                        ast,
                        pos.line + 1,
                        pos.character as u16,
                        rs,
                        &logical_path,
                        &self.state.string_table,
                    );
                    info.and_then(|i| match i.hint {
                        ReferenceHint::TypeRef { type_name, value } => Some((type_name, value)),
                        _ => None,
                    })
                } else {
                    None
                }
            } else {
                None
            }
        };

        let (type_name, instance_name) = match type_ref {
            Some(r) => r,
            None => return Ok(None),
        };

        // Collect definition + use-site locations (reuse references logic)
        let mut all_locs: Vec<(String, cwtools_info::SourceLocation, usize)> = Vec::new();

        {
            let info = self.state.info_service.read();
            let instances = info.type_index.instances(&type_name);
            for (file_uri, inst) in instances.iter().filter(|(_, i)| i.name == instance_name) {
                all_locs.push((file_uri.clone(), inst.location, instance_name.len()));
            }
        }

        {
            let docs = self.state.documents.lock();
            let ruleset_guard = self.state.ruleset.lock();
            let ws_uri2 = self.state.workspace_uri.lock().clone();
            if let Some(rs) = ruleset_guard.as_ref() {
                let use_sites = scan_use_sites(
                    &type_name,
                    &instance_name,
                    &docs,
                    rs,
                    &ws_uri2,
                    &self.state.string_table,
                );
                for (file_uri, loc) in use_sites {
                    all_locs.push((file_uri, loc, instance_name.len()));
                }
            }
        }

        if all_locs.is_empty() {
            return Ok(None);
        }

        // Build WorkspaceEdit: group text edits by file URI
        let mut changes: HashMap<Url, Vec<TextEdit>> = HashMap::new();
        for (file_uri, loc, name_len) in all_locs {
            let url = match file_uri.parse::<Url>() {
                Ok(u) => u,
                Err(_) => continue,
            };
            let edit = TextEdit {
                range: Range {
                    start: Position {
                        line: loc.line.saturating_sub(1),
                        character: loc.col as u32,
                    },
                    end: Position {
                        line: loc.line.saturating_sub(1),
                        character: loc.col as u32 + name_len as u32,
                    },
                },
                new_text: new_name.clone(),
            };
            changes.entry(url).or_default().push(edit);
        }

        Ok(Some(WorkspaceEdit {
            changes: Some(changes),
            document_changes: None,
            change_annotations: None,
        }))
    }

    async fn execute_command(&self, params: ExecuteCommandParams) -> Result<Option<Value>> {
        match params.command.as_str() {
            "getFileTypes" => {
                if let Some(uri_val) = params.arguments.first() {
                    let uri = uri_val.as_str().unwrap_or("");
                    let types = self.determine_file_types(uri).await;
                    let arr: Vec<Value> = types.into_iter().map(Value::String).collect();
                    return Ok(Some(Value::Array(arr)));
                }
                Ok(Some(Value::Array(vec![])))
            }
            _ => Ok(None),
        }
    }
}

impl Backend {
    // ── Custom notification helpers ───────────────────────────────────────────

    /// Send the `loadingBar` server→client notification so the VS Code extension
    /// status bar reflects background indexing/validation work.
    /// Payload: `{ "enable": bool, "value": string }`.
    async fn send_loading_bar(&self, enable: bool, value: &str) {
        let payload = serde_json::json!({ "enable": enable, "value": value });
        self.client.send_notification::<LoadingBar>(payload).await;
    }

    /// Send the `updateFileList` server→client notification so the VS Code
    /// extension file explorer populates.
    /// Payload: `{ "fileList": [{ "scope": string, "uri": string, "logicalpath": string }] }`.
    async fn send_update_file_list(&self, file_list: Vec<serde_json::Value>) {
        let payload = serde_json::json!({ "fileList": file_list });
        self.client
            .send_notification::<UpdateFileList>(payload)
            .await;
    }

    /// Scan the entire workspace for relevant game files and validate them all.
    #[tracing::instrument(skip_all)]
    async fn validate_entire_workspace(&self) {
        cwtools_profiling::log_rss("workspace_scan_start");
        self.send_loading_bar(true, "Indexing workspace…").await;

        let workspace_uri = {
            let guard = self.state.workspace_uri.lock();
            guard.clone()
        };

        let root_path = match workspace_uri {
            Some(uri) => {
                let p = uri.strip_prefix("file://").unwrap_or(&uri);
                std::path::PathBuf::from(p)
            }
            None => {
                self.client
                    .log_message(
                        MessageType::WARNING,
                        "No workspace folder; skipping full-workspace validation.",
                    )
                    .await;
                return;
            }
        };

        let extensions: Vec<&str> = vec!["txt", "gui", "gfx", "sfx", "asset", "map"];

        // Snapshot the user-configured ignore globs once for the whole walk.
        // The engine's hard-coded baseline (Changelog.txt, README.*, *.md)
        // is layered on top inside the walker closure so it can't be
        // accidentally cleared by a user who sets an empty list.
        let extra_file_globs = self.state.ignore_file_patterns.read().clone();
        let extra_dir_globs = self.state.ignore_dir_patterns.read().clone();

        let mut files_to_validate = Vec::new();
        fn walk_dir(
            path: &std::path::Path,
            extensions: &[&str],
            extra_file_globs: &[String],
            extra_dir_globs: &[String],
            out: &mut Vec<std::path::PathBuf>,
        ) {
            if let Ok(entries) = std::fs::read_dir(path) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        let raw_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                        let lower_name = raw_name.to_lowercase();
                        let skip = matches!(
                            lower_name.as_str(),
                            ".git" | "node_modules" | "out" | "dist" | "target" | "bin" | "obj"
                            // `resources/` is a developer scratch area in many mods,
                            // not a path the game loads — don't validate it.
                            | "resources" | ".vscode"
                        ) || extra_dir_globs
                            .iter()
                            .any(|pat| lsp_glob_match(pat, raw_name));
                        if !skip {
                            walk_dir(&path, extensions, extra_file_globs, extra_dir_globs, out);
                        }
                    } else if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                        if !extensions.contains(&ext) {
                            continue;
                        }
                        // Engine baseline for free-form text files. These are
                        // NOT in the user globs (which only extend), so
                        // Changelog.txt / README.* / LICENSE.* / *.md always
                        // get skipped here even if the user sends an empty
                        // `ignoreFilePatterns` list.
                        let file_name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                        let engine_skip = matches!(
                            file_name,
                            "Changelog.txt"
                                | "README.txt"
                                | "LICENSE.txt"
                                | "README.md"
                                | "LICENSE.md"
                        ) || file_name.to_ascii_lowercase().ends_with(".md");
                        if engine_skip {
                            continue;
                        }
                        if extra_file_globs
                            .iter()
                            .any(|pat| lsp_glob_match(pat, file_name))
                        {
                            continue;
                        }
                        out.push(path);
                    }
                }
            }
        }
        walk_dir(
            &root_path,
            &extensions,
            &extra_file_globs,
            &extra_dir_globs,
            &mut files_to_validate,
        );

        if files_to_validate.is_empty() {
            self.client
                .log_message(MessageType::INFO, "No workspace files found to validate.")
                .await;
            return;
        }

        self.client
            .log_message(
                MessageType::INFO,
                format!(
                    "Validating {} workspace files under {:?} ...",
                    files_to_validate.len(),
                    root_path
                ),
            )
            .await;

        // Resolve the parse-cache directory and settings fingerprint. The
        // fingerprint encodes the game, ruleset shape, and workspace root so
        // stale caches are cleared automatically when any of those change.
        let (cache_info, cache_was_valid) = {
            let cache_dir = self.state.cache_dir.lock().clone();
            match cache_dir {
                Some(cd) => {
                    let language = self.state.language.lock().clone();
                    let ruleset_snap = self.state.ruleset.lock().clone();
                    let fp = match ruleset_snap {
                        Some(ref rs) => {
                            workspace_cache::settings_fingerprint(&language, rs, &root_path)
                        }
                        None => workspace_cache::settings_fingerprint(
                            &language,
                            &RuleSet::new(),
                            &root_path,
                        ),
                    };
                    let valid = workspace_cache::validate_or_clear(&cd, fp);
                    (Some((cd, fp)), valid)
                }
                None => (None, true),
            }
        };
        self.client
            .log_message(
                MessageType::INFO,
                if cache_was_valid {
                    "Parse cache: hit (settings match)"
                } else {
                    "Parse cache: settings changed, cleared"
                },
            )
            .await;

        // Pass 1: parse + index every file (types, scripted triggers/effects,
        // modifiers) so cross-file references resolve before any file is
        // validated. The parsed ASTs are kept resident in `parsed_files` and
        // handed to pass 2 — re-parsing 7413 files in pass 2 cost ~4-6s on MD
        // and produced no observable benefit, just CPU and allocator churn.
        // The total resident set between the two passes is bounded by what the
        // loc service allocates next, so peak RSS doesn't grow meaningfully.
        //
        // On a cache hit the AST is deserialized from disk (.cwb) instead of
        // parsed, then kept resident like any other; on a miss we parse and
        // persist for the next scan. The disk cache speeds the cold→warm scan
        // across restarts; keeping the AST resident avoids a pass-2 re-parse
        // within a single scan.
        self.send_loading_bar(true, "Indexing workspace…").await;
        let mut parsed_files: Vec<Option<ParsedFile>> = Vec::with_capacity(files_to_validate.len());
        let mut cache_hits = 0u64;
        let mut cache_misses = 0u64;
        for (i, file_path) in files_to_validate.iter().enumerate() {
            let uri = format!("file://{}", file_path.display());
            let parsed = match std::fs::read_to_string(file_path) {
                Ok(text) => {
                    // Try the parse cache first.
                    if let Some((ref cd, fp)) = cache_info
                        && let Some(parsed) =
                            workspace_cache::load(cd, fp, &text, &self.state.string_table)
                    {
                        self.index_parsed_file(&uri, &parsed);
                        cache_hits += 1;
                        Some(parsed)
                    } else if let Some(parsed) = self.index_document(&uri, &text).await {
                        // Cache miss — parse + index, then persist for next scan.
                        if let Some((ref cd, fp)) = cache_info {
                            workspace_cache::store(cd, fp, &text, &parsed, &self.state.string_table);
                        }
                        cache_misses += 1;
                        Some(parsed)
                    } else {
                        None
                    }
                }
                Err(_) => None,
            };
            parsed_files.push(parsed);
            // Yield every 50 files so LSP requests (hover, completion) can
            // interleave with the workspace scan.
            if i % 50 == 49 {
                tokio::task::yield_now().await;
            }
        }

        self.client
            .log_message(
                MessageType::INFO,
                format!(
                    "Indexing pass: {} cache hits, {} misses",
                    cache_hits, cache_misses
                ),
            )
            .await;

        // Build the base-game index from a `vanilla` dir (or auto-discovery) if
        // we have one and haven't indexed it yet. Populates `vanilla_index`.
        self.ensure_vanilla_index().await;

        // Merge the pre-generated vanilla index (if loaded) so base-game
        // references resolve. Re-merge each pass after dropping the prior copy
        // to avoid unbounded growth on re-validation.
        {
            let vanilla_guard = self.state.vanilla_index.lock();
            if let Some(per_type) = vanilla_guard.as_ref() {
                let mut info_guard = self.state.info_service.write();
                info_guard.type_index.remove_file("<vanilla-cache>");
                info_guard
                    .type_index
                    .merge("<vanilla-cache>", per_type.clone());
            }
        }

        // Rebuild the cached modifier-key set now that the type index is
        // complete (templated modifiers like production_speed_<building>_factor
        // expand against the full instance list).
        self.rebuild_modifier_keys();

        // Pass 1 just dropped every file's parsed AST; glibc may still be
        // holding onto the heap pages. Trim now so the loc service's ~2M-entry
        // allocation in rebuild_and_publish_loc doesn't sit on top of pages
        // we're never going to touch again.
        cwtools_profiling::trim_memory();
        cwtools_profiling::log_rss("scan: post-index trim");

        // Build the loc-key index (workspace + vanilla) so pass 2's config
        // validation can check LocalisationField references (CW100/CW122), and
        // publish loc-file diagnostics (CW225 etc.) for the workspace loc files.
        self.rebuild_and_publish_loc(&root_path).await;

        // Pass 2: validate each file against the now-complete index using
        // the ASTs we already parsed in pass 1. Diagnostics are published to
        // the editor; the file is intentionally NOT stored in
        // `self.state.documents`. That map holds only files the editor has
        // open (populated by did_open) — the scan used to insert every
        // workspace file there, pinning all texts+ASTs in memory for the
        // whole session.
        self.send_loading_bar(true, "Validating workspace…").await;
        let mut total_errors = 0usize;
        let total_files = files_to_validate.len();
        // Snapshot modifier_keys once before the loop; the set doesn't change
        // during validation and we can't hold the guard across await points.
        let modifier_keys_snap: HashSet<String> = self.state.modifier_keys.read().clone();
        // Build the scope registry + enum_map ONCE for the whole scan instead of
        // once per file: they depend only on (ruleset, game) and are the
        // expensive part of per-file setup (many inserts + lowercasing +
        // per-iterator `format!`). Both are reused across the rayon section.
        let scan_game = {
            let language = self.state.language.lock().clone();
            cwtools_game::constants::Game::from_str(&language)
        };
        // Own an `Arc<RuleSet>` clone for the whole batch so the `enum_map`
        // (which borrows the ruleset) stays valid across the parallel section
        // without holding the ruleset mutex across rayon work.
        let scan_ruleset: Option<Arc<RuleSet>> = {
            let ruleset_guard = self.state.ruleset.lock();
            ruleset_guard.as_ref().map(|rs| Arc::new(rs.clone()))
        };
        let scan_registry = scan_ruleset
            .as_ref()
            .map(|rs| build_scope_registry_arc(rs, scan_game));

        // Validate every file in parallel, then publish serially. The
        // CPU-bound validation runs under a single shared `info_service` /
        // `loc_index` read guard (both `&...` references are `Sync`), with no
        // async and no client calls inside the rayon section. Publishing is
        // async and stays out of the parallel block.
        use rayon::prelude::*;
        let results: Vec<(String, Vec<Diagnostic>)> = {
            let info_guard = self.state.info_service.read();
            let loc_guard = self.state.loc_index.read();
            let type_index = &info_guard.type_index;
            let loc_index = loc_guard.as_ref();
            let registry = scan_registry.as_ref().and_then(|r| r.as_ref());
            // Build enum_map once for the batch; it borrows `scan_ruleset`,
            // which is owned for the whole parallel section above.
            let enum_map = scan_ruleset.as_ref().map(|rs| build_enum_map(rs));

            files_to_validate
                .par_iter()
                .zip(parsed_files.par_iter())
                .filter_map(|(file_path, parsed_opt)| {
                    // Skip files that failed to parse in pass 1.
                    let parsed = parsed_opt.as_ref()?;
                    let uri = format!("file://{}", file_path.display());
                    let diagnostics = match (scan_ruleset.as_ref(), enum_map.as_ref()) {
                        (Some(ruleset), Some(enum_map)) => validate_parsed_with_indexes(
                            &uri,
                            parsed,
                            &modifier_keys_snap,
                            ruleset,
                            scan_game,
                            registry,
                            enum_map,
                            type_index,
                            loc_index,
                            &self.state.string_table,
                        ),
                        _ => parsed
                            .errors
                            .iter()
                            .map(parse_error_to_diagnostic)
                            .collect(),
                    };
                    Some((uri, diagnostics))
                })
                .collect()
            // info_guard / loc_guard dropped here, before any await.
        };

        for (i, (uri, diagnostics)) in results.into_iter().enumerate() {
            total_errors += diagnostics
                .iter()
                .filter(|d| d.severity == Some(DiagnosticSeverity::ERROR))
                .count();

            if let Ok(uri_obj) = Url::parse(&uri) {
                self.client
                    .publish_diagnostics(uri_obj, diagnostics, None)
                    .await;
            }
            if i % 50 == 49 {
                tokio::task::yield_now().await;
            }
        }
        // Pass 2 is done. Drop the per-file ASTs before the file-list / profile
        // summary so the RSS we report reflects the steady-state working set
        // (loc index + type index + open documents), not the in-flight
        // validation peak.
        drop(parsed_files);

        self.client
            .log_message(
                MessageType::INFO,
                format!(
                    "Workspace validation complete: {} errors across {} files",
                    total_errors, total_files
                ),
            )
            .await;

        // Build and send the file list for the extension's file explorer.
        let ws_uri = self.state.workspace_uri.lock().clone();
        let file_list: Vec<serde_json::Value> = files_to_validate
            .iter()
            .map(|file_path| {
                let uri = format!("file://{}", file_path.display());
                let logical_path = logical_path_from_uri(&uri, &ws_uri);
                let scope = logical_path
                    .split('/')
                    .next()
                    .unwrap_or("unknown")
                    .to_string();
                serde_json::json!({
                    "scope": scope,
                    "uri": uri,
                    "logicalpath": logical_path
                })
            })
            .collect();
        self.send_update_file_list(file_list).await;

        if cwtools_profiling::profile_enabled() {
            let st = self.state.string_table.stats();
            let info_summary = self.state.info_service.read().profile_summary();
            let vanilla = self
                .state
                .vanilla_index
                .lock()
                .as_ref()
                .map(|m| m.values().map(|v| v.len()).sum::<usize>())
                .unwrap_or(0);
            let loc_keys = self
                .state
                .loc_index
                .read()
                .as_ref()
                .map(|i| i.union().len())
                .unwrap_or(0);
            tracing::info!(target: "cwtools::profile", "{}", info_summary);
            tracing::info!(target: "cwtools::profile",
                "string_table {} MiB ({} entries) | vanilla_index {} instances | loc union {} keys",
                st.total_bytes() / (1024 * 1024), st.entries, vanilla, loc_keys);
        }
        cwtools_profiling::log_rss("workspace_scan_done");
        // The scan dropped large transients (the whole base-game parse, ~2M loc
        // entries, every file's AST). Hand the freed heap back to the OS so RSS
        // reflects the real working set, not the scan peak.
        cwtools_profiling::trim_memory();
        cwtools_profiling::log_rss("after_trim");
        self.send_loading_bar(false, "").await;
    }

    /// Build the loc-key index from the workspace root plus the vanilla install,
    /// store it in state (for CW100/CW122 on config files), and publish loc-file
    /// diagnostics (CW225/CW234/CW259/CW268/CW275) for the workspace loc files.
    #[tracing::instrument(skip_all)]
    async fn rebuild_and_publish_loc(&self, root_path: &std::path::Path) {
        let game = {
            let language = self.state.language.lock().clone();
            cwtools_game::constants::Game::from_str(&language)
        };
        let loc_game = engine_to_loc_game(game);

        let mut loc_dirs: Vec<std::path::PathBuf> = vec![root_path.to_path_buf()];
        if let Some(v) = self.state.vanilla_dir.lock().clone() {
            loc_dirs.push(v);
        }
        let dir_refs: Vec<&std::path::Path> = loc_dirs.iter().map(|p| p.as_path()).collect();
        let loc_languages = self.state.loc_languages.lock().clone();

        // Build the index and collect per-file diagnostics in one block, then
        // drop the LocService before the index is published. The service holds
        // the full per-file loc ASTs (~2M entries on Millennium Dawn); keeping
        // it alive while we also hold the lowercased key set in LocIndex
        // pushes peak RSS by hundreds of MiB for no reason. After the block
        // closes only LocIndex (keys) and the diagnostic map survive.
        let root_str = root_path.to_string_lossy().to_string();
        let (loc_index, mut by_file) = {
            let service = cwtools_localization::LocService::from_folders(&dir_refs);
            let idx = cwtools_localization::LocIndex::build_scoped(
                &service,
                loc_game,
                loc_languages.as_deref(),
            );
            let mut by_file: HashMap<String, Vec<Diagnostic>> = HashMap::new();
            for d in cwtools_localization::validate_loc_project_scoped(
                &service,
                loc_game,
                loc_languages.as_deref(),
            ) {
                if !d.file.starts_with(&root_str) {
                    continue;
                }
                let ve = loc_diag_to_validation_error(&d);
                by_file
                    .entry(d.file.clone())
                    .or_default()
                    .push(validation_error_to_diagnostic(&ve));
            }
            (idx, by_file)
        };
        *self.state.loc_index.write() = Some(loc_index);

        // Publish per-file loc diagnostics, but only for workspace loc files
        // (not vanilla). Group by file so each gets a complete diagnostic set.
        for (file, diags) in by_file.drain() {
            let uri = format!("file://{}", file);
            if let Ok(uri_obj) = Url::parse(&uri) {
                self.client.publish_diagnostics(uri_obj, diags, None).await;
            }
        }
        cwtools_profiling::log_rss("loc_rebuild_done");
    }

    /// Parse a file and add it to the symbol + info (type) indexes WITHOUT
    /// validating. The first pass of a full-workspace scan calls this for every
    /// file so cross-file references (scripted triggers/effects, type instances,
    /// templated modifiers) resolve before ANY file is validated. Without this,
    /// a file validated early can't see definitions that live in later files.
    async fn index_document(&self, uri: &str, text: &str) -> Option<ParsedFile> {
        let parsed = parse_string(text, &self.state.string_table).ok()?;
        self.index_parsed_file(uri, &parsed);
        Some(parsed)
    }

    /// Refresh the per-document token set used to scope the dependent sweep.
    /// `ast = None` (e.g. a file that failed to parse) clears the set, so the
    /// sweep treats the doc as "unknown" and always includes it.
    fn update_doc_tokens(&self, uri: &str, ast: Option<&Arc<ParsedFile>>) {
        let mut tokens = self.state.doc_tokens.write();
        match ast {
            Some(ast) => {
                tokens.insert(uri.to_string(), collect_doc_tokens(ast, &self.state.string_table));
            }
            None => {
                tokens.remove(uri);
            }
        }
    }

    /// Index an already-parsed AST into the symbol + info indexes. Extracted
    /// from `index_document` so the workspace scan can index cache-hit ASTs
    /// without re-parsing.
    fn index_parsed_file(&self, uri: &str, parsed: &ParsedFile) {
        {
            let mut index = self.state.symbol_index.lock();
            index.clear_document(uri);
            index.index_document(uri, parsed, &self.state.string_table);
        }
        let ws_uri = self.state.workspace_uri.lock().clone();
        let logical_path = logical_path_from_uri(uri, &ws_uri);
        let ruleset_guard = self.state.ruleset.lock();
        let mut info = self.state.info_service.write();
        info.clear_file(uri);
        if let Some(ruleset) = ruleset_guard.as_ref() {
            info.index_file_with_path(
                uri,
                parsed,
                &self.state.string_table,
                ruleset,
                &logical_path,
            );
        }
    }

    /// Validate an already-parsed document against the (already-built) workspace
    /// index, using a precomputed modifier-key set. No parsing, no re-indexing,
    /// no per-file logging — this is the hot path for a full-workspace scan.
    fn validate_parsed(
        &self,
        uri: &str,
        parsed: &ParsedFile,
        modifier_keys: &std::collections::HashSet<String>,
    ) -> Vec<Diagnostic> {
        let ruleset_guard = self.state.ruleset.lock();
        let Some(ruleset) = ruleset_guard.as_ref() else {
            // No ruleset: only the parse errors apply.
            return parsed
                .errors
                .iter()
                .map(parse_error_to_diagnostic)
                .collect();
        };
        let language = self.state.language.lock().clone();
        let game = cwtools_game::constants::Game::from_str(&language);
        // Build the per-run shared state for this single file. Multi-file callers
        // (the workspace scan) should build these ONCE and call
        // `validate_parsed_prebuilt` to avoid rebuilding per file.
        let registry = build_scope_registry_arc(ruleset, game);
        let enum_map = build_enum_map(ruleset);
        self.validate_parsed_prebuilt(
            uri,
            parsed,
            modifier_keys,
            ruleset,
            game,
            registry.as_ref(),
            &enum_map,
        )
    }

    /// As [`validate_parsed`], but with the ruleset already locked and the
    /// per-run scope registry / enum_map prebuilt by the caller. The
    /// full-workspace scan builds those ONCE outside its loop and reuses them.
    #[allow(clippy::too_many_arguments)]
    fn validate_parsed_prebuilt(
        &self,
        uri: &str,
        parsed: &ParsedFile,
        modifier_keys: &std::collections::HashSet<String>,
        ruleset: &RuleSet,
        game: Option<cwtools_game::constants::Game>,
        registry: Option<&std::sync::Arc<cwtools_game::scope_registry::ScopeRegistry>>,
        enum_map: &std::collections::HashMap<&str, &cwtools_rules::rules_types::EnumDefinition>,
    ) -> Vec<Diagnostic> {
        let info_guard = self.state.info_service.read();
        let loc_guard = self.state.loc_index.read();
        validate_parsed_with_indexes(
            uri,
            parsed,
            modifier_keys,
            ruleset,
            game,
            registry,
            enum_map,
            &info_guard.type_index,
            loc_guard.as_ref(),
            &self.state.string_table,
        )
    }

    /// Parse and validate a single document.
    /// Validate `uri` at `expected_version` after the debounce, but only if it is
    /// still the latest edit (a newer change supersedes it). Publishes the
    /// changed file's diagnostics, then refreshes the other open documents so
    /// cross-file references reflect the edit instead of showing stale results.
    #[tracing::instrument(skip_all, fields(uri = %uri, version = expected_version))]
    async fn debounced_validate(&self, uri: String, expected_version: i32, generation: u64) {
        // A newer change landed during the debounce — let that one validate.
        let text = {
            let docs = self.state.documents.lock();
            match docs.get(&uri) {
                Some(d) if d.version == expected_version => d.text.clone(),
                _ => return,
            }
        };

        // Snapshot the file's cross-file exports before re-indexing, so we can
        // tell whether this edit can affect any other file (see below). The
        // name set lets the dependent sweep target only docs that reference a
        // name that changed.
        let (exports_before, names_before) = {
            let info = self.state.info_service.read();
            (info.export_fingerprint(&uri), info.export_names(&uri))
        };

        let (diagnostics, parsed) = self.parse_and_validate(&uri, &text).await;
        {
            let ast = parsed.map(Arc::new);
            let mut docs = self.state.documents.lock();
            if let Some(d) = docs.get_mut(&uri)
                && d.version == expected_version {
                    self.update_doc_tokens(&uri, ast.as_ref());
                    d.ast = ast;
                }
        }
        if let Ok(uri_obj) = Url::parse(&uri) {
            self.client
                .publish_diagnostics(uri_obj, diagnostics, Some(expected_version))
                .await;
        }

        // Only sweep the other open files if this edit actually changed what the
        // file exports (a definition added/renamed/removed). Editing inside a
        // rule body leaves the exports identical, so no dependent can change and
        // the sweep is skipped entirely — the common case stays cheap.
        let (exports_after, names_after) = {
            let info = self.state.info_service.read();
            (info.export_fingerprint(&uri), info.export_names(&uri))
        };
        if exports_before != exports_after {
            // Only the names that were added or removed can change another
            // file's diagnostics. Revalidate the open docs that reference any of
            // them (symmetric difference of the before/after name sets).
            //
            // The fingerprint also tracks multiplicity, so it can differ while
            // the name SET is unchanged (e.g. a duplicate definition added, or a
            // type changed under the same name) — a case that can still flip a
            // dependent's diagnostic. When that happens `changed_names` is empty;
            // fall back to `None` (revalidate every dependent) so we never miss
            // one. Soundness beats scoping here.
            let changed_names: HashSet<String> = names_before
                .symmetric_difference(&names_after)
                .cloned()
                .collect();
            let scope = if changed_names.is_empty() {
                None
            } else {
                Some(&changed_names)
            };
            // Tagged with this edit's generation so a newer edit preempts it.
            self.revalidate_open_dependents(&uri, generation, scope)
                .await;
        } else {
            tracing::debug!(uri = %uri, "exports unchanged; skipping dependent sweep");
        }
    }

    /// Re-validate and republish every open document except `changed_uri`, using
    /// the freshly updated indexes. Bounded by the number of open files, so a
    /// definition edit propagates to the gui/event/etc. files that reference it.
    ///
    /// `generation` is the edit counter at the time the triggering change landed.
    /// If a newer edit bumps the counter while the sweep is running, the sweep
    /// stops: the newer edit's own sweep will revalidate everything against the
    /// fully-updated index, so finishing this one is wasted work (and would
    /// double-validate). Each dependent's diagnostics are published with that
    /// doc's current version, and skipped if the doc changed mid-sweep, so the
    /// sweep never clobbers a fresher in-flight result for a file being edited.
    ///
    /// `changed_names`, when `Some`, scopes the sweep to the open docs whose
    /// token set mentions one of the (lowercased) names that were added or
    /// removed. A doc with no recorded token set is always included (sound
    /// over-approximation: never skip a file that might depend on the change).
    /// `None` revalidates every open dependent (used when the exact set of
    /// changed names can't be pinned down, e.g. a multiplicity-only change).
    async fn revalidate_open_dependents(
        &self,
        changed_uri: &str,
        generation: u64,
        changed_names: Option<&HashSet<String>>,
    ) {
        // Snapshot each open dependent's cached AST (a cheap `Arc` clone) with
        // its version. The dependents' own text didn't change, so they don't
        // need re-parsing or re-indexing — only re-validation against the
        // now-updated global index. When `changed_names` is `Some`, skip docs
        // whose token set references none of the changed names.
        let others: Vec<(String, i32, Arc<ParsedFile>)> = {
            let tokens = self.state.doc_tokens.read();
            let docs = self.state.documents.lock();
            docs.iter()
                .filter(|(u, _)| u.as_str() != changed_uri)
                .filter(|(u, _)| match changed_names {
                    None => true,
                    Some(names) => match tokens.get(u.as_str()) {
                        // No token set recorded for this doc — include it rather
                        // than risk missing a real dependent.
                        None => true,
                        Some(doc_set) => names.iter().any(|n| doc_set.contains(n)),
                    },
                })
                .filter_map(|(u, d)| d.ast.clone().map(|ast| (u.clone(), d.version, ast)))
                .collect()
        };
        if others.is_empty() {
            return;
        }
        // Modifier keys only change on a full scan, so snapshot once for the
        // whole sweep rather than re-locking per file.
        let modifier_keys = self.state.modifier_keys.read().clone();
        tracing::debug!(
            count = others.len(),
            generation,
            "revalidate_open_dependents"
        );
        for (uri, snapshot_version, ast) in others {
            // Preempt: a newer edit arrived, its sweep will cover everything.
            if self.state.edit_generation.load(Ordering::SeqCst) != generation {
                tracing::debug!(generation, "revalidate_open_dependents superseded");
                return;
            }
            let diagnostics = self.validate_parsed(&uri, &ast, &modifier_keys);
            // Skip if this dependent was itself edited while we validated it —
            // its own debounced pass owns the fresher result.
            let still_current = {
                let docs = self.state.documents.lock();
                docs.get(&uri)
                    .map(|d| d.version == snapshot_version)
                    .unwrap_or(false)
            };
            if still_current
                && let Ok(uri_obj) = Url::parse(&uri) {
                    self.client
                        .publish_diagnostics(uri_obj, diagnostics, Some(snapshot_version))
                        .await;
                }
        }
    }

    #[tracing::instrument(skip_all, fields(uri = %uri, bytes = text.len()))]
    async fn parse_and_validate(
        &self,
        uri: &str,
        text: &str,
    ) -> (Vec<Diagnostic>, Option<ParsedFile>) {
        let mut diagnostics = Vec::new();

        // Localisation files are parsed and validated as loc, not config.
        if uri.ends_with(".yml") || uri.ends_with(".yaml") || uri.ends_with(".csv") {
            let path = uri.strip_prefix("file://").unwrap_or(uri);
            let union = {
                let guard = self.state.loc_index.read();
                guard
                    .as_ref()
                    .map(|idx| idx.union().clone())
                    .unwrap_or_default()
            };
            for d in cwtools_localization::validate_loc_file_text(text, path, &union) {
                let ve = loc_diag_to_validation_error(&d);
                diagnostics.push(validation_error_to_diagnostic(&ve));
            }
            return (diagnostics, None);
        }

        self.client
            .log_message(MessageType::INFO, format!("[validate] parsing: {}", uri))
            .await;

        match parse_string(text, &self.state.string_table) {
            Ok(parsed) => {
                for parse_err in &parsed.errors {
                    let diag = match parse_err {
                        ParseError::Pos(_file, line, col, msg) => Diagnostic {
                            range: Range {
                                start: Position {
                                    line: line.saturating_sub(1),
                                    character: *col as u32,
                                },
                                end: Position {
                                    line: line.saturating_sub(1),
                                    character: *col as u32 + 1,
                                },
                            },
                            severity: Some(DiagnosticSeverity::ERROR),
                            code: None,
                            code_description: None,
                            source: Some("cwtools".to_string()),
                            message: msg.clone(),
                            related_information: None,
                            tags: None,
                            data: None,
                        },
                        ParseError::General(msg) => Diagnostic {
                            range: Range {
                                start: Position::default(),
                                end: Position::default(),
                            },
                            severity: Some(DiagnosticSeverity::ERROR),
                            code: None,
                            code_description: None,
                            source: Some("cwtools".to_string()),
                            message: msg.clone(),
                            related_information: None,
                            tags: None,
                            data: None,
                        },
                    };
                    diagnostics.push(diag);
                }

                // Update symbol index
                {
                    let mut index = self.state.symbol_index.lock();
                    index.clear_document(uri);
                    index.index_document(uri, &parsed, &self.state.string_table);
                }

                // Derive logical path for type-instance indexing
                let ws_uri = self.state.workspace_uri.lock().clone();
                let logical_path = logical_path_from_uri(uri, &ws_uri);

                // Update info service
                {
                    let ruleset_guard = self.state.ruleset.lock();
                    let mut info = self.state.info_service.write();
                    info.clear_file(uri);
                    if let Some(ruleset) = ruleset_guard.as_ref() {
                        info.index_file_with_path(
                            uri,
                            &parsed,
                            &self.state.string_table,
                            ruleset,
                            &logical_path,
                        );
                    }
                }

                // Validation
                let (errors, log_msg) = {
                    let ruleset_guard = self.state.ruleset.lock();
                    if let Some(ruleset) = ruleset_guard.as_ref() {
                        let language = self.state.language.lock().clone();
                        let game = cwtools_game::constants::Game::from_str(&language);
                        let start = std::time::Instant::now();
                        // Pass the workspace TypeIndex for cross-file type reference checking.
                        let info_guard = self.state.info_service.read();
                        let type_index = &info_guard.type_index;
                        let modifier_keys = self.state.modifier_keys.read();
                        let loc_guard = self.state.loc_index.read();
                        // Single-file path: build the per-run shared state once
                        // for this one file.
                        let registry = build_scope_registry_arc(ruleset, game);
                        let enum_map = build_enum_map(ruleset);
                        let mut errs = validate_ast_with_loc_prebuilt(
                            &parsed,
                            ruleset,
                            &self.state.string_table,
                            uri,
                            game,
                            Some(type_index),
                            Some(&*modifier_keys),
                            loc_guard.as_ref(),
                            registry.as_ref(),
                            &enum_map,
                        );
                        drop(loc_guard);
                        drop(modifier_keys);
                        drop(info_guard);
                        let elapsed = start.elapsed();
                        const MAX_ERRORS: usize = 100;
                        let total = errs.len();
                        if total > MAX_ERRORS {
                            errs.truncate(MAX_ERRORS);
                            errs.push(cwtools_validation::ValidationError {
                                message: format!(
                                    "... {} additional errors truncated",
                                    total - MAX_ERRORS
                                ),
                                severity: cwtools_validation::ErrorSeverity::Information,
                                line: 0,
                                col: 0,
                                file: uri.to_string(),
                                code: None,
                            });
                        }
                        let msg = format!(
                            "[validate] {} errors in {:?} ({} types, {} enums, {} aliases)",
                            total,
                            elapsed,
                            ruleset.types.len(),
                            ruleset.enums.len(),
                            ruleset.aliases.len()
                        );
                        (errs, Some(msg))
                    } else {
                        (Vec::new(), None)
                    }
                };

                if let Some(msg) = log_msg {
                    self.client.log_message(MessageType::INFO, msg).await;
                }

                for err in &errors {
                    diagnostics.push(validation_error_to_diagnostic(err));
                }
                (diagnostics, Some(parsed))
            }
            Err(e) => {
                diagnostics.push(Diagnostic {
                    range: Range {
                        start: Position::default(),
                        end: Position::default(),
                    },
                    severity: Some(DiagnosticSeverity::ERROR),
                    code: None,
                    code_description: None,
                    source: Some("cwtools".to_string()),
                    message: format!("Parse error: {}", e),
                    related_information: None,
                    tags: None,
                    data: None,
                });
                (diagnostics, None)
            }
        }
    }

    /// Rebuild the cached modifier-key set from the current ruleset and type index.
    fn rebuild_modifier_keys(&self) {
        let ruleset_guard = self.state.ruleset.lock();
        let info_guard = self.state.info_service.read();
        let keys = match ruleset_guard.as_ref() {
            Some(rs) => build_modifier_keys(rs, &info_guard.type_index),
            None => HashSet::new(),
        };
        *self.state.modifier_keys.write() = keys;
    }

    /// Lazily index the base-game install into `vanilla_index` (once). Resolves
    /// the dir from the `vanilla` init option, falling back to auto-discovery by
    /// game. No-op if already indexed, if no dir is found, or if the ruleset
    /// isn't loaded yet (we need it to know which type each definition is).
    async fn ensure_vanilla_index(&self) {
        // Already have a vanilla index (from a cache or a prior build)? Done.
        if self.state.vanilla_index.lock().is_some() {
            return;
        }
        // Resolve the install dir: explicit `vanilla` option, else auto-discover.
        let dir = {
            let explicit = self.state.vanilla_dir.lock().clone();
            explicit.or_else(|| {
                let game = self.state.language.lock().clone();
                discover_vanilla_dir(&game)
            })
        };
        let dir = match dir {
            Some(d) if d.is_dir() => d,
            _ => return,
        };

        let game = self.state.language.lock().clone();

        // We need the ruleset both to key the cache (the fingerprint folds in the
        // ruleset shape) and to map definitions to their types when rebuilding.
        // Clone it out in its own statement so the parking_lot guard is dropped
        // before the `match` (guards aren't Send and the None arm awaits below).
        let ruleset_opt = self.state.ruleset.lock().clone();
        let ruleset = match ruleset_opt {
            Some(rs) => rs,
            None => {
                self.client
                    .log_message(
                        MessageType::WARNING,
                        "Base-game dir set but no rules loaded yet; skipping vanilla index.",
                    )
                    .await;
                return;
            }
        };

        // Fingerprint = base-game version + ruleset shape. The base game only
        // changes when it updates and the rules only when the config changes, so a
        // cache keyed by both is reused across sessions and is safe to publish, yet
        // a rules change correctly invalidates it (the cached instances are
        // extracted by the rules; see `vanilla_cache::combined_fingerprint`).
        let fingerprint = cwtools_info::vanilla_cache::combined_fingerprint(&dir, &ruleset);
        let cache_path = self.vanilla_cache_path(&game, &fingerprint);

        // Try a fresh cache first — skip parsing the whole base game entirely.
        if let Some(cp) = &cache_path
            && cp.exists() {
                match cwtools_info::vanilla_cache::load(cp) {
                    Ok((cache_game, cache_fp, per_type))
                        if cache_game == game && cache_fp == fingerprint =>
                    {
                        let total: usize = per_type.values().map(|v| v.len()).sum();
                        *self.state.vanilla_index.lock() = Some(per_type);
                        self.client
                            .log_message(
                                MessageType::INFO,
                                format!(
                                    "Loaded {} base-game instances from cache {} ({})",
                                    total,
                                    cp.display(),
                                    fingerprint
                                ),
                            )
                            .await;
                        return;
                    }
                    Ok((_, cache_fp, _)) => {
                        self.client
                            .log_message(
                                MessageType::INFO,
                                format!(
                                    "Vanilla cache stale (cached {}, install {}); rebuilding",
                                    cache_fp, fingerprint
                                ),
                            )
                            .await;
                    }
                    Err(e) => {
                        self.client
                            .log_message(
                                MessageType::WARNING,
                                format!("Could not load vanilla cache {}: {}", cp.display(), e),
                            )
                            .await;
                    }
                }
            }

        self.send_loading_bar(true, "Indexing base game…").await;
        self.client
            .log_message(
                MessageType::INFO,
                format!(
                    "Indexing base game at {} ({}) …",
                    dir.display(),
                    fingerprint
                ),
            )
            .await;

        // Indexing parses thousands of files; run it off the async executor.
        let table = self.state.string_table.clone();
        let index_dir = dir.clone();
        let per_type =
            tokio::task::spawn_blocking(move || index_vanilla_dir(&index_dir, &ruleset, &table))
                .await
                .unwrap_or_default();

        let total: usize = per_type.values().map(|v| v.len()).sum();

        // Persist for next startup so the base game isn't re-parsed every time.
        if let Some(cp) = &cache_path {
            match cwtools_info::vanilla_cache::save_per_type(&per_type, &game, &fingerprint, cp) {
                Ok(n) => {
                    self.client
                        .log_message(
                            MessageType::INFO,
                            format!(
                                "Cached {} base-game instances to {} ({})",
                                n,
                                cp.display(),
                                fingerprint
                            ),
                        )
                        .await
                }
                Err(e) => {
                    self.client
                        .log_message(
                            MessageType::WARNING,
                            format!("Could not write vanilla cache {}: {}", cp.display(), e),
                        )
                        .await
                }
            }
        }

        *self.state.vanilla_index.lock() = Some(per_type);
        self.client
            .log_message(
                MessageType::INFO,
                format!("Indexed {} base-game instances.", total),
            )
            .await;
    }

    /// Path of the persistent base-game cache for `game` at `fingerprint`, under
    /// the client-provided `cacheDir` (else an OS cache dir). Versioned in the
    /// filename so multiple game versions can coexist and a published cache for a
    /// given version drops straight in. `None` if no cache dir can be resolved.
    fn vanilla_cache_path(&self, game: &str, fingerprint: &str) -> Option<std::path::PathBuf> {
        let base = self
            .state
            .cache_dir
            .lock()
            .clone()
            .or_else(default_cache_dir)?;
        let safe = |s: &str| -> String {
            s.chars()
                .map(|c| {
                    if c.is_ascii_alphanumeric() || c == '.' || c == '-' {
                        c
                    } else {
                        '_'
                    }
                })
                .collect()
        };
        Some(base.join(format!("vanilla-{}-{}.json", safe(game), safe(fingerprint))))
    }

    async fn determine_file_types(&self, uri: &str) -> Vec<String> {
        let path = uri.to_lowercase();
        let mut types = Vec::new();

        if path.contains("/events/") {
            types.push("event".to_string());
        }
        if path.contains("/common/") {
            types.push("script".to_string());
        }
        if path.contains("/common/scripted_effects") {
            types.push("scripted_effect".to_string());
        }
        if path.contains("/common/scripted_triggers") {
            types.push("scripted_trigger".to_string());
        }
        if path.ends_with(".txt") {
            types.push("txt".to_string());
        }

        types
    }
}

// ── Use-site scanning ─────────────────────────────────────────────────────────

/// Scan all documents indexed in `info` (whose text is in `docs`) for leaves
/// whose value equals `instance_name` and whose rule context is a TypeField
/// for `type_name`.
///
/// Returns a list of (file_uri, SourceLocation) use-sites.
///
/// Implementation: walks every leaf in every indexed file's AST.  For each
/// leaf, we call `info_at_position` to classify it.  If it comes back as a
/// TypeRef for the right type+name, we record the location.
///
/// This is O(files × leaves) but runs only on demand (find-references / rename)
/// so is acceptable for mod-sized workspaces.
fn scan_use_sites(
    type_name: &str,
    instance_name: &str,
    docs: &HashMap<String, ParsedDoc>,
    ruleset: &RuleSet,
    workspace_uri: &Option<String>,
    string_table: &cwtools_string_table::string_table::StringTable,
) -> Vec<(String, cwtools_info::SourceLocation)> {
    let mut results = Vec::new();

    for (file_uri, parsed_doc) in docs {
        let ast = match &parsed_doc.ast {
            Some(a) => a,
            None => continue,
        };
        let logical_path = logical_path_from_uri(file_uri, workspace_uri);

        scan_ast_for_type_ref(
            &ast.root_children,
            &ast.arena,
            type_name,
            instance_name,
            file_uri,
            ruleset,
            &logical_path,
            string_table,
            &mut results,
        );
    }

    results
}

/// Recursively walk children and record leaves whose value classifies as a
/// TypeRef for the specified type+name.
#[allow(clippy::too_many_arguments)]
fn scan_ast_for_type_ref(
    children: &[cwtools_parser::ast::Child],
    arena: &cwtools_parser::ast::Arena,
    type_name: &str,
    instance_name: &str,
    file_uri: &str,
    ruleset: &RuleSet,
    logical_path: &str,
    table: &cwtools_string_table::string_table::StringTable,
    out: &mut Vec<(String, cwtools_info::SourceLocation)>,
) {
    use cwtools_parser::ast::{Child, Value};

    for child in children {
        match child {
            Child::Leaf(idx) => {
                let leaf = &arena.leaves[*idx as usize];
                let key = table.get_string(leaf.key.normal).unwrap_or_default();
                let val = match &leaf.value {
                    Value::String(t) | Value::QString(t) => {
                        table.get_string(t.normal).unwrap_or_default()
                    }
                    _ => String::new(),
                };
                if val == instance_name {
                    // Check if this leaf's rule context is TypeField(type_name)
                    if is_type_ref_leaf(ruleset, &key, type_name, logical_path) {
                        out.push((
                            file_uri.to_string(),
                            cwtools_info::SourceLocation {
                                line: leaf.pos.start.line,
                                col: leaf.pos.start.col,
                            },
                        ));
                    }
                }
                // Recurse into clause values
                if let Value::Clause(ch) = &leaf.value {
                    scan_ast_for_type_ref(
                        ch,
                        arena,
                        type_name,
                        instance_name,
                        file_uri,
                        ruleset,
                        logical_path,
                        table,
                        out,
                    );
                }
            }
            Child::Node(idx) => {
                let node = &arena.nodes[*idx as usize];
                scan_ast_for_type_ref(
                    &node.children,
                    arena,
                    type_name,
                    instance_name,
                    file_uri,
                    ruleset,
                    logical_path,
                    table,
                    out,
                );
            }
            Child::LeafValue(idx) => {
                let lv = &arena.leaf_values[*idx as usize];
                let val = match &lv.value {
                    cwtools_parser::ast::Value::String(t)
                    | cwtools_parser::ast::Value::QString(t) => {
                        table.get_string(t.normal).unwrap_or_default()
                    }
                    _ => String::new(),
                };
                if val == instance_name {
                    // LeafValue type refs: classified via parent context — best effort skip for now.
                    let _ = (type_name, logical_path, ruleset);
                }
            }
            _ => {}
        }
    }
}

/// Check if a leaf with key `leaf_key` is a TypeField reference to `type_name`.
/// Walks root_rules shallowly (depth 1) looking for a LeafRule whose left
/// is SpecificField(leaf_key) and right is TypeField(Simple(type_name)).
fn is_type_ref_leaf(
    ruleset: &RuleSet,
    leaf_key: &str,
    type_name: &str,
    logical_path: &str,
) -> bool {
    for root_rule in &ruleset.root_rules {
        let (rule_type_name, (rule_type, _)) = match root_rule {
            RootRule::TypeRule(n, r) => (Some(n.as_str()), r),
            RootRule::AliasRule(n, r) => (Some(n.as_str()), r),
            RootRule::SingleAliasRule(n, r) => (Some(n.as_str()), r),
        };

        // For TypeRules, check path filter
        if let RootRule::TypeRule(..) = root_rule
            && let Some(name) = rule_type_name
                && let Some(&idx) = ruleset.type_by_name.get(name) {
                    let td = &ruleset.types[idx];
                    if !cwtools_info_path_check(&td.path_options, logical_path) {
                        continue;
                    }
                }

        let rules = match rule_type {
            RuleType::NodeRule { rules, .. } => rules.as_slice(),
            _ => continue,
        };

        for (inner, _) in rules {
            if let RuleType::LeafRule {
                left: NewField::SpecificField(k),
                right: NewField::TypeField(cwtools_rules::rules_types::TypeType::Simple(t)),
            } = inner
                && k.eq_ignore_ascii_case(leaf_key) && t == type_name {
                    return true;
                }
        }
    }
    false
}

/// Collect the lowercased identifier-like tokens a parsed file mentions: every
/// key and every (quoted or unquoted) string value, plus key/value prefixes.
/// Used by the dependent sweep to decide which open docs reference a changed
/// export. Deliberately broad (an over-approximation): including a token that
/// isn't really a cross-file reference only costs an extra revalidation, while
/// missing one would silently skip a file that should be revalidated.
fn collect_doc_tokens(ast: &ParsedFile, table: &StringTable) -> HashSet<String> {
    use cwtools_parser::ast::Value;
    let mut tokens = HashSet::new();
    let push = |id: cwtools_string_table::string_table::StringId, set: &mut HashSet<String>| {
        if let Some(s) = table.get_string(id)
            && !s.is_empty()
        {
            set.insert(s);
        }
    };
    // The arena holds every element flatly, so iterating the per-kind vectors
    // covers the whole tree without a recursive walk. `.lower` is the canonical
    // lowercased form, so the resulting set is already case-folded.
    let arena = &ast.arena;
    for leaf in &arena.leaves {
        push(leaf.key.lower, &mut tokens);
        if let Value::String(t) | Value::QString(t) = &leaf.value {
            push(t.lower, &mut tokens);
        }
    }
    for node in &arena.nodes {
        push(node.key.lower, &mut tokens);
        if let Some(p) = &node.key_prefix {
            push(p.lower, &mut tokens);
        }
        if let Some(p) = &node.value_prefix {
            push(p.lower, &mut tokens);
        }
    }
    for lv in &arena.leaf_values {
        if let Value::String(t) | Value::QString(t) = &lv.value {
            push(t.lower, &mut tokens);
        }
    }
    for vc in &arena.value_clauses {
        for k in &vc.keys {
            push(k.lower, &mut tokens);
        }
    }
    tokens
}

/// Validate one already-parsed file against caller-supplied indexes, returning
/// LSP diagnostics. The `type_index` / `loc_index` references are passed in (not
/// re-locked here) so the full-workspace pass can take a single read guard and
/// share it across rayon threads — `&TypeIndex` / `&LocIndex` are `Sync`. This
/// is the lock-free core of [`Backend::validate_parsed_prebuilt`].
#[allow(clippy::too_many_arguments)]
fn validate_parsed_with_indexes(
    uri: &str,
    parsed: &ParsedFile,
    modifier_keys: &std::collections::HashSet<String>,
    ruleset: &RuleSet,
    game: Option<cwtools_game::constants::Game>,
    registry: Option<&std::sync::Arc<cwtools_game::scope_registry::ScopeRegistry>>,
    enum_map: &std::collections::HashMap<&str, &cwtools_rules::rules_types::EnumDefinition>,
    type_index: &TypeIndex,
    loc_index: Option<&cwtools_localization::LocIndex>,
    string_table: &StringTable,
) -> Vec<Diagnostic> {
    let mut diagnostics: Vec<Diagnostic> = parsed
        .errors
        .iter()
        .map(parse_error_to_diagnostic)
        .collect();
    let mut errs = validate_ast_with_loc_prebuilt(
        parsed,
        ruleset,
        string_table,
        uri,
        game,
        Some(type_index),
        Some(modifier_keys),
        loc_index,
        registry,
        enum_map,
    );
    const MAX_ERRORS: usize = 100;
    let total = errs.len();
    if total > MAX_ERRORS {
        errs.truncate(MAX_ERRORS);
        errs.push(cwtools_validation::ValidationError {
            message: format!("... {} additional errors truncated", total - MAX_ERRORS),
            severity: cwtools_validation::ErrorSeverity::Information,
            line: 0,
            col: 0,
            file: uri.to_string(),
            code: None,
        });
    }
    for err in &errs {
        diagnostics.push(validation_error_to_diagnostic(err));
    }
    diagnostics
}

fn parse_error_to_diagnostic(e: &ParseError) -> Diagnostic {
    let (line, col, msg) = match e {
        ParseError::Pos(_f, line, col, msg) => (line.saturating_sub(1), *col as u32, msg.clone()),
        ParseError::General(msg) => (0, 0, msg.clone()),
    };
    Diagnostic {
        range: Range {
            start: Position {
                line,
                character: col,
            },
            end: Position {
                line,
                character: col + 1,
            },
        },
        severity: Some(DiagnosticSeverity::ERROR),
        code: None,
        code_description: None,
        source: Some("cwtools".to_string()),
        message: msg,
        related_information: None,
        tags: None,
        data: None,
    }
}

fn validation_error_to_diagnostic(err: &ValidationError) -> Diagnostic {
    Diagnostic {
        range: Range {
            start: Position {
                line: err.line.saturating_sub(1),
                character: err.col as u32,
            },
            end: Position {
                line: err.line.saturating_sub(1),
                character: err.col as u32 + 1,
            },
        },
        severity: match err.severity {
            cwtools_validation::ErrorSeverity::Error => Some(DiagnosticSeverity::ERROR),
            cwtools_validation::ErrorSeverity::Warning => Some(DiagnosticSeverity::WARNING),
            cwtools_validation::ErrorSeverity::Information => Some(DiagnosticSeverity::INFORMATION),
            cwtools_validation::ErrorSeverity::Hint => Some(DiagnosticSeverity::HINT),
        },
        code: err
            .code
            .as_deref()
            .map(|c| NumberOrString::String(c.to_string())),
        code_description: None,
        source: Some("cwtools".to_string()),
        message: err.message.clone(),
        related_information: None,
        tags: None,
        data: None,
    }
}

fn main() {
    // Logs/profiling go to stderr (stdout is the LSP JSON-RPC channel). Quiet
    // unless RUST_LOG or CWTOOLS_PROFILE is set. See PROFILING.md.
    cwtools_profiling::init_tracing();
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(async {
            let state = Arc::new(DocumentState::new());
            let (stdin, stdout) = (tokio::io::stdin(), tokio::io::stdout());
            // Use LspService::build to register the custom didFocusFile notification
            // so tower-lsp doesn't reject it with an error response.
            let (service, socket) = LspService::build(|client| Backend {
                client,
                state: state.clone(),
            })
            .custom_method("didFocusFile", Backend::on_did_focus_file)
            .finish();
            Server::new(stdin, stdout, socket).serve(service).await;
        });
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use cwtools_parser::parser::parse_string;
    use cwtools_rules::rules_types::{
        EnumDefinition, NewField, NewRule, Options, PathOptions, RootRule, RuleType,
        TypeDefinition, ValueType,
    };
    use cwtools_string_table::string_table::StringTable;

    // ── helpers ──────────────────────────────────────────────────────────────

    fn make_leaf_rule(key: &str, right: NewField) -> NewRule {
        (
            RuleType::LeafRule {
                left: NewField::SpecificField(key.to_string()),
                right,
            },
            Options::default(),
        )
    }

    fn make_node_rule(key: &str, children: Vec<NewRule>) -> NewRule {
        (
            RuleType::NodeRule {
                left: NewField::SpecificField(key.to_string()),
                rules: children,
            },
            Options::default(),
        )
    }

    fn bool_enum_ruleset() -> RuleSet {
        let mut rs = RuleSet::new();

        // enum: my_enum = { alpha beta gamma }
        rs.enums.push(EnumDefinition {
            key: "my_enum".to_string(),
            description: String::new(),
            values: vec!["alpha".to_string(), "beta".to_string(), "gamma".to_string()],
        });

        // type: my_type paths = { events }
        rs.types.push(TypeDefinition {
            name: "my_type".to_string(),
            name_field: Some("id".to_string()),
            path_options: PathOptions {
                paths: vec!["events".to_string()],
                path_strict: false,
                path_file: None,
                path_extension: None,
                paths_lower: Vec::new(),
            },
            subtypes: Vec::new(),
            type_key_filter: None,
            skip_root_key: Vec::new(),
            starts_with: None,
            type_per_file: false,
            key_prefix: None,
            warning_only: false,
            unique: false,
            should_be_referenced: false,
            localisation: Vec::new(),
            graph_related_types: Vec::new(),
            modifiers: Vec::new(),
        });

        // TypeRule for my_type with child fields
        rs.root_rules.push(RootRule::TypeRule(
            "my_type".to_string(),
            make_node_rule(
                "my_type",
                vec![
                    make_leaf_rule(
                        "kind",
                        NewField::ValueField(ValueType::Enum("my_enum".to_string())),
                    ),
                    make_leaf_rule("active", NewField::ValueField(ValueType::Bool)),
                ],
            ),
        ));

        rs.reindex();
        rs
    }

    // ── hover markdown tests ─────────────────────────────────────────────────

    #[test]
    fn test_hover_type_ref() {
        let md = build_hover_markdown(
            &PositionElement::Leaf {
                key: "ethos".to_string(),
                value: "my_ethos".to_string(),
            },
            &ReferenceHint::TypeRef {
                type_name: "ethoses".to_string(),
                value: "my_ethos".to_string(),
            },
            None,
        );
        assert!(md.contains("Type reference"), "got: {}", md);
        assert!(md.contains("my_ethos"), "got: {}", md);
        assert!(md.contains("ethoses"), "got: {}", md);
    }

    #[test]
    fn test_hover_enum_ref() {
        let md = build_hover_markdown(
            &PositionElement::Leaf {
                key: "kind".to_string(),
                value: "alpha".to_string(),
            },
            &ReferenceHint::EnumRef {
                enum_name: "my_enum".to_string(),
                value: "alpha".to_string(),
            },
            None,
        );
        assert!(md.contains("Enum value"), "got: {}", md);
        assert!(md.contains("alpha"), "got: {}", md);
        assert!(md.contains("my_enum"), "got: {}", md);
    }

    #[test]
    fn test_hover_unknown_falls_back_to_raw() {
        let md = build_hover_markdown(
            &PositionElement::Leaf {
                key: "foo".to_string(),
                value: "bar".to_string(),
            },
            &ReferenceHint::Unknown,
            None,
        );
        assert!(md.contains("foo") && md.contains("bar"), "got: {}", md);
    }

    #[test]
    fn test_hover_with_rule_description() {
        let mut rs = bool_enum_ruleset();
        // Add a description to the "kind" leaf rule
        if let Some(RootRule::TypeRule(_, (RuleType::NodeRule { rules, .. }, _))) =
            rs.root_rules.first_mut()
            && let Some((_, opts)) = rules.first_mut() {
                opts.description = Some("The kind of this thing".to_string());
            }

        let md = build_hover_markdown(
            &PositionElement::Leaf {
                key: "kind".to_string(),
                value: "alpha".to_string(),
            },
            &ReferenceHint::EnumRef {
                enum_name: "my_enum".to_string(),
                value: "alpha".to_string(),
            },
            Some(&rs),
        );
        assert!(md.contains("The kind of this thing"), "got: {}", md);
    }

    // ── completion context tests ─────────────────────────────────────────────

    #[test]
    fn test_completions_from_rules_enum() {
        let rs = bool_enum_ruleset();
        let info = cwtools_info::InfoService::new();

        // Grab the inner rules from the TypeRule
        let rules = if let Some(RootRule::TypeRule(_, (RuleType::NodeRule { rules, .. }, _))) =
            rs.root_rules.first()
        {
            rules.as_slice()
        } else {
            panic!("expected TypeRule");
        };

        let items = completions_from_rules(rules, &rs, &info, "stellaris");

        // "kind" should appear with a snippet containing enum values
        let kind_item = items.iter().find(|i| i.label == "kind");
        assert!(
            kind_item.is_some(),
            "expected 'kind' completion, got: {:?}",
            items.iter().map(|i| &i.label).collect::<Vec<_>>()
        );
        let kind = kind_item.unwrap();
        assert_eq!(kind.insert_text_format, Some(InsertTextFormat::SNIPPET));
        let snippet = kind.insert_text.as_deref().unwrap_or("");
        assert!(snippet.contains("alpha"), "snippet: {}", snippet);

        // "active" should have yes/no snippet
        let active_item = items.iter().find(|i| i.label == "active");
        assert!(active_item.is_some(), "expected 'active' completion");
        let active = active_item.unwrap();
        let asnip = active.insert_text.as_deref().unwrap_or("");
        assert!(asnip.contains("yes"), "active snippet: {}", asnip);
    }

    #[test]
    fn test_enclosing_key_path() {
        let table = StringTable::new();
        let source = "country_event = {\n  id = foo\n}\n";
        let parsed = parse_string(source, &table).unwrap();

        // Line 2 (1-based), somewhere in the id leaf
        let path = enclosing_key_path(&parsed, 2, 5, &table);
        assert_eq!(path, vec!["country_event"], "got: {:?}", path);
    }

    #[test]
    fn test_logical_path_from_uri_strips_workspace() {
        let ws = Some("file:///home/user/mod".to_string());
        let lp = logical_path_from_uri("file:///home/user/mod/events/foo.txt", &ws);
        assert_eq!(lp, "events/foo.txt");
    }

    #[test]
    fn test_logical_path_fallback() {
        let lp = logical_path_from_uri("file:///some/path/events/foo.txt", &None);
        assert_eq!(lp, "/some/path/events/foo.txt");
    }

    // ── snippet generation tests ─────────────────────────────────────────────

    #[test]
    fn test_generate_node_snippet_no_required_fields() {
        let rs = bool_enum_ruleset();
        // Build a rule with no required children (min=0)
        let snippet = generate_node_snippet("my_block", &[], &rs);
        assert!(snippet.contains("my_block = {"), "got: {}", snippet);
        assert!(
            snippet.contains("$0"),
            "expected cursor $0, got: {}",
            snippet
        );
    }

    #[test]
    fn test_generate_node_snippet_with_required_bool() {
        let rs = bool_enum_ruleset();
        // Build rules with min=1
        let required_rules = vec![(
            RuleType::LeafRule {
                left: NewField::SpecificField("active".to_string()),
                right: NewField::ValueField(ValueType::Bool),
            },
            Options {
                min: 1,
                ..Options::default()
            },
        )];
        let snippet = generate_node_snippet("my_type", &required_rules, &rs);
        assert!(snippet.contains("my_type = {"), "got: {}", snippet);
        assert!(
            snippet.contains("active"),
            "expected 'active' in snippet: {}",
            snippet
        );
        assert!(
            snippet.contains("yes") || snippet.contains("${1"),
            "expected bool placeholder: {}",
            snippet
        );
    }

    #[test]
    fn test_generate_node_snippet_with_required_enum() {
        let rs = bool_enum_ruleset();
        let required_rules = vec![(
            RuleType::LeafRule {
                left: NewField::SpecificField("kind".to_string()),
                right: NewField::ValueField(ValueType::Enum("my_enum".to_string())),
            },
            Options {
                min: 1,
                ..Options::default()
            },
        )];
        let snippet = generate_node_snippet("my_type", &required_rules, &rs);
        // The enum values alpha, beta, gamma should appear as choices
        assert!(
            snippet.contains("alpha"),
            "expected enum choices in snippet: {}",
            snippet
        );
    }

    #[test]
    fn test_generate_node_snippet_ignores_optional_fields() {
        let rs = bool_enum_ruleset();
        // Only the min=1 field should appear; min=0 should not.
        let rules = vec![
            (
                RuleType::LeafRule {
                    left: NewField::SpecificField("required_field".to_string()),
                    right: NewField::ValueField(ValueType::Bool),
                },
                Options {
                    min: 1,
                    ..Options::default()
                },
            ),
            (
                RuleType::LeafRule {
                    left: NewField::SpecificField("optional_field".to_string()),
                    right: NewField::ValueField(ValueType::Bool),
                },
                Options {
                    min: 0,
                    ..Options::default()
                },
            ),
        ];
        let snippet = generate_node_snippet("my_type", &rules, &rs);
        assert!(
            snippet.contains("required_field"),
            "should have required: {}",
            snippet
        );
        assert!(
            !snippet.contains("optional_field"),
            "should not have optional: {}",
            snippet
        );
    }

    // ── root-type snippets tests ─────────────────────────────────────────────

    #[test]
    fn test_root_type_snippets_path_match() {
        let rs = bool_enum_ruleset();
        // The type "my_type" is in path "events"
        let items = root_type_snippets(&rs, "events/test.txt");
        let labels: Vec<&str> = items.iter().map(|i| i.label.as_str()).collect();
        assert!(
            labels.contains(&"my_type") || !labels.is_empty(),
            "expected type items: {:?}",
            labels
        );
    }

    #[test]
    fn test_root_type_snippets_path_mismatch() {
        let rs = bool_enum_ruleset();
        // The type "my_type" is in path "events", not "common"
        let items = root_type_snippets(&rs, "common/foo.txt");
        assert!(
            items.is_empty(),
            "should not offer types for wrong path, got: {:?}",
            items.iter().map(|i| &i.label).collect::<Vec<_>>()
        );
    }

    // ── use-site scanning tests ──────────────────────────────────────────────

    #[test]
    fn test_is_type_ref_leaf() {
        let mut rs = bool_enum_ruleset();
        // Add a TypeRule with a leaf that references type "my_type"
        rs.root_rules.push(RootRule::TypeRule(
            "owner_type".to_string(),
            (
                RuleType::NodeRule {
                    left: NewField::SpecificField("owner_type".to_string()),
                    rules: vec![(
                        RuleType::LeafRule {
                            left: NewField::SpecificField("base".to_string()),
                            right: NewField::TypeField(
                                cwtools_rules::rules_types::TypeType::Simple("my_type".to_string()),
                            ),
                        },
                        Options::default(),
                    )],
                },
                Options::default(),
            ),
        ));

        // "base" field referencing "my_type" should be recognized
        assert!(is_type_ref_leaf(&rs, "base", "my_type", "events/test.txt"));
        // "base" field referencing a different type should not match
        assert!(!is_type_ref_leaf(
            &rs,
            "base",
            "other_type",
            "events/test.txt"
        ));
        // unrelated field should not match
        assert!(!is_type_ref_leaf(
            &rs,
            "unrelated",
            "my_type",
            "events/test.txt"
        ));
    }

    #[test]
    fn test_scan_use_sites() {
        let table = StringTable::new();
        // Nested: foo node containing a leaf "base = my_instance"
        let source = "foo = { base = my_instance }\n";
        let parsed = parse_string(source, &table).unwrap();

        let mut rs = bool_enum_ruleset();
        // Use an AliasRule (not path-filtered) that contains base -> TypeField(my_type)
        rs.root_rules.push(RootRule::AliasRule(
            "effect:use_type".to_string(),
            (
                RuleType::NodeRule {
                    left: NewField::SpecificField("use_type".to_string()),
                    rules: vec![(
                        RuleType::LeafRule {
                            left: NewField::SpecificField("base".to_string()),
                            right: NewField::TypeField(
                                cwtools_rules::rules_types::TypeType::Simple("my_type".to_string()),
                            ),
                        },
                        Options::default(),
                    )],
                },
                Options::default(),
            ),
        ));

        let mut docs = HashMap::new();
        docs.insert(
            "file:///test.txt".to_string(),
            ParsedDoc {
                version: 0,
                text: source.to_string(),
                ast: Some(Arc::new(parsed)),
            },
        );

        let ws_uri = Some("file:///".to_string());
        let sites = scan_use_sites("my_type", "my_instance", &docs, &rs, &ws_uri, &table);
        assert!(!sites.is_empty(), "expected use sites, got none");
        assert!(
            sites.iter().any(|(uri, _)| uri == "file:///test.txt"),
            "expected correct uri"
        );
    }

    // ── vanilla indexing tests ───────────────────────────────────────────────

    #[test]
    fn test_discover_vanilla_dir_unknown_game_is_none() {
        assert!(discover_vanilla_dir("not_a_real_game").is_none());
        assert!(discover_vanilla_dir("").is_none());
    }

    #[test]
    fn test_index_vanilla_dir_collects_instances() {
        // A type[foo] whose instances live under common/foos; the node key is the
        // instance name (no name_field). Mirrors how a base-game type is indexed.
        let mut rs = RuleSet::new();
        rs.types.push(TypeDefinition {
            name: "foo".to_string(),
            name_field: None,
            path_options: PathOptions {
                paths: vec!["common/foos".to_string()],
                path_strict: false,
                path_file: None,
                path_extension: None,
                paths_lower: Vec::new(),
            },
            subtypes: Vec::new(),
            type_key_filter: None,
            skip_root_key: Vec::new(),
            starts_with: None,
            type_per_file: false,
            key_prefix: None,
            warning_only: false,
            unique: false,
            should_be_referenced: false,
            localisation: Vec::new(),
            graph_related_types: Vec::new(),
            modifiers: Vec::new(),
        });
        rs.reindex();

        // Lay out a tiny "game install" in a temp dir.
        let root = std::env::temp_dir().join("cwtools_lsp_vanilla_test");
        let foos = root.join("common").join("foos");
        std::fs::create_dir_all(&foos).unwrap();
        std::fs::write(foos.join("a.txt"), "foo_one = { }\nfoo_two = { }\n").unwrap();

        let table = StringTable::new();
        let per_type = index_vanilla_dir(&root, &rs, &table);

        let names: Vec<&str> = per_type
            .get("foo")
            .map(|v| v.iter().map(|i| i.name.as_str()).collect())
            .unwrap_or_default();
        assert!(names.contains(&"foo_one"), "got: {:?}", names);
        assert!(names.contains(&"foo_two"), "got: {:?}", names);

        let _ = std::fs::remove_dir_all(&root);
    }
}
