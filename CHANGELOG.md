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
- Autocompleting a plain field inserts `name = ` with the cursor after the `=`, instead of just `name`
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