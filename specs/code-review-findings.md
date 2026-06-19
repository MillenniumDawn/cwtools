# cwtools-rs Code Review Findings

Workspace: 15 crates under `cwtools-rs/crates/`, ~38k lines, edition 2024.
Six parallel reviews covered every crate in full. Findings deduplicated and ranked.

---

## Verification & PR mapping

Every concrete finding below was re-checked against the current tree (2026-06-19) by nine
read-only verification passes. The review holds up: 0 fully wrong, 3 stale, ~15 partial, the
rest valid. Findings ship as three iterative PRs, mapped to the tiers:

- **1.6** (shipped) = Tier 1 (correctness/bugs) + Tier 2 (all hot-path perf). Done and removed from this doc; see `CHANGELOG.md`.
- **1.7** = Tier 3 (over-engineering / design).
- **1.8** = Tier 4 (maintainability / large functions / duplication).

Legend used inline: **[STALE]** already fixed, removed from scope · **[PARTIAL]** real but the
citation or fix is imprecise · unmarked = valid. Numbers are never reused; downstream notes
reference them.

**Stale (already fixed by later rewrites, dropped from scope):**
- **#5** — config is now statement-scoped read-clone-drop, never co-held with the rules lock.
- **#80** — `make_prepared` is now a zero-alloc borrow wrapper; the scope-registry/enum tables it warned about are already cached once.
- **#131** — the `MAX_INLINE_DEPTH` comment now cites F# parity (the fix the finding asked for).

**Reclassified:**
- **#107** moves to **1.7**. `ValueClause` is *not* dead — the cache (`convert.rs`) and `inline_expansion.rs` produce it and ~15 sites consume it. It is a cache-format-aware refactor, not a deletion.
- **#125** is worse than written and stays in **1.7**: the *entire* `StringMetadata` subsystem (all six flags + compute + getter) is dead. Removing it subsumes **#1** (the misnamed `starts_with_amp` lives inside it), so #1 is not a standalone 1.6 fix.
- **#207** moves to **1.7**. There are zero `colour`/`color` tokens in the HOI4 config, so `build_colour_rules` never runs for Millennium Dawn — the permissive `_ =>` arm has no corpus impact and is a trusted-input design nit, not a bug.

**Partial (real, but citation/fix imprecise — corrected here):**
- **#35**, **#38**, **#175**, **#177**, **#194**, **#208** (the real twins are `child_scalar`/`child_scalars`), **#225** (O(files×langs), not langs²), **#239** (plain `assert_eq!`, runs in release too).
- **#57** — alloc is real, but the hot caller is `validation/src/scope.rs`, not `change_scope`.
- **#99** — the root_children-per-typedef walk is gone; the O(types×root_rules) scan remains in `collect_defined_variables_from_rules`.
- **#122** — `FileError::Pattern` is declared but never constructed (partly dead).
- **#134** — the scan caps moved to `workspace_cache.rs`.
- **#150** — the `dynamic_values.rs:65` citation is stale; only the four `index/lib.rs` sites are valid.
- **#163** — the two emission paths are near-parallel, not identical (the project path emits CW254, the single-file path does not). Risk Y: a shared helper must preserve that.
- **#183** — the CK3 mislabel is real at `constants.rs:373-377`; the cited lines 642/781 are stale stub headers.

**File relocations (citations drifted; findings still hold):** `per_game/*` and `loc_field.rs`
moved to `crates/validation/src/`; `vanilla_cache.rs` / `write_cache` to `crates/index/src/`;
the LSP scan caps to `workspace_cache.rs`.

---

## Tier 3 — Over-Engineering / Design  — **PR 1.7**

### 107. Dead `ValueClause` arena slab  — **[PARTIAL → PR 1.7]**
> Not dead: the cache (`convert.rs`) and `inline_expansion.rs` produce `ValueClause` and ~15 sites consume it. This is a cache-format-aware refactor, not a deletion. Moved to 1.7.
- **File:** `parser/ast.rs:53,73,89,103,135-139`
- `ValueClause`/`ValueClauseIdx`/`Child::ValueClause`/`Arena::value_clauses`/`Arena::push_value_clause` are never produced by the parser.
- Doc at `:67-68` says "there is ONE clause representation... the parser produces nothing else."
- Speculative generality — whole arena slab + index type + enum variant + push method reserved for nobody.

### 108. `Value::Clause(Vec<Child>)` inflates every Value
- **File:** `parser/ast.rs:63`
- Stores children inline while `Child::Leaf(LeafIdx)` indexes the arena.
- The `Clause` variant carries a 24-byte `Vec` inline, inflating every `Value` (including `Int(i64)`, `Bool(bool)`).
- Either arena-allocate clauses via the unused `ValueClause` machinery or drop the slab.

### 109. Two identical newtypes `Scope`/`ScopeId`
- **Files:** `scope.rs:5` `Scope(pub u32)`, `scope_engine.rs:9` `ScopeId(pub u32)`
- Manual `.0` unwraps at every bridge (`scope_registry.rs:124, :134`).
- Doc justification is "matching the original public API" — weak; a single type removes the friction.

### 110. `ScopeLink.is_scope_change` redundant with `target.is_some()`
- **File:** `scope_engine.rs:91-101`
- `is_scope_change` is `target.is_some()` by construction (`scope_registry.rs:213`, `scope_engine.rs:514`).
- Two fields encoding one fact.

### 111. `current() -> Option<ScopeId>` for a never-empty stack
- **File:** `scope_engine.rs:133-135`
- Every caller does `.copied().unwrap_or(self.root)`.
- The `Option` is defensive against an unreachable empty-stack state; the type lies about failure mode.
- **Fix:** Return `ScopeId` directly.

### 112. `apply_replace_scope` defensive else branch
- **File:** `scope_engine.rs:218-224`
- `if let Some(last) = self.scopes.last_mut() { *last = t; } else { self.scopes.push(t); }` — else branch unreachable.

### 113. Four scope-def types
- **File:** `scope_registry.rs:13-43`
- `ScopeInput`, `ScopeDefOwned`, `ScopeDef`, plus `Scope`/`ScopeId`.
- Three-layer mirroring is borderline but defensible given const/owned/config-source split.

### 114. `reference_details: Option<(bool, String)>` — bool-as-variant-tag
- **File:** `rules/rules_types.rs:499-514`
- An enum `{Outgoing(String), Incoming(String)}` would self-document.

### 115. `SkipRootKey::MultipleKeys(Vec<String>, bool)` — bool flag
- **File:** `rules/rules_types.rs:363`
- The `bool` is `should_match` (`==` vs `<>`). An enum `MatchKind` would self-document.

### 116. `PatternKind` derives `Clone` not `Copy`
- **File:** `rules/rules_types.rs:67-76`
- Fieldless enum. Adding `Copy` removes the clone at `:134`.

### 117. `&mut bool` threaded through recursion
- **File:** `rules/post_process.rs:72,116`
- Classic out-parameter anti-pattern. Callers at `:57-59` pass `&mut true` to force-rewrite.
- **Fix:** Return a small enum or `Option<NewRule>`.

### 118. `InfoService` redundant HashSet+count pairs
- **File:** `info/lib.rs:160-168`
- `all_event_targets` + `event_target_counts`, `all_variables` + `variable_counts`, `all_inline_scripts` + `inline_script_counts`.
- The `HashSet`s are redundant with the counts (`count > 0` = exists). Halve the fields.

### 119. Two `Game` enums
- **Files:** `localization/commands.rs:85-97`, `cwtools_game::constants::Game`
- Localization one carries `Generic`/`Custom` variants. Source of confusion.

### 120. Three parallel match functions for `LocErrorKind`
- **File:** `localization/pipeline.rs:38-85`
- `loc_error_message`/`loc_error_code`/`loc_error_severity` — adding a variant requires editing three places.
- **Fix:** A single match returning `(code, severity, message_fn)` or a method on `LocErrorKind`.

### 121. Two bool params that are a 4-state enum
- **File:** `loc_field.rs`
- `validate_localisation_field` takes `synced: bool, is_inline: bool`.
- **Fix:** `enum LocFieldKind { Inline, Synced, Unsynced }`.

### 122. Stringly-typed errors
- **Files:** `cache/io.rs:7-17` `CacheError`, `file_manager.rs:136-139` `FileError`
- `Serialize(String)`/`Deserialize(String)`/`Compression(String)` and `Parse(String)`/`Pattern(String)`.
- **Fix:** Wrap underlying errors structurally.

### 123. `string_token_to_str` returns String, named `_to_str`
- **File:** `cache/convert.rs:61-66`
- Misleading; `_to_string` or `_to_owned` matches the return type.

### 124. `StringTable::Clone` shares state
- **File:** `string_table.rs:74-80`
- `Arc::clone(&self.inner)` — "cloning" shares all state. Surprising for `Clone`.
- **Fix:** `fn share(&self) -> StringTable` or `Arc<StringTable>` would communicate aliasing better.

### 125. `StringMetadata` — six bool flags, usage audit needed  — **[worse than written → PR 1.7]**
> The entire `StringMetadata` subsystem (all six flags + compute + getter) is dead. Removing it subsumes **#1** (the misnamed `starts_with_amp` lives inside it).
- **File:** `string_table.rs:28-36`
- Only `starts_with_amp` and a couple others documented as used.
- If some flags are unused, eager computation is wasted work per new string.

### 126. `StringId` overflow collision with NULL
- **File:** `string_table.rs:9`
- `NULL = u32::MAX`. `next_id += 1` at `:153/:170` would silently wrap past `u32::MAX` after 4 billion interns, colliding with `NULL`.
- **Fix:** Debug assert or checked add.

### 127. `Option<(bool, String)>` in `Options` (see #114)
- **File:** `rules/rules_types.rs:499-514`

### 128. `ruleset: &mut RuleSet` never used in `children_to_rules`
- **File:** `rules/rules_converter/mod.rs:191`
- `#[allow(clippy::only_used_in_recursion)]` admits it's only forwarded, never used.
- **Fix:** Remove it (and from `leaf_to_rule`, `process_root_leaf`).

### 129. Dead code in `clone_and_expand_child_r`
- **File:** `info/inline_expansion.rs:259-294`
- `Child::Leaf` arm is dead code — `clone_and_expand_children` handles `Child::Leaf` directly (`:206-248`) and only calls `clone_and_expand_child_r` for `other` children (`:250-253`).

### 130. Dead parameters
- **File:** `info/inline_expansion.rs:37,88`
- `load_directory` takes `_table: &mut StringTable` (unused). `expand_inline_script` takes `_leaf_idx: u32` (unused).

### 131. `MAX_INLINE_DEPTH = 5` — magic number  — **[STALE]**
> Fixed: the comment now cites F# parity. Dropped from scope.
- **File:** `info/inline_expansion.rs:13`
- No comment on why 5. F# parity? Cite the source.

### 132. Dead test file
- **File:** `info/debug_test.rs`
- Just `println!`s the AST. Delete or move to scratch.

### 133. Hardcoded build banner
- **File:** `lsp/config.rs:74`
- `"★ CWTools RUST LSP server — build: two-pass-index + modifier-keys (rust-2025-06b)"` — needs manual bumping. Will rot.

### 134. Magic numbers throughout LSP
- **Files:** `lsp/main.rs:267` `DEBOUNCE_MS = 250`, `lsp/completion.rs:133` `FALLBACK_CAP = 2000`, `lsp/scan.rs:197,200,203` caps `50_000`/`2 GiB`/`0.8`, `lsp/validate.rs:41` `MAX_FILE_ERRORS = 100`

### 135. Lock order documented in prose only
- **File:** `lsp/main.rs:137-143`
- 17-field `DocumentState` with 11 locks; a future edit can silently ABBA. `validate.rs:637` already bends the contract.
- **Fix:** Debug-assertion or a wrapper that tracks acquisition order.

### 136. `SeqCst` vs `Relaxed` inconsistency
- **Files:** `main.rs:675/705`, `validate.rs:512`, `scan.rs:114`, `config.rs:419/455`
- `edit_generation` uses `SeqCst`, `index_ready`/`hover_*` use `Relaxed`, `vanilla_merged` uses `SeqCst`. No consistent policy.

### 137. `tokio::task::yield_now().await` magic 50
- **File:** `lsp/scan.rs:461-463`
- `if i % 50 == 49 { tokio::task::yield_now().await; }` — undocumented.

### 138. `prepare_rename` TODO
- **File:** `lsp/navigation.rs:487-496`
- Range starts at `pos.character`, not the true token start. Known incorrect for mid-token cursors.

### 139. Rename error message is a run-on sentence
- **File:** `lsp/navigation.rs:560-575`

### 140. `Url::parse("file:///unknown").unwrap()`
- **File:** `lsp/navigation.rs:438`
- `unwrap` on known-good literal fine but clippy flags it.

### 141. `determine_file_types` fallback uses `path.contains`
- **File:** `lsp/config.rs:491-505`
- Multiple `path.contains("/events/")` etc. Per command, fine.

### 142. `clearAllCaches` long blocking command
- **File:** `lsp/config.rs:443-453`
- `block_in_place` → `remove_dir_all` → then `validate_entire_workspace().await`. Explicit user command, acceptable.

### 143. `client.log_message` on every validate call
- **File:** `lsp/validate.rs:682`
- Per-keystroke client round-trip + `format!` allocation.

### 144. `runtime.build().unwrap()` in main
- **File:** `lsp/main.rs:836`
- Panics in non-test `main`. `expect` with context better.

### 145. `json_escape` hand-rolled instead of serde_json
- **File:** `cli/main.rs:148-162`
- Workspace has `serde_json` as a dep (lsp uses it); cli doesn't list it.
- Reinventing JSON serialization is a maintenance liability.

### 146. `std::collections::HashMap` vs `FxHashMap` inconsistency
- **File:** `per_game/common.rs:5`
- Uses `std::collections::HashMap` while rest of crate uses `rustc_hash::FxHashMap`.

### 147. `eprintln!` vs `tracing` inconsistency
- **Files:** `file_manager.rs:306,347`, `inline_expansion.rs:59-63`, `driver/lib.rs:138-141,161`
- Uses `eprintln!` in libraries; index uses `tracing`. `file_manager`'s Cargo.toml has no `tracing` dep.

### 148. `driver/lib.rs:138-141,161` — `eprintln!` in a library
- Driver should return errors, not print. CLI coupling leak.

### 149. `Deref` pyramid
- **File:** `driver/lib.rs:419-424`
- `impl Deref for SessionWithFiles` — a method or explicit field would be clearer.

### 150. `unwrap_or_default()` on string lookups
- **Files:** `index/lib.rs:24, 760, 825, 868`, `dynamic_values.rs:65`
- For a valid `StringId` from a parsed AST, `get_string` should never return `None`.
- Silently substitutes `""` for corrupt/invalid id, masking bugs.

---

## Tier 4 — Maintainability (large functions / duplication)  — **PR 1.8**

### Large functions

#### 151. `parse_value` — 305 lines
- **File:** `parser.rs:201-506`
- Handles quoted strings (two modes), rgb, hsv, yes, no, metaprogramming, int, float, fallback string.
- Quoted-string branch alone 100 lines with multi-paragraph heuristic comment (`:256-270`).
- **Fix:** Split into `parse_quoted_value`, `parse_bool_keyword`, `parse_number_or_string`.

#### 152. `validate_children` — 430 lines
- **File:** `rule_core.rs:447-880`
- Does first-pass counting, second-pass validation, and cardinality enforcement in one function.
- **Fix:** Three phases would read better as three functions sharing count maps.

#### 153. `field_from_string` — 336 lines
- **File:** `rules/rules_converter/field_parser.rs:8-344`
- Single function handling ~30 field types via linear `starts_with` + `ends_with(']')` probes.
- **Fix:** Group by prefix into helpers.

#### 154. `process_type_node` — 185 lines
- **File:** `rules/rules_converter/types.rs:29-224`
- `skip_root_key` branch alone (`:133-187`) is 54 lines.
- **Fix:** Extract `parse_skip_root_key_block`/`parse_skip_root_key_leaf`.

#### 155. `index_child_heuristic` — 117 lines
- **File:** `info/lib.rs:531-648`
- 8 separate `if` branches off `key`.
- **Fix:** Split into `record_top_level_key`, `record_type_definition`, etc.

#### 156. `parse_loc_text` — 207 lines
- **File:** `localization/yaml_parser.rs:169-376`
- Entry-parsing loop (`:215-333`) is 118 lines.
- **Fix:** Extract `parse_entry(line, line_idx, stream_name) -> LocEntry`.

#### 157. `completions_from_rules` — 260 lines
- **File:** `lsp/completion.rs:218-478`
- Many arms pushing CompletionItems. Alias branch (`:386-442`) and typed-key branch (`:331-353`) share logic.

#### 158. `resolve_single_with_lower` — 125 lines
- **File:** `scope_engine.rs:339-463`
- 75-line `match lower { ... }` arm block (`:346-422`).
- **Fix:** Lookup table `(name, action)` or helper.

### Duplicated logic

#### 159. Duplicated scope tables across games
- **Files:** `constants.rs:372-778`, `scope_engine.rs:526-1782`
- ~400 lines of CK3/VIC2/IR scope tables differing only by ID offset (500/600/700).
- ~1300 lines of static link tables. Eight near-identical `load_<game>_links` functions.
- **Fix:** Macro `scope_links! { stellaris { ... } }` collapses hundreds of lines.

#### 160. Duplicated enum-threshold heuristic
- **Files:** `common.rs:200`, `rule_core.rs:985`, `rule_core.rs:1071`
- `def.values.len() > 5` restated three times.
- **Fix:** Hoist to `fn enum_is_authoritative(def) -> bool`.

#### 161. Duplicated dispatch logic
- **Files:** `lib.rs:306` (`validate_prepared`), `position.rs:83` (`rules_at_pos`)
- `type_per_file` branch, exact-match branch, path-fallback branch. Two copies of the dispatch tree.
- `position.rs` version even has `best_content_type` fallback the validator lacks. Drift risk.

#### 162. Duplicated chain validation logic
- **File:** `localization/scope_validation.rs:151-232 vs :239-318`
- `validate_command_string` and `validate_jomini_chain` near-parallel. ~180 lines duplicated.
- **Fix:** Generic `validate_chain<I: IntoIterator<Item = Segment>>`.

#### 163. Duplicated diagnostic emission
- **File:** `localization/pipeline.rs:201-243 vs :265-290`
- Same CW254/255/256/257/001/225/234/259/268/275 codes, same messages.
- **Fix:** Shared `build_diagnostics(file, union, extra_valid_refs) -> Vec<LocDiagnostic>`.

#### 164. Duplicated block parsing
- **File:** `rules/rules_converter/types.rs:280-332 vs 334-380`
- `parse_localisation_block` and `parse_modifiers_block` near-identical.
- **Fix:** Generic helper or macro.

#### 165. Duplicated 3-way root-rule iteration
- **File:** `rules/post_process.rs:236-249, 428-442, 489-503`
- `replace_value_marker_fields`/`replace_ignore_marker_fields`/`replace_colour_field`/`replace_single_aliases` all repeat.
- **Fix:** `for_each_root_rule_mut(ruleset, |rule| …)` helper.

#### 166. Duplicated `unquote`
- **Files:** `index/lib.rs:15-19`, `index/dynamic_values.rs:151-157`
- Different semantics (lib.rs handles single `"` edge case; dynamic_values requires `len >= 2`).
- **Fix:** Consolidate.

#### 167. Duplicated `ZSTD_LEVEL = 3`
- **Files:** `cache/io.rs:19`, `vanilla_cache.rs:217`
- **Fix:** Promote to shared constant.

#### 168. Duplicated exclude-dir + exclude-pattern + glob filtering
- **File:** `file_manager.rs` (`collect_paths` and `walk_workspace_inner`)

#### 169. Duplicated refcount decrement-and-remove
- **Files:** `index/lib.rs` (VarIndex, TypeIndex name_counts, TypeIndex instance_sets), `dynamic_values.rs` (NamedValueIndex)
- `*c -= 1; if *c == 0 { remove }` pattern appears 4×.
- **Fix:** `fn dec_ref` helper.

#### 170. Duplicated brace-list parsing
- **File:** `rules/rules_converter/types.rs:251-255, 268-272`
- `parse_type_key_filter_from_comments` and `parse_graph_related_types_from_comments`.
- **Fix:** `parse_brace_list(rhs) -> impl Iterator<Item=&str>` helper.

#### 171. Duplicated `has_directive`/`find_directive` prefix logic
- **File:** `rules/rules_converter/comment_directives.rs:29-49 vs 59-79`

#### 172. Duplicated extension lists
- **Files:** `lsp/scan.rs:158`, `driver/lib.rs:606`, `file_manager.rs:119-130`, `file_manager.rs:207-214`
- Four sources of truth for "what's a script file."

#### 173. Duplicated `children_to_rules`/`leaf_to_rule` dead param threading
- **File:** `rules/rules_converter/mod.rs:191`
- `ruleset: &mut RuleSet` never used but threaded through entire recursive descent.

### Maintainability notes

#### 174. Parse error filename field always empty
- **File:** `parser/ast.rs:5-6` `ParseError::Pos(String, ...)`
- Every call site passes `"".to_string()` (`parser.rs:302, 526, 576, 683`).
- **Fix:** Pass a filename/path through `parse_string` or make the field `Option<String>`.

#### 175. `parse_clause` and `parse` duplicate loop structure
- **File:** `parser.rs:508-537 vs :695-706`
- `parse` doesn't handle unclosed-brace error; `parse_clause` does.
- **Fix:** Shared `parse_children(&mut self, terminator: char) -> Vec<Child>`.

#### 176. F# source-line references
- **Files:** `parser.rs:127,421,649-652`, `scope_engine.rs` comments
- If F# engine is retired, these are dead weight.

#### 177. `parse_real_file` test depends on external path
- **File:** `parser.rs:769-779`
- `../../testfiles/performancetest2/...` — fragile across checkouts/worktrees.
- **Fix:** Use `include_str!` of a small fixture or skip if path doesn't exist.

#### 178. Interned text stores quotes + has `quoted` flag
- **Files:** `parser.rs:130/156, 217/311`, `string_table.rs:23`
- `String::from('"')` then `push('"')` stores the quoted form including outer `"`.
- The `quoted: bool` flag on `StringTokens` partially addresses this.
- Redundant signal — pick one.

#### 179. `sc()` — 2-letter function name
- **File:** `scope_engine.rs:510-517`
- Too short for greppability.

#### 180. `load_scope_links` is `pub` but only used internally
- **File:** `scope_engine.rs:493-507`
- Only used by `ScopeRegistry::from_hardcoded` (`scope_registry.rs:138`).

#### 181. Unknown link input-scope silently becomes SCOPE_ANY
- **File:** `scope_registry.rs:208`
- `reg.id_of(n).unwrap_or(SCOPE_ANY)` masks typo'd scope names in `links.cwt`.
- **Fix:** Warn at load time.

#### 182. Stale "deleted" comment
- **File:** `scope_engine.rs:1784`
- "validate_scope_field deleted: no callers and the implementation was incorrect."

#### 183. Comment mislabels scope tables
- **File:** `constants.rs:373-377, 642, 781`
- CK3 comment lists Value/Bool/Flag/Color which are not CK3 scopes; appears copy-pasted from IR/VIC2.

#### 184. Mixed-case alias entry
- **File:** `constants.rs:155`
- Stellaris `Federation` aliases include `"Alliance"` while every other alias list is lowercase. Dead weight.

#### 185. `game_to_engine` silent fallback to HOI4
- **File:** `localization/scope_validation.rs:135`
- `Generic | Custom => EngineGame::Hoi4` — `tracing::warn!` or `Option<EngineGame>` would make the fallback visible.

#### 186. `had_lenient_intermediate` bool threaded through Jomini chain
- **File:** `localization/scope_validation.rs:259,268,282,313`
- A "poisoned" flag; enum `ChainState { Clean, Lenient }` would be clearer.

#### 187. `LocSeverity` duplicates `ErrorSeverity`
- **File:** `localization/pipeline.rs:17-23`
- Comment says "without taking a dependency on it." Re-export would avoid duplication.

#### 188. `is_loc_value_char` — 20-arm `||` chain
- **File:** `localization/yaml_parser.rs:383-428`
- A `match` on `(u >> 8)` ranges or lookup table cleaner.

#### 189. `lang_from_filename` — 12 sequential `contains` checks
- **File:** `localization/yaml_parser.rs:72-101`
- Called once per file, not hot, but verbose.

#### 190. `find_invalid_loc_char` + `parse_loc_elements` double-scan desc
- **File:** `localization/yaml_parser.rs:434-441`
- `desc` scanned at least twice.
- **Fix:** Fold invalid-char check into loc-element parser.

#### 191. `ScopeResult::ValueFound` mid-chain divergence buried
- **File:** `localization/scope_validation.rs:194-197`
- "Treat as terminal (lenient — F# would error but we accept)." Good documentation but buried mid-match-arm.

#### 192. `loc_error_message`/`loc_error_code`/`loc_error_severity` three parallel matches
- **File:** `localization/pipeline.rs:38-85`
- Adding a variant requires editing three places.

#### 193. "prefer English" logic match arm reads confusingly
- **File:** `localization/loc_index.rs:63-68`
- `match entries.get(&lower) { Some(_) if lang != Lang::English => {}, _ => { entries.insert(...) } }`

#### 194. `looks_like_compound_ref` heuristic is subtle
- **File:** `localization/validation.rs:98-104`
- `first_space != last_space` checks for multiple spaces.

#### 195. `CachedFile` derives `Clone`
- **File:** `cache/cache_format.rs:4`
- Full deep clone of a cached AST unlikely to be needed.

#### 196. `type_rules_idx` first-wins vs `type_by_name`/`enum_by_name` last-wins
- **File:** `rules/rules_types.rs:291,295,300-302`
- Inconsistent — the comment at `:300` explains it, but asymmetry is a footgun.

#### 197. `RuleSet` has 17 fields
- **File:** `rules/rules_types.rs:3-58`
- Reindex-built indexes (`37-57`) interleaved with source data (`3-33`).
- **Fix:** Split into `RuleSetData` (loaded) + `RuleSetIndex` (built).

#### 198. Three different collection shapes for "set of names"
- **File:** `rules/rules_types.rs:12-21`
- `values: HashMap<String, Vec<String>>` but `modifiers: Vec<String>` and `scope_links: HashSet<String>`.

#### 199. `inline_single_alias_rule` duplicated resolution logic
- **File:** `rules/post_process.rs:78-98 vs :142-167`
- Two paths handle `LeafRule(SingleAliasField)`. Maintainability hazard.

#### 200. Cycle detection via count comparison
- **File:** `rules/post_process.rs:200-214`
- `unresolved >= prev_unresolved` is clever but fragile.
- **Fix:** A visited-set or max-total-nodes bound would be more robust.

#### 201. Magic numbers in `expand_colour_rule`
- **File:** `rules/post_process.rs:304-313`
- `-256.0`, `256.0`, `3`. Named consts would help.

#### 202. Dead `SingleAliasClauseField` comment
- **File:** `rules/post_process.rs:99`

#### 203. `SkipRootKey` promotion logic intricate
- **File:** `rules/rules_converter/types.rs:160-185`
- Mutation-while-iterating (`drain(..)`) is subtle. A fold/reduce clearer.

#### 204. `should_be_used` sets `should_be_referenced`
- **File:** `rules/rules_converter/types.rs:130-132`
- Naming mismatch between the `.cwt` directive and the field.

#### 205. Magic byte-offset slicing in `field_from_string`
- **File:** `rules/rules_converter/field_parser.rs:118,137,153`
- `&trimmed[9..trimmed.len() - 1]` — hardcoded lengths per prefix.
- **Fix:** `strip_prefix`/`strip_suffix` safer.

#### 206. `scope_group[…]` discards its argument
- **File:** `rules/rules_converter/field_parser.rs:204-208`
- `let _group = …; return ScopeField(vec!["any".to_string()]);`

#### 207. `build_colour_rules` silently permissive fallback  — **[→ PR 1.7]**
> Zero `colour`/`color` tokens in the HOI4 config, so `build_colour_rules` never runs for Millennium Dawn. No corpus impact; trusted-input design nit, not a bug. Moved to 1.7.
- **File:** `rules/rules_converter/mod.rs:292-353`
- `_ => vec![both]` at `:323` emits both rgb and hsv rules for unknown formats.

#### 208. `child_clause_values` + `child_scalars` near-duplicate
- **File:** `rules/rules_converter/scopes_links.rs:59-76 vs 79-99`

#### 209. `filtered_children` clones 49 to skip 1
- **File:** `rules/rules_converter/subtypes.rs:89-106`
- **Fix:** Pass a filter predicate to `children_to_rules` or drain-skip in place.

#### 210. `extract_description_from_comments` clones first line
- **File:** `rules/rules_converter/comment_directives.rs:13-22`
- `desc_lines[0].clone()` when iterator `next()` would do.

#### 211. `build_name_tree` recurses without depth guard
- **File:** `rules/rules_converter/enums.rs:161-202`
- Low risk (cwt is trusted).

#### 212. `walk_child` recurses without depth guard
- **File:** `rules/config_validation.rs:40-97`
- Low risk (trusted input).

#### 213. `parse_mod_descriptor` hand-parses instead of using existing parser
- **File:** `file_manager.rs:445-475`
- `replace_path` lines with trailing comments, inline `{...}`, or `=` inside quotes would break.
- Latent bug for non-trivial formatting.

#### 214. `classify_directory` `MultipleMod` check is verbose
- **File:** `file_manager.rs:737-789`
- 20-line nested `read_dir().ok().map(|mut entries| entries.any(...))` returning `Some(())`/`None` to feed `.is_some()`.
- **Fix:** Plain `for` loop with early return.

#### 215. `search_config_for` hardcodes ~50-entry array
- **File:** `driver/lib.rs:547-602`
- `known_script_folders` array inline. Duplicated extension list at `:606`.

#### 216. `Deref` pyramid for `SessionWithFiles`
- **File:** `driver/lib.rs:419-424`

#### 217. `is_loc_file` only checks `.yml`
- **File:** `lsp/paths.rs:80`
- Completion (`.yaml`) and validate (`.yaml`, `.csv`) diverge.

#### 218. `byte slicing `&key[1..key.len() - 1]` — safe only because `<`/`>` are ASCII
- **File:** `lsp/symbols.rs:70,88`
- **Fix:** Document the invariant.

#### 219. `clear_document` O(locs) per name
- **File:** `lsp/symbols.rs:122-141`
- Fine for typical sizes.

#### 220. `symbol_impl` allocates per instance
- **File:** `lsp/navigation.rs:425-461`
- `inst.name.to_lowercase().contains(&query)` per instance.

#### 221. `index_child` recurses with no depth bound
- **File:** `lsp/symbols.rs:60-62`
- AST clause depth is file-bounded, but no guard.

#### 222. `append_localisation` allocates per translation per hover
- **File:** `lsp/hover.rs:80,116,249-256`
- `format!` per language; `key.to_lowercase()` and `format!("{k}_desc")` even when no loc entry exists.

#### 223. `Position` derives `PartialEq` on `Arc<str>`
- **File:** `localization/commands.rs:243`
- Comparing compares full str (O(path_len) per comparison). If positions compared in hot loops, expensive.

#### 224. `csv_parser` collects then indexes, clones twice
- **File:** `localization/csv_parser.rs:59,64,81`

#### 225. `languages()` O(files × langs²)
- **File:** `localization/service.rs:125-135`
- Called rarely.

#### 226. `ruleset_loader` sequential parsing
- **File:** `rules/ruleset_loader.rs:99-136`
- Localization crate uses `rayon`; rules crate doesn't.

#### 227. `asts` Vec retains every parsed AST
- **File:** `rules/ruleset_loader.rs:97`
- For large cwt set holds all ASTs in memory simultaneously.

#### 228. `validate_stellaris_loc` dead code
- **File:** `per_game/stellaris.rs:410`
- "Not yet wired into run_game_validators."

#### 229. `child_has_always_no`/`child_is_bool` naming
- **File:** `per_game/stellaris.rs:296,305`
- `expected` bool param reads as a flag.
- **Fix:** Rename to `child_is_bool_value(child, ast, table, expected: bool)`.

#### 230. `Keywords` interning doc
- **File:** `per_game/structural.rs:34`
- Documents 25% win. Good example.

#### 231. `push` helper takes `msg: String`
- **File:** `per_game/structural.rs:91`
- Forces caller to allocate.
- **Fix:** `impl Into<String>` or `&str` + format internally.

#### 232. `walk` allocates `key_string` per block
- **File:** `per_game/hoi4.rs:51`

#### 233. `child_is_bool` rename suggestion
- **File:** `per_game/stellaris.rs:296,305`

#### 234. `VarIndex::merge` clones key into entry API
- **File:** `index/lib.rs:187-191`
- Could use `entry(name.as_str())` if `names` were `HashMap<String, _>` keyed by ref.

#### 235. `FileIndex::walk` synchronous recursive `read_dir`
- **File:** `index/lib.rs:72-86`
- I/O-bound for very large installs; parallelism happens later.

#### 236. `comment_to_cached` clones `c.text`
- **File:** `cache/convert.rs:184-193`
- Clone unavoidable.

#### 237. Mirror-image conversion pairs
- **File:** `cache/convert.rs:202-247`
- `value_to_cached`/`cached_value_to_value`, `op_to_cached`/`cached_op_to_op`, `children_to_cached`/`children_from_cached`.
- **Fix:** `derive`-based approach or single `From<>` impl pair.

#### 238. `CachedValue::Clause(Vec<CachedChild>)` vs Arena flat layout
- **File:** `cache/cache_format.rs:76`
- Mismatch intentional (cache self-contained, arena index-friendly) but non-trivial conversion cost.

#### 239. `assert_eq!` debug asserts per cache load
- **File:** `cache/convert.rs:40,43,48,52`
- Worth a comment explaining the invariant.

#### 240. Two independent version constants
- **File:** `cache/io.rs:32` `FORMAT_VERSION = 2`, `vanilla_cache.rs:39` `CACHE_VERSION = 5`
- Track different formats (`.cwb` vs `.cwv`). Magic bytes differ. Correct.

#### 241. `#[repr(u8)]` on cached types
- **File:** `cache/cache_format.rs:16,69,81`
- Good for rkyv tag compactness.

---

## Cross-Cutting Themes

### A. Per-token / per-leaf `String` allocation on hot paths
The dominant perf theme across ALL crates. Specific instances:
- `leaf_value_string` (`index/lib.rs:22`), `FileIndex::contains`, `check_path_dir`, `VarIndex::normalize`, `starts_with_matches`, `scan_children_for_varset`, `collect_value_sets_in`, `string_table::get_string`, `change_scope`, `is_subscope_or_eq`, `validate_children` `key_counts`/`key_card`, `matching_candidates`, `field_matches_key`/`alias_overloads`, `loc_completions`, `scope_completion_names`, `substitute_params`, `parse_loc_elements`, `validate_command_string`, etc.

**Fix:** Systematic shift to `with_string` / `&str` threading, plus `HashMap<&str, _>` (or `HashMap<StringId, _>`) for lookups.

### B. `to_lowercase()` / `to_ascii_lowercase()` everywhere
20+ lowercase allocations across hot paths, many recomputing the same key's lowercase in successive functions (`field_matches_key` then `alias_overloads` then `validate_alias_usage`; `change_scope` then `id_of`; etc.).

**Fix:** A `LowerKey` newtype cached at entry points would eliminate most.

### C. `modifier_keys: HashSet<String>` cloned heavily
`scan.rs:389,619`, `validate.rs:576`, `driver.rs:364`.

**Fix:** Store as `Arc<HashSet<String>>` and clone the Arc.

### D. `workspace_uri: String` cloned on nearly every handler
`completion.rs:30`, `navigation.rs:65/189/234/476/506/537`, `validate.rs:616`, `hover.rs:38`, `config.rs:468`, `scan.rs:139/482`.

**Fix:** Make it `Arc<str>`.

### E. `make_prepared` rebuilt per request
Hover, goto, references, rename, completion all rebuild `Prepared` on every call.

**Fix:** Cache for duration of a request batch or cache scope registry's per-game derived tables.

### F. Duplicated enum-threshold heuristic
`def.values.len() > 5` at `common.rs:200`, `rule_core.rs:985`, `rule_core.rs:1071`.

**Fix:** Hoist to `fn enum_is_authoritative(def) -> bool`.

### G. Duplicated dispatch logic
Between `validate_prepared` (`lib.rs:306`) and `rules_at_pos` (`position.rs:83`). Two copies of the dispatch tree; drift risk.

### H. `eprintln!` vs `tracing` inconsistency
`file_manager.rs:306,347`, `inline_expansion.rs:59-63`, `driver/lib.rs:138-141,161` use `eprintln!` in libraries; index uses `tracing`. `file_manager`'s Cargo.toml has no `tracing` dep.

### I. `unwrap_or_default()` on string lookups
Pervasive (`table.get_string(id).unwrap_or_default()`). For a valid `StringId` from a parsed AST, `get_string` should never return `None`. Masks bugs. Debug-assert-or-return-Empty clearer.

### J. No benchmarks
No `[[bench]]`, no `benches/` dirs in any crate. Claims about per-token cost can't be validated locally.

**Fix:** Add criterion benchmarks for `change_scope`, `validate_children`, `parse_string`, `intern`, `glob_dp`, `index_file_with_path`, `parse_loc_text`.

### K. F# source-line references
Scattered in `parser.rs` and `scope_engine.rs` comments. If F# engine is retired, these are dead weight.

### L. `#[non_exhaustive]` not used
Not on any enum/struct that may grow (`ScopeResult`, `ReferenceHint`, `FileError`, `CacheError`, etc.).

### M. No `#[inline]` on hottest small functions
`change_scope`, `resolve_single_with_lower`, `id_of`, `is_subscope_or_eq`. Workspace has `lto = "fat"` in release profile but no `#[inline]` hints.

### N. Let-chains
Used in several places (`scope_registry.rs:316`, `parser.rs:588-589`). Stable since Rust 1.88. Workspace edition is 2024. Confirmed OK but worth noting MSRV implication.

---

## Suggested Attack Order

1. **Tier 1 correctness** (9 items) — small, isolated, unblock trust in the codebase.
2. **Tier 2 top 5 perf wins**: `glob_dp` single-row DP (#17), `matching_candidates` alloc-free (#14), `change_scope` stack-buffer lowercase (#10), parser byte-offset interning (#13), `ValidationError::code: &'static str` (#16). Highest-leverage and mostly localized.
3. **Cross-cutting infra**: `Arc<HashSet<String>>` for modifier_keys, `Arc<str>` for workspace_uri, `LowerKey` newtype. Unblocks the rest of the perf list.
4. **Tier 3 design cleanup**: drop dead `ValueClause` slab, unify `Scope`/`ScopeId`, fix `ScopeLink` redundancy. Reduces friction for future work.
5. **Tier 4 large functions**: split `parse_value`, `validate_children`, `process_type_node`, `parse_loc_text`. Do alongside the perf passes that touch the same code.
6. **Add criterion benchmarks** before claiming perf wins — without them the impact ranking is speculative.
