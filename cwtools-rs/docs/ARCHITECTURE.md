# Architecture

cwtools-rs is a Rust workspace (under `cwtools-rs/`) of 15 crates: a parser, a
rule engine, per-game validators, a localisation subsystem, and two front ends
(the `cwtools` CLI and the `cwtools-server` LSP). This doc maps the crates, the
load pipeline, and the lockstep sites for adding a game or an error code. For the
diagnostic catalog see [`ERROR_CODES.md`](ERROR_CODES.md).

## Crate map

Layer 0 (leaves, no cwtools dependencies):

- `profiling`: tracing subscriber + RSS sampling for the CLI/LSP binaries (see `PROFILING.md`).
- `error_codes`: the shared `CW###` catalog. Deliberately dependency-free so
  `validation` and `localization` share the same codes without a dependency edge.
- `string_table`: the process-wide string interner. AST keys are `u32` ids into it.
- `game`: the `Game` enum plus scope/link data (the `ScopeDef` tables, the scope engine
  with `ScopeId`/`ScopeContext`/transitions, and the config-driven `ScopeRegistry`).

Then, roughly bottom-up:

- `parser`: Paradox script text to an arena AST (on `string_table`).
- `file_manager`: file discovery + parse orchestration (which dirs/files to walk,
  the exclude globs; on `parser`, `string_table`).
- `cache`: rkyv+zstd on-disk AST cache (`.cwb`) (on `parser`, `string_table`).
- `rules`: `.cwt` rule loading, giving a `RuleSet` of types/aliases/enums and
  scope/link inputs (on `game`, `parser`, `string_table`, `error_codes`).
- `localization`: `.yml` loc parsing, `LocService`/`LocIndex`, loc reference and
  scope validation (on `error_codes`, `game`, `file_manager`).
- `index`: `TypeIndex`/`VarIndex`/`FileIndex` + value-set/complex-enum collection
  + the vanilla cache payload (on `parser`, `file_manager`, `string_table`, `rules`, `cache`).
- `validation`: the rule engine and per-game validators; emits `ValidationError`s
  (on `error_codes`, `parser`, `rules`, `string_table`, `game`, `index`, `localization`).
- `info`: the incremental per-file index (`InfoService`) backing LSP hover, goto,
  and find-references (on `parser`, `string_table`, `rules`, `index`).
- `driver`: the shared load-and-validate pipeline both front ends call
  (on `validation`, `index`, `localization`, and the layers below).
- `lsp`: the tower-lsp server, `cwtools-server` (on `driver`, `validation`, `info`,
  `cache`, `profiling`, and below).
- `cli`: the `cwtools` binary (on `driver`, `validation`, `info`, `profiling`, and below).

The dependency graph is acyclic. `error_codes` and `game` sit at the bottom with no
cwtools dependencies, so everything above can key off them.

## The batch pipeline

The full load pipeline lives in `crates/driver/src/lib.rs`:

1. Load the `.cwt` rules into a `RuleSet`.
2. Discover and parse the mod files (sharing one `StringTable`, so interned ids match).
3. Build the `TypeIndex` (plus the variable index and, when a vanilla install or
   cache is given, the vanilla index).
4. Expand the modifier keys valid in `alias_name[modifier]` slots.
5. Build the loc index (`LocIndex`) over the mod + vanilla loc keys.
6. Build the per-run `ScopeRegistry` from the config's scopes and links.
7. Validate every file against that prebuilt state.

Steps 3 through 7 are the reusable primitives (`index_game_dir`,
`build_scope_registry_arc`, `Prepared`/`validate_prepared`). Both front ends call
them directly, so the order can't drift the way it did before.

### CLI vs LSP

`Session` (in `driver`) bundles the primitives into the CLI's batch model: load
everything from disk once into immutable-after-load state, then validate the whole
set (`validate_all`). One `Session` per CLI run.

The LSP does NOT use `Session`. Its index is mutable and incremental (single files
are re-indexed on each edit behind an `RwLock`, with no whole-workspace re-parse),
which doesn't fit `Session`'s load-once ownership. Instead the LSP holds its own
workspace state and builds a `Prepared` from the same shared primitives per
validation. Same sequence, different ownership.

### Background reindex

A long-running session drifts: files deleted while the server had no watcher event,
a settings change that only lands on the next scan. To catch that, the LSP runs a
periodic quiet rescan (`background_reindex_loop` in `scan.rs`, spawned once from
`initialized`). It re-runs the same `validate_entire_workspace` the startup scan
does, but `quiet`: no loading bar, though diagnostics still publish, so an error
fixed outside the editor clears.

The loop is idle-gated. Each cycle re-reads the effective interval, sleeps it out,
then waits for the user to go idle before running (`should_run_background_pass`:
the initial index is ready, no scan already running, and at least 15s since the
last activity). `mark_activity` resets that idle clock on edits, completion, hover,
and navigation, so a background pass never competes with a request the user is
waiting on. The re-entrancy guard (`scan_in_progress`) means a background pass and a
foreground scan can't overlap; the loser skips.

The cadence is the `backgroundReindexIntervalMinutes` initializationOption (default
30, `0` disables). It is also live-updatable through `workspace/didChangeConfiguration`
(`config.rs`), so toggling the setting takes effect without a restart. The
`reindexWorkspace` executeCommand forces an immediate foreground rescan on demand;
it reports "already in progress" when it loses the guard instead of silently
no-oping. `CWTOOLS_REINDEX_INTERVAL_SECS` / `CWTOOLS_REINDEX_IDLE_SECS` override the
interval and idle window for tests.

## Per-game validators

Generic rule validation runs first (the `.cwt` engine in `validation/src/rule_core`).
Then `run_game_validators` (`validation/src/per_game/mod.rs`) adds:

- `common` checks (unique types, `should_be_referenced`, warning-only downgrades),
- cross-game `structural` hints (empty `if`/`limit`, `NOT` misuse, redundant booleans), then
- a dispatch on `Game`: `stellaris` (full validators), `hoi4` (cleanup hints),
  `eu4` (stub), and `_ =>` common-only for everything else.

The `_ =>` fallback is intentional: a game with no per-game module still gets the
common + structural checks. Scope and link behavior is config-driven, not hardcoded
per game. `scopes.cwt` and `links.cwt` load through `ScopeRegistry`
(`game/src/scope_registry.rs`), so the scope checks (CW104/105/106, CW243-245,
CW247, CW248, CW260) work for any game that ships those files.

## Adding a new game

A new `Game` variant touches these sites, in lockstep:

1. `game/src/constants.rs`: add the variant, its `Display` arm, a `from_str` arm,
   and a `scope_defs` arm (point it at a `*_SCOPES` table, or `&[]` for a
   config-only game like HOI4).
2. `game/src/scope_engine/links.rs`: add a `load_scope_links` arm (a hardcoded link
   fallback, or `{}` when the config supplies everything).
3. `game/src/scope_registry.rs`: no new arm. The registry is generic; it reads the
   game's data through `scope_defs()` and `load_scope_links()`, so steps 1 and 2 cover it.
4. `validation/src/per_game/mod.rs`: add a dispatch arm only if the game gets a
   dedicated validator module. Otherwise the `_ =>` default handles it.
5. `validation/src/per_game/structural.rs`: add a CW223 message arm only if the
   game's boolean operators differ from the default (HOI4 already overrides it).
6. `localization/src/commands.rs`: add the game's language list to
   `languages_for_game` (else it falls to the accept-all default).
7. `localization/src/scope_validation.rs`: add the variant to `game_to_engine`'s
   pass-through list (else loc scope checks fall back to lenient HOI4).
8. `lsp/src/paths.rs`: add the Steam install-folder name to `discover_vanilla_dir`.
9. Ship `scopes.cwt` and `links.cwt` in the game's `.cwt` config (a separate repo).

The compiler catches some of these for you. The `Game` matches in `constants.rs`
(`Display`, `scope_defs`) and `scope_engine/links.rs` (`load_scope_links`) have no
`_ =>`, so a new variant won't compile until you handle them. That is the safety
net. Do not add a catch-all to silence it. The remaining sites (`from_str`,
`languages_for_game`, `game_to_engine`, `discover_vanilla_dir`, the per-game
dispatch, the CW223 message) have deliberate fallbacks, so a new variant compiles
and behaves as the generic default until you add its arm.

## Adding an error code

There is no central registry to update. Three edits:

1. Add one `pub const CW###_NAME: ErrorCode = ...` in `error_codes/src/lib.rs`.
2. Reference it at the emit site (in `validation` or `localization`), usually via
   `ValidationError::from_code`.
3. Add a row to `docs/ERROR_CODES.md`.

## Module layouts (the split god-files)

The four largest areas are directory modules, each a thin `mod`/`lib` over focused files:

- `validation/src/rule_core/`: the `.cwt` rule engine (`matching`, `children`,
  `leaf`, `alias`, `subtype_merge`, `mod`). The biggest of the four.
- `game/src/scope_engine/`: `engine` (`ScopeId`/`ScopeContext`/transitions) vs
  `links` (per-game hardcoded link tables), over `mod`.
- `lsp/src/completion/`: `builders` (item construction), `snippets`, `scope_names`,
  `resolve` (lazy `completionItem/resolve`: documentation and detail filled in for
  the one item the editor focuses, kept out of the initial list), over `mod`.
- `index/src/`: `type_index`, `path_match`, `collect`, `variables`, `dynamic_values`,
  `vanilla_cache`, behind a thin `lib.rs` that re-exports the public surface.

## Localisation subsystem

The loc system resolves `$KEY$` references in game script files to their translated
text (shown in hover tooltips) and validates that referenced keys actually exist
(CW100/CW122), plus the loc-file checks (CW225/234/259/268/275).

### Data flow

```
.yml files on disk
       |
       v
  LocService (parses .yml -> Vec<LocFile>)
       |
       |--> LocIndex (lowercased key sets, per-language)
       |         - exists_any(key): does this key exist in any language?
       |         - missing_synced_languages(key): which languages lack it?
       |
       |--> loc_text map (HashMap<key, Vec<(Lang, text)>>)
                 - used by hover to show translations
                 - rebuilt during workspace scan
```

### Current implementation

All loc data lives in memory:

- **`LocService`**: owns every parsed `LocFile` (the full AST of every `.yml`
  file). Built from disk during the workspace scan. Dropped after the index is
  built to free memory (~2M entries on Millennium Dawn).
- **`LocIndex`**: lowercased key sets per language + a union set. Built from
  `LocService`, then the service is dropped. Answers existence queries for
  config validation.
- **`loc_text`**: `HashMap<String, Vec<(Lang, String)>>` for hover display.
  Built from `LocService` before it's dropped. Only rebuilt during the full
  workspace scan.
- **`loc_live_overlay`**: per-open-file key sets for incremental `$ref$`
  checks. Updated on every loc file edit so newly-added keys resolve
  immediately without a full rescan.

A SQLite-backed hover-text store was considered (to skip re-parsing unchanged files
and stream results) but is not planned. The in-memory maps are cheap enough to rebuild.

## Build and profiling

See `BUILD.md` for build instructions and `PROFILING.md` for build/runtime profiling.
