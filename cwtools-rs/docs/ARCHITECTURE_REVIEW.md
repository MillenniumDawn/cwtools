# cwtools-rs architecture review

Read-only structural review of the Rust workspace. Scope: crate boundaries,
duplication, god-modules, layering, and a low-risk refactor roadmap. No source
was modified. Functional/correctness leads are parked in `ARCHITECTURE_BUGS.md`.

End goal context: the Rust side is meant to replace the F# binary entirely, so
both entry points (CLI and LSP) must drive the same core and agree on results.
Today they each reimplement the driver, which is the central problem below.

## Current architecture

13 crates, edition 2024, resolver 3. Layering is mostly clean at the bottom and
tangled at the top.

```
                         cli            lsp          (entry points / surfaces)
                        /  | \         / | \
        +--------------+   |  +---+---+  |  +-------------------+
        |                  |      |      |                      |
   validation -----------> info   |  localization          profiling
     |   |  \              |  \    |     |   \
     |   |   +--> info     |   rules     |    game
     |   |   +--> localization  |        |     |
     |   +------> game ---------+--------+-----+
     |   +------> rules
     |   +------> error_codes
     |
   rules --> parser --> string_table
   file_manager --> parser, string_table
   cache --> parser, string_table
   game --> string_table   (unused dep, see machete)
```

Layer sketch, bottom to top:

- L0 leaf: `string_table`, `error_codes`, `profiling`.
- L1 parse/io: `parser` (-> string_table), `file_manager` (-> parser),
  `cache` (-> parser).
- L2 rules: `rules` (-> parser). The `.cwt` ruleset model and converter.
- L3 domain: `game` (scopes, constants), `localization` (-> game, file_manager),
  `info` (-> rules, file_manager). These are siblings but cross-reference.
- L4 engine: `validation` (-> rules, game, info, localization, error_codes).
- L5 surfaces: `cli` and `lsp`, each depending on nearly everything.

### What works well

- The bottom three layers are clean. `parser`/`string_table`/`cache`/`file_manager`
  have tight, single-purpose APIs and no upward dependencies. `parser` is an arena
  AST (`crates/parser/src/ast.rs`) with index-based `Child` references, the right shape
  for a hot path and for serialization (`cache`). Caveat: the arena is HYBRID, not
  pure — clause children are stored inline as `Vec<Child>` in `Node`/`ValueClause`/
  `ParsedFile`, AND a leaf-valued clause (`key = { .. }`) lands in a SECOND inline form,
  `Value::Clause(Vec<Child>)` (`ast.rs:64`, built at `parser.rs:492`). Every recursive
  walker must therefore handle clause children two ways; there are 71 `Value::Clause`
  match sites and `info/src/lib.rs:439` already normalizes the two by hand. A single
  clause-children accessor (or folding leaf-clauses into `Node` at parse time) would
  remove a whole class of "handled Node but forgot Leaf-clause" walker bugs.
- `error_codes` is a real catalog: `ValidationError::from_code`
  (`crates/validation/src/lib.rs:31`) pulls severity + template from one place, so
  call sites don't restate the code->severity mapping. Good seam.
- The validation engine already separates "build per-run shared state once" from
  "validate one file": `build_enum_map` / `build_scope_registry_arc` /
  `validate_ast_with_loc_prebuilt` (`crates/validation/src/lib.rs:214-244`). That is
  exactly the seam a shared driver needs; it just isn't used by a shared driver yet.
- The scope engine is config-driven off `scopes.cwt` + `links.cwt` via
  `ScopeRegistry` (`crates/game/src/scope_registry.rs`), not hardcoded tables. Good
  direction.

## Top structural problems, ranked by maintenance impact

### 1. No shared driver. CLI and LSP each reimplement the pipeline.

The "load rules -> discover/parse -> build TypeIndex -> var index -> vanilla index
-> modifier keys -> loc index -> per-file validate" sequence is written twice:

- CLI: `crates/cli/src/main.rs:555-900` (the `Validate` arm).
- LSP: `crates/lsp/src/main.rs:2282` (`validate_entire_workspace`) plus
  `parse_and_validate` (`:3032`), `validate_parsed_prebuilt` (`:2847`),
  `ensure_vanilla_index` (`:3235`), `rebuild_modifier_keys` (`:3221`).

Concrete duplication this produces:

- Game->loc-Game mapping in THREE places (`validation/src/lib.rs:3041`,
  `lsp/src/main.rs:54`, `cli/src/main.rs:757`).
- Modifier-key `<type>` expansion in two (`cli/src/main.rs:733`, `lsp/src/main.rs:33`).
- File discovery in two (`file_manager`'s `collect_files_recursive` used by CLI vs
  the LSP's own `walk_dir` at `lsp/src/main.rs:2317`), with separate skip lists.
- Glob matching in two (`file_manager::glob_match` vs `lsp::lsp_glob_match`).

Impact: every behavior change (a new check, a new game, a new ignore rule) has to be
made twice and the two copies drift (see `ARCHITECTURE_BUGS.md`). This is the single
biggest barrier to "delete the F# binary": the Rust CLI and LSP don't even agree
with each other yet.

### 2. `validation` is a god-crate (`lib.rs` is ~3800 LOC; the only other src file is a 1-line shim).

`crates/validation/src/lib.rs` is one flat module with 72 functions (the only other
file is a 1-line re-export shim) and several 200-500 line functions:
`validate_children` (`:1953-2441`, 489 LOC), `validate_ast_with_loc_prebuilt`
(`:233-545`, 313 LOC), `validate_leaf` (`:3213-3469`, 257), `field_matches_value`
(`:3528-3748`, 221), `validate_alias_usage` (`:2806-3021`, 216), `validate_with_type`
(`:546-736`, 191). It mixes at least six concerns that should be separate modules:

- file/type resolution (`find_type_by_path_and_key`, `find_grandchild_type`,
  `type_path_matches`, `should_skip_root_key` — `:737-1329`),
- subtype matching (`subtype_matches`, `subtype_rules_match` — `:1342-1559`),
- the rule-vs-AST core (`validate_children`/`validate_leaf`/`validate_node`),
- scope seeding/tracking (`seed_root_scope`, `enter_block_scope`,
  `apply_replace_scopes`, `scope_matches_required`, `validate_scope_target` —
  `:973-2524`),
- registry construction (`build_scope_registry` — `:803`, which arguably belongs in
  `game`),
- localisation-field checks (`validate_localisation_field`,
  `push_loc_command_diagnostic`, `engine_game_to_loc_game` — `:3041-3213`).

The 11 `#[allow(clippy::too_many_arguments)]` are the symptom: validation context
(ast, ruleset, table, scope_context, game, type_index, modifier_keys, loc_index,
errors) is threaded by hand through every function instead of living in a `ValidationCtx`
struct. Any new piece of context means editing ~10 signatures.

### 3. `validation -> info` is a layering inversion.

`validation` depends on the entire `info` crate but uses only `info::TypeIndex`
(verified: 20 `info::` sites, all `TypeIndex`; 0 references to `InfoService`). `info`
itself pulls in `rules` + `file_manager` and houses the LSP's `InfoService`, position
lookup, hover data, etc. So the core validator transitively drags in editor-feature
code it never calls. `TypeIndex`/`VarIndex`/`FileIndex`/`TypeInstance` are plain
reference-index data structures (their `use` block at `info/src/lib.rs:1-6` imports
only `parser`/`rules`/`string_table`/`std` — no editor or position deps) and belong in
a lower crate; the `InfoService` editor layer should sit ABOVE validation.

The stronger argument for the extraction is correctness, not purity: `info` and
`validation` carry two independent copies of the same path/type resolver
(`info::dir_contains_segment:278` vs `validation::path_contains_segment:1138`, plus the
`check_path_dir`/`skip_root_key_matches`/`type_key_filter_matches` family vs
`find_type_by_path_and_key`/`type_path_matches`/`should_skip_root_key`). The
`info/src/lib.rs:270-277` comment concedes they MUST stay in sync so a file is indexed
by the same type that validates it. Pulling these resolvers down into the shared index
crate so both sides call one copy removes a phantom-diagnostic drift hazard (see
`ARCHITECTURE_BUGS.md`).

### 4. Three rules/game monoliths: `rules_converter.rs` (2322), `scope_engine.rs` (2142), `post_process.rs` (801).

`crates/rules/src/rules_converter.rs` is one 60-function module doing the entire
`.cwt` AST -> `RuleSet` conversion: type extraction, enum extraction, subtype
parsing, comment-directive parsing, scope/link extraction, colour rules. Its single
biggest function is `field_from_string` (`:244-580`, ~337 LOC) — the `.cwt`
field-type string parser (`"scalar"`, `"int[..]"`, `<type>`, `enum[..]`), larger than
any function in `validation` and the prime extraction candidate (a `field_parser.rs`).
Splitting by output kind (field parser, types, enums, subtypes, scopes/links, comment
directives) would make it reviewable.

`crates/rules/src/post_process.rs` (801 LOC) is the rules crate's undocumented second
monolith: a mutate-the-RuleSet-in-place pass doing single-alias inlining
(`replace_single_aliases:27`, `inline_rules_list:116`), colour-field expansion
(`expand_colour_rule:292`), and value-scope/ignore-marker rewrites
(`replace_value_marker_fields:428`, `replace_ignore_marker_fields:489`). It shares the
colour-rule concept with `rules_converter::build_colour_rules:755` — colour logic
lives in both files.

`crates/game/src/scope_engine.rs` is more than a monolith file: its bulk is hardcoded
per-game link tables (`load_stellaris_links:557`, `load_eu4_links:917`,
`load_ck2_links:1082`, `load_ck3_links:1290`, `load_vic2_links:1482`,
`load_ir_links:1596`) that `ScopeRegistry::from_hardcoded` (`scope_registry.rs:92-111`)
consumes. It is the hardcoded half of the scope-graph dual source of truth (see
problem 3-adjacent note and `ARCHITECTURE_BUGS.md`): HOI4's scope/link knowledge lives
in `.cwt` config, every other game's lives hardcoded here.

### 5. `lsp/main.rs` is 4295 LOC mixing transport, driver, and feature logic.

`DocumentState` (`crates/lsp/src/main.rs:204-266`) holds 17 fields spanning
LSP-transport state (documents, versions, doc_tokens, symbol_index, workspace_uri,
edit_generation) and core-engine state (ruleset, info_service, modifier_keys,
loc_index, vanilla_index, cache_dir, language, loc_languages, ignore patterns) behind
an ad-hoc mix of `Mutex` and `parking_lot::RwLock`. Hover, completion, goto, rename,
references, the workspace driver, vanilla caching, and the debounce machinery all live
in one file. The LSP protocol surface (the `LanguageServer` impl) and the
"session/driver" that owns engine state should be separate. Note: a lock-order audit
found no actual inversion (all multi-guard sites use the same order, most
snapshot-and-drop), so this is a structure/clarity problem, not a live deadlock; the
one concrete fix is making the read-mostly `ruleset` a `RwLock` instead of `Mutex`
(`:208`).

## Duplication / dead-code findings

Duplication is itemized in problems 1, 2, 4 above and in `ARCHITECTURE_BUGS.md`.
Summary of the concrete copies to collapse:

- Game->loc-Game mapping x3 (identical today, no live drift, but no shared source).
- Modifier-key `<type>` expansion x2 (`cli:733`, `lsp:33`, byte-identical).
- Vanilla indexing x2 (`cli::index_game_dir:18`, `lsp::index_vanilla_dir:95` — the
  largest of the cross-entry-point copies; the LSP doc-comment admits it mirrors CLI).
- File-walk + ignore-glob x2 (`file_manager::collect_files_recursive:604` vs
  `lsp::walk_dir:2317`, with DIVERGENT skip lists; `glob_match:696` vs
  `lsp_glob_match:359`).
- Within `localization`, BOTH `validate_quotes` AND the REPLACE_ME/TODO_CD placeholder
  check are defined twice (`validation.rs:168/199` vs `yaml_parser.rs:430/478`); the
  crate's `lib.rs:27-28` comment already admits it.
- Path/type resolution duplicated across `info` and `validation` (the
  `info/src/lib.rs:270` comment concedes they must "stay in sync") — highest
  correctness stakes, drives the `cwtools_index` extraction.

Unused dependencies (cargo-machete, low priority, do in a sweep):

- `localization`: `anyhow`, `cwtools_string_table`, `thiserror`, `tracing`
- `cache`: `anyhow`, `bytecheck`, `memmap2`
- `game`: `cwtools_string_table`, `tracing`
- `file_manager`: `globset`, `walkdir`
- `rules`, `validation`: `thiserror`; `cli`: `anyhow`; `lsp`: `serde`

`validation/Cargo.toml` mixes `{ workspace = true }` and `{ path = "..." }` styles
for its deps (`crates/validation/Cargo.toml`); normalize to workspace.
`validation/src/error_codes.rs` is a one-line `pub use cwtools_error_codes::*;`
re-export shim — harmless, but it's why `validation` looks like it owns error codes.

## Crate-boundary and layering recommendations

What to extract / move / merge:

1. New crate `cwtools_index` (or move into `rules`): lift `TypeIndex`, `VarIndex`,
   `FileIndex`, `TypeInstance`, `collect_type_instances`, `index_discovered_files`,
   `variable_defining_effects`, `collect_set_variable_names` (the `info/src/lib.rs:14-1035`
   index half) out of `info` into a crate that sits at L3 with no LSP concerns. Pull
   the shared path resolvers down with them (`dir_contains_segment:278`,
   `check_path_dir:303`, `skip_root_key_matches:347`, `type_key_filter_matches:359`,
   `starts_with_matches:371`) and repoint `validation`'s `find_type_by_path_and_key:1174`
   at them so the forked resolver (problem 3) collapses to one copy. Then `validation`
   depends on `cwtools_index` instead of `info`, the inversion is gone, and `info`
   keeps only the editor-facing `InfoService`/position/hover code (`:1037-2226`) and
   moves ABOVE `validation`. Verified clean: `info` does not depend on `game`, so no new
   cycle is introduced.

2. New crate `cwtools_driver` (L4.5, between engine and surfaces): owns the pipeline
   from problem 1. One `Session`/`Project` type that loads rules, discovers files via
   `file_manager`, builds the indexes + modifier keys + loc index, holds the prebuilt
   `enum_map`/`ScopeRegistry`, and exposes `validate_file` / `validate_all`. Both
   `cli` and `lsp` become thin: CLI maps it to argv + report formatting, LSP maps it
   to protocol messages + incremental re-validation. The game-mapping, modifier
   expansion, and discovery duplication collapse into this crate.

3. Move `build_scope_registry` (`validation/src/lib.rs:803`) into `game` next to
   `ScopeRegistry::from_hardcoded`. Registry construction is game knowledge, not
   validation logic, and it's the only thing keeping a second scope-graph source of
   truth in the validator.

4. Within `validation`, split `lib.rs` into modules along the seams in problem 2:
   `resolve.rs` (`:737-1329`), `subtype.rs` (`:1342-1576`), `scope.rs` (`:908-1136` +
   `:2442-2618`), `loc_field.rs` (`:3041-3212`), `rule_core.rs` (the validate_* family),
   and a `ctx.rs` holding a `ValidationCtx` struct to retire the `too_many_arguments`
   allows. Behavior-preserving, mechanical.

5. Split `rules_converter.rs` by output kind, lifting `field_from_string:244-580` into
   a `field_parser.rs` first (biggest single win), then `types.rs`/`enums.rs`/
   `subtypes.rs`/`scopes_links.rs`/`comment_directives.rs`. Consolidate the colour
   logic split between `rules_converter:755` and `post_process.rs:235-413`.

6. Make `localization` consume the shared loc-Game mapping and a single
   `validate_quotes` + placeholder-check helper (kill the in-crate double-definitions);
   drop its unused deps.

Dependency-graph smells to fix: `validation -> info` (problem 3); `game` declaring an
unused `string_table` dep; `validation` mixing `path`/`workspace` dep styles.

## Phased, low-risk refactor roadmap

Each phase is independently shippable and behavior-preserving. Order is by
risk/reward: cheap dedup first, structural moves last.

**Phase 0 — hygiene (hours).**
Remove unused deps (machete list), normalize `validation/Cargo.toml` to workspace
deps. No behavior change. Gives a clean baseline before moving code.

**Phase 1 — collapse the cheap duplications (1-2 days).**
Single `engine_game_to_loc_game` (move to `localization` or a small shared spot),
called from all three sites. One modifier-key expansion helper. One
placeholder-check in `localization`. Delete `lsp::lsp_glob_match`; route the LSP
ignore matching through `file_manager::glob_match`. Each is a small, testable diff
that kills a drift source.

**Phase 2 — unify file discovery (2-3 days).**
Make the LSP's `validate_entire_workspace` walk via `file_manager` instead of its own
`walk_dir`, so both entry points see the same file set and ignore rules. This is the
highest-value dedup because it makes CLI and LSP results comparable.

**Phase 3 — extract `cwtools_index` (2-3 days).**
Move the index data types out of `info` into a lower crate; repoint `validation` and
the entry points. Fixes the layering inversion. `info` shrinks to editor features.

**Phase 4 — split `validation/lib.rs` into modules + `ValidationCtx` (3-5 days).**
Mechanical module extraction along the six seams; introduce the context struct to
retire the `too_many_arguments` allows. Pure refactor, guard with the existing
validation snapshot/corpus tests (run the MD corpus before/after, diff counts).

**Phase 4.5 — unify the scope-graph source of truth (2-3 days).**
Move `build_scope_registry` (`validation/src/lib.rs:803`) into `game` next to
`from_hardcoded`, and decide one construction path (config-with-hardcoded-backfill, so
a partial `scopes.cwt` no longer silently drops scopes). Do this BEFORE the driver:
the driver needs a single scope-registry entry point, and today there are two (the
`scope_engine.rs` hardcoded tables + the config-driven `build_scope_registry`).

**Phase 5 — extract `cwtools_driver` (1 week).**
The big payoff and the riskiest. Build one `Session` type owning the engine state
(the ~12 engine fields the LSP's `DocumentState` holds, plus the CLI's `Validate`-arm
locals: game_id, rules table, RuleSet, file set, TypeIndex/var/vanilla, modifier_keys,
loc index, prebuilt `enum_map`/`ScopeRegistry`). The LSP keeps the ~5 transport-only
fields (documents, versions, workspace_uri, symbol_index, edit_generation, doc_tokens)
and wraps the Session in its lock. Session exposes both whole-workspace and single-file
entry points (both already exist as separate functions today). Port the CLI on first
(simpler, batch), then the LSP's full-workspace and incremental paths. The five
duplications (loc-map, modifier expansion, vanilla indexing, file-walk, glob) collapse
into Session internals. After this, the F# binary can be retired with confidence that
CLI and LSP share one implementation.

Phases 0-2 are pure wins with near-zero risk and should land regardless. Phases 3-5
are the structural payoff and each needs the corpus-diff guard (validate the MD mod
before and after, assert error/warn/info counts and hashes are unchanged).

## First concrete implementation step

Phase 0 + the Phase 1 game-mapping dedup, in one PR: delete the two extra
`*_to_loc_game` copies, keep one in `localization`, and remove the machete-flagged
unused deps. Smallest possible change that kills a real drift source and proves the
corpus-diff guard works before the bigger moves.
