# 1.21.0

## Bug Fixes

- Go-to-definition, find-references, and workspace-symbol on a base-game (vanilla) definition now land in the real vanilla file instead of a bogus line in whatever document happened to be open. Vanilla instances were indexed under a synthetic `<vanilla-cache>` key that failed to parse as a URI and silently fell back to the current document; each instance now carries its own source file through both the live-index and cache-load paths. This is the root cause behind the go-to-definition issue the earlier de-dup (see the pre-1.19.0 notes below) only papered over. (cwtools-vscode#62)
- Completion now offers subtype-gated fields inside an entity body before the discriminator that would activate them is typed. Inside a decision, `days_remove` and the other `timed` fields are suggested even though `days_remove` is itself the discriminator; in a character body the role blocks (`corps_commander`, `country_leader`, ...) are suggested. Completion unions every subtype's rules, while validation still resolves the exact matching subtype, so a field belonging to an inactive subtype is still flagged. (cwtools-vscode#89)
- Snippet insertion is hardened so a completion can't land in the document as literal `${...}` text. A raw config literal containing `$` or `}` is now escaped for snippet-text context, and a required nested block prefills as `key = { $1 }` instead of `key = ${1:{ }}`, whose brace closed the placeholder early.

## Notes

- The vanilla cache format is bumped to v7 so a cache written by an older server (which left the per-file path blank) rebuilds on next load. No user action needed; the base game is re-indexed once.

## Developer

- The vanilla index (`vanilla_index`, `VanillaCacheData::per_type`) threads a per-instance source URI. `TypeIndex::merge_with_uris` merges each base-game instance under its real file, and `TypeIndex::remove_files` drops the whole contribution in one index pass on a re-merge (`cacheVanilla` / `clearAllCaches`). `parse_uri` now logs a warning when a string fails to parse instead of silently substituting the fallback (the failure mode that hid this bug).
- Added a black-box goto test (a mod reference to a vanilla definition resolves into the vanilla file, not the request document) and a cache round-trip test (save + load preserves each instance's source file).
- `merged_rules_for_type` gains a `union_all_subtypes` mode threaded through `rules_at_pos(for_completion)` and `enter_entity`; only the completion call site opts in (hover/goto and validation pass `false`). Added position-resolver tests for the decision/character subtype union and its non-leak into the validation descent, a `validate_ast` test that an inactive subtype's field is still unexpected, and a hand-rolled VS Code snippet-grammar checker run over every generated snippet.

# 1.20.0

## Improvements

- The workspace index now refreshes itself in the background. An idle-gated loop re-scans on a client-set interval (`backgroundReindexIntervalMinutes` via initializationOptions or didChangeConfiguration, default 30 minutes, 0 disables), and only after 15s without activity; edits, completion, hover, and navigation all count as activity, so a pass never competes with a request the user is waiting on. Background passes are quiet: no loading bar, but diagnostics still publish, so errors fixed outside the editor clear.
- Rescans prune files deleted on disk from the index, symmetrically with the watched-file delete path (diagnostics cleared; open buffers and vanilla entries untouched).
- A rescan skips the localisation rebuild when the loc file set is unchanged by stat signature (paths, sizes, mtimes).
- Added a `reindexWorkspace` command for an on-demand full rescan. It reports "already in progress" when it loses to a running scan instead of silently doing nothing.
- The workspace scan is guarded against overlapping runs. `clearAllCaches` retries until it wins that guard (bounded at 3 minutes), so a cache purge can no longer skip the rebuild while still reporting success.
- Completion responses are capped and prefiltered by a subsequence match against the typed token, so a large list no longer floods the editor. A resolved list is returned complete only when it is small and comes from a clean, current parse, so the editor filters it locally with no further requests; a filtered, capped, stale, or still-changing list stays incomplete, so the editor re-queries on the next keystroke and a stale popup stops sticking.
- Completion item documentation and detail are resolved lazily, only for the item the editor focuses (`completionItem/resolve`), so responses carry a smaller payload and the popup opens faster.

## Notes

- `CWTOOLS_REINDEX_INTERVAL_SECS` / `CWTOOLS_REINDEX_IDLE_SECS` override the interval and idle window; both are test knobs, the client setting is the real channel.
- The loc stat signature can miss a same-length edit landing in the same second on a coarse-mtime filesystem (FAT/NFS); the next foreground scan picks it up.

## Developer

- Black-box LSP tests cover the quiet background pass (new file picked up, no loading bar), scan re-entrancy, prune-on-rescan, the loc signature, and the clearAllCaches message; `is_idle` unit tests pin the gating math.
- Completion is instrumented with tracing spans and a per-request `cwtools_completion` summary line (total/ast/rules/build timings, item count, strategy, path); an ignored MD-scale `perf_completion_md` baseline test spawns the real server against a full Millennium Dawn checkout and prints cold/warm timings.
- `ParsedDoc.text` is now an `Arc<str>`, so a request that only reads the open buffer clones a refcount bump instead of copying the whole document under the documents lock.
- `did_close` clears the closed document's `completion_generation` entry, which previously leaked across every close/reopen cycle.

# 1.19.0

## Bug Fixes

- Completion no longer dumps every saved variable and event target at a value position that has no context-specific suggestions. A leaf whose rule field type is numeric, a plain scalar, a localisation key, or an empty value set now suggests nothing instead of a wall of unrelated variables. Affects `add_tech_bonus = { name = }`, focus `x`/`y` positions, and `has_country_flag` among others; localisation fields now offer loc keys. (cwtools-vscode#74, cwtools-vscode#75, cwtools-vscode#79)
- `has_dlc` and other short-enum value snippets no longer insert a literal `${0|...|}`. The choice sat on the final `$0` tab stop, which VS Code cannot render, so it appeared as plain text. Choices now use a real tab stop, choice delimiters are escaped, and values containing spaces or colons are quoted (`has_dlc = "Together for Victory"`). (cwtools-vscode#77)
- Rename resolves each edit to the value token and ignores trailing comments, so a name repeated in a comment on the same line is no longer renamed in place of the real value. Rename refuses rather than emit an edit when the token cannot be located.

## Improvements

- `mio:<organization>` scope-switch keys are suggested at effect-block key positions, driven by the config's `mio:` link and the indexed MIO instances. Only prefixed scope-link keys are offered; high-cardinality bare links (country tags, state ids) are not flooded into the list. (cwtools-vscode#76)
- Effect and modifier completion is scope-aware. Modifier categories are retained at parse and `modifier_categories.cwt`'s `supported_scopes` is read for the first time. In-scope effects and modifiers rank to the top of the list while out-of-scope ones sink to the bottom instead of being hidden, so imperfect scope tracking never removes a valid completion. (cwtools-vscode#78)
- New editor features: hierarchical document symbols (Outline and breadcrumbs), folding ranges for brace blocks, document highlight for the symbol under the cursor, and a workspace-wide reference index so Find All References and Rename span closed files, not just open documents.
- New `ignoredErrorCodes` initialization option suppresses diagnostics by error code, applied live on configuration change.
- `genlocall` (generate localisation stubs for missing keys) and `reloadrulesconfig` (reload the rules folder without restarting the server) are re-implemented as server commands.

## Developer

- `RuleSet.modifiers` now carries the modifier category (`Vec<(String, String)>`) and a new `modifier_categories` map is parsed from `modifier_categories.cwt`; `cwtools_validation::scope_matches_required` is now public so completion and validation agree on scope compatibility.
- Added completion tests for the variable-dump guard (numeric and localisation value positions, two-overload `has_country_flag`), the `has_dlc` snippet escaping and quoting, MIO scope-link keys, and scope-aware ranking; navigation tests for nested document symbols, folding, document highlight, and cross-file references and rename including the trailing-comment case; and unit tests for error-code suppression and the loc-stub renderer.

---

_Everything below predates the changelog being resumed at 1.19.0. The releases v1.9.0 through v1.18.0 shipped without changelog entries of their own. The two sections that follow were drafted under provisional "1.8.5" and "1.8.6" headings that never matched a real tag (tags jump from v1.8.4 straight to v1.9.0); their content actually shipped inside the v1.9.x+ tags. The headings are retitled below to say so; treat them as pre-1.19.0 history, not released versions._

# Pre-1.19.0: loc linting, diagnostic positions, goto de-dup, Stellaris support

## Bug Fixes

- Localisation linting now flags an unterminated quoted string (CW268). A value with only an opening quote (`KEY:0 "unclosed`) was truncated to its single `"` and read as balanced, so it was silently accepted even though it breaks every loc line after it. (cwtools-vscode#59)
- Localisation keys containing spaces or other invalid characters are now flagged with the new CW276. Only the value was checked before; the key was not. (cwtools-vscode#59)
- A missing required field now reports its warning on the block's own key (`my_decision = {`) instead of on the first field inside the block. The file-root case (a `type_per_file` entity) keeps reporting on its first child so it doesn't land on line 0. (cwtools-vscode#63)
- Go-to-definition no longer returns the same definition twice. An entity present in both the vanilla cache and the mod produced two locations that collapsed to the same place; results are now de-duplicated by location, while genuinely distinct definitions are kept. (cwtools-vscode#62)

## Improvements

- Scripted effects are suggested inside effect blocks. A type-pattern alias (`alias[effect:<scripted_effect>]`) now expands to the actual scripted-effect names instead of emitting the literal `<scripted_effect>` placeholder. (cwtools-vscode#64)
- Modifiers are suggested inside `dynamic_modifier` blocks (`alias_keys_field[modifier]`). (cwtools-vscode#65)
- Duplicate autocomplete entries are removed. (cwtools-vscode#66)
- Stellaris modding support lands. The engine now consumes the `cwtools-stellaris-config` as the source of truth for scope/link/pretrigger resolution: scopes and links come from `scopes.cwt`/`links.cwt` via the runtime `ScopeRegistry`, and the pretrigger set comes from `alias[<scope>_pre_trigger:<name>] = bool` declarations in `pre_triggers.cwt`. The hardcoded `STELLARIS_SCOPES` const and `load_stellaris_links` table stay in place as a backfill for partial configs and tests.
- New Stellaris-specific validators, ported from `CWTools/Validation/Stellaris/STLValidation.fs`, folder-scoped like F# and dispatched case-insensitively:
  - **CW108/CW109** `research_leader` blocks nested in `common/technology` definitions: missing `area` (CW108), or disagreeing with the technology's (CW109; args are leader-then-tech, F# had them swapped)
  - **CW110** any root block of `common/technology/*.txt` (the `category/` subfolder excluded) whose `category` holds no value
  - **CW120** pretrigger placement: a known pretrigger in an event's `trigger` block (event types with `pre_triggers` support, per scope) or a pop job's `possible` block
  - **CW227/CW229** `ship_design`/`global_ship_design` references an unknown `section_template`/`component_template`. Gated like CW500 (complete index, known instances); `DEFAULT_COLONIZATION_SECTION`/`DEFAULT_CONSTRUCTION_SECTION` are exempt
  - **CW250** a `common/component_templates` block with `type = planet_killer` missing its `on_destroy_planet_with_<key>` on_action or `can_destroy_planet_with_<key>` scripted trigger (same completeness gate)
- `RuleSet` exposes `pretriggers: HashMap<String, HashSet<String>>` (scope -> trigger names) from `reindex()`. CW301 duplicated CW120 on the same leaf and is retired.
- Editor warm start is faster: a parse-cache hit now reads the archived bytes in place instead of building an owned copy of every cached string first. Warm load over a 6293-file corpus drops from 0.73s to 0.52s.
- Validation allocates far less per node: cardinality counting reuses one stack buffer and only allocates per distinct key, rule keys are lowercased once instead of per block, and the common value checks (type refs, file paths, variables, scope targets) borrow the value instead of copying it. The parser also stops materializing comments it throws away.

## Notes

- Stellaris coverage in this release is best-effort: there is no vanilla corpus or real-world mod in the test data, so a few checks (CW228 slot-in-section, CW230 size mismatch, CW231 unused technology, CW233 entity defined) are defined and wired but emit no diagnostic yet. They need per-template field data indexed from mod files; the engine is ready to consume that data when it lands.
- The cwtools-stellaris-config's `scopes.cwt` declares `Alliance` and `Federation` as separate scopes; the previous hardcoded table merged them under one id. With the config-driven path live, `id_of("alliance")` and `id_of("federation")` now resolve to different ids. Pinning test: `crates/validation/tests/stellaris_config.rs::config_alliance_and_federation_are_separate_scopes`.
- `docs/ARCHITECTURE.md` is now an actual architecture doc: the 15-crate map and layering, the batch pipeline, the CLI-vs-LSP split, and checklists for adding a game or an error code. The old loc-only content survives as a trimmed section.
- `docs/ERROR_CODES.md` corrections: CW104/105/106 are on by default (opt out with `CWTOOLS_NO_SCOPE_CHECKS=1`; the doc cited a `CWTOOLS_SCOPE_CHECKS` variable that never existed), CW281/CW282 are now listed, and the CW275 message matches the code.
- `PROFILING.md` documents `CWTOOLS_PROFILE` and `CWTOOLS_TIMINGS`; `BUILD.md` lists all eight subcommands.

## Developer

- Added tests for the unterminated-quote and invalid-key loc checks, the missing-required diagnostic position (nested block and top-level regression), go-to-definition de-duplication, scripted-effect and dynamic-modifier completion, completion de-duplication, and a CLI test asserting the unterminated-quote fixture is flagged.
- Added `cwtools-rs/testfiles/stellaris-config/` (a small subset of the external cwtools-stellaris-config: `scopes.cwt`, `links.cwt`, `pre_triggers.cwt`) plus `crates/validation/tests/stellaris_config.rs` (7 integration tests: scope/alias resolution, link resolution, synthesized iterators, Alliance-vs-Federation separation, per-scope pretrigger population through the real loader, an end-to-end CW120 run, fixture-files-exist sanity check).
- Added unit tests under `crates/validation/src/per_game/stellaris.rs::tests` for the new validators (CW108, CW109, CW110, CW120, CW227, CW229, CW250) including folder-scoping, per-scope pretrigger separation, completeness-gate, and mixed-case dispatch cases.
- `child_scalar` in the Stellaris validators now strips the quotes QString tokens carry, so `template = "SSM_..."` lookups compare the bare name (quoted values never matched the type index before).
- The hot `RuleSet`/`TypeIndex` lookup maps use `FxHashMap` (trusted internal maps probed several times per leaf; SipHash bought nothing).
- `Options` boxes its two fat rarely-set fields (`replace_scopes`, `reference_details`): 240 down to 128 bytes, cheaper rule merging.
- The duplicate localisation `Game` enum is gone; loc APIs take `Option<cwtools_game::constants::Game>`. One fewer lockstep site when adding a game. `LocSeverity` likewise deleted in favor of the shared `ErrorSeverity`.
- The three stateless per-game walkers share one `walk_blocks` visitor, and the hand-rolled `ValidationError` literals in `stellaris.rs` go through `from_code`.
- Four oversized files became directory modules (pure moves, public paths preserved): `validation/rule_core/`, `index` submodules, `game/scope_engine/`, `lsp/completion/`.
- The driver crate has tests now (8 smoke tests: `search_config_for` branches, `index_game_dir`, `Session::load` determinism). Workspace total is 642.
- Every change in this batch was gated on a Kaiserreich full-corpus sorted-CSV diff (byte-identical throughout).
- Deleted `specs/code-review-findings.md`, a 38KB planning artifact that had been committed by accident.
- Simplification pass over this branch's new code: dropped the dead `game` parameter threaded through the loc build/validate pipeline (both terminals ignored it), deleted the owned cache reverse path (`cached_to_arena` and its helpers; the CLI `deserialize` and the round-trip tests now reload through the zero-copy archived path), removed the production-dead `languages_for_game`/`key_to_language_for_game`, and collapsed the Stellaris `child_has_always_no`/`child_is_bool` pair into one `child_is_always_no`. Five now-dead language-list tests removed (suite 637); Kaiserreich corpus byte-identical.

# Pre-1.19.0: completion responsiveness and ordering

## Improvements

- Autocomplete no longer freezes or goes generic when the user returns to a half-typed file. Context-aware completions are now flagged `is_incomplete` so the editor re-queries on every keystroke (instead of caching the previous list client-side past the point where the live text has moved on — the "popup sticks on a half-typed prefix" symptom). The exclusive `documents` lock and the rules read guard are released as soon as the request snapshots the data it needs, so the debounced validate's AST update and concurrent `did_open`/`did_change`/`did_close` no longer block behind a slow completion. The loc-completion and fallback lists are cached by an info-revision counter, so a stable workspace (the common case while typing into a half-typed file) skips the per-request `info.files` walk. A stale completion request for a URI is cancelled when a newer one arrives, so a burst of keystrokes no longer stacks N parallel AST walks. When the last parse failed (`doc.ast` is None), the completion handler re-parses on demand for that request only (not written back) to recover a useful context for the half-typed state.
- Every completion item now carries a kind-aware `sortText`, so as the user iterates the popup keeps a useful order (concrete leaf fields ahead of node blocks ahead of alias-driven keys ahead of type instances ahead of enum values ahead of scope names) instead of falling back to alphabetical. The bucket is a single digit so the secondary label sort stays stable. The scope-aware `0_<name>` bucket for `required_scopes` items (already in place) is unchanged and still leads the list.

## Notes

- The cwtools-hoi4-config `regimental_support` rule (`Config/effects.cwt`, `Config/history/oobs.cwt`, `Config/common/consolidated_ai.cwt`) accepts any `<unit.support_unit>` key — `anti_air_battery`, `mot_fire_support`, `mot_recon`, `artillery`, etc. as used by Kaiserreich `common/ai_templates/*.txt`. If the engine flags one of these as an error, the cause is almost always that the HOI4 vanilla install path isn't picked up by the extension, so the `game/common/units/*.txt` files that register support units are not loaded; double-check the `cwtools.cache.hoi4` setting and rebuild the vanilla cache.

## Developer

- Added two completion tests: one asserting context-aware completions are marked `is_incomplete`, and one exercising the half-typed state (text that fails to parse from the start)
- Added two completion tests covering the new `sortText` order: one pins the bucket mapping (1-9 per kind, 0 reserved for scope-aware) and one walks a real completion list to confirm every item has a `sortText` and that the first item by sort is a concrete leaf field.

# 1.8.4

## Bug Fixes

- Resource definitions (`common/resources/`) are now indexed, so a resource trigger (`oil`, `steel`, …) resolves to its real rule. File discovery excluded every directory named `resources` (meant to skip a top-level dev-scratch `resources/` folder) and that also dropped the game's `common/resources/`, so `oil` was never a known `<resource>`. Its hover then fell through to an unrelated overload, showing "Check ratio of this type of unit for commander" with scopes `unit_leader`/`combat` instead of "Check amount of resource state or country has" with scopes `country`/`state`. The `resources` exclusion is now anchored to the workspace root: a top-level `resources/` is still skipped, the nested `common/resources/` is indexed. This is the root cause behind the 1.8.2 resource-trigger scope false positive (the validator worked around it; now resources resolve for real).
- A `state`-category modifier in a country idea or national-spirit `modifier = { }` block (`state_resource_cost_<resource>`, …) is no longer flagged "used in incorrect scope … expected state". A modifier's `## scope` is its category (where it takes effect), not where it may be written; a country idea legitimately carries state-category modifiers that cascade to its owned states. The CW104/105/106 scope check now exempts the `modifier` category (it still applies to triggers and effects).

## Developer

- Added a file-discovery regression test: a top-level `resources/` is skipped while `common/resources/` is indexed, on both the CLI (`collect_paths`) and LSP (`walk_workspace_files`) paths
- Added a position-resolver test that an indexed resource key resolves to the `<resource>` overload (the hover ordering it depends on) ahead of a coincidental empty-enum match
- Added scope regression tests: a state-category modifier in country scope isn't CW106, while a same-scoped trigger still is CW104

# 1.8.3

## Improvements

- Autocomplete for effects and triggers now inserts a usable snippet instead of just the bare keyword. A block effect/trigger (`if`, `random`, `every_state`, …) completes to `key = { … }` with its required child fields pre-filled and tab stops — e.g. `if` expands to `if = { limit = { } }`. A value effect/trigger (`add_political_power`, `set_country_flag`, …) completes to `key =` with the cursor after the `=`, ready for the value. Modifier keys in `modifier`/`equipment_bonus` blocks likewise complete with `=`.

## Developer

- Added completion tests covering the alias (effect/trigger) snippet shapes: a block alias pre-fills its required child, a value alias inserts `key = <placeholder>` on a single line

# 1.8.2

## Bug Fixes

- A quoted value followed by a bare value in the same clause (a HOI4 `common/names` callsign list like `{ "Sunshine" Demon }`) no longer makes the parser swallow the clause's closing `}` and cascade a bogus "unclosed clause: expected '}'" to end of file, dropping the rest of the file. A quoted string now closes at the first interior quote for namelist values too, matching the game (Clausewitz splits a name at its first interior quote). Names that embed quotes (`"Division "Castillejos""`) therefore split into multiple values, exactly as the game reads them, instead of being kept whole.
- A resource trigger (`oil`, `steel`, …) in a state scope is no longer falsely flagged "used in incorrect scope … expected combat or unit_leader". The scope check matched the key against an unpopulated game-derived `enum[..]` (which matches anything when empty, e.g. when vanilla isn't indexed) and inherited that alias's scope; it now only trusts a match it can verify (exact key, or a pattern whose backing enum/value/type is populated)
- A bare integer scope block (`129 = { ... }`) now resolves to the `state` scope in HOI4, so triggers/effects inside it (and the hover) see state instead of an opaque "any". `random_list`/`random` weight buckets (`int = { ... }`) keep the current scope, so their bodies aren't falsely scope-checked

## Developer

- The names-file parse regression test now covers a callsigns clause mixing quoted and bare values (the real trigger), not just quoted names with apostrophes/non-ASCII
- Added scope regression tests: a key matching only an empty enum isn't scope-checked, a confident literal trigger still is, and a numeric block resolves to state while a random_list weight doesn't

# 1.8.1

## Bug Fixes

- Adding a localisation key in an open `.yml` now clears the missing-localisation warning (CW100/CW122) on the open game files that reference it — e.g. a new event option's loc key — without waiting for a full rescan: the live overlay now feeds the game-file loc checks, and editing a `.yml` re-validates the open game files that depend on it
- The "Indexing workspace…" status bar always clears now; an absent/empty workspace or a panic mid-scan previously left it spinning forever

## Developer

- Added a regression test for the live localisation overlay resolving an otherwise-missing key on a game file

# 1.8.0

## Features

- `.cwt` rule config files get their own structural linting instead of being validated as game script: opening a rule file no longer floods it with field errors or hangs on "indexing workspace", and a rule that references an undefined type, enum, or single_alias is flagged on the offending line, live as you edit
- Hovering an event or decision id shows its localised title, resolved from the definition's `title` field (or a name-derived loc key) so it works across files
- New `hover.scopeDisplay` setting: in `resolved` mode hover adds a `Resolves to` line showing the scope a link or FROM/ROOT/PREV keyword evaluates to, alongside the ambient current scope (`context`, the default, shows the current scope alone)

## Bug Fixes

- Localisation diagnostics update as you type instead of only after a window reload: a `$ref$` to a key you just added in an open `.yml` resolves immediately, and the change propagates to other open localisation files
- Go-to-definition resolves events and decisions by their dotted id (`namespace.1`), including the `id = ...` references the rule walk types as a plain scalar; it previously looked up the field key and failed
- Autocomplete recovers after a partial edit instead of sticking on generic suggestions: the fallback list is marked incomplete so the editor re-queries as you type, and a transient parse error no longer wipes the last good parse that completion, hover, and go-to-definition resolve context from

## Changed

- Hover tooltips separate documentation, required scope, and the current-scope table with a horizontal rule instead of running them together

## Developer

- Added regression tests for names-file parsing (no false unclosed-clause error), case-insensitive `replace_scope` keys, dotted-id instance lookup, and the live localisation overlay

# 1.7.2

## Bug Fixes

- An empty type-reference value (`soundeffect = ""`, `textureFile = ""`) is no longer flagged as a missing instance (CW500); the engine treats an empty value as "none"
- Texture references resolve via their sibling extension: a `.tga` reference is satisfied by a shipped `.dds` and vice versa (vanilla `core.gfx` points at `.tga` files while only the `.dds` ships), so CW113 only fires when neither extension is present
- A sound/entity `.asset` `file =` resolves relative to the `.asset`'s own directory instead of the field's root prefix (e.g. `zom_idle_001.wav` beside `sound/zom/zom_vo.asset`), instead of reporting CW113
- A naval equipment variant that inherits its subtype through `archetype = <equipment.naval_equip>` now activates `naval_equip`, so its `model =` is accepted instead of being flagged (CW267); `<type.subtype>` references resolve against a subtype-membership index
- `## replace_scope = { THIS = state ROOT = state }` written with uppercase keys (as HOI4's operations.cwt does) is parsed; the keys are now case-insensitive, so scope checks on nested block rules no longer false-positive (CW104/CW105)
- The CW275 message now attributes the unexpected characters to the localisation value rather than the key, and the allow-list is widened to cover scripts and symbols the game renders (fuller Cyrillic, Greek, Armenian, Devanagari, Ethiopic, Tifinagh, IPA, combining marks, currency, arrows, number/letterlike forms) while invisible and format junk (zero-width space, figure space, bidi marks) still flags
- Tooling directories (`.claude` git-worktree mirrors, `node_modules`) are skipped during file and localisation discovery, so a mirrored copy of the mod tree no longer double-counts files and loc entries

## Performance

- Subtype-membership collection skips types that declare no subtypes, avoiding a second full instance-navigation pass over the corpus during indexing

## Developer

- Added regression tests for empty type-reference values, texture sibling-extension resolution, `.asset`-relative file resolution, naval-equip subtype membership, uppercase `replace_scope` keys, the widened and still-rejected localisation character sets, and tooling-dir skipping

# 1.7.1

- Fixes the squiggle placement in the cwtools

# 1.7.0

## Bug Fixes

- A variable carrying a null-coalescing default selector (`my_var?150`) lexes as one key instead of splitting at the `?`; the `my_var?150 = { ... }` and `my_var?150 = 100` forms are no longer reported as an unexpected value plus an orphan block (CW264/CW265)
- A character created by `generate_character` (`token_base = ...`) can be used as a scope (`<token> = { ... }`) instead of being flagged (CW262); value-sets defined in mod files are now collected (previously vanilla-only) and read from the field the rule actually binds rather than a fixed key guess
- Loop effects (`for_each_loop` and friends) seed their implicit `v`/`i`/`break` temp variables, and any explicitly named `value`/`index`/`break`, so the body can read them without being flagged as unset (CW246)

## Changed

- Hover shows ROOT, PREV, FROM, and FROM.FROM next to the current scope, restoring the scope table

## Developer

- The language server warns when the rules directory it is given does not exist, mirroring the vanilla-dir check, which helps diagnose an unresolved Windows `rules_folder`

# 1.6.0

## Bug Fixes

- Stellaris if/else and event checks now detect mixed-case keys (e.g. `IF`, `Trigger`, `Mean_Time_To_Happen`); the ambiguous-if/else and every-tick-event checks (CW236/CW237/CW238/CW107) were silently skipped whenever a key wasn't already lowercase
- Hover, go-to-definition, and completion treat `.yaml` and `.csv` localisation files the same as `.yml`; previously hover and go-to-definition only recognised `.yml`, so `$KEY$` resolution silently skipped the other two
- `.mod` descriptor values with a trailing comment or a quoted `=` parse correctly (e.g. `replace_path = "common/ideas" # keep`); the old parser left the closing quote attached, so the directory failed to override vanilla
- While typing, diagnostics already computed for open dependent files are published instead of discarded when a newer edit arrives mid-pass
- Two game installs that can't be fingerprinted (no launcher file, unreadable mtime) no longer share one vanilla-cache key; the install path is hashed in as a tiebreaker

## Performance

- Glob matching (file include/exclude, run for every file and directory) uses a single rolling DP row instead of allocating an (m+1)x(n+1) grid per match
- Validation error codes are stored as `&'static` references instead of allocating a `String` per finding
- LSP: a burst of keystrokes coalesces to one pending validation task per file instead of stacking a debounced task per keystroke, the shared modifier-key set is snapshotted by refcount instead of deep-copied per scan, and the per-document token lock is no longer held across the full arena walk that rebuilds the set
- Scope resolution (run per command token during validation) is ~49% faster: the per-token lowercase, scope-stack copy, and subscope check no longer allocate
- Parsing is ~32% faster: tokens intern slices of the source directly instead of building a throwaway `String` first, and the character-class checks use a lookup table
- Localisation parsing is ~14% faster: streaming line and element parsing with borrowed text runs instead of an allocation per entry
- Rule validation does fewer per-leaf and per-block allocations: candidate matching, alias and scope-key lookups, cardinality counting, and subtype selection lowercase lazily, reuse scratch buffers, and use O(1) alias/type-instance maps
- The workspace index avoids per-leaf string allocations via thread-local lowercase buffers, borrowed value lookups, and a one-pass type-instance map
- LSP requests reuse work instead of rebuilding it: parallel first-pass scanning, memoised scope and enum completions, a refcounted workspace URI, in-place document updates, and async file-existence checks off the request path
- Loading a cached file interns its strings in one locked batch instead of taking the interner lock per string

## Changed

- The CLI exits with distinct codes so CI can tell an operational failure from validation findings: 3 = file discovery failed, 2 = report write failed, 1 = validation found errors, 0 = clean. Previously all three returned 1

## Developer

- A `.cwt` link that lists an unknown input scope is now logged (naming the link and the bad scope) instead of being silently treated as any-scope
- Filesystem read errors during index building are logged instead of silently producing an incomplete index, and `write_cache` propagates a `create_dir_all` failure instead of swallowing it
- Added a criterion benchmark harness for hot-path functions (glob matching, scope resolution, parsing, string interning, localisation parsing)
- Added a code-review findings spec mapping the reviewed issues to the 1.6/1.7/1.8 releases
- Collapsed the duplicate `JominiCommand`/`JominiParam` types into one: the localisation parser stores command chains directly instead of converting between near-identical types and dropping nested parameters (validation output unchanged)

# 1.5.0

## Bug Fixes

- Order of battle references (`load_oob`, `oob`, `set_naval_oob`, `set_air_oob`) resolve on the Windows build again; the file under `history/units` is found instead of reporting CW500
- `NOT = { AND = { ... } }` is no longer flagged as an unnecessary AND (CW251); HOI4 `NOT` acts as NOR, so the AND is a meaningful NAND, not redundant
- An `AND` inside a `count_triggers` block is no longer flagged as unnecessary (CW251); each direct child is a separately counted condition, so the AND groups several into one counted unit
- A localisation-field value that embeds an inline `[...]` command with a literal prefix/suffix (e.g. a `meta_effect` variable `"[?ROOT...GetTokenKey]_subtype"`) is no longer flagged as an undefined loc key (CW100); it resolves at runtime
- Built-in game variables used without the `var:` prefix (e.g. `faction_leader`) are no longer flagged as unset variables (CW246)
- Event and news pictures set through scripted localisation (`picture = "[SomeFunction]"`) are no longer flagged as an unknown sprite (CW500)
- Localisation `$...$` references to dynamic modifiers, game-object names, and script variables no longer flag as undefined (CW225); genuine typos still do
- Filepath references with a redundant double slash (e.g. `gfx//interface/...`) resolve as the engine treats them instead of reporting CW113
- Windows: trigger/effect documentation (`###`) tooltips, go-to-definition, and validation now work for files whose paths use backslash separators

## Changed

- Hover tooltips show the current scope at the cursor
- Hover and Ctrl+Click work on nested `$KEY$` references inside localisation .yml files: hover shows the referenced entry's text, Ctrl+Click jumps to it
- A broken rules config is flagged: a `.cwt` rule referencing an undefined type, enum, or single_alias reports an error on the offending line
- Autocompleting a plain field inserts `name =` with the cursor after the `=`, instead of just `name`
- Autocompletion on a fresh line after a field offers the block's fields again, instead of the editor falling back to plain word suggestions (most visible in shared_focus / focus files)
- Objects whose type declares `## required` localisation are flagged when that loc key is missing (CW100), so missing localisation is visible again

## Developer

- Normalised Windows path separators across type resolution, the file index, and logical-path derivation so editor features hold up on the Windows build
- The LSP workspace scan walks files in a deterministic, sorted order independent of the filesystem's directory order
- Added regression tests for CRLF `###` docs, scoped hover, `$KEY$` navigation, scripted-loc references, NOT/AND structural checks, count_triggers AND, embedded `[...]` loc commands, built-in variables, backslash path resolution, blank-line completion, and missing-localisation
- `cwtools_index` no longer declares an unused `serde` dependency; `cargo machete` is clean

# 1.4.1

## Bug Fixes

- Ctrl+Click and hover now resolve scripted-effect calls inside on_actions effect blocks (e.g. a `*_on_actions` effect called under `on_weekly`), not just in event and decision effect blocks

# 1.0.3

## Bug Fixes

- Ctrl+Click (go-to-definition) now resolves character, focus, idea, scripted_effect, oob, localisation key, and special_project references, including quoted values and sp:-prefixed scope links
- Localisation key go-to-definition jumps to the English (primary) entry instead of whichever language loaded first
- Cross-file references (e.g. scripted effects) no longer show transient "not found" errors before the workspace finishes indexing; opening a definition file now re-validates the open files that use it
- Autocomplete now works in localisation .yml files
- `if` without a `limit` is flagged as an error again, and an empty `limit = { }` now warns (CW281)

## Changed

- Hover tooltips show only localisation, description, and required scopes by default; the raw field/type classification moved behind the new `cwtools.hover.debug` setting (off by default)
- Ctrl+Click opens the definition in a peek by default for cwtools files; set `editor.definitionLinkOpensInPeek` to false to jump straight to it
- Diagnostics now wait until the initial workspace index finishes, so references aren't briefly flagged as undefined on startup
- Rules-config parse errors are surfaced in the editor (a popup, the CWTools output channel, and a diagnostic on the offending .cwt line) instead of being logged silently

# 1.0.2

## Performance

- Full-mod validation is ~12% faster (8.1s to 7.1s on Millennium Dawn) via fewer allocations in the rule-matching hot path and parallel index building
- Validation memory use cut by ~470MB on large mods (shared file-path and string storage)
- Editor hover/completion/typing no longer rebuild rule lookup tables on every request

## Bug Fixes

- Fixed completion snippets and find-references missing types constrained by filename or extension

## Changed

- Scope-name completions now come from the loaded scopes.cwt/links.cwt instead of a fixed per-game list
- getFileTypes derives file types from the loaded rules instead of hardcoded paths

## Developer

- Split the LSP server from one 4,950-line file into focused modules; shared state regrouped behind two lifecycle locks with a simpler lock order
- CWTOOLS_NO_SCOPE_CHECKS / CWTOOLS_VAR_CHECKS are read once and threaded through validation contexts instead of process-global statics
- Removed ~270 lines of dead per-game tables; configs are the source of truth for scopes, links, and type paths

# 1.0.1

## Bug Fixes

- Fixed modifiers and localization false positives when validating localization strings
- Fixed the NOR/NAND information from Stellaris/EU4 bleeding HOI4 which has different flow controls

## Developer

- Implemented pre-commit for enforcing stylization to default Rust findings for consistency

# 1.0.0

Initial release
