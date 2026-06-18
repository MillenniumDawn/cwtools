# 1.5.0

## Bug Fixes
- Order of battle references (`load_oob`, `oob`, `set_naval_oob`, `set_air_oob`) resolve on the Windows build again; the file under `history/units` is found instead of reporting CW500
- `NOT = { AND = { ... } }` is no longer flagged as an unnecessary AND (CW251); HOI4 `NOT` acts as NOR, so the AND is a meaningful NAND, not redundant
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
- Added regression tests for CRLF `###` docs, scoped hover, `$KEY$` navigation, scripted-loc references, NOT/AND structural checks, built-in variables, backslash path resolution, blank-line completion, and missing-localisation

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