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