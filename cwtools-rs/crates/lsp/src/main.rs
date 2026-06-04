use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tower_lsp::jsonrpc::Result;
use tower_lsp::lsp_types::*;
use tower_lsp::{Client, LanguageServer, LspService, Server};
use serde_json::Value;

use cwtools_parser::parser::parse_string;
use cwtools_parser::ast::{ParsedFile, ParseError};
use cwtools_rules::rules_types::{NewField, RootRule, RuleSet, RuleType, TypeType, ValueType};
use cwtools_rules::ruleset_loader::load_ruleset_from_dir;
use cwtools_string_table::string_table::StringTable;
use cwtools_validation::{validate_ast, ValidationError};
use cwtools_info::{
    info_at_position, PositionElement, ReferenceHint,
};

mod position;
mod symbols;

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
    /// computed info service for type/references/definitions
    info_service: Mutex<cwtools_info::InfoService>,
    /// workspace folder URI captured from initialize params
    workspace_uri: Mutex<Option<String>>,
}

struct ParsedDoc {
    version: i32,
    text: String,
    ast: Option<ParsedFile>,
}

impl DocumentState {
    fn new() -> Self {
        Self {
            documents: Mutex::new(HashMap::new()),
            ruleset: Mutex::new(None),
            string_table: StringTable::new(),
            language: Mutex::new("paradox".to_string()),
            symbol_index: Mutex::new(symbols::SymbolIndex::new()),
            info_service: Mutex::new(cwtools_info::InfoService::new()),
            workspace_uri: Mutex::new(None),
        }
    }
}

struct Backend {
    client: Client,
    state: Arc<DocumentState>,
}

// ── Custom notification stubs ─────────────────────────────────────────────────

impl Backend {
    /// Called when the VS Code extension tells us the user switched to a file.
    /// We receive it but don't act on it yet.
    async fn on_did_focus_file(&self, _params: Value) {
        // C→S: accept silently.
    }
}

// ── Hover helpers ─────────────────────────────────────────────────────────────

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
        (Some(rules), PositionElement::Leaf { key, .. }) | (Some(rules), PositionElement::Node { key }) => {
            find_rule_description(rules, key)
        }
        _ => (None, Vec::new()),
    };

    let mut parts: Vec<String> = Vec::new();

    // Primary classification
    match hint {
        ReferenceHint::TypeRef { type_name, value } => {
            parts.push(format!("**Type reference** — `{}` (`{}`)", value, type_name));
        }
        ReferenceHint::EnumRef { enum_name, value } => {
            parts.push(format!("**Enum value** — `{}` (member of `{}`)", value, enum_name));
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
                RuleType::LeafRule { left: NewField::SpecificField(k), .. }
                | RuleType::NodeRule { left: NewField::SpecificField(k), .. } => {
                    if k.eq_ignore_ascii_case(key) {
                        return (opts.description.clone(), opts.required_scopes.clone());
                    }
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
fn enclosing_key_path(
    ast: &ParsedFile,
    line: u32,
    col: u16,
    table: &StringTable,
) -> Vec<String> {
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
                if let Value::Clause(ch) = &leaf.value {
                    if pos_in_range_simple(target, &leaf.pos) {
                        let key = table.get_string(leaf.key.normal).unwrap_or_default();
                        path.push(key);
                        if collect_enclosing_path(ch, arena, target, table, path) {
                            return true;
                        }
                        return true;
                    }
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
    let top_key = &key_path[0];
    for root_rule in &ruleset.root_rules {
        let (type_name, (rule_type, _)) = match root_rule {
            RootRule::TypeRule(n, r) => (n, r),
            _ => continue,
        };
        let type_def = ruleset.types.iter().find(|t| &t.name == type_name);
        if let Some(td) = type_def {
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
            RuleType::NodeRule { left: NewField::SpecificField(k), rules: inner, .. }
            | RuleType::NodeRule { left: NewField::AliasValueKeysField(k), rules: inner, .. } => {
                if k.eq_ignore_ascii_case(next_key) {
                    return descend_rules(inner, &remaining[1..]);
                }
            }
            _ => {}
        }
    }
    None
}

/// Thin wrapper around the info crate's path check (avoids re-exporting it).
fn cwtools_info_path_check(opts: &cwtools_rules::rules_types::PathOptions, logical_path: &str) -> bool {
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

/// Build context-aware completion items from the child rules at the cursor's
/// position.
///
/// Limitation: AliasField expansion is not fully recursive (that requires
/// following the alias chain, which can be large).  TypeField completions use
/// the TypeIndex from the InfoService.  ScopeField completions are placeholder
/// scope names.
fn completions_from_rules<'a>(
    rules: &[(RuleType, cwtools_rules::rules_types::Options)],
    ruleset: &'a RuleSet,
    info: &cwtools_info::InfoService,
    language: &str,
) -> Vec<CompletionItem> {
    let mut items: Vec<CompletionItem> = Vec::new();

    for (rule_type, opts) in rules {
        match rule_type {
            // A concrete key in the block
            RuleType::LeafRule { left: NewField::SpecificField(k), right } => {
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
            // A node block key
            RuleType::NodeRule { left: NewField::SpecificField(k), .. } => {
                items.push(CompletionItem {
                    label: k.clone(),
                    kind: Some(CompletionItemKind::STRUCT),
                    detail: opts.description.clone(),
                    insert_text: Some(format!("{} = {{\n\t$0\n}}", k)),
                    insert_text_format: Some(InsertTextFormat::SNIPPET),
                    ..Default::default()
                });
            }
            // An enum value at the leaf level
            RuleType::LeafValueRule { right: NewField::ValueField(ValueType::Enum(e)) } => {
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
            RuleType::LeafRule { right: NewField::AliasField(cat), .. }
            | RuleType::LeafValueRule { right: NewField::AliasField(cat) }
            | RuleType::NodeRule { left: NewField::AliasField(cat), .. } => {
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
            RuleType::LeafRule { right: NewField::ScopeField(_), .. }
            | RuleType::LeafValueRule { right: NewField::ScopeField(_) } => {
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
            RuleType::LeafValueRule { right: NewField::ValueField(ValueType::Bool) } => {
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

fn enum_values_for<'a>(ruleset: &'a RuleSet, enum_name: &str) -> Vec<String> {
    if let Some(e) = ruleset.enums.iter().find(|e| e.key == enum_name) {
        return e.values.clone();
    }
    Vec::new()
}

/// Best-effort scope name list for the current game.
fn scope_names_for_game(language: &str) -> &'static [&'static str] {
    match language {
        "hoi4" => &[
            "THIS", "ROOT", "PREV", "FROM", "OVERLORD", "FACTION_LEADER",
            "capital_scope", "owner",
        ],
        "stellaris" => &[
            "this", "root", "prev", "from", "owner", "controller",
            "space_owner", "space_controller", "solar_system",
        ],
        "eu4" => &[
            "THIS", "ROOT", "PREV", "FROM", "EMPEROR",
            "capital_scope", "owner", "controller",
        ],
        "ck3" => &[
            "this", "root", "prev", "from",
            "liege", "employer", "host",
        ],
        _ => &["this", "root", "prev", "from"],
    }
}

#[tower_lsp::async_trait]
impl LanguageServer for Backend {
    async fn initialize(
        &self,
        params: InitializeParams,
    ) -> Result<InitializeResult> {
        // Store language from init options
        if let Some(opts) = &params.initialization_options {
            if let Some(lang) = opts.get("language").and_then(|v| v.as_str()) {
                *self.state.language.lock().unwrap() = lang.to_string();
                self.client
                    .log_message(MessageType::INFO, format!("language: {}", lang))
                    .await;
            }
            self.client
                .log_message(MessageType::INFO, format!("init options: {:?}", opts))
                .await;

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
                    *self.state.ruleset.lock().unwrap() = Some(combined_ruleset);
                } else {
                    self.client
                        .log_message(
                            MessageType::WARNING,
                            format!(
                                "No rules loaded from {}. Errors: {:?}",
                                cache, parse_errors
                            ),
                        )
                        .await;
                }
            }
        }

        // Store workspace URI if provided
        if let Some(folders) = &params.workspace_folders {
            if let Some(first) = folders.first() {
                *self.state.workspace_uri.lock().unwrap() = Some(first.uri.to_string());
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

    // --- Text document sync ---
    async fn did_open(&self, params: DidOpenTextDocumentParams) {
        let uri = params.text_document.uri.to_string();
        let text = params.text_document.text;
        let version = params.text_document.version;

        let (diagnostics, parsed) = self.parse_and_validate(&uri, &text).await;

        {
            let mut docs = self.state.documents.lock().unwrap();
            docs.insert(
                uri.clone(),
                ParsedDoc {
                    version,
                    text: text.clone(),
                    ast: parsed,
                },
            );
        }

        self.client
            .publish_diagnostics(params.text_document.uri, diagnostics, Some(version))
            .await;
    }

    async fn did_change(&self, params: DidChangeTextDocumentParams) {
        let uri = params.text_document.uri.to_string();
        let version = params.text_document.version;

        if let Some(change) = params.content_changes.into_iter().next() {
            let text = change.text;

            let (diagnostics, parsed) = self.parse_and_validate(&uri, &text).await;

            {
                let mut docs = self.state.documents.lock().unwrap();
                docs.insert(
                    uri.clone(),
                    ParsedDoc {
                        version,
                        text: text.clone(),
                        ast: parsed,
                    },
                );
            }

            self.client
                .publish_diagnostics(params.text_document.uri, diagnostics, Some(version))
                .await;
        }
    }

    async fn did_save(&self, params: DidSaveTextDocumentParams) {
        let uri = params.text_document.uri.to_string();
        if let Some(text) = {
            let docs = self.state.documents.lock().unwrap();
            docs.get(&uri).map(|d| d.text.clone())
        } {
            let (diagnostics, _) = self.parse_and_validate(&uri, &text).await;
            self.client
                .publish_diagnostics(params.text_document.uri, diagnostics, None)
                .await;
        }
    }

    async fn did_close(&self, params: DidCloseTextDocumentParams) {
        let uri = params.text_document.uri.to_string();
        {
            let mut docs = self.state.documents.lock().unwrap();
            docs.remove(&uri);
        }
        self.client
            .publish_diagnostics(params.text_document.uri, vec![], None)
            .await;
    }

    // --- Language features ---

    async fn hover(
        &self,
        params: HoverParams,
    ) -> Result<Option<Hover>> {
        let uri = params.text_document_position_params.text_document.uri.to_string();
        let pos = params.text_document_position_params.position;

        let docs = self.state.documents.lock().unwrap();
        if let Some(doc) = docs.get(&uri) {
            if let Some(ast) = &doc.ast {
                let lsp_line = pos.line + 1; // LSP is 0-based; parser is 1-based
                let lsp_col = pos.character as u16;

                let ws_uri = self.state.workspace_uri.lock().unwrap().clone();
                let logical_path = logical_path_from_uri(&uri, &ws_uri);

                let ruleset_guard = self.state.ruleset.lock().unwrap();
                let pos_info = if let Some(rs) = ruleset_guard.as_ref() {
                    info_at_position(ast, lsp_line, lsp_col, rs, &logical_path, &self.state.string_table)
                } else {
                    // No rules: fall back to position-only lookup
                    None
                };
                drop(ruleset_guard);

                if let Some(info) = pos_info {
                    let ruleset_guard2 = self.state.ruleset.lock().unwrap();
                    let md = build_hover_markdown(
                        &info.element,
                        &info.hint,
                        ruleset_guard2.as_ref(),
                    );
                    return Ok(Some(Hover {
                        contents: HoverContents::Markup(MarkupContent {
                            kind: MarkupKind::Markdown,
                            value: md,
                        }),
                        range: None,
                    }));
                }

                // Fallback: use the simpler position finder (no rule context)
                let source_pos = cwtools_parser::ast::SourcePos {
                    line: pos.line + 1,
                    col: pos.character as u16,
                };
                if let Some(element) = position::find_at_position(ast, &source_pos, &self.state.string_table) {
                    let contents = match element {
                        position::AstElement::Node { key, .. } => {
                            format!("**Node**: `{}`", key)
                        }
                        position::AstElement::Leaf { key, value, .. } => {
                            format!("**Field**: `{} = {}`", key, value)
                        }
                        position::AstElement::LeafValue { value, .. } => {
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
        }
        Ok(None)
    }

    async fn completion(
        &self,
        params: CompletionParams,
    ) -> Result<Option<CompletionResponse>> {
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
        let ws_uri = self.state.workspace_uri.lock().unwrap().clone();
        let logical_path = logical_path_from_uri(&uri, &ws_uri);
        let language = self.state.language.lock().unwrap().clone();

        let context_items: Vec<CompletionItem> = {
            let docs = self.state.documents.lock().unwrap();
            let ruleset_guard = self.state.ruleset.lock().unwrap();
            let info_guard = self.state.info_service.lock().unwrap();

            if let (Some(doc), Some(rs)) = (docs.get(&uri), ruleset_guard.as_ref()) {
                if let Some(ast) = &doc.ast {
                    let key_path = enclosing_key_path(ast, lsp_line, lsp_col, &self.state.string_table);
                    if let Some(rules) = rules_for_context(rs, &key_path, &logical_path) {
                        completions_from_rules(rules, rs, &*info_guard, &language)
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

        let ruleset = self.state.ruleset.lock().unwrap();
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

        let info = self.state.info_service.lock().unwrap();
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
        let uri = params.text_document_position_params.text_document.uri.to_string();

        let ws_uri = self.state.workspace_uri.lock().unwrap().clone();
        let logical_path = logical_path_from_uri(&uri, &ws_uri);

        // First try the rule-aware lookup via info_at_position so we get a
        // TypeRef hint and can look up the actual definition location.
        let type_ref: Option<(String, String)> = {
            let docs = self.state.documents.lock().unwrap();
            let ruleset_guard = self.state.ruleset.lock().unwrap();
            if let (Some(doc), Some(rs)) = (docs.get(&uri), ruleset_guard.as_ref()) {
                if let Some(ast) = &doc.ast {
                    let info = info_at_position(
                        ast, pos.line + 1, pos.character as u16,
                        rs, &logical_path, &self.state.string_table,
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
            let info = self.state.info_service.lock().unwrap();
            let instances = info.type_index.instances(&type_name);
            let found: Vec<Location> = instances
                .iter()
                .filter(|(_, inst)| inst.name == instance_name)
                .map(|(file_uri, inst)| Location {
                    uri: file_uri.parse().unwrap_or_else(|_| {
                        params.text_document_position_params.text_document.uri.clone()
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
                })
                .collect();
            if !found.is_empty() {
                return Ok(Some(GotoDefinitionResponse::Array(found)));
            }
        }

        // Fallback: heuristic symbol-based lookup
        let docs = self.state.documents.lock().unwrap();
        if let Some(doc) = docs.get(&uri) {
            if let Some(ast) = &doc.ast {
                let source_pos = cwtools_parser::ast::SourcePos {
                    line: pos.line + 1,
                    col: pos.character as u16,
                };
                if let Some(element) = position::find_at_position(ast, &source_pos, &self.state.string_table) {
                    let symbol = match &element {
                        position::AstElement::Node { key, .. } => key.clone(),
                        position::AstElement::Leaf { key, .. } => key.clone(),
                        position::AstElement::LeafValue { value, .. } => value.clone(),
                    };
                    drop(docs);
                    let info = self.state.info_service.lock().unwrap();
                    if let Some(defs) = info.find_definitions(&symbol) {
                        let locations: Vec<Location> = defs.iter().map(|(file_uri, loc)| Location {
                            uri: file_uri.parse().unwrap_or_else(|_| params.text_document_position_params.text_document.uri.clone()),
                            range: Range {
                                start: Position { line: loc.line.saturating_sub(1), character: loc.col as u32 },
                                end: Position { line: loc.line.saturating_sub(1), character: (loc.col + symbol.len() as u16) as u32 },
                            },
                        }).collect();
                        if !locations.is_empty() {
                            return Ok(Some(GotoDefinitionResponse::Array(locations)));
                        }
                    }
                }
            }
        }
        Ok(None)
    }

    async fn references(
        &self,
        params: ReferenceParams,
    ) -> Result<Option<Vec<Location>>> {
        let pos = params.text_document_position.position;
        let uri = params.text_document_position.text_document.uri.to_string();

        let ws_uri = self.state.workspace_uri.lock().unwrap().clone();
        let logical_path = logical_path_from_uri(&uri, &ws_uri);

        // Try rule-aware: identify a TypeRef at cursor then scan type_index for
        // all locations where that type's instances are referenced.
        //
        // Limitation: reference scanning walks the TypeIndex for definition
        // locations only.  Tracking every *use* of a type instance across the
        // workspace would require an additional references index that is not yet
        // built.  Full cross-file reference tracking is left as future work.
        let type_ref: Option<(String, String)> = {
            let docs = self.state.documents.lock().unwrap();
            let ruleset_guard = self.state.ruleset.lock().unwrap();
            if let (Some(doc), Some(rs)) = (docs.get(&uri), ruleset_guard.as_ref()) {
                if let Some(ast) = &doc.ast {
                    let info = info_at_position(
                        ast, pos.line + 1, pos.character as u16,
                        rs, &logical_path, &self.state.string_table,
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
            let info = self.state.info_service.lock().unwrap();
            let instances = info.type_index.instances(&type_name);
            let found: Vec<Location> = instances
                .iter()
                .filter(|(_, inst)| inst.name == instance_name)
                .map(|(file_uri, inst)| Location {
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
                })
                .collect();
            if !found.is_empty() {
                return Ok(Some(found));
            }
        }

        // Fallback: heuristic-based approach
        let docs = self.state.documents.lock().unwrap();
        if let Some(doc) = docs.get(&uri) {
            if let Some(ast) = &doc.ast {
                let source_pos = cwtools_parser::ast::SourcePos {
                    line: pos.line + 1,
                    col: pos.character as u16,
                };
                if let Some(element) = position::find_at_position(ast, &source_pos, &self.state.string_table) {
                    let symbol = match &element {
                        position::AstElement::Node { key, .. } => key.clone(),
                        position::AstElement::Leaf { key, .. } => key.clone(),
                        position::AstElement::LeafValue { value, .. } => value.clone(),
                    };
                    drop(docs);
                    let info = self.state.info_service.lock().unwrap();
                    let mut all_locs = Vec::new();
                    if let Some(defs) = info.find_definitions(&symbol) {
                        all_locs.extend(defs.iter().map(|(file_uri, loc)| Location {
                            uri: file_uri.parse().unwrap_or_else(|_| params.text_document_position.text_document.uri.clone()),
                            range: Range {
                                start: Position { line: loc.line.saturating_sub(1), character: loc.col as u32 },
                                end: Position { line: loc.line.saturating_sub(1), character: (loc.col + symbol.len() as u16) as u32 },
                            },
                        }));
                    }
                    if let Some(refs) = info.find_references(&symbol) {
                        all_locs.extend(refs.iter().map(|(file_uri, loc)| Location {
                            uri: file_uri.parse().unwrap_or_else(|_| params.text_document_position.text_document.uri.clone()),
                            range: Range {
                                start: Position { line: loc.line.saturating_sub(1), character: loc.col as u32 },
                                end: Position { line: loc.line.saturating_sub(1), character: (loc.col + symbol.len() as u16) as u32 },
                            },
                        }));
                    }
                    let index = self.state.symbol_index.lock().unwrap();
                    if let Some(locs) = index.find_references(&symbol) {
                        all_locs.extend(locs.iter().map(|l| Location {
                            uri: l.uri.parse().unwrap_or_else(|_| params.text_document_position.text_document.uri.clone()),
                            range: Range {
                                start: Position { line: l.line.saturating_sub(1), character: l.col as u32 },
                                end: Position { line: l.line.saturating_sub(1), character: (l.col + symbol.len() as u16) as u32 },
                            },
                        }));
                    }
                    if !all_locs.is_empty() {
                        return Ok(Some(all_locs));
                    }
                }
            }
        }
        Ok(None)
    }

    async fn document_symbol(
        &self,
        params: DocumentSymbolParams,
    ) -> Result<Option<DocumentSymbolResponse>> {
        let uri = params.text_document.uri.to_string();
        let info = self.state.info_service.lock().unwrap();

        let file_info = match info.files.get(&uri) {
            Some(f) => f,
            None => return Ok(None),
        };

        // Emit type instances as document symbols (one per named instance).
        let mut symbols: Vec<SymbolInformation> = Vec::new();
        for (type_name, instances) in &file_info.type_instances {
            for inst in instances {
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
                    container_name: Some(type_name.clone()),
                });
            }
        }

        // Also include @-variables as symbols
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

    async fn execute_command(
        &self,
        params: ExecuteCommandParams,
    ) -> Result<Option<Value>> {
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
    /// Scan the entire workspace for relevant game files and validate them all.
    async fn validate_entire_workspace(&self) {
        let workspace_uri = {
            let guard = self.state.workspace_uri.lock().unwrap();
            guard.clone()
        };

        let root_path = match workspace_uri {
            Some(uri) => {
                let p = uri.strip_prefix("file://").unwrap_or(&uri);
                std::path::PathBuf::from(p)
            }
            None => {
                self.client
                    .log_message(MessageType::WARNING, "No workspace folder; skipping full-workspace validation.")
                    .await;
                return;
            }
        };

        let language = {
            let guard = self.state.language.lock().unwrap();
            guard.clone()
        };
        let extensions: Vec<&str> = match language.as_str() {
            "hoi4" => vec!["txt"],
            "stellaris" => vec!["txt"],
            "eu4" => vec!["txt"],
            "ck2" => vec!["txt"],
            "ck3" => vec!["txt"],
            "vic2" => vec!["txt"],
            "vic3" => vec!["txt"],
            "imperator" => vec!["txt"],
            "eu5" => vec!["txt"],
            _ => vec!["txt", "gfx", "gui"],
        };

        let mut files_to_validate = Vec::new();
        fn walk_dir(
            path: &std::path::Path,
            extensions: &[&str],
            out: &mut Vec<std::path::PathBuf>,
        ) {
            if let Ok(entries) = std::fs::read_dir(path) {
                for entry in entries.flatten() {
                    let path = entry.path();
                    if path.is_dir() {
                        let name = path.file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or("")
                            .to_lowercase();
                        let skip = matches!(
                            name.as_str(),
                            ".git" | "node_modules" | "out" | "dist" | "target" | "bin" | "obj"
                        );
                        if !skip {
                            walk_dir(&path, extensions, out);
                        }
                    } else if let Some(ext) = path.extension().and_then(|e| e.to_str()) {
                        if extensions.contains(&ext) {
                            out.push(path);
                        }
                    }
                }
            }
        }
        let ext_slice: &[&str] = &extensions;
        walk_dir(&root_path, ext_slice, &mut files_to_validate);

        if files_to_validate.is_empty() {
            self.client
                .log_message(MessageType::INFO, "No workspace files found to validate.")
                .await;
            return;
        }

        self.client
            .log_message(MessageType::INFO, format!(
                "Validating {} workspace files under {:?} ...",
                files_to_validate.len(),
                root_path
            ))
            .await;

        let mut total_errors = 0usize;
        let mut total_files = 0usize;
        for file_path in &files_to_validate {
            let uri = format!("file://{}", file_path.display());
            let text = match std::fs::read_to_string(file_path) {
                Ok(t) => t,
                Err(e) => {
                    self.client
                        .log_message(MessageType::WARNING, format!(
                            "Could not read {}: {}",
                            file_path.display(),
                            e
                        ))
                        .await;
                    continue;
                }
            };

            let (diagnostics, parsed) = self.parse_and_validate(&uri, &text).await;
            total_errors += diagnostics.iter()
                .filter(|d| d.severity == Some(DiagnosticSeverity::ERROR))
                .count();
            total_files += 1;

            {
                let mut docs = self.state.documents.lock().unwrap();
                docs.insert(
                    uri.clone(),
                    ParsedDoc {
                        version: 0,
                        text: text.clone(),
                        ast: parsed,
                    },
                );
            }

            if let Ok(uri_obj) = Url::parse(&uri) {
                self.client
                    .publish_diagnostics(uri_obj, diagnostics, None)
                    .await;
            }
        }

        self.client
            .log_message(MessageType::INFO, format!(
                "Workspace validation complete: {} errors across {} files",
                total_errors,
                total_files
            ))
            .await;
    }

    /// Parse and validate a single document.
    async fn parse_and_validate(
        &self,
        uri: &str,
        text: &str,
    ) -> (Vec<Diagnostic>, Option<ParsedFile>) {
        let mut diagnostics = Vec::new();

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
                    let mut index = self.state.symbol_index.lock().unwrap();
                    index.clear_document(uri);
                    index.index_document(uri, &parsed, &self.state.string_table);
                }

                // Derive logical path for type-instance indexing
                let ws_uri = self.state.workspace_uri.lock().unwrap().clone();
                let logical_path = logical_path_from_uri(uri, &ws_uri);

                // Update info service
                {
                    let ruleset_guard = self.state.ruleset.lock().unwrap();
                    let mut info = self.state.info_service.lock().unwrap();
                    info.clear_file(uri);
                    if let Some(ruleset) = ruleset_guard.as_ref() {
                        info.index_file_with_path(uri, &parsed, &self.state.string_table, ruleset, &logical_path);
                    }
                }

                // Validation
                let (errors, log_msg) = {
                    let ruleset_guard = self.state.ruleset.lock().unwrap();
                    if let Some(ruleset) = ruleset_guard.as_ref() {
                        let language = self.state.language.lock().unwrap().clone();
                        let game = cwtools_game::constants::Game::from_str(&language);
                        let start = std::time::Instant::now();
                        let mut errs = validate_ast(&parsed, ruleset, &self.state.string_table, uri, game);
                        let elapsed = start.elapsed();
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
                            });
                        }
                        let msg = format!(
                            "[validate] {} errors in {:?} ({} types, {} enums, {} aliases)",
                            total, elapsed, ruleset.types.len(), ruleset.enums.len(), ruleset.aliases.len()
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
        code: None,
        code_description: None,
        source: Some("cwtools".to_string()),
        message: err.message.clone(),
        related_information: None,
        tags: None,
        data: None,
    }
}

fn main() {
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
                    make_leaf_rule("kind", NewField::ValueField(ValueType::Enum("my_enum".to_string()))),
                    make_leaf_rule("active", NewField::ValueField(ValueType::Bool)),
                ],
            ),
        ));

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
        {
            if let Some((_, opts)) = rules.first_mut() {
                opts.description = Some("The kind of this thing".to_string());
            }
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
        assert!(kind_item.is_some(), "expected 'kind' completion, got: {:?}", items.iter().map(|i| &i.label).collect::<Vec<_>>());
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
}
