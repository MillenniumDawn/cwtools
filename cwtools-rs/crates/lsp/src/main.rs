use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};

use cwtools_info::{PositionElement, ReferenceHint, TypeInstance};
use cwtools_parser::ast::ParsedFile;
use cwtools_rules::rules_types::{NewField, RuleSet, RuleType, TypeType, ValueType};
use cwtools_rules::ruleset_loader::load_ruleset_from_dir;
use cwtools_string_table::string_table::StringTable;
use cwtools_validation::position::rules_at_pos;
use cwtools_validation::{
    Prepared, build_enum_map, build_modifier_keys, build_scope_registry_arc, checks_from_env,
};

mod completion;
mod hover;
mod navigation;
mod paths;
mod symbols;
mod validate;
mod workspace_cache;

use validate::{
    loc_diag_to_validation_error, parse_error_to_diagnostic, validate_parsed_with_indexes,
    validation_error_to_diagnostic,
};

use paths::{
    default_cache_dir, discover_vanilla_dir, logical_path_from_uri, path_to_uri, strip_loc_quotes,
    uri_to_path_str,
};

/// Index a base-game ("vanilla") install into per-type instances, ready to merge
/// into the workspace TypeIndex. Delegates to the shared driver's `index_game_dir`
/// so the LSP and CLI discover and index vanilla the SAME way (the driver's
/// `search_config_for` config, which is the broader, corpus-verified one). The
/// discovered ASTs are used directly (no re-parse) because vanilla files are only
/// indexed, never validated. Drops the per-instance file_uri; the merge slot only
/// needs the instances.
///
/// Also returns the cache aux payload (loc keys, file paths, variable names) so
/// a cache written by the LSP is as complete as one from the CLI's
/// `cache-vanilla`.
fn index_vanilla_dir(
    dir: &std::path::Path,
    ruleset: &RuleSet,
    table: &StringTable,
) -> (
    HashMap<String, Vec<TypeInstance>>,
    cwtools_info::vanilla_cache::VanillaCacheAux,
) {
    let var_effects = cwtools_info::variable_defining_effects(ruleset);
    let index = cwtools_driver::index_game_dir(dir, ruleset, table, &var_effects);
    let aux = cwtools_driver::build_vanilla_cache_aux(dir, &index);
    let per_type = index
        .map
        .into_iter()
        .map(|(k, v)| (k, v.into_iter().map(|(_, inst)| inst).collect()))
        .collect();
    (per_type, aux)
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
///
/// LOCK ORDER: when holding more than one guard, acquire in field-declaration
/// order — `documents` -> `ruleset` -> `scope_registry` -> `info_service` ->
/// `modifier_keys` -> `loc_index`. Most sites snapshot-and-drop
/// (`.lock().clone()`) instead of co-holding; the only places two
/// ruleset-family guards are co-held are `set_ruleset` (writer) and the
/// single-file validate (reader), both `ruleset` -> `scope_registry`. Never
/// acquire an earlier lock while holding a later one.
struct DocumentState {
    /// file URI -> parsed document
    documents: Mutex<HashMap<String, ParsedDoc>>,
    /// loaded .cwt ruleset. `RwLock` so the many readers (hover, completion,
    /// validation, the cross-file sweep) share the guard and don't serialize
    /// behind a debounced validate; only the rare ruleset load/reload takes
    /// `write()`.
    ruleset: parking_lot::RwLock<Option<Arc<RuleSet>>>,
    /// Scope/link registry built from `ruleset` (config-driven scopes.cwt +
    /// links.cwt). Cached here because `build_scope_registry` is the expensive
    /// part of per-file validation setup and depends only on the loaded ruleset,
    /// which changes rarely. Rebuilt at the ruleset write site, so it always
    /// matches the ruleset it was derived from. `None` until the first load.
    scope_registry: parking_lot::RwLock<Option<Arc<cwtools_game::scope_registry::ScopeRegistry>>>,
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
    /// Vanilla loc keys per language (display name -> lowercased keys), from the
    /// vanilla cache or extracted when rebuilding it. When set, the loc rebuild
    /// skips walking the install's loc files and merges these instead.
    #[allow(clippy::type_complexity)]
    vanilla_loc_keys: Mutex<Option<Vec<(String, Vec<String>)>>>,
    /// cached modifier-key set; rebuilt after ruleset load and after each full
    /// workspace scan when the type index is complete.
    modifier_keys: parking_lot::RwLock<HashSet<String>>,
    /// loc-key index (workspace + vanilla) for CW100/CW122 on config files and
    /// for scope-aware loc-command checks. Rebuilt on each full workspace scan.
    loc_index: parking_lot::RwLock<Option<cwtools_localization::LocIndex>>,
    /// Display text per loc key (lowercased) → list of (language, display text).
    /// Built from the LocService during workspace scan so hover can show
    /// localisation without re-reading loc files. Outer quotes are stripped
    /// from the desc for cleaner display.
    #[allow(clippy::type_complexity)]
    loc_text: parking_lot::RwLock<HashMap<String, Vec<(cwtools_localization::Lang, String)>>>,
    /// languages to validate loc against, from the `localisationLanguages` init
    /// option. `None` = all languages with data (the default). When set, the
    /// missing-translation check and per-file loc checks are scoped to these,
    /// so an english-targeted mod isn't flagged for every other language vanilla
    /// happens to ship.
    loc_languages: Mutex<Option<Vec<cwtools_localization::Lang>>>,
    /// When `false` (the default), hover shows localisation for the primary
    /// language only (the first of `loc_languages`, else English) and the
    /// `loc_text` map only stores that language. Set via the
    /// `hoverShowAllLanguages` init option. Storing one language keeps the map
    /// small; the user opts into all translations explicitly.
    hover_show_all_languages: std::sync::atomic::AtomicBool,
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
    /// Names that changed during a preempted dependent sweep. When a sweep is
    /// aborted because a newer edit landed, the union of names it was processing
    /// is merged here so the next sweep (triggered by the newer edit) drains and
    /// includes them, preventing stale dependents after rapid successive edits.
    pending_changed_names: Mutex<HashSet<String>>,
    /// Set to `true` once the vanilla index has been loaded and merged into
    /// `info_service.type_index`. After the merge the raw `vanilla_index` data
    /// is dropped to eliminate double residency; this flag prevents
    /// `ensure_vanilla_index` from re-running on subsequent workspace scans.
    vanilla_merged: std::sync::atomic::AtomicBool,
}

pub(crate) struct ParsedDoc {
    pub(crate) version: i32,
    pub(crate) text: String,
    /// Shared so the cross-file dependent sweep can validate against it without
    /// re-parsing (an `Arc` clone instead of a full re-parse per open file).
    pub(crate) ast: Option<Arc<ParsedFile>>,
}

impl DocumentState {
    fn new() -> Self {
        Self {
            documents: Mutex::new(HashMap::new()),
            ruleset: parking_lot::RwLock::new(None),
            scope_registry: parking_lot::RwLock::new(None),
            string_table: StringTable::new(),
            language: Mutex::new("paradox".to_string()),
            symbol_index: Mutex::new(symbols::SymbolIndex::new()),
            info_service: parking_lot::RwLock::new(cwtools_info::InfoService::new()),
            workspace_uri: Mutex::new(None),
            vanilla_index: Mutex::new(None),
            vanilla_dir: Mutex::new(None),
            vanilla_loc_keys: Mutex::new(None),
            modifier_keys: parking_lot::RwLock::new(HashSet::new()),
            loc_index: parking_lot::RwLock::new(None),
            loc_text: parking_lot::RwLock::new(HashMap::new()),
            loc_languages: Mutex::new(None),
            hover_show_all_languages: std::sync::atomic::AtomicBool::new(false),
            cache_dir: Mutex::new(None),
            edit_generation: AtomicU64::new(0),
            ignore_file_patterns: parking_lot::RwLock::new(Vec::new()),
            ignore_dir_patterns: parking_lot::RwLock::new(Vec::new()),
            doc_tokens: parking_lot::RwLock::new(HashMap::new()),
            pending_changed_names: Mutex::new(HashSet::new()),
            vanilla_merged: std::sync::atomic::AtomicBool::new(false),
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

    /// Resolve the leaf under the cursor with the position resolver and
    /// classify it: the AST element, a [`ReferenceHint`] derived from the
    /// matched rule's right-hand side, the alias category the key resolves
    /// through (trigger/effect/…), and the matched rule's description +
    /// required scopes (for hover).
    ///
    /// Shared by hover, goto_definition, references, prepare_rename, and
    /// rename. Returns `None` when the cursor isn't on a leaf inside a known
    /// entity — callers fall back to `element_at_position`.
    pub(crate) fn rule_info_at_cursor(
        &self,
        uri: &str,
        pos: tower_lsp::lsp_types::Position,
        logical_path: &str,
    ) -> Option<RuleCursorInfo> {
        let language = self.state.language.lock().clone();
        // Lock order: documents -> ruleset -> scope_registry -> info_service
        // -> modifier_keys (field-declaration order, see DocumentState).
        let docs = self.state.documents.lock();
        let ruleset_guard = self.state.ruleset.read();
        let registry_guard = self.state.scope_registry.read();
        let info_guard = self.state.info_service.read();
        let modifier_guard = self.state.modifier_keys.read();
        let doc = docs.get(uri)?;
        let rs = ruleset_guard.as_ref()?;
        let ast = doc.ast.as_ref()?;

        let enum_map = build_enum_map(rs);
        let (scope_checks, var_checks) = checks_from_env();
        let prepared = Prepared {
            ruleset: rs,
            table: &self.state.string_table,
            game: cwtools_game::constants::Game::from_str(&language),
            type_index: Some(&info_guard.type_index),
            modifier_keys: Some(&modifier_guard),
            loc_index: None,
            registry: registry_guard.as_ref(),
            enum_map: &enum_map,
            scope_checks,
            var_checks,
        };
        let rctx = rules_at_pos(
            ast,
            logical_path,
            &prepared,
            pos.line + 1,
            pos.character as u16,
        )?;
        let leaf = rctx.leaf?;

        let element = if leaf.key.is_empty() {
            PositionElement::LeafValue {
                value: leaf.value.clone(),
            }
        } else {
            PositionElement::Leaf {
                key: leaf.key.clone(),
                value: leaf.value.clone(),
            }
        };

        let mut hint = ReferenceHint::Unknown;
        let mut description: Option<String> = None;
        let mut scopes: Vec<String> = Vec::new();
        for (rule_type, opts) in &rctx.value_rules {
            if description.is_none() && opts.description.is_some() {
                description = opts.description.clone();
            }
            if scopes.is_empty() && !opts.required_scopes.is_empty() {
                scopes = opts.required_scopes.clone();
            }
            if matches!(hint, ReferenceHint::Unknown) {
                hint = hint_from_rule_right(rule_type, &leaf.value);
            }
        }
        let category = if leaf.key.is_empty() {
            None
        } else {
            cwtools_validation::position::alias_category_for_key(
                rs,
                Some(&info_guard.type_index),
                &rctx.child_rules,
                &leaf.key,
            )
        };
        Some(RuleCursorInfo {
            element,
            hint,
            category,
            description,
            required_scopes: scopes,
        })
    }
}

/// What `rule_info_at_cursor` resolves for the leaf under the cursor.
pub(crate) struct RuleCursorInfo {
    pub(crate) element: PositionElement,
    pub(crate) hint: ReferenceHint,
    /// Alias category the key resolves through (`trigger`, `effect`, …), for
    /// the hover header.
    pub(crate) category: Option<String>,
    /// The matched rule's `###` description.
    pub(crate) description: Option<String>,
    pub(crate) required_scopes: Vec<String>,
}

/// Map a matched leaf rule's right-hand field to a [`ReferenceHint`] for the
/// leaf's value (the same classification `info_at_position` used to do at
/// depth 0-1, now fed by the full position resolver).
fn hint_from_rule_right(rule_type: &RuleType, value: &str) -> ReferenceHint {
    let right = match rule_type {
        RuleType::LeafRule { right, .. } => right,
        RuleType::LeafValueRule { right } => right,
        _ => return ReferenceHint::Unknown,
    };
    match right {
        NewField::TypeField(TypeType::Simple(t)) => ReferenceHint::TypeRef {
            type_name: t.clone(),
            value: value.to_string(),
        },
        // `modifier:production_speed_<building>_factor` style: strip the
        // literal affixes so the instance name is what's looked up.
        NewField::TypeField(TypeType::Complex {
            prefix,
            name,
            suffix,
        }) => {
            let inner = value
                .strip_prefix(prefix.as_str())
                .unwrap_or(value)
                .strip_suffix(suffix.as_str())
                .unwrap_or(value);
            ReferenceHint::TypeRef {
                type_name: name.clone(),
                value: inner.to_string(),
            }
        }
        NewField::ValueField(ValueType::Enum(e)) => ReferenceHint::EnumRef {
            enum_name: e.clone(),
            value: value.to_string(),
        },
        NewField::LocalisationField { .. } => ReferenceHint::LocRef {
            key: value.to_string(),
        },
        NewField::FilepathField { .. } => ReferenceHint::FileRef {
            path: value.to_string(),
        },
        NewField::VariableGetField(ns) => ReferenceHint::Variable {
            name: value.to_string(),
            namespace: ns.clone(),
        },
        NewField::ScopeField(_) => ReferenceHint::ScopeName {
            name: value.to_string(),
        },
        _ => ReferenceHint::Unknown,
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

            // Whether hover shows all loc languages or just the primary one.
            if let Some(all) = opts.get("hoverShowAllLanguages").and_then(|v| v.as_bool()) {
                self.state
                    .hover_show_all_languages
                    .store(all, std::sync::atomic::Ordering::Relaxed);
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
                    Ok((game, _fingerprint, data)) => {
                        let total: usize = data.per_type.values().map(|v| v.len()).sum();
                        *self.state.vanilla_index.lock() = Some(data.per_type);
                        if !data.loc_keys.is_empty() {
                            *self.state.vanilla_loc_keys.lock() = Some(data.loc_keys);
                        }
                        self.merge_vanilla_dynamic_values(
                            data.complex_enum_values,
                            data.value_set_values,
                        );
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
                    self.set_ruleset(combined_ruleset);
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

        // Store workspace URI: prefer workspace_folders (multi-root aware), fall
        // back to the legacy root_uri field for clients that only send that.
        if let Some(folders) = &params.workspace_folders
            && let Some(first) = folders.first()
        {
            *self.state.workspace_uri.lock() = Some(first.uri.to_string());
        } else if let Some(root_uri) = &params.root_uri {
            *self.state.workspace_uri.lock() = Some(root_uri.to_string());
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
                    commands: vec![
                        "getFileTypes".to_string(),
                        "exportProfilingLog".to_string(),
                        "cacheVanilla".to_string(),
                        "clearAllCaches".to_string(),
                    ],
                    work_done_progress_options: Default::default(),
                }),
                document_symbol_provider: Some(OneOf::Left(true)),
                workspace_symbol_provider: Some(OneOf::Left(true)),
                rename_provider: Some(OneOf::Right(RenameOptions {
                    prepare_provider: Some(true),
                    work_done_progress_options: Default::default(),
                })),
                // Position encoding: we do not negotiate position_encoding here,
                // so clients default to UTF-16 (LSP spec default). The parser
                // counts Unicode scalar values (chars), NOT UTF-16 code units.
                // On BMP-only files (no surrogate pairs) the counts agree; on
                // files containing emoji or non-BMP characters, column offsets
                // will be off by the number of astral code points on the line.
                // TODO: negotiate utf-8 or utf-32 once tower-lsp exposes
                // PositionEncodingKind in InitializeResult, so clients that
                // support utf-32 get exact columns for free.
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
        let handle = tokio::spawn(async move {
            let backend = Backend { client, state };
            backend.validate_entire_workspace().await;
        });
        // Log if the workspace scan panics — without this, a panic is silently
        // swallowed (the JoinHandle is dropped) and the server runs in a
        // degraded state with no diagnostics.
        tokio::spawn(async move {
            if let Err(e) = handle.await {
                tracing::error!("validate_entire_workspace panicked: {}", e);
            }
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

        // FULL-sync spec requires last-wins; use the last change in the batch.
        let Some(change) = params.content_changes.into_iter().last() else {
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
        self.hover_impl(params).await
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        self.completion_impl(params).await
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        self.goto_definition_impl(params).await
    }

    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        self.references_impl(params).await
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        self.document_symbol_impl(params).await
    }

    async fn symbol(
        &self,
        params: WorkspaceSymbolParams,
    ) -> Result<Option<Vec<SymbolInformation>>> {
        self.symbol_impl(params).await
    }

    async fn prepare_rename(
        &self,
        params: TextDocumentPositionParams,
    ) -> Result<Option<PrepareRenameResponse>> {
        self.prepare_rename_impl(params).await
    }

    async fn rename(&self, params: RenameParams) -> Result<Option<WorkspaceEdit>> {
        self.rename_impl(params).await
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
            "exportProfilingLog" => Ok(Some(Value::String(
                cwtools_profiling::export_profiling_log(),
            ))),
            // Re-index the base-game install and re-write the vanilla cache,
            // even when a fresh-looking cache exists.
            "cacheVanilla" => {
                self.state.vanilla_merged.store(false, Ordering::SeqCst);
                *self.state.vanilla_index.lock() = None;
                *self.state.vanilla_loc_keys.lock() = None;
                self.ensure_vanilla_index(true).await;
                self.merge_pending_vanilla_index();
                self.rebuild_modifier_keys();
                // ensure_vanilla_index turns the loading bar on but, unlike a full
                // workspace scan, this command never reaches the code that turns it
                // off; do it here so the status bar doesn't spin forever.
                self.send_loading_bar(false, "").await;
                Ok(Some(Value::String("Vanilla cache rebuilt.".to_string())))
            }
            // Purge every on-disk cache (parse cache + vanilla caches), drop the
            // in-memory vanilla state, and re-scan the workspace from scratch.
            "clearAllCaches" => {
                let dir = self
                    .state
                    .cache_dir
                    .lock()
                    .clone()
                    .or_else(default_cache_dir);
                if let Some(dir) = &dir {
                    let dir = dir.clone();
                    tokio::task::block_in_place(|| {
                        let _ = std::fs::remove_dir_all(dir.join("parse-cache"));
                        if let Ok(entries) = std::fs::read_dir(&dir) {
                            for e in entries.flatten() {
                                let name = e.file_name();
                                if name.to_string_lossy().starts_with("vanilla-") {
                                    let _ = std::fs::remove_file(e.path());
                                }
                            }
                        }
                    });
                }
                self.state.vanilla_merged.store(false, Ordering::SeqCst);
                *self.state.vanilla_index.lock() = None;
                *self.state.vanilla_loc_keys.lock() = None;
                self.validate_entire_workspace().await;
                Ok(Some(Value::String(
                    "Caches cleared; workspace re-indexed.".to_string(),
                )))
            }
            _ => Ok(None),
        }
    }
}

impl Backend {
    // ── Custom notification helpers ───────────────────────────────────────────

    /// Merge vanilla dynamic values (complex-enum + value_set members, from the
    /// vanilla cache or a live index) into the workspace type index so
    /// completion offers them. Keyed under one synthetic file so a re-merge
    /// replaces the previous contribution.
    fn merge_vanilla_dynamic_values(
        &self,
        complex_enums: Vec<(String, Vec<String>)>,
        value_sets: Vec<(String, Vec<String>)>,
    ) {
        if complex_enums.is_empty() && value_sets.is_empty() {
            return;
        }
        let mut info = self.state.info_service.write();
        // NOT "<vanilla-cache>": the workspace scan's instance merge calls
        // remove_file("<vanilla-cache>") before re-merging, which would wipe
        // these as a side effect.
        info.type_index
            .complex_enum_values
            .merge_file("<vanilla-dynamic>", complex_enums.into_iter().collect());
        info.type_index
            .value_set_values
            .merge_file("<vanilla-dynamic>", value_sets.into_iter().collect());
    }

    /// Merge a pending `vanilla_index` (from the cache or a live index) into
    /// the workspace type index. After the merge the raw per-type data is
    /// dropped from `vanilla_index` to eliminate double residency (the
    /// type_index already owns the instances). `vanilla_merged` prevents
    /// `ensure_vanilla_index` re-running on subsequent workspace scans.
    fn merge_pending_vanilla_index(&self) {
        let per_type = self.state.vanilla_index.lock().take();
        if let Some(per_type) = per_type {
            let mut info_guard = self.state.info_service.write();
            info_guard.type_index.remove_file("<vanilla-cache>");
            info_guard.type_index.merge("<vanilla-cache>", per_type);
            // Vanilla data is loaded, so the index now holds every base-game
            // instance. Mark it complete so the CW500/CW222 type-reference
            // checks fire (they're gated on `complete` to avoid false
            // positives during mod-only validation). The driver's Session
            // sets this for the CLI path; the LSP merges vanilla directly and
            // must set it here too. See rule_core.rs gate on `idx.complete`.
            info_guard.type_index.complete = true;
            // `vanilla_index` is now None — mark it merged so
            // ensure_vanilla_index does not re-run on the next scan.
            self.state.vanilla_merged.store(true, Ordering::SeqCst);
        }
    }

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
            Some(ref uri) => std::path::PathBuf::from(uri_to_path_str(uri)),
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

        // Whole-tree discovery shares file_manager's skip/exclude config so the
        // LSP and CLI agree on what to skip (engine/IDE dirs, free-form text).
        // The user-configured globs extend that baseline.
        let files_to_validate = tokio::task::block_in_place(|| {
            cwtools_file_manager::file_manager::walk_workspace_files(
                &root_path,
                &extensions,
                &extra_file_globs,
                &extra_dir_globs,
            )
        });

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
                    let ruleset_snap = self.state.ruleset.read().clone();
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
        // Snapshot the set of currently-open document URIs so both passes can
        // skip them: open docs were already indexed by did_open/did_change and
        // their fresher in-memory diagnostics must not be clobbered by stale
        // disk-text validation with version=None.
        let open_uris: HashSet<String> = {
            let docs = self.state.documents.lock();
            docs.keys().cloned().collect()
        };

        let mut parsed_files: Vec<Option<ParsedFile>> = Vec::with_capacity(files_to_validate.len());
        let mut cache_hits = 0u64;
        let mut cache_misses = 0u64;
        // block_in_place tells tokio this thread is about to do synchronous
        // blocking I/O; the runtime shifts its remaining tasks to other workers
        // so the LSP request loop is not starved.
        tokio::task::block_in_place(|| {
            for file_path in &files_to_validate {
                let uri = path_to_uri(file_path);
                // Open docs are already indexed from their in-memory text; skip so
                // we don't re-index stale disk content on top of the live version.
                if open_uris.contains(&uri) {
                    parsed_files.push(None);
                    continue;
                }
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
                        } else if let Some(parsed) = self.index_document_sync(&uri, &text) {
                            // Cache miss — parse + index, then persist for next scan.
                            if let Some((ref cd, fp)) = cache_info {
                                workspace_cache::store(
                                    cd,
                                    fp,
                                    &text,
                                    &parsed,
                                    &self.state.string_table,
                                );
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
            }
        });

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
        self.ensure_vanilla_index(false).await;

        // Merge the pre-generated vanilla index (if loaded) so base-game
        // references resolve.
        self.merge_pending_vanilla_index();

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
        // Snapshot the shared `Arc<RuleSet>` for the whole batch so the
        // `enum_map` (which borrows the ruleset) stays valid across the
        // parallel section without holding the ruleset lock across rayon work.
        let scan_ruleset: Option<Arc<RuleSet>> = self.state.ruleset.read().clone();
        // The scope registry is cached (built once at ruleset load); snapshot the
        // `Arc` for the batch instead of rebuilding it for the whole scan.
        let scan_registry = self.state.scope_registry.read().clone();

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
            let registry = scan_registry.as_ref();
            // Build enum_map once for the batch; it borrows `scan_ruleset`,
            // which is owned for the whole parallel section above.
            let enum_map = scan_ruleset.as_ref().map(|rs| build_enum_map(rs));
            let (scope_checks, var_checks) = checks_from_env();
            // One Prepared for the whole batch (None if the ruleset isn't loaded).
            // It is Copy + all-borrows, so it is shared freely across rayon threads.
            let prepared =
                scan_ruleset
                    .as_ref()
                    .zip(enum_map.as_ref())
                    .map(|(ruleset, enum_map)| Prepared {
                        ruleset,
                        table: &self.state.string_table,
                        game: scan_game,
                        type_index: Some(type_index),
                        modifier_keys: Some(&modifier_keys_snap),
                        loc_index,
                        registry,
                        enum_map,
                        scope_checks,
                        var_checks,
                    });

            files_to_validate
                .par_iter()
                .zip(parsed_files.par_iter())
                .filter_map(|(file_path, parsed_opt)| {
                    // Skip files that failed to parse in pass 1, and open docs
                    // whose fresher in-memory diagnostics must not be overwritten.
                    let parsed = parsed_opt.as_ref()?;
                    let uri = path_to_uri(file_path);
                    if open_uris.contains(&uri) {
                        return None;
                    }
                    let diagnostics = match &prepared {
                        Some(prepared) => validate_parsed_with_indexes(&uri, parsed, prepared),
                        None => parsed
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
                let uri = path_to_uri(file_path);
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
        let loc_game = cwtools_localization::Game::from_engine(game);

        // Cached vanilla loc keys (from the vanilla cache) stand in for walking
        // the install's loc files — only the workspace is walked then.
        let cached_vanilla_loc = self.state.vanilla_loc_keys.lock().clone();
        let mut loc_dirs: Vec<std::path::PathBuf> = vec![root_path.to_path_buf()];
        if cached_vanilla_loc.is_none()
            && let Some(v) = self.state.vanilla_dir.lock().clone()
        {
            loc_dirs.push(v);
        }
        let dir_refs: Vec<&std::path::Path> = loc_dirs.iter().map(|p| p.as_path()).collect();
        let loc_languages = self.state.loc_languages.lock().clone();

        // Hover language scope: unless the user opted into all translations, keep
        // only the primary language (first configured loc language, else English)
        // in the hover map so it stays small.
        let hover_all = self
            .state
            .hover_show_all_languages
            .load(std::sync::atomic::Ordering::Relaxed);
        let primary_lang = loc_languages
            .as_deref()
            .and_then(|l| l.first().copied())
            .unwrap_or(cwtools_localization::Lang::English);

        // Build the index and collect per-file diagnostics in one block, then
        // drop the LocService before the index is published. The service holds
        // the full per-file loc ASTs (~2M entries on Millennium Dawn); keeping
        // it alive while we also hold the lowercased key set in LocIndex
        // pushes peak RSS by hundreds of MiB for no reason. After the block
        // closes only LocIndex (keys) and the diagnostic map survive.
        // Names a `$ref$` may resolve to besides loc keys: `$modifier$` / `$idea$`
        // embeds resolve against those registries (mirrors the CLI/driver path).
        // With cached vanilla loc keys, the service holds no vanilla keys, so
        // they join this set too — otherwise mod loc referencing a base-game key
        // would flag CW225.
        let extra_valid_refs: HashSet<String> = {
            let mut extra = self.state.modifier_keys.read().clone();
            let info = self.state.info_service.read();
            for (_uri, inst) in info.type_index.instances("idea") {
                extra.insert(inst.name.to_lowercase());
            }
            if let Some(cached) = &cached_vanilla_loc {
                for (_, keys) in cached {
                    extra.extend(keys.iter().cloned());
                }
            }
            extra
        };

        let root_str = root_path.to_string_lossy().to_string();
        // block_in_place: the loc service reads and parses hundreds of loc files
        // from disk — synchronous I/O that must not starve the async executor.
        let (loc_index, mut by_file, loc_text_map) = tokio::task::block_in_place(|| {
            let service = cwtools_localization::LocService::from_folders(&dir_refs);
            let mut idx = cwtools_localization::LocIndex::build_scoped(
                &service,
                loc_game,
                loc_languages.as_deref(),
            );
            if let Some(cached) = cached_vanilla_loc {
                let typed: Vec<(cwtools_localization::Lang, Vec<String>)> = cached
                    .into_iter()
                    .filter_map(|(name, ks)| {
                        cwtools_localization::Lang::from_name(&name).map(|l| (l, ks))
                    })
                    .collect();
                idx.merge_cached_keys(typed, loc_languages.as_deref());
            }
            let mut by_file: HashMap<String, Vec<Diagnostic>> = HashMap::new();
            for d in cwtools_localization::validate_loc_project_scoped(
                &service,
                loc_game,
                loc_languages.as_deref(),
                &extra_valid_refs,
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
            // Extract per-key display text for hover before dropping the service.
            let mut lt: HashMap<String, Vec<(cwtools_localization::Lang, String)>> = HashMap::new();
            for file in service.files() {
                let lang = file.lang.unwrap_or(cwtools_localization::Lang::English);
                if !hover_all && lang != primary_lang {
                    continue;
                }
                for entry in &file.entries {
                    let display = strip_loc_quotes(&entry.desc);
                    if !display.is_empty() {
                        lt.entry(entry.key.to_lowercase())
                            .or_default()
                            .push((lang, display.to_string()));
                    }
                }
            }
            (idx, by_file, lt)
        });
        *self.state.loc_index.write() = Some(loc_index);
        *self.state.loc_text.write() = loc_text_map;

        // Publish per-file loc diagnostics, but only for workspace loc files
        // (not vanilla). Group by file so each gets a complete diagnostic set.
        for (file, diags) in by_file.drain() {
            let uri = path_to_uri(std::path::Path::new(&file));
            if let Ok(uri_obj) = Url::parse(&uri) {
                self.client.publish_diagnostics(uri_obj, diags, None).await;
            }
        }
        cwtools_profiling::log_rss("loc_rebuild_done");
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
    /// Install a freshly-loaded ruleset and rebuild the cached scope registry to
    /// match it. The registry depends only on `(ruleset, game)`; building it here
    /// (once per load) keeps it out of the per-file validation hot path. Holds
    /// `ruleset.write()` across the `scope_registry.write()` so the two never
    /// disagree; no other site takes both ruleset and scope_registry.
    fn set_ruleset(&self, ruleset: RuleSet) {
        let game = {
            let language = self.state.language.lock().clone();
            cwtools_game::constants::Game::from_str(&language)
        };
        let mut guard = self.state.ruleset.write();
        let registry = build_scope_registry_arc(&ruleset, game);
        *self.state.scope_registry.write() = registry;
        // Cache the variable-defining effects so per-file indexing can collect
        // value_set[variable] names (and values) for the CW246 / VariableGetField
        // checks and for hover/goto.
        let var_effects = cwtools_info::variable_defining_effects(&ruleset);
        self.state.info_service.write().set_var_effects(var_effects);
        *guard = Some(Arc::new(ruleset));
    }

    /// Rebuild the cached modifier-key set from the current ruleset and type index.
    fn rebuild_modifier_keys(&self) {
        let ruleset_guard = self.state.ruleset.read();
        let info_guard = self.state.info_service.read();
        let keys = match ruleset_guard.as_ref() {
            Some(rs) => build_modifier_keys(rs, &info_guard.type_index),
            None => HashSet::new(),
        };
        *self.state.modifier_keys.write() = keys;
    }

    /// Lazily index the base-game install into `vanilla_index` (once). Resolves
    /// the dir from the `vanilla` init option, falling back to auto-discovery by
    /// game. No-op if already indexed (or already merged into the type_index),
    /// if no dir is found, or if the ruleset isn't loaded yet.
    ///
    /// `force_rebuild` skips the cache-load fast path (and the already-indexed
    /// check) so the install is re-indexed and the cache re-written — the
    /// `cacheVanilla` command.
    async fn ensure_vanilla_index(&self, force_rebuild: bool) {
        // Already populated (or already merged into type_index and dropped)? Done.
        if !force_rebuild
            && (self.state.vanilla_index.lock().is_some()
                || self.state.vanilla_merged.load(Ordering::SeqCst))
        {
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
        let ruleset_opt = self.state.ruleset.read().clone();
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
        if !force_rebuild
            && let Some(cp) = &cache_path
            && cp.exists()
        {
            match cwtools_info::vanilla_cache::load(cp) {
                Ok((cache_game, cache_fp, data))
                    if cache_game == game && cache_fp == fingerprint =>
                {
                    let total: usize = data.per_type.values().map(|v| v.len()).sum();
                    *self.state.vanilla_index.lock() = Some(data.per_type);
                    if !data.loc_keys.is_empty() {
                        *self.state.vanilla_loc_keys.lock() = Some(data.loc_keys);
                    }
                    self.merge_vanilla_dynamic_values(
                        data.complex_enum_values,
                        data.value_set_values,
                    );
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
        let join_result =
            tokio::task::spawn_blocking(move || index_vanilla_dir(&index_dir, &ruleset, &table))
                .await;
        let (per_type, aux) = match join_result {
            Ok(result) => result,
            Err(e) => {
                // The blocking task panicked or was cancelled. Log loudly and
                // bail without setting vanilla_merged, so that type_index stays
                // incomplete and CW500/CW222 reference checks are suppressed
                // (avoiding a flood of false positives against an empty base game).
                self.client
                    .log_message(
                        MessageType::ERROR,
                        format!(
                            "Vanilla indexing task failed for {} — base-game references will not resolve. Error: {}",
                            dir.display(),
                            e
                        ),
                    )
                    .await;
                tracing::error!("spawn_blocking vanilla index panicked: {}", e);
                return;
            }
        };

        let total: usize = per_type.values().map(|v| v.len()).sum();

        // The freshly-extracted loc keys and dynamic values feed this session
        // directly too (not just the persisted cache).
        if !aux.loc_keys.is_empty() {
            *self.state.vanilla_loc_keys.lock() = Some(aux.loc_keys.clone());
        }
        self.merge_vanilla_dynamic_values(
            aux.complex_enum_values.clone(),
            aux.value_set_values.clone(),
        );

        // Persist for next startup so the base game isn't re-parsed every time.
        if let Some(cp) = &cache_path {
            match cwtools_info::vanilla_cache::save_per_type(
                &per_type,
                &game,
                &fingerprint,
                cp,
                aux,
            ) {
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
        Some(base.join(format!("vanilla-{}-{}.cwv", safe(game), safe(fingerprint))))
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

fn main() {
    // Handle --help / --version before entering the LSP serve loop so the
    // binary prints useful output instead of silently blocking on stdin.
    let args: Vec<String> = std::env::args().collect();
    if args.iter().any(|a| a == "--help" || a == "-h") {
        eprintln!("cwtools-server {}", env!("CARGO_PKG_VERSION"));
        eprintln!();
        eprintln!("CWTools language server for Paradox game scripts.");
        eprintln!("Communicates over stdin/stdout using the Language Server Protocol.");
        eprintln!();
        eprintln!("USAGE:");
        eprintln!("    cwtools-server              Start the LSP server (default)");
        eprintln!("    cwtools-server --help       Show this help");
        eprintln!("    cwtools-server --version    Show version");
        std::process::exit(0);
    }
    if args.iter().any(|a| a == "--version" || a == "-V") {
        println!("cwtools-server {}", env!("CARGO_PKG_VERSION"));
        std::process::exit(0);
    }

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
            tracing::info!("LSP server shut down (stdin closed)");
        });
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::completion::{completions_from_rules, generate_node_snippet, root_type_snippets};
    use crate::navigation::{is_type_ref_leaf, scan_use_sites};
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
                ..Default::default()
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

        let items = completions_from_rules(rules, &rs, &info, "stellaris", &HashSet::new());

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
                ..Default::default()
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
        let (per_type, _aux) = index_vanilla_dir(&root, &rs, &table);

        let names: Vec<&str> = per_type
            .get("foo")
            .map(|v| v.iter().map(|i| i.name.as_str()).collect())
            .unwrap_or_default();
        assert!(names.contains(&"foo_one"), "got: {:?}", names);
        assert!(names.contains(&"foo_two"), "got: {:?}", names);

        let _ = std::fs::remove_dir_all(&root);
    }
}
