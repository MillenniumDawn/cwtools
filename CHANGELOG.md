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