use parking_lot::Mutex;
use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use serde_json::Value;
use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};

use cwtools_info::{PositionElement, ReferenceHint, TypeInstance};
use cwtools_parser::ast::ParsedFile;
use cwtools_rules::rules_types::{NewField, RuleSet, RuleType, TypeType, ValueType};
use cwtools_string_table::string_table::{StringId, StringTable};
use cwtools_validation::position::rules_at_pos;

mod completion;
mod config;
mod hover;
mod navigation;
mod paths;
mod scan;
mod validate;
mod workspace_cache;

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

/// Settings group: values set once at `initialize` / `didChangeConfiguration`
/// and only read (clone-and-drop) everywhere else. Held behind a single
/// `RwLock<Config>` so a config read never serializes behind an unrelated
/// write. The guard is never held across another lock or an await — every
/// reader clones what it needs and drops the guard immediately.
pub(crate) struct Config {
    /// game language from init options
    pub(crate) language: String,
    /// workspace folder URI captured from initialize params. `Arc<str>` so the
    /// per-handler reads clone a cheap refcount bump, not the whole string.
    pub(crate) workspace_uri: Option<Arc<str>>,
    /// Normalized, decoded workspace path prefix, precomputed from
    /// `workspace_uri` so per-request logical-path derivation doesn't re-parse
    /// the constant workspace URI (see `paths::workspace_prefix_of`).
    pub(crate) workspace_prefix: Option<Arc<str>>,
    /// base-game install dir (from the `vanilla` init option, or auto-discovered).
    /// Indexed lazily into `vanilla_index` on the first full-workspace scan.
    pub(crate) vanilla_dir: Option<std::path::PathBuf>,
    /// Writable directory for persistent caches (from the `cacheDir` init
    /// option, else an OS cache dir). The base-game type index is cached here
    /// keyed by game + version, so it isn't re-parsed on every startup.
    pub(crate) cache_dir: Option<std::path::PathBuf>,
    /// languages to validate loc against, from the `localisationLanguages` init
    /// option. `None` = all languages with data (the default). When set, the
    /// missing-translation check and per-file loc checks are scoped to these,
    /// so an english-targeted mod isn't flagged for every other language vanilla
    /// happens to ship.
    pub(crate) loc_languages: Option<Vec<cwtools_localization::Lang>>,
    /// Extra filename glob patterns to skip during the workspace scan (on top
    /// of the engine baseline like Changelog.txt / README.md). Sourced from
    /// `ignoreFilePatterns` in `initializationOptions` and the
    /// `workspace/didChangeConfiguration` payload.
    pub(crate) ignore_file_patterns: Vec<String>,
    /// Extra directory glob patterns to skip during the workspace scan. Sourced
    /// from `ignoreDirectories` in `initializationOptions` and
    /// `workspace/didChangeConfiguration`.
    pub(crate) ignore_dir_patterns: Vec<String>,
    /// Diagnostic codes (e.g. `CW100`) the user suppressed via `errors.ignore`
    /// (`ignoredErrorCodes`). Stored lowercased; matched case-insensitively
    /// against each diagnostic's code just before publishing.
    pub(crate) ignored_error_codes: Vec<String>,
    /// Rules-config directory loaded at `initialize` (the `rulesCache` init
    /// option). Retained so the `reloadrulesconfig` command can re-read it.
    pub(crate) rules_dir: Option<std::path::PathBuf>,
    pub(crate) scope_checks: bool,
    pub(crate) var_checks: bool,
    /// Minutes between quiet background re-index passes (0 = off, the
    /// default). Sourced from `backgroundReindexIntervalMinutes` in
    /// `initializationOptions` and `workspace/didChangeConfiguration`. A raw
    /// client that never sends either keeps this at 0, so the periodic loop
    /// stays disabled unless explicitly configured.
    pub(crate) background_reindex_interval_minutes: u64,
    /// Seconds the user must be idle before a background pass runs (default
    /// 15). Sourced from `backgroundReindexIdleSeconds` in
    /// `initializationOptions` and `workspace/didChangeConfiguration`; the
    /// `CWTOOLS_REINDEX_IDLE_SECS` test override wins over this value. A live
    /// change applies on the next reindex cycle.
    pub(crate) background_reindex_idle_seconds: u64,
    /// Position encoding negotiated with the client. LSP defaults to UTF-16.
    pub(crate) position_encoding: tower_lsp::lsp_types::PositionEncodingKind,
}

impl Config {
    fn new() -> Self {
        let (scope_checks, var_checks) = cwtools_validation::checks_from_env();
        Self {
            language: "paradox".to_string(),
            workspace_uri: None,
            workspace_prefix: None,
            vanilla_dir: None,
            cache_dir: None,
            loc_languages: None,
            ignore_file_patterns: Vec::new(),
            ignore_dir_patterns: Vec::new(),
            ignored_error_codes: Vec::new(),
            rules_dir: None,
            scope_checks,
            var_checks,
            background_reindex_interval_minutes: 0,
            background_reindex_idle_seconds: 15,
            position_encoding: tower_lsp::lsp_types::PositionEncodingKind::UTF16,
        }
    }

    /// Resolve the configured language to an engine [`Game`], for the many
    /// sites that only need the typed game (not the raw language string).
    pub(crate) fn game(&self) -> Option<cwtools_game::constants::Game> {
        cwtools_game::constants::Game::from_str(&self.language)
    }
}

/// Ruleset-derived group: rebuilt together whenever a ruleset is loaded.
/// One `RwLock<RuleData>` so the readers that need all three (hover,
/// completion, the workspace scan) take a single guard instead of three.
pub(crate) struct RuleData {
    /// loaded .cwt ruleset. The many readers (hover, completion, validation,
    /// the cross-file sweep) share the guard and don't serialize behind a
    /// debounced validate; only the rare ruleset load/reload takes `write()`.
    pub(crate) ruleset: Option<Arc<RuleSet>>,
    /// Scope/link registry built from `ruleset` (config-driven scopes.cwt +
    /// links.cwt). Cached here because `build_scope_registry` is the expensive
    /// part of per-file validation setup and depends only on the loaded ruleset,
    /// which changes rarely. Rebuilt at the ruleset write site, so it always
    /// matches the ruleset it was derived from. `None` until the first load.
    pub(crate) scope_registry: Option<Arc<cwtools_game::scope_registry::ScopeRegistry>>,
    /// cached modifier-key set; rebuilt after ruleset load and after each full
    /// workspace scan when the type index is complete. `Arc` so the workspace
    /// scan snapshots it with a cheap refcount bump instead of deep-copying the
    /// whole set (#78).
    pub(crate) modifier_keys: Arc<HashSet<String>>,
    /// expanded modifier name → its category's `supported_scopes`, for
    /// scope-aware modifier ranking in completion. A pure function of
    /// ruleset + type index, rebuilt together with `modifier_keys` so the two
    /// can never disagree.
    pub(crate) modifier_scopes: Arc<HashMap<String, Vec<String>>>,
}

impl RuleData {
    fn new() -> Self {
        Self {
            ruleset: None,
            scope_registry: None,
            modifier_keys: Arc::new(HashSet::new()),
            modifier_scopes: Arc::new(HashMap::new()),
        }
    }
}

/// Server state.
///
/// LOCK ORDER: when holding more than one guard, acquire in this order —
/// `documents` -> `rules` -> `info_service` -> `loc_index`. `config` is a
/// settings snapshot: it is always read-clone-dropped and never held across
/// another lock or an await. Most sites snapshot-and-drop the others too; the
/// places that co-hold are the workspace scan and single-file validate
/// (`rules` -> `info_service` -> `loc_index`). Never acquire an earlier lock
/// while holding a later one.
struct DocumentState {
    /// file URI -> parsed document
    documents: Mutex<HashMap<String, ParsedDoc>>,
    /// Settings set at init / didChangeConfiguration, read-clone-dropped
    /// elsewhere. See [`Config`].
    config: parking_lot::RwLock<Config>,
    /// Ruleset + scope registry + modifier keys, rebuilt together on ruleset
    /// load. See [`RuleData`].
    rules: parking_lot::RwLock<RuleData>,
    /// shared string table
    string_table: StringTable,
    /// computed info service for type/references/definitions. `RwLock` so the
    /// full-workspace pass-2 validation can share a single read guard across
    /// rayon threads, and the many read-only consumers (hover, completion,
    /// document-symbol, export fingerprinting, validation) don't serialize.
    info_service: parking_lot::RwLock<cwtools_info::InfoService>,
    /// pre-generated base-game type instances (from a vanilla cache OR a live
    /// index of `config.vanilla_dir`), merged into the workspace index so the
    /// editor resolves base-game references. Each instance keeps its real source
    /// path (raw, the driver / cache form) so goto-definition into base-game
    /// content lands in the right file once the merge maps it to a `file://` URI.
    #[allow(clippy::type_complexity)]
    vanilla_index: Mutex<Option<HashMap<String, Vec<(Arc<str>, TypeInstance)>>>>,
    /// The distinct source URIs the current vanilla contribution was merged
    /// under. Tracked so a re-merge (`cacheVanilla` / `clearAllCaches`) drops
    /// exactly the previous base-game instances in one index pass, without a
    /// `"<vanilla-cache>"` sentinel.
    vanilla_merged_uris: Mutex<HashSet<Arc<str>>>,
    /// Vanilla loc keys per language (display name -> lowercased keys), from the
    /// vanilla cache or extracted when rebuilding it. When set, the loc rebuild
    /// skips walking the install's loc files and merges these instead.
    #[allow(clippy::type_complexity)]
    vanilla_loc_keys: Mutex<Option<Vec<(String, Vec<String>)>>>,
    /// loc-key index (workspace + vanilla) for CW100/CW122 on config files and
    /// for scope-aware loc-command checks. Rebuilt on each full workspace scan.
    loc_index: parking_lot::RwLock<Option<cwtools_localization::LocIndex>>,
    /// Display text per loc key (lowercased) → list of (language, display text).
    /// Built from the LocService during workspace scan so hover can show
    /// localisation without re-reading loc files. Outer quotes are stripped
    /// from the desc for cleaner display.
    #[allow(clippy::type_complexity)]
    loc_text: parking_lot::RwLock<HashMap<String, Vec<(cwtools_localization::Lang, String)>>>,
    /// Definition site per loc key (lowercased) → (file URI, 0-based line). Built
    /// from the LocService during workspace scan so goto-definition on a
    /// `localisation` reference jumps to the `.yml` entry. One representative
    /// (primary-language) location per key is enough for navigation.
    loc_locations: parking_lot::RwLock<HashMap<String, (String, u32)>>,
    /// Live per-file loc keys (lowercased) for currently-open loc files, keyed by
    /// URI. Overlays the scanned `loc_index` so a key added to (or present in) an
    /// open `.yml` resolves immediately in `$ref$` checks without waiting for a
    /// full rescan (#36). Bounded by the number of open loc files, so it stays
    /// tiny next to the global index. A key only removed from disk still resolves
    /// against the baseline `loc_index` until the next scan — the overlay only
    /// adds keys, it can't subtract from the baseline union.
    loc_live_overlay: parking_lot::RwLock<HashMap<String, HashSet<String>>>,
    /// When `false` (the default), hover shows localisation for the primary
    /// language only (the first of `config.loc_languages`, else English) and the
    /// `loc_text` map only stores that language. Set via the
    /// `hoverShowAllLanguages` init option. Storing one language keeps the map
    /// small; the user opts into all translations explicitly.
    hover_show_all_languages: std::sync::atomic::AtomicBool,
    /// Developer hover toggle (`hoverDebug` init option). When `true`, hover
    /// includes the raw rule classification (field/type/scope) lines; off by
    /// default so users see only localisation, description, and required scopes.
    hover_debug: std::sync::atomic::AtomicBool,
    /// When `true` (the `hover.scopeDisplay = "resolved"` setting), hover adds a
    /// `Resolves to` line showing the scope the hovered link/keyword evaluates to
    /// (run through `change_scope`), alongside the ambient current scope. Off by
    /// default — the ambient scope is shown alone. (#37)
    hover_resolved_scope: std::sync::atomic::AtomicBool,
    /// Whether the client advertised `hierarchicalDocumentSymbolSupport` at
    /// initialize. When `true`, documentSymbol returns a nested `DocumentSymbol`
    /// tree; otherwise it falls back to the flat `SymbolInformation` list.
    hierarchical_symbols: std::sync::atomic::AtomicBool,
    /// `false` until the first full workspace scan has finished building the
    /// index. While `false`, per-file validation still parses and indexes, but
    /// suppresses published diagnostics (clears instead) so the user never sees
    /// transient "not found" errors for cross-file references whose defining file
    /// isn't indexed yet. The scan publishes the real diagnostics once the index
    /// is complete. Set `true` with no workspace folder (nothing to index).
    index_ready: std::sync::atomic::AtomicBool,
    /// Monotonic edit counter, bumped on every `did_change`. A debounced
    /// validation captures the value at spawn time; the cross-file dependent
    /// sweep bails the moment a newer edit lands, so concurrent sweeps collapse
    /// into the latest one instead of stacking up and double-validating.
    edit_generation: AtomicU64,
    /// Per open document, the interned `.lower` ids of the identifier-like
    /// tokens it mentions (keys + string values from its parsed AST). Used by
    /// the dependent sweep to revalidate only the open docs that actually
    /// reference a changed export, instead of every open doc. A SOUND
    /// OVER-APPROXIMATION: when a doc's token set is missing, it's always
    /// included. Updated on did_open / did_change, removed on did_close.
    doc_tokens: parking_lot::RwLock<HashMap<String, HashSet<StringId>>>,
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
    /// Guards `validate_entire_workspace` against re-entrant scans. The
    /// startup scan, `clearAllCaches`, and (in a later phase) a periodic
    /// background rescan all funnel through it; without this, two overlapping
    /// scans would race serial `info_service` writes against each other.
    /// `compare_exchange`-guarded on entry; a losing caller logs and returns
    /// immediately instead of queueing behind the running scan.
    scan_in_progress: AtomicBool,
    /// Per-URI debounce task handle. `did_change` aborts the previous sleeper for
    /// the same file before spawning a new one, so a burst of keystrokes coalesces
    /// to a single pending task instead of stacking hundreds of sleepers.
    debounce_handles: Mutex<HashMap<String, tokio::task::JoinHandle<()>>>,
    /// Monotonic counter bumped on every mutation of `info_service` or
    /// `rules` (the two state sources the fallback completion cache depends
    /// on). The completion handler reads this on each request; when
    /// the value matches a cached entry, it can return the cached list
    /// without walking `info.files` again. Hot in the half-typed case: the
    /// user is in a state where the AST is stale and every completion
    /// falls through to the fallback, but info/rules haven't moved since
    /// the last build, so the cache hit saves a full workspace walk.
    info_revision: AtomicU64,
    /// Cached fallback list (the flat type/enum/var dump reached when
    /// context-aware matching returns nothing).
    fallback_cache: parking_lot::Mutex<Option<CompletionCacheEntry>>,
    /// Per-URI generation counter for in-flight completion requests. Each new
    /// `completion` request for a URI increments this and captures the value;
    /// the request checks the counter before doing any heavy work and bails
    /// if a newer request for the same URI has already started. Avoids
    /// stacking N parallel AST walks when the user types fast — only the
    /// latest one matters, the rest are wasted work.
    completion_generation: parking_lot::Mutex<HashMap<String, u64>>,
    /// Stat-only signature (path, size, mtime) over the loc files a scan last
    /// rebuilt, so the periodic background pass can skip
    /// `rebuild_and_publish_loc` (the biggest transient cost of a scan) when
    /// nothing loc-related has changed on disk. `None` until the first scan
    /// runs.
    last_loc_signature: parking_lot::Mutex<Option<u64>>,
    /// `(stat_signature_for(walked files), settings_generation)` stored after
    /// the last successful full pass. A QUIET pass whose freshly-computed pair
    /// matches this short-circuits the whole reindex. `None` until the first
    /// pass; never stored for an empty walk (a transiently-unreadable root).
    last_scan_fingerprint: parking_lot::Mutex<Option<(u64, u64)>>,
    /// Bumped whenever a rules or config change could alter validation output,
    /// folded into `last_scan_fingerprint` so such a change forces the next
    /// quiet pass to run. `SeqCst`: rare writer, single reader, so ordering
    /// cost doesn't matter — chosen for clarity.
    settings_generation: AtomicU64,
    /// Server start time, the epoch `last_activity_ms` is measured against.
    start: std::time::Instant,
    /// Milliseconds since `start` at the last `did_change` / `completion`
    /// request — the idle clock the background reindex loop watches.
    /// `Relaxed`: the periodic loop is the only reader and tolerates a
    /// slightly-stale value.
    last_activity_ms: AtomicU64,
    /// URIs of non-open watched files (create/modify) waiting for the next
    /// coalescing window. A burst of `didChangeWatchedFiles` events (git
    /// checkout, generator, AV/OneDrive churn) collapses into one drain
    /// instead of validating 1:1 on the message future and starving the
    /// bounded request queue (#90).
    watched_pending: Mutex<HashSet<String>>,
    /// URIs of watched files DELETED since the last drain, coalesced into the
    /// same window as `watched_pending` instead of clearing inline per event.
    /// A URI that also arrived as a CHANGED/CREATED this window is treated as
    /// a change, not a delete.
    watched_deleted: Mutex<HashSet<String>>,
    /// The single in-flight watched-batch window, if one is armed. A live
    /// (not-yet-finished) handle means a window is already scheduled, so a
    /// continuous event stream can't keep pushing the trailing window back.
    watched_debounce: Mutex<Option<tokio::task::JoinHandle<()>>>,
    /// Per-URI stat signature (file size, mtime-nanos) of the last watched
    /// validation — the per-file analogue of `last_loc_signature`. A CHANGED
    /// event whose bytes never moved (cloud sync, git, the running game
    /// rewriting identical content) matches and skips the revalidate. A DELETE
    /// drops the entry; a URI with no entry always validates.
    watched_signatures: Mutex<HashMap<String, (u64, u128)>>,
}

/// One cached completion list. Stored behind a `Mutex<Option<_>>` so the
/// completion handler can swap a freshly built list in on cache miss without
/// holding any other lock.
pub(crate) struct CompletionCacheEntry {
    pub(crate) revision: u64,
    pub(crate) items: Vec<CompletionItem>,
}

pub(crate) struct ParsedDoc {
    pub(crate) version: i32,
    /// `Arc` so every reader that only needs to look at the text (completion,
    /// hover, the cross-file dependent sweep) clones a refcount bump instead
    /// of the whole document under the `documents` lock.
    pub(crate) text: Arc<str>,
    /// Shared so the cross-file dependent sweep can validate against it without
    /// re-parsing (an `Arc` clone instead of a full re-parse per open file).
    pub(crate) ast: Option<Arc<ParsedFile>>,
    /// Document version the cached AST was parsed from. `None` means there is
    /// no cached AST; a value different from `version` means completion/hover
    /// are looking at the last good parse while debounce validation catches up.
    pub(crate) ast_version: Option<i32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum AstSource {
    StoredCurrent,
    StoredStale,
    FreshParse,
    None,
}

impl AstSource {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            AstSource::StoredCurrent => "stored_current",
            AstSource::StoredStale => "stored_stale",
            AstSource::FreshParse => "fresh_parse",
            AstSource::None => "none",
        }
    }

    pub(crate) fn is_current(self) -> bool {
        matches!(self, AstSource::StoredCurrent | AstSource::FreshParse)
    }
}

pub(crate) struct AstSnapshot {
    pub(crate) ast: Arc<ParsedFile>,
    pub(crate) source: AstSource,
}

/// What kicked off a `parse_and_validate` call. Threaded through so the
/// `[validate]` log names its trigger, which makes a validate storm's source
/// legible in the server log (issue #90) instead of a wall of identical lines.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ValidateTrigger {
    DidOpen,
    DidSave,
    Watched,
    DidChange,
    ConfigChange,
    Reindex,
}

impl ValidateTrigger {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            ValidateTrigger::DidOpen => "didOpen",
            ValidateTrigger::DidSave => "didSave",
            ValidateTrigger::Watched => "watched",
            ValidateTrigger::DidChange => "didChange",
            ValidateTrigger::ConfigChange => "configChange",
            ValidateTrigger::Reindex => "reindex",
        }
    }
}

impl DocumentState {
    fn new() -> Self {
        Self {
            documents: Mutex::new(HashMap::new()),
            config: parking_lot::RwLock::new(Config::new()),
            rules: parking_lot::RwLock::new(RuleData::new()),
            string_table: StringTable::new(),
            info_service: parking_lot::RwLock::new(cwtools_info::InfoService::new()),
            vanilla_index: Mutex::new(None),
            vanilla_merged_uris: Mutex::new(HashSet::new()),
            vanilla_loc_keys: Mutex::new(None),
            loc_index: parking_lot::RwLock::new(None),
            loc_text: parking_lot::RwLock::new(HashMap::new()),
            loc_locations: parking_lot::RwLock::new(HashMap::new()),
            loc_live_overlay: parking_lot::RwLock::new(HashMap::new()),
            hover_show_all_languages: std::sync::atomic::AtomicBool::new(false),
            hover_debug: std::sync::atomic::AtomicBool::new(false),
            hover_resolved_scope: std::sync::atomic::AtomicBool::new(false),
            hierarchical_symbols: std::sync::atomic::AtomicBool::new(false),
            index_ready: std::sync::atomic::AtomicBool::new(false),
            edit_generation: AtomicU64::new(0),
            doc_tokens: parking_lot::RwLock::new(HashMap::new()),
            pending_changed_names: Mutex::new(HashSet::new()),
            vanilla_merged: std::sync::atomic::AtomicBool::new(false),
            scan_in_progress: AtomicBool::new(false),
            debounce_handles: Mutex::new(HashMap::new()),
            info_revision: AtomicU64::new(0),
            fallback_cache: parking_lot::Mutex::new(None),
            completion_generation: parking_lot::Mutex::new(HashMap::new()),
            last_loc_signature: parking_lot::Mutex::new(None),
            last_scan_fingerprint: parking_lot::Mutex::new(None),
            settings_generation: AtomicU64::new(0),
            start: std::time::Instant::now(),
            last_activity_ms: AtomicU64::new(0),
            watched_pending: Mutex::new(HashSet::new()),
            watched_deleted: Mutex::new(HashSet::new()),
            watched_debounce: Mutex::new(None),
            watched_signatures: Mutex::new(HashMap::new()),
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
    /// Spawn a background validation for `uri` at `version` and register the
    /// task in `debounce_handles`, aborting any predecessor for the same URI
    /// (`did_close` aborts it too). `delay_ms` is the debounce sleep before
    /// validating: `did_change` passes `DEBOUNCE_MS` to coalesce keystrokes,
    /// open/save pass 0 to validate promptly. The task re-reads a
    /// version-checked snapshot inside `debounced_validate`, so a newer edit
    /// landing in the gap supersedes this one instead of publishing stale
    /// results.
    fn spawn_debounced_validate(
        &self,
        uri: String,
        version: i32,
        generation: u64,
        trigger: ValidateTrigger,
        delay_ms: u64,
    ) {
        let client = self.client.clone();
        let state = self.state.clone();
        let key = uri.clone();
        let handle = tokio::spawn(async move {
            if delay_ms > 0 {
                tokio::time::sleep(std::time::Duration::from_millis(delay_ms)).await;
            }
            Backend { client, state }
                .debounced_validate(uri, version, generation, trigger)
                .await;
        });
        if let Some(prev) = self.state.debounce_handles.lock().insert(key, handle) {
            prev.abort();
        }
    }

    /// Bump the info-revision counter. Called from every site that mutates
    /// `info_service` or `rules` (the two state sources the loc/fallback
    /// completion caches depend on), so the completion cache invalidates
    /// exactly when the inputs change. `Relaxed` is enough — the only
    /// consumer is a single-threaded `load` that tolerates missing an
    /// in-flight bump (the next request picks it up).
    pub(crate) fn bump_info_revision(&self) {
        self.state
            .info_revision
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    }

    /// Called when the VS Code extension tells us the user switched to a file.
    /// Focus is user activity: reset the background-reindex idle clock.
    async fn on_did_focus_file(&self, _params: Value) {
        self.mark_activity();
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
        let CursorResolution { rctx, ruleset, .. } =
            self.resolve_at_cursor(uri, pos, logical_path)?;
        let rs = ruleset.as_ref();
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
                hint = hint_from_rule_right(rule_type, &leaf.value, rs);
            }
        }
        // Key-position references: when the right-hand classification yields
        // nothing and the cursor is on a key (e.g. `<character> = { … }` used as
        // a scoped-trigger block, or a `type[…]` definition key), classify the
        // key against the matched rule's LEFT field so hover renders a rich
        // header and goto can resolve the definition.
        if matches!(hint, ReferenceHint::Unknown) && !leaf.key.is_empty() {
            for (rule_type, _) in &rctx.value_rules {
                let left_hint = hint_from_rule_left(rule_type, &leaf.key);
                if !matches!(left_hint, ReferenceHint::Unknown) {
                    hint = left_hint;
                    break;
                }
            }
        }
        // Scope-link key: a bare key that is a known instance of a type used as a
        // link `data_source` (e.g. a character name scoping into that character).
        // Such keys don't match a rule, so `value_rules` is empty and any
        // description that did match comes from a coincidental alias — resolve the
        // key to its type and drop the misleading description/category.
        let mut scope_link_key = false;
        let info_guard = self.state.info_service.read();
        if !leaf.in_value
            && !leaf.key.is_empty()
            && !matches!(hint, ReferenceHint::TypeRef { .. })
            && let Some(type_name) = scope_link_key_type(rs, &info_guard.type_index, &leaf.key)
        {
            hint = ReferenceHint::TypeRef {
                type_name,
                value: leaf.key.clone(),
            };
            description = None;
            scope_link_key = true;
        }
        let category = if leaf.key.is_empty() || scope_link_key {
            None
        } else {
            cwtools_validation::position::alias_category_for_key(
                rs,
                Some(&info_guard.type_index),
                &rctx.child_rules,
                &leaf.key,
            )
        };
        drop(info_guard);
        // Current scope at the cursor (the scope the containing block evaluates
        // in), so a hover shows where you are regardless of whether the rule
        // declares a required scope. The related scopes (ROOT/PREV and the FROM
        // chain) come along for the hover scope table. In every case suppress the
        // wildcards (`any`/`invalid`) and the unnamed-scope fallback (`scope_N`,
        // when no config scope is loaded): showing those is noise.
        let resolve_scope = |sc: &cwtools_game::scope_engine::ScopeContext,
                             id: cwtools_game::ScopeId| {
            let name = sc.registry.name_of(id);
            let placeholder = name == "any"
                || name == "invalid"
                || name.strip_prefix("scope_").and_then(|s| s.parse().ok()) == Some(id.0);
            (!placeholder).then_some(name)
        };
        let (current_scope, root_scope, prev_scope, from_scopes) = match rctx.scope.as_ref() {
            Some(sc) => {
                let current = resolve_scope(sc, sc.current());
                let root = resolve_scope(sc, sc.root);
                // PREV is the scope one level out: the second-from-top of the stack.
                let prev = (sc.scopes.len() >= 2)
                    .then(|| sc.scopes[sc.scopes.len() - 2])
                    .and_then(|id| resolve_scope(sc, id));
                // FROM chain: [0] = FROM, [1] = FROM.FROM, …; drop placeholders.
                let from = sc
                    .from
                    .iter()
                    .filter_map(|id| resolve_scope(sc, *id))
                    .collect();
                (current, root, prev, from)
            }
            None => (None, None, None, Vec::new()),
        };
        // The scope the hovered key resolves TO: run it through `change_scope` on
        // a clone of the cursor's context. For a scope-changing link (`owner`) or
        // a meta keyword (`FROM`/`ROOT`/`PREV`) this is the target scope; for
        // anything that doesn't change scope it stays the ambient one (and is
        // suppressed at display when it matches). Only computed when the
        // `hover.scopeDisplay = "resolved"` setting is on. (#37)
        let resolved_scope = self
            .state
            .hover_resolved_scope
            .load(Ordering::Relaxed)
            .then(|| match (rctx.scope.as_ref(), &element) {
                (Some(sc), PositionElement::Leaf { key, .. }) if !key.is_empty() => {
                    let mut probe = sc.clone();
                    probe.change_scope(key);
                    resolve_scope(&probe, probe.current())
                }
                _ => None,
            })
            .flatten();
        Some(RuleCursorInfo {
            element,
            hint,
            category,
            description,
            required_scopes: scopes,
            current_scope,
            root_scope,
            prev_scope,
            from_scopes,
            resolved_scope,
        })
    }
}

impl Backend {
    /// Snapshot the document AST for `uri`, plus whether it came from the
    /// current document version. When there is no cached AST, re-parse the live
    /// text for THIS request only so hover/goto/completion still resolve a
    /// context mid-edit. The fresh AST is not written back. The debounced
    /// validate owns the long-term one. The `documents` mutex is held only for
    /// the snapshot, never across the parse.
    pub(crate) fn ast_snapshot_for(&self, uri: &str) -> Option<AstSnapshot> {
        let text = {
            let docs = self.state.documents.lock();
            let doc = docs.get(uri)?;
            if let Some(ast) = &doc.ast {
                let source = if doc.ast_version == Some(doc.version) {
                    AstSource::StoredCurrent
                } else {
                    AstSource::StoredStale
                };
                return Some(AstSnapshot {
                    ast: ast.clone(),
                    source,
                });
            }
            doc.text.clone()
        };
        let table = self.state.string_table.clone();
        tokio::task::block_in_place(|| {
            cwtools_parser::parser::parse_string(&text, &table)
                .ok()
                .map(|ast| AstSnapshot {
                    ast: Arc::new(ast),
                    source: AstSource::FreshParse,
                })
        })
    }

    /// Snapshot the document AST for `uri`, preserving the existing behavior for
    /// hover/goto callers that do not need freshness metadata.
    pub(crate) fn ast_for(&self, uri: &str) -> Option<Arc<ParsedFile>> {
        self.ast_snapshot_for(uri).map(|snapshot| snapshot.ast)
    }

    /// The classified element under the cursor via `element_at_position`, run on
    /// the snapshotted AST (with the mid-edit re-parse fallback from `ast_for`).
    /// Shared by hover and goto's heuristic fallbacks.
    pub(crate) fn element_at_cursor(
        &self,
        uri: &str,
        pos: tower_lsp::lsp_types::Position,
    ) -> Option<PositionElement> {
        let ast = self.ast_for(uri)?;
        let text = {
            let docs = self.state.documents.lock();
            docs.get(uri).map(|doc| doc.text.clone())
        }?;
        let position_encoding = self.state.config.read().position_encoding.clone();
        let (line, col) = crate::paths::lsp_pos_to_source_in_text(&text, pos, &position_encoding);
        cwtools_info::element_at_position(&ast, line, col, &self.state.string_table)
    }

    /// Resolve the rule context at the cursor, snapshotting the AST and ruleset
    /// so neither the `documents` mutex nor the rules guard is held across
    /// `rules_at_pos`. Shared by completion and `rule_info_at_cursor` (hover /
    /// goto). `RuleContext` is owned, so all guards are released on return.
    pub(crate) fn resolve_at_cursor(
        &self,
        uri: &str,
        pos: tower_lsp::lsp_types::Position,
        logical_path: &str,
    ) -> Option<CursorResolution> {
        let (game, scope_checks, var_checks, position_encoding) = {
            let cfg = self.state.config.read();
            (
                cfg.game(),
                cfg.scope_checks,
                cfg.var_checks,
                cfg.position_encoding.clone(),
            )
        };
        let ast = self.ast_for(uri)?;
        let (ruleset, modifier_keys, scope_registry) = {
            let rules_guard = self.state.rules.read();
            (
                rules_guard.ruleset.clone()?,
                rules_guard.modifier_keys.clone(),
                rules_guard.scope_registry.clone(),
            )
        };
        let document_text = {
            let docs = self.state.documents.lock();
            docs.get(uri).map(|doc| Arc::clone(&doc.text))
        };
        let (line, col) = document_text.as_deref().map_or_else(
            || crate::paths::lsp_pos_to_source(pos),
            |text| crate::paths::lsp_pos_to_source_in_text(text, pos, &position_encoding),
        );
        // info_service read is held only for the resolve; `rules_at_pos` returns
        // owned data, so it is dropped before the caller runs.
        let info_guard = self.state.info_service.read();
        let prepared = crate::validate::make_prepared(
            &ruleset,
            &self.state.string_table,
            game,
            &info_guard.type_index,
            &modifier_keys,
            None,
            None,
            scope_registry.as_ref(),
            scope_checks,
            var_checks,
        );
        let rctx = rules_at_pos(&ast, logical_path, &prepared, line, col, false)?;
        drop(info_guard);
        Some(CursorResolution { rctx, ruleset })
    }

    /// The `$KEY$` loc reference under the cursor in an open `.yml` document, plus
    /// its `[start, end)` range in the negotiated position encoding. `None` when
    /// the cursor isn't on a reference (or the document isn't open). Shared by
    /// hover and goto.
    pub(crate) fn loc_ref_at_cursor_doc(
        &self,
        uri: &str,
        pos: tower_lsp::lsp_types::Position,
    ) -> Option<(String, u32, u32)> {
        let position_encoding = self.state.config.read().position_encoding.clone();
        let docs = self.state.documents.lock();
        let doc = docs.get(uri)?;
        let line = doc.text.lines().nth(pos.line as usize)?;
        crate::paths::loc_ref_at_cursor_with_encoding(line, pos.character, &position_encoding)
    }
}

/// The resolved rule context at the cursor plus the ruleset snapshot it was
/// resolved against, returned by [`Backend::resolve_at_cursor`]. The Arcs keep
/// the ruleset/registry alive for callers that inspect the context after the
/// guards are dropped.
pub(crate) struct CursorResolution {
    pub(crate) rctx: cwtools_validation::position::RuleContext,
    pub(crate) ruleset: Arc<RuleSet>,
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
    /// The scope context at the cursor (the scope the block evaluates in), for
    /// the hover. `None` when no registry or the scope is the `any` wildcard.
    pub(crate) current_scope: Option<String>,
    /// Related scopes at the cursor, for the hover scope table. ROOT is the
    /// outermost block's scope; PREV is the enclosing scope (one level out).
    /// Each is `None` when absent or a suppressed placeholder.
    pub(crate) root_scope: Option<String>,
    pub(crate) prev_scope: Option<String>,
    /// The FROM chain: `[0]` = FROM, `[1]` = FROM.FROM, … (placeholders dropped).
    pub(crate) from_scopes: Vec<String>,
    /// The scope the hovered key resolves to (run through `change_scope`). Shown
    /// as a `Resolves to` line only when the `hover.scopeDisplay = "resolved"`
    /// setting is on and it differs from the current scope. (#37)
    pub(crate) resolved_scope: Option<String>,
}

/// Map a matched leaf rule's right-hand field to a [`ReferenceHint`] for the
/// leaf's value (the same classification `info_at_position` used to do at
/// depth 0-1, now fed by the full position resolver).
fn hint_from_rule_right(rule_type: &RuleType, value: &str, ruleset: &RuleSet) -> ReferenceHint {
    let right = match rule_type {
        RuleType::LeafRule { right, .. } => right,
        RuleType::LeafValueRule { right } => right,
        _ => return ReferenceHint::Unknown,
    };
    field_to_hint(right, value, ruleset)
}

/// Map a matched rule's LEFT field to a [`ReferenceHint`] for the key — for
/// references that sit on the key, like a `<character>` used as a scoped-trigger
/// block key or a `type[…]` entity-definition key.
fn hint_from_rule_left(rule_type: &RuleType, key: &str) -> ReferenceHint {
    let left = match rule_type {
        RuleType::LeafRule { left, .. } => left,
        RuleType::NodeRule { left, .. } => left,
        _ => return ReferenceHint::Unknown,
    };
    match left {
        NewField::TypeField(_) | NewField::ValueField(ValueType::Enum(_)) => {
            // No ruleset needed for the type/enum cases; the scope-link upgrade
            // only applies to right-hand values, so pass an empty ruleset.
            field_to_hint_simple(left, key)
        }
        _ => ReferenceHint::Unknown,
    }
}

/// Shared field → hint mapping for the type/enum cases that don't need the
/// ruleset (used by the key-side classifier).
fn field_to_hint_simple(field: &NewField, value: &str) -> ReferenceHint {
    match field {
        NewField::TypeField(TypeType::Simple(t)) => ReferenceHint::TypeRef {
            type_name: t.clone(),
            value: value.to_string(),
        },
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
        _ => ReferenceHint::Unknown,
    }
}

/// Full field → hint mapping for a right-hand value. Resolves a prefixed scope
/// reference (e.g. `sp:sp_nuclear_reactor`) to a `TypeRef` via the matching
/// link's `data_source` `<type>`, so goto/hover treat the value as that instance.
fn field_to_hint(field: &NewField, value: &str, ruleset: &RuleSet) -> ReferenceHint {
    match field {
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
        NewField::ScopeField(_) => {
            scope_prefixed_type_ref(value, ruleset).unwrap_or_else(|| ReferenceHint::ScopeName {
                name: value.to_string(),
            })
        }
        other => field_to_hint_simple(other, value),
    }
}

/// A prefixed scope reference like `sp:sp_nuclear_reactor` resolves through the
/// link whose `prefix` matches (`sp` → `prefix = sp:`, `data_source =
/// <special_project>`). Strip the prefix and point at the data-source type. The
/// scope-field's scope NAME (`special_project`) is a scope type, not the link
/// name, so matching must be by value prefix.
fn scope_prefixed_type_ref(value: &str, ruleset: &RuleSet) -> Option<ReferenceHint> {
    for li in &ruleset.link_inputs {
        let prefix = li.prefix.as_deref()?;
        if prefix.is_empty() {
            continue;
        }
        let Some(rest) = value.strip_prefix(prefix) else {
            continue;
        };
        for ds in &li.data_source {
            if let Some(t) = ds.strip_prefix('<').and_then(|s| s.strip_suffix('>')) {
                return Some(ReferenceHint::TypeRef {
                    type_name: t.to_string(),
                    value: rest.to_string(),
                });
            }
        }
    }
    None
}

/// A bare key that is a known instance of a type used as a prefix-less link
/// `data_source` (e.g. a character name, where the `character` link's
/// `data_source` is `<character>`). Returns the type name so the key resolves to
/// its definition. Used for keys that scope into an entity without a rule match.
fn scope_link_key_type(
    ruleset: &RuleSet,
    type_index: &cwtools_info::TypeIndex,
    key: &str,
) -> Option<String> {
    for li in &ruleset.link_inputs {
        // A bare key carries no prefix, so only prefix-less links apply.
        if li.prefix.is_some() {
            continue;
        }
        for ds in &li.data_source {
            if let Some(t) = ds.strip_prefix('<').and_then(|s| s.strip_suffix('>'))
                && type_index
                    .instances(t)
                    .iter()
                    .any(|(_, inst)| inst.name == key)
            {
                return Some(t.to_string());
            }
        }
    }
    None
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(&self, params: InitializeParams) -> Result<InitializeResult> {
        self.initialize_impl(params).await
    }

    async fn initialized(&self, _params: InitializedParams) {
        self.client
            .log_message(MessageType::INFO, "CWTools server initialized!")
            .await;

        // Workspace-wide initial validation spawned in background so the LSP
        // handshake returns promptly.
        let client = self.client.clone();
        let state = self.state.clone();
        let watch_state = self.state.clone();
        let watch_client = self.client.clone();
        let handle = tokio::spawn(async move {
            let backend = Backend { client, state };
            backend.validate_entire_workspace(false).await;
        });
        // Log if the workspace scan panics — without this, a panic is silently
        // swallowed (the JoinHandle is dropped) and the server runs in a
        // degraded state with no diagnostics.
        tokio::spawn(async move {
            if let Err(e) = handle.await {
                tracing::error!("validate_entire_workspace panicked: {}", e);
                // The scan didn't reach the point where it flips index_ready, so
                // diagnostics would stay suppressed forever. Release the gate so
                // per-file validation still publishes (degraded but not silent).
                watch_state
                    .index_ready
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                // The panic also skipped the wrapper's bar-off, so the status bar
                // would spin on "Indexing workspace…" forever. Clear it here.
                let payload = serde_json::json!({ "enable": false, "value": "" });
                watch_client.send_notification::<LoadingBar>(payload).await;
            }
        });

        // Periodic quiet re-scan so a long-running session doesn't accumulate
        // stale index state. Off by default (background_reindex_interval_minutes
        // == 0); runs only while the user is idle, and every notification the
        // scan would normally send to the status bar is suppressed.
        let reindex_client = self.client.clone();
        let reindex_state = self.state.clone();
        tokio::spawn(async move {
            let backend = Backend {
                client: reindex_client,
                state: reindex_state,
            };
            backend.background_reindex_loop().await;
        });
    }

    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }

    async fn did_change_configuration(&self, params: DidChangeConfigurationParams) {
        self.did_change_configuration_impl(params).await
    }

    // --- Text document sync ---
    #[tracing::instrument(skip_all)]
    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        self.mark_activity();
        let uri = params.text_document.uri.to_string();
        let text = params.text_document.text;
        let version = params.text_document.version;
        tracing::debug!(%uri, version, bytes = text.len(), "did_open");

        // Insert synchronously (no `.await` before the mutation) so a request in
        // the gap sees the new text; `ast: None` is filled in by the spawned
        // validate, and requests before then fresh-parse via `ast_snapshot_for`.
        {
            let mut docs = self.state.documents.lock();
            docs.insert(
                uri.clone(),
                ParsedDoc {
                    version,
                    text: Arc::from(text),
                    ast: None,
                    ast_version: None,
                },
            );
        }

        // Offload validation off the message future so a burst of opens can't
        // hold the bounded request queue (#90). `debounced_validate`'s
        // export-diff-gated dependent sweep replaces the old inline sweep here:
        // opening a file whose exports match what's already indexed skips the
        // sweep entirely, and a changed export refreshes only real dependents.
        // Bump the edit counter so that sweep is tagged and a later edit
        // supersedes it.
        let generation = self.state.edit_generation.fetch_add(1, Ordering::AcqRel) + 1;
        self.spawn_debounced_validate(uri, version, generation, ValidateTrigger::DidOpen, 0);
    }

    #[tracing::instrument(skip_all)]
    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        self.mark_activity();
        let uri = params.text_document.uri.to_string();
        let version = params.text_document.version;

        // FULL-sync spec requires last-wins; use the last change in the batch.
        let Some(change) = params.content_changes.into_iter().last() else {
            return;
        };
        let text = change.text;
        tracing::debug!(%uri, version, bytes = text.len(), "did_change");
        let text: Arc<str> = Arc::from(text);

        // Store the new text+version immediately (keep the prior AST until we
        // revalidate). The debounced task checks the version to know whether this
        // is still the latest edit.
        {
            let mut docs = self.state.documents.lock();
            // Update the text+version in place, preserving the prior AST (kept
            // until the debounced task revalidates). get_mut avoids a
            // remove+reinsert and the uri clone the insert would need.
            if let Some(d) = docs.get_mut(&uri) {
                d.version = version;
                d.text = text;
            } else {
                docs.insert(
                    uri.clone(),
                    ParsedDoc {
                        version,
                        text,
                        ast: None,
                        ast_version: None,
                    },
                );
            }
        }

        // Bump the global edit counter so any in-flight dependent sweep from an
        // earlier edit knows it has been superseded and can stop early.
        let generation = self.state.edit_generation.fetch_add(1, Ordering::AcqRel) + 1;

        // Validate in the background after a short debounce so a burst of
        // keystrokes coalesces into one validation and the handler returns
        // immediately (no per-keystroke re-parse lag). The helper aborts any
        // pending sleeper for this file so a burst can't stack hundreds of
        // debounce tasks (#47).
        self.spawn_debounced_validate(
            uri,
            version,
            generation,
            ValidateTrigger::DidChange,
            DEBOUNCE_MS,
        );
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        let uri = params.text_document.uri.to_string();
        // A save isn't an edit, so don't bump the edit counter, just re-read the
        // current version and generation. Offload the validation like did_change
        // (#90); the entry version guard in `debounced_validate` makes a racing
        // did_change safe. The export-diff-gated dependent sweep also refreshes
        // callers when a save changed this file's exports.
        let Some(version) = ({
            let docs = self.state.documents.lock();
            docs.get(&uri).map(|d| d.version)
        }) else {
            return;
        };
        let generation = self.state.edit_generation.load(Ordering::Acquire);
        self.spawn_debounced_validate(uri, version, generation, ValidateTrigger::DidSave, 0);
    }

    #[tracing::instrument(skip_all)]
    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri.to_string();
        tracing::debug!(%uri, "did_close");
        self.state.documents.lock().remove(&uri);
        let pending_validation = self.state.debounce_handles.lock().remove(&uri);
        if let Some(handle) = pending_validation {
            handle.abort();
            let _ = handle.await;
        }

        if self.state.documents.lock().contains_key(&uri) {
            return;
        }

        let (exports_before, names_before) = {
            let info = self.state.info_service.read();
            (info.export_fingerprint(&uri), info.export_names(&uri))
        };
        let disk_ast = if !crate::paths::is_loc_file(&uri) && !crate::paths::is_cwt_file(&uri) {
            let path = crate::paths::uri_to_path_str(&uri);
            let table = self.state.string_table.clone();
            tokio::task::spawn_blocking(move || {
                let text =
                    cwtools_file_manager::file_manager::read_text(std::path::Path::new(&path))
                        .ok()?;
                cwtools_parser::parser::parse_string(&text, &table).ok()
            })
            .await
            .ok()
            .flatten()
        } else {
            None
        };

        let (exports_after, names_after, generation) = {
            let mut doc_tokens = self.state.doc_tokens.write();
            let documents = self.state.documents.lock();
            if documents.contains_key(&uri) {
                return;
            }

            if let Some(parsed) = disk_ast.as_ref() {
                self.index_parsed_file(&uri, parsed);
            } else {
                self.state.info_service.write().clear_file(&uri);
                self.bump_info_revision();
            }
            doc_tokens.remove(&uri);
            self.state.loc_live_overlay.write().remove(&uri);
            self.state.completion_generation.lock().remove(&uri);

            let info = self.state.info_service.read();
            (
                info.export_fingerprint(&uri),
                info.export_names(&uri),
                self.state.edit_generation.fetch_add(1, Ordering::AcqRel) + 1,
            )
        };

        cwtools_profiling::log_rss("did_close");
        if !self.state.documents.lock().contains_key(&uri) {
            self.client
                .publish_diagnostics(params.text_document.uri, vec![], None)
                .await;
        }

        if exports_before != exports_after {
            let mut changed_names: HashSet<String> = names_before
                .symmetric_difference(&names_after)
                .cloned()
                .collect();
            changed_names.extend(self.state.pending_changed_names.lock().drain());
            self.revalidate_open_dependents(
                &uri,
                generation,
                (!changed_names.is_empty()).then_some(&changed_names),
            )
            .await;
        }
    }

    // --- Language features ---

    async fn hover(&self, params: HoverParams) -> Result<Option<Hover>> {
        self.mark_activity();
        self.hover_impl(params).await
    }

    async fn completion(&self, params: CompletionParams) -> Result<Option<CompletionResponse>> {
        self.completion_impl(params).await
    }

    async fn completion_resolve(&self, item: CompletionItem) -> Result<CompletionItem> {
        Ok(self.completion_resolve_impl(item))
    }

    async fn goto_definition(
        &self,
        params: GotoDefinitionParams,
    ) -> Result<Option<GotoDefinitionResponse>> {
        self.mark_activity();
        self.goto_definition_impl(params).await
    }

    async fn references(&self, params: ReferenceParams) -> Result<Option<Vec<Location>>> {
        self.mark_activity();
        self.references_impl(params).await
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        self.mark_activity();
        self.document_symbol_impl(params).await
    }

    async fn symbol(
        &self,
        params: WorkspaceSymbolParams,
    ) -> Result<Option<Vec<SymbolInformation>>> {
        self.mark_activity();
        self.symbol_impl(params).await
    }

    async fn folding_range(&self, params: FoldingRangeParams) -> Result<Option<Vec<FoldingRange>>> {
        self.folding_range_impl(params).await
    }

    async fn document_highlight(
        &self,
        params: DocumentHighlightParams,
    ) -> Result<Option<Vec<DocumentHighlight>>> {
        self.document_highlight_impl(params).await
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
        self.execute_command_impl(params).await
    }

    async fn did_change_watched_files(&self, params: DidChangeWatchedFilesParams) {
        self.did_change_watched_files_impl(params).await;
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
        .expect("failed to build tokio runtime")
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

        let items = completions_from_rules(
            rules,
            &rs,
            &info,
            "stellaris",
            &HashSet::new(),
            &Default::default(),
            None,
            None,
            "",
        )
        .0;

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
                text: Arc::from(source),
                ast: Some(Arc::new(parsed)),
                ast_version: Some(0),
            },
        );

        let ws_uri: Option<std::sync::Arc<str>> = Some("file:///".into());
        let sites = scan_use_sites("my_type", "my_instance", &docs, &rs, &ws_uri, &table);
        assert!(!sites.is_empty(), "expected use sites, got none");
        assert!(
            sites.iter().any(|(uri, _)| uri == "file:///test.txt"),
            "expected correct uri"
        );
    }

    // ── didFocusFile (background-reindex idle clock) ─────────────────────────

    #[test]
    fn test_did_focus_file_marks_activity() {
        // A focus switch is user activity: the handler must reset the idle
        // clock the background reindex loop watches, like edits and feature
        // requests do. Sentinel u64::MAX can never be a real elapsed value.
        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        rt.block_on(async {
            let (service, _socket) = LspService::build(|client| Backend {
                client,
                state: Arc::new(DocumentState::new()),
            })
            .finish();
            let backend = service.inner();
            backend
                .state
                .last_activity_ms
                .store(u64::MAX, Ordering::Relaxed);
            backend.on_did_focus_file(Value::Null).await;
            assert_ne!(
                backend.state.last_activity_ms.load(Ordering::Relaxed),
                u64::MAX,
                "didFocusFile must reset the background-reindex idle clock"
            );
        });
    }
}
