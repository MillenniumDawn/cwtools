# CWTools VS Code Extension Integration Playbook

## Context

This repository (`cwtools`) contains a Rust rewrite of the CWTools core (parser, AST, rules engine, validation, LSP server).

The VS Code extension lives in a **separate repository**: `cwtools-vscode`.

The original F# backend has been replaced by a Rust LSP server (`cwtools-server`). The binary is already built and placed at `bin/server/linux-x64/CWTools Server`.

## Goal

Wire the `cwtools-vscode` extension to use the Rust LSP binary instead of the old F# .NET binary.

---

## 1. What Already Works in cwtools Repo

### LSP Server Binary
- **Path**: `bin/server/linux-x64/CWTools Server` (4.4M release binary)
- **Transport**: stdio (same as F# backend — extension code unchanged)
- **Framework**: tower-lsp

### LSP Capabilities Implemented

| LSP Feature | Rust Status |
|---|---|
| `textDocument/publishDiagnostics` | ✅ Parse + validation on `didOpen`/`didChange` |
| `textDocument/hover` | ✅ Shows Node/Field/Value info |
| `textDocument/completion` | ✅ Types, enums, variables, event targets |
| `textDocument/definition` | ✅ Cross-file goto-definition via InfoService |
| `textDocument/references` | ✅ Cross-file references via InfoService |
| `workspace/executeCommand(getFileTypes)` | ✅ File type heuristics (events, common, etc.) |
| `initialize` / `initialized` | ✅ Receives init options |
| Custom notifications | ⚠️ Needs to be silent/ignored (see §3) |

### Custom Notifications the Extension Sends

The extension sends these custom notifications to the server. The F# server handled them. The Rust server currently does NOT handle them — they should be silently ignored or stubbed.

| Notification | Direction | What It Does | Rust Action Needed |
|---|---|---|---|
| `loadingBar` | S→C | Shows/hides status bar loading message | Silently ignore |
| `debugBar` | S→C | Shows/hides debug status bar | Silently ignore |
| `createVirtualFile` | S→C | Opens a virtual document in VS Code | Silently ignore |
| `promptReload` | S→C | Prompts user to reload extension | Silently ignore |
| `forceReload` | S→C | Forces extension reload | Silently ignore |
| `promptVanillaPath` | S→C | Asks user for vanilla game folder | Silently ignore |
| `didFocusFile` | C→S | User switched to a .txt file | Accept + ignore |
| `updateFileList` | S→C | Sends list of parsed files to file explorer | Silently ignore |

**Important**: `tower-lsp` auto-rejects unknown notifications by default. If the extension crashes when these are not acknowledged, add methods to `Backend` that accept them (see §4 Code Changes).

---

## 2. What Must Change

### Extension Changes Required (cwtools-vscode repo)

**NONE for basic functionality.** The extension already looks for:
```typescript
serverExe = context.asAbsolutePath(path.join('bin', 'server', 'linux-x64', 'CWTools Server'))
```

This binary exists in this repo. The extension's `serverOptions` already uses `TransportKind.stdio`.

**However**, the extension expects the server to handle `.cwt` rule downloading. The F# backend did this internally. The Rust server does NOT download rules.

### Options for Rule Loading

#### Option A: Extension Downloads Rules, Server Loads Them (Recommended)

1. Keep the extension's existing rule-download logic (it already downloads from `repoPath` to `cacheDir`)
2. After download, the extension should send the local rules directory path in `initializationOptions.rulesCache`
3. The Rust server, on `initialize`, walks `rulesCache/**/*.cwt`, parses them, and stores the `RuleSet`

#### Option B: User Points to Existing Rules Directory

1. Extension adds a setting: `cwtools.rulesPath` (path to local `.cwt` files)
2. Extension passes this in `initializationOptions`
3. Server loads `.cwt` files from that path

### Rust Server Changes Required (cwtools repo)

The server needs to:
1. Accept `initializationOptions.rulesCache` and load `.cwt` files from it
2. Accept custom notifications without crashing
3. Handle `didFocusFile` (currently not implemented)

---

## 3. Exact Code Changes

### In `cwtools-rs/crates/lsp/src/main.rs` (Rust server)

#### A. Add ruleset loading on `initialize`

Add after the existing `language` extraction in `initialize()`:

```rust
// Load .cwt rules from rulesCache if provided
if let Some(opts) = &params.initialization_options {
    if let Some(cache) = opts.get("rulesCache").and_then(|v| v.as_str()) {
        let cwt_files = std::fs::read_dir(cache)
            .ok()
            .into_iter()
            .flatten()
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.path().extension().map(|ext| ext == "cwt").unwrap_or(false)
            })
            .map(|e| e.path())
            .collect::<Vec<_>>();
        
        if !cwt_files.is_empty() {
            let mut combined_ruleset = RuleSet::new();
            for path in &cwt_files {
                if let Ok(content) = std::fs::read_to_string(path) {
                    if let Ok(parsed) = parse_string(&content, &self.state.string_table) {
                        let ruleset = cwtools_rules::rules_converter::ast_to_ruleset(&parsed, &self.state.string_table);
                        combined_ruleset.types.extend(ruleset.types);
                        combined_ruleset.enums.extend(ruleset.enums);
                        combined_ruleset.aliases.extend(ruleset.aliases);
                        combined_ruleset.subtypes.extend(ruleset.subtypes); // if field exists
                    }
                }
            }
            *self.state.ruleset.lock().unwrap() = Some(combined_ruleset);
            self.client.log_message(MessageType::INFO, format!("Loaded {} CWT rule files", cwt_files.len())).await;
        }
    }
}
```

#### B. Add stub handlers for custom notifications

Add these methods to `impl LanguageServer for Backend`:

```rust
async fn did_focus_file(&self, _params: serde_json::Value) {
    // Custom notification from VS Code extension — silently accept
}
```

If `tower-lsp` doesn't have a `did_focus_file` hook, register a custom notification handler. Alternatively, the extension can stop sending it.

#### C. Stub `loadingBar`, `debugBar`, etc.

These are **server → client** notifications. The F# server sent them. The Rust server currently doesn't send them, which is fine — the extension will simply never receive them.

But if you want the extension to handle them without F# parity:
- `loadingBar`: Not sent by Rust server. Extension already handles receiving it. No change needed.
- `updateFileList`: The Rust server currently doesn't populate file lists. The extension's file explorer will remain empty. To populate it, the server needs to scan the workspace on initialize and send `updateFileList` periodically.

For MVP: **Accept missing file explorer** or add workspace scanning later.

### In VS Code Extension (cwtools-vscode)

#### A. Ensure `rulesCache` directory contains `.cwt` files

The extension likely downloads a zip/tar from `repoPath` and extracts it. Check what format it downloads. The Rust server only reads plain `.cwt` text files, not packed archives.

If the extension downloads a zip:
1. After extraction, ensure `.cwt` files are placed in a flat or structured directory under `cacheDir`
2. Pass the directory path in `initializationOptions.rulesCache` (already done for F#)

If the extension downloads via git clone:
1. The cloned repo should contain `.cwt` files
2. Pass the repo root in `initializationOptions.rulesCache`

**Verify**: After extension activates, check if `.cwt` files exist in the cache directory. Use VS Code Developer Tools console to inspect `cacheDir`.

#### B. Optional: Change server capability announcement

The extension registers server capabilities. Currently the Rust server announces:
- textDocumentSync
- hoverProvider
- completionProvider
- definitionProvider
- referencesProvider
- executeCommandProvider (`getFileTypes`)

These match the extension's usage. No changes needed.

---

## 4. Testing Checklist

### Manual Test Steps

1. Ensure `bin/server/linux-x64/CWTools Server` exists (already in repo)
2. Install the extension in VS Code
3. Open a game mod folder (e.g., a Stellaris mod)
4. Set game language to `stellaris`
5. Extension spawns the binary → check VS Code Output panel for "CWTools server initialized!"
6. Open a `.txt` file (e.g., `common/scripted_effects/foo.txt`)
7. **Expected**: Diagnostics appear in Problems panel (parse errors or validation warnings)
8. **Expected**: Hover over a key shows tooltip ("Node: ..." or "Field: ...")
9. **Expected**: Ctrl+Click on a type reference jumps to definition (if rules loaded)

### Troubleshooting

| Symptom | Likely Cause | Fix |
|---|---|---|
| "Server exited with code 127" | Binary not found or not executable | `chmod +x bin/server/linux-x64/CWTools Server` |
| "Server exited with code 1" | Rust server panicked | Check extension Output panel for Rust panic text |
| No diagnostics | Rules not loaded | Verify `.cwt` files in `rulesCache` directory |
| No completions | Empty ruleset | Same as above |
| Hover shows nothing | AST element lookup failure | Parser may not find element at that position |
| Extension crashes on startup | Rust server missing capability | Add stub handler for unimplemented notification |

---

## 5. Current Architecture

```
cwtools-vscode/         (separate repo — the extension)
├── client/
│   └── extension/
│       └── extension.ts  → Spawns bin/server/linux-x64/CWTools Server
│                           via LSP stdio
│
cwtools/                (this repo — the Rust LSP server)
├── bin/
│   └── server/
│       └── linux-x64/
│           └── CWTools Server   ← 4.4M release binary
│
└── cwtools-rs/
    └── crates/
        └── lsp/
            └── src/
                └── main.rs      ← tower-lsp server
        ├── parser/             ← Paradox script parser
        ├── rules/              ← .cwt rule parser
        ├── validation/         ← Rule-driven validation engine
        ├── info/               ← InfoService (computed data)
        ├── localization/       ← YAML/CSV loc parsers
        ├── game/               ← Game constants, scope engine
        ├── cache/              ← rkyv serialization
        ├── cli/                ← CLI binary (parse, validate, loc)
        ├── file_manager/       ← File discovery
        └── string_table/       ← String interning
```

---

## 6. Remaining Parity Gaps (Non-blocking)

| Feature | Rust Status | Blocking? |
|---|---|---|
| Windows/macOS binaries | Only Linux x64 built | **Yes** for multi-platform users |
| Graph panel (`techGraph`) | Not implemented | No — extension has it commented out |
| Inline script expansion | ✅ Implemented in InfoService | No |
| Per-game validation | Stellaris events, EU4 stubs | No |
| Error codes CW### | Catalog defined, not wired to diagnostics yet | No |
| Custom severity (`warning_only`) | Parsed, not used in validator | No |
| `type_per_file` path lookup | Parsed, not implemented | No |

---

## 7. Cross-compilation Status

Local cross-compilation for Windows/macOS failed due to missing toolchains (MinGW, macOS SDK, MSVC). 

**Recommended**: Use GitHub Actions CI with:
- `ubuntu-latest` → `cargo build --release` for Linux
- `windows-latest` → `cargo build --release` for Windows (Visual Studio build tools available)
- `macos-latest` → `cargo build --release` for macOS

Then upload artifacts to GitHub Releases.

---

## 8. Summary for Another Agent

**If you're picking this up:**

1. ✅ **Binary is ready** at `bin/server/linux-x64/CWTools Server`
2. ⚠️ **Rules loading** is the main gap — Rust server needs `.cwt` files loaded from `rulesCache`
3. ⚠️ **Custom notifications** need stub handlers in Rust server
4. 🔧 **Extension code** — check what format `repoPath` downloads, ensure `.cwt` files end up in `rulesCache`
5. 🧪 **Test** — Open any game mod `.txt` file and verify diagnostics appear

**Files to modify in cwtools repo:**
- `cwtools-rs/crates/lsp/src/main.rs` — Add ruleset loading + custom notification stubs

**Files to inspect in cwtools-vscode repo:**
- `client/extension/extension.ts` — Verify server spawn path and init options
- Check how `repoPath` downloads rules and where they land relative to `rulesCache`

