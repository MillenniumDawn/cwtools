# Architecture review: bug parking lot

Functional/correctness issues noticed while reviewing structure. Not a fix list,
not verified against a repro. Each line is a lead to chase later.

## Duplicated logic that can drift out of sync

- `crates/validation/src/lib.rs:3041` (`engine_game_to_loc_game`), `crates/lsp/src/main.rs:54`
  (`engine_to_loc_game`), `crates/cli/src/main.rs:757` (inline `match game_id`):
  the engine-Game -> loc-Game mapping exists three times. CORRECTION (verified both
  Rust agents): the three copies are currently IDENTICAL (7 explicit games Hoi4/
  Stellaris/Eu4/Ck3/Ir/Vic3/Eu5 + `_ => Generic`); there is NO live drift today, and
  routing CK2/VIC2 to `Generic` is correct because the loc `Game` enum
  (`localization/src/commands.rs:85-96`) has no CK2/VIC2 variants. The risk is
  structural (3 copies, no shared source), not a present bug.
- `crates/info/src/lib.rs:278` (`dir_contains_segment`) vs `crates/validation/src/lib.rs:1138`
  (`path_contains_segment`): two byte-for-byte copies of the same segment-boundary
  path scan. The `info/src/lib.rs:270-277` comment explicitly requires they "mirror"
  each other so a file is INDEXED by the same type that VALIDATES it. Broader fork:
  `info`'s `check_path_dir:303`/`skip_root_key_matches:347`/`type_key_filter_matches:359`/
  `starts_with_matches:371` vs `validation`'s `find_type_by_path_and_key:1174`/
  `type_path_matches:1267`/`should_skip_root_key:737`. If one accepts a file the other
  rejects, the type index and validator disagree, producing phantom CW500 /
  "not a known instance" / "unexpected property". Highest correctness-stakes dup.
- `crates/localization/src/validation.rs:199` and `crates/localization/src/yaml_parser.rs:478`
  both implement the REPLACE_ME / TODO_CD placeholder check. CORRECTION: both copies
  are IDENTICAL and both match ONLY the double-quoted form (`"REPLACE_ME"`/`"TODO_CD"`),
  so an unquoted `REPLACE_ME` value passes unflagged in both. (The earlier "one only
  matches the double-quoted form" note was wrong.)
- `crates/localization/src/validation.rs:168` (`&LocEntry`) vs
  `crates/localization/src/yaml_parser.rs:430` (`&mut LocEntry`): `validate_quotes` is
  ALSO defined twice, and the two signatures already differ. The crate's own
  `lib.rs:27-28` comment concedes both files define `validate_quotes` /
  `validate_replace_me`. Drift-prone.
- Modifier-key `<type>` template expansion: `crates/cli/src/main.rs:733` vs
  `crates/lsp/src/main.rs:33`. Two copies of the same `<...>` slice-and-expand;
  divergence means CLI and LSP accept different modifier names.
- Glob matching: `crates/file_manager/src/file_manager.rs:696` (`glob_match`) vs
  `crates/lsp/src/main.rs:359` (`lsp_glob_match`). The LSP ignore-pattern matcher is a
  second implementation; the CLI and LSP can disagree on which files an ignore glob
  skips. Minor functional divergence: `lsp_glob_match:361-363` special-cases bare
  `*`->`true` while `glob_match` reaches the same result via `ends_with("")`.
  Equivalent today, divergence-prone.
- Vanilla indexing: `crates/cli/src/main.rs:18` (`index_game_dir`) vs
  `crates/lsp/src/main.rs:95` (`index_vanilla_dir`); the LSP copy's doc-comment
  (`:90-94`) literally says it "Mirrors the CLI's index_game_dir / --vanilla". A
  larger duplicated body than the loc-map/modifier/walk/glob copies.

## Workspace-walk divergence (CLI vs LSP see different file sets)

- `crates/lsp/src/main.rs:2317` (`walk_dir`) hardcodes its own skip list
  (`.git`, `node_modules`, `out`, `dist`, `target`, `bin`, `obj`, `resources`,
  `.vscode`) and engine baseline (`Changelog.txt`, `README.*`, `*.md`), separate from
  `file_manager`'s `FileManagerConfig` defaults and `collect_files_recursive`
  (`crates/file_manager/src/file_manager.rs:604`). The CLI walks via `file_manager`;
  the LSP walks via `walk_dir`. The two entry points can validate different file sets
  for the same workspace.

## Concurrency / state

- `crates/lsp/src/main.rs:204-266` `DocumentState`: 17 fields (not 16) behind a mixed
  `Mutex` / `parking_lot::RwLock` guard set over interdependent engine state (ruleset,
  info_service, modifier_keys, loc_index, vanilla_index). CORRECTION: a lock-order
  audit found NO actual inversion. Every multi-guard site holds them in the same order
  (documents -> ruleset -> info_service -> modifier_keys -> loc_index, verified at
  `:2556-2571`, `:3115-3140`, `:3222-3228`) and most sites snapshot-and-drop
  (`.lock().clone()`). The hazard is latent (undocumented order, a few co-held-guard
  sites), not currently triggered. Action: document the lock order; do not treat as a
  live deadlock.
- `crates/lsp/src/main.rs:208`: `ruleset` is a `Mutex<Option<RuleSet>>` (exclusive) but
  is read on every hover/completion/validate and written rarely. Every reader
  serializes. Should be `RwLock` like the adjacent engine state. (Perf smell, not a
  correctness bug.)

## Scope engine

- `crates/validation/src/lib.rs:803-806` `build_scope_registry` rebuilds the registry
  from `ruleset.scope_inputs`, falling back to `ScopeRegistry::from_hardcoded(game)`
  (`crates/game/src/scope_registry.rs:92`) only when inputs are ENTIRELY empty.
  CORRECTION: it is all-or-nothing, not a mix. A non-empty-but-PARTIAL `scopes.cwt`
  builds a registry from the partial config with NO hardcoded backfill, so scopes
  present in `scope_engine.rs` but absent from the partial config resolve to `None`
  silently. Two sources of truth for the scope graph (HOI4 in `.cwt`, every other game
  hardcoded in `game/src/scope_engine.rs:557-1808`); registry construction is game
  knowledge that belongs in `game`.
- `crates/validation/src/lib.rs:991` `scope_matches_required` returns `true` (lenient)
  whenever the current scope name `starts_with("scope_")`, i.e. whenever scope
  tracking failed to resolve a name. Masks real wrong-scope errors behind any tracking
  gap, so CW104/105/106 silently under-report whenever the scope chain has a hole.
- `crates/validation/src/lib.rs:1000` `scope_matches_required` has a SECOND leniency
  layer: `id_of(r).is_none_or(...)` treats an unresolvable *requirement* name as
  satisfied too. A second silent under-report path beyond the `:991` `scope_` prefix
  check.

## Parser data model

- `crates/parser/src/ast.rs:64` (`Value::Clause(Vec<Child>)`) + `parser.rs:492`: a
  leaf-valued clause (`key = { ... }`) is stored as `Leaf{ value: Value::Clause(..) }`,
  an inline second clause form that lives OUTSIDE the arena, parallel to
  `Node.children`. Any recursive walker that handles `Node.children` but forgets the
  `Leaf -> Value::Clause` case (or vice versa) silently skips a subtree. 71
  `Value::Clause` match sites across the workspace; `info/src/lib.rs:439` already has
  to branch to normalize the two shapes. Easy to miss one.

## Notes (not bugs, verify before acting)

- `crates/validation/src/lib.rs` carries 11 `#[allow(clippy::too_many_arguments)]`.
  Not a bug, but each one is a signature that's one parameter away from a context
  struct; threading errors (passing the wrong `node_key`/`loc_index`) would not be
  caught by the type system.
