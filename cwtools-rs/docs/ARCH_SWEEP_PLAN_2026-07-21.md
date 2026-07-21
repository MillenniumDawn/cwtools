# Architecture Sweep Implementation Plan (2026-07-21)

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship the highest-value findings from the 7-agent architecture review: two bug fixes, a dead-code purge, hot-path perf wins, three CLI features, and v1 of the cleanup capability (suggested fixes + `cwtools fix` + LSP quick-fix code actions).

**Architecture:** Three phases. Phase 1 is seven parallel sonnet tasks on disjoint crates (dedup, dead code, small perf, CLI flags). Phase 2 is four parallel opus tasks (fix-payload engine + `fix` subcommand, parser hardening, cache bounds, walker dedup). Phase 3 sequences the LSP code-action surface on top of Phase 2's fix types, then stretch perf work, then release chores.

**Tech Stack:** Rust workspace (edition 2024), tower-lsp 0.20, rkyv cache, rayon. Engine v2.1.0 -> v2.2.0.

## Global Constraints

- **Corpus guard is mandatory.** Baseline: Kaiserreich at `/mnt/Linux/github-projects/Kaiserreich-4-Development` validates to 1605 errors / 45 warnings with the CSV saved at `$SCRATCH/kr_baseline.csv` (orchestrator holds the path). After each phase gate: `cargo build --release -p cwtools_cli && ./target/release/cwtools validate --game hoi4 --directory /mnt/Linux/github-projects/Kaiserreich-4-Development --rules /mnt/Linux/github-projects/cwtools-hoi4-config/Config --vanilla "/home/kmccormick/.steam/steam/steamapps/common/Hearts of Iron IV" --report-type csv --output-file /tmp/kr_after.csv`, then `LC_ALL=C sort` both CSVs and diff. Must be empty. Any task that would move it is marked behavioral below and must justify the exact diff.
- **Test floor:** `cargo test --workspace` passes 780 at baseline. Never goes down; new tests are added where tasks say so.
- **Lint gates:** `cargo fmt --all` and `cargo clippy --workspace --all-targets` produce zero warnings. Never silence a lint to pass.
- **Commit mechanics:** the pre-commit hook stashes unstaged files and breaks partial commits. Run fmt+clippy manually, then commit per-task with `--no-verify`, staging only that task's files. No attribution trailers.
- **Line numbers in this plan are evidence pointers from the review pass.** Verify each against the current tree before editing; nearby drift is expected.
- Findings tagged behavioral affect only LSP-published diagnostics or CLI exit codes, never the corpus CSV, unless stated otherwise.

---

## Phase 1 — parallel sonnet tasks (disjoint crates)

### Task 1: LSP indexing dedup, SymbolIndex removal, micro-perf [sonnet]

**Files:**
- Delete: `crates/lsp/src/symbols.rs`
- Modify: `crates/lsp/src/validate.rs` (~983-1010, ~362-398), `crates/lsp/src/scan.rs` (~447), `crates/lsp/src/main.rs` (~778-799, ~1203-1243), `crates/lsp/src/navigation.rs` (~268-332, ~594-621)

**Findings:** L1, L2, L5, L7, L8 from the review.

- [ ] **Step 1 (L2, behavioral bug fix):** `parse_and_validate` inlines a drifted subset of `index_parsed_file` and omits the `collect_subtype_instances` merge, so open files lose `<type.subtype>` membership while being edited. Replace the inlined block at `validate.rs:994-1010` with a call to `index_parsed_file` (the shared helper used by scan and did_close). Add a regression test if the crate's test layout allows exercising `parse_and_validate` against a fixture with a subtype archetype; otherwise document the manual verification in the commit message.
- [ ] **Step 2 (L1):** Delete `SymbolIndex` (`symbols.rs`): `definitions` has no reader, `references` only matches `.cwt`-syntax `<type>` tokens that never appear in game script, and its one consumer at `navigation.rs:272-273` is redundant with `info.find_references` directly above. Remove the module, its field on the server state, all `index_document`/`clear_document` call sites (did_open/did_change/scan/did_close/prune), and the redundant navigation branch. Run the full LSP test suite after.
- [ ] **Step 3 (L5):** `symbol_impl` (`navigation.rs:594-621`) allocates `inst.name.to_lowercase()` per instance across the whole type map. Make the case-insensitive substring test allocation-free (iterate `char::to_ascii_lowercase`; names are ASCII-dominant, and the existing behavior is `to_lowercase().contains(&query)` — preserve match results for ASCII and keep a slow-path fallback for non-ASCII names so results do not change). Early-return the existing capped scan when the query is empty.
- [ ] **Step 4 (L7, L8):** Collapse the double `documents.lock()` in `collect_use_sites` (`navigation.rs:298-332`) into one guard scope. Fold the paired `config.read()` calls in `resolve_at_cursor` (`main.rs:778-799`) and `document_highlight_impl` (`navigation.rs:471-473`) into single guards.
- [ ] **Step 5:** `cargo test -p cwtools_lsp`, then fmt+clippy. Commit: `lsp: dedup open-file indexing onto index_parsed_file, drop dead SymbolIndex, tighten locks`

**Interfaces:** Produces: `parse_and_validate` now calls `index_parsed_file(...)`; `SymbolIndex` no longer exists (T9 touches `validate.rs` later and must not resurrect it).

### Task 2: Dead-code sweep [sonnet]

**Files:**
- Delete: `crates/info/src/inline_expansion.rs`
- Modify: `crates/info/src/lib.rs` (mod decl at :7; fields ~131-132, pushes ~801, ~907, ~913), `crates/driver/src/lib.rs` (~358-362), `crates/error_codes/src/lib.rs`, `crates/string_table/src/string_table.rs` (:12), `docs/ERROR_CODES.md`

**Findings:** D1, D2/L4, D3, V5, PP7.

- [ ] **Step 1 (D1):** Delete `crates/info/src/inline_expansion.rs` (~500 lines, zero callers workspace-wide) and its `pub mod` decl. Note: T10 cites `inline_expansion.rs:132` as an unchecked-index example; deletion removes that consumer, which is fine.
- [ ] **Step 2 (D3):** Remove `FileInfo.effect_blocks`, `trigger_blocks`, `top_level_keys` and their population code (`record_top_level_key`, `record_effect_trigger_block` if now unused). Grep for readers first to confirm zero.
- [ ] **Step 3 (D2):** Remove `Session::validate_file` (no callers; CLI uses `validate_all`). Keep `prepared()`. Trim the module doc sentence that rationalizes it.
- [ ] **Step 4 (V5):** Delete the never-emitted, never-pending error-code consts `CW002, CW101, CW102, CW103, CW241, CW249, CW998, CW999` and their entries in `docs/ERROR_CODES.md` (add a one-line note in the doc's header that superseded F# codes were dropped; CW101/102/103 -> CW262/263, CW241 -> CW262-265). Do NOT touch the documented emission-pending codes (CW220/221/228/230/231/233/239/273).
- [ ] **Step 5 (PP7):** Remove `StringId::NULL` (never read; the two debug_assert message strings mention it textually but do not reference it).
- [ ] **Step 6:** `cargo test --workspace`, fmt+clippy. Commit: `dead code: drop inline_expansion, FileInfo block vecs, Session::validate_file, orphan error codes, StringId::NULL`

### Task 3: Localisation bug fixes [sonnet]

**Files:**
- Modify: `crates/localization/src/pipeline.rs` (~120-159, ~186-188), `crates/localization/src/service.rs` (~156-167), `crates/localization/src/scope_validation.rs` (~87-92, ~115, ~160-179)
- Test: `crates/localization/src` unit tests or the crate's existing test files

**Findings:** B1 (real bug), P1.

- [ ] **Step 1 (B1, behavioral for CK2/VIC2 only):** CSV-derived `LocFile`s get `language_prefix: "english"` (etc.), which `key_to_language` never matches (it only knows `l_english` forms), so CW256 fires on every CSV loc file. Write a failing test: build a CSV-backed loc service, run `build_diagnostics`, assert no CW255/256/257. Then fix: tag `LocFile` with its source format (or gate `lang_header_diagnostic` on the file extension) so the YAML-only header check skips CSV files. HOI4/Stellaris corpus output unaffected (YAML only) — corpus guard must stay clean.
- [ ] **Step 2 (P1):** `validate_loc_commands` computes `game_to_engine(data.game)` per loc-referencing leaf even when `data.registry.is_some()` makes it dead, and for Ck2/Vic2/Custom that fires `tracing::warn!` per leaf. Skip the computation when `registry.is_some()`; hoist the no-engine-mapping warning so it logs once per run (e.g. a `Once`/session-level guard), not per leaf.
- [ ] **Step 3:** `cargo test -p cwtools_localization`, fmt+clippy. Commit: `loc: skip YAML lang-header check for CSV files (CW256 FP), stop per-leaf engine warn`

### Task 4: CLI features [sonnet]

**Files:**
- Modify: `crates/cli/src/main.rs` (Validate args ~429, exit/severity ~205-215 and ~554-561, Loc command ~741-779)

**Findings:** Ft1, Ft2, Ft3.

- [ ] **Step 1 (Ft1):** Add repeatable `--loc-language <lang>` to `Validate`, parsed into `Vec<Lang>` (reuse the existing `Lang` parse used by the LSP config), passed as `SessionConfig.loc_languages: Some(...)` instead of the hardcoded `None`. Unknown language = clap error listing valid values.
- [ ] **Step 2 (Ft3):** Add `--min-severity <error|warning|info|hint>` to `Validate`, filtering `diags` before report render and before hash output (same placement as the existing `ignore_hashes` filter). Default keeps current behavior (no filter).
- [ ] **Step 3 (Ft2, behavioral for `loc` exit codes):** Route `Commands::Loc` through the same severity-aware exit-code helper `validate` uses (exit 1 only on Error-severity), instead of `exit(1)` on any diagnostic including Information-severity CW234. Keep the plain-text output format unchanged. Note the exit-code change for the changelog task (T15).
- [ ] **Step 4:** Add/extend CLI arg tests if the crate has them; otherwise verify by running the binary against a testfiles fixture with each new flag and record output in the commit message. `cargo test -p cwtools_cli`, fmt+clippy. Commit: `cli: --loc-language and --min-severity for validate, severity-aware exit for loc`

**Interfaces:** Consumes `SessionConfig.loc_languages` (`crates/driver/src/lib.rs:91`, already plumbed). T8 adds a `fix` subcommand to the same file later; keep `main.rs` command wiring tidy.

### Task 5: Foundation robustness [sonnet]

**Files:**
- Modify: `crates/parser/src/parser.rs` (:69), `crates/file_manager/src/file_manager.rs` (~778-796)
- Test: parser unit tests; existing file_manager tests at ~:1016, :1069

**Findings:** PP2 (verified panic), PP4.

- [ ] **Step 1 (PP2):** Write a failing test: parse a single line of >65,535 chars; debug build currently panics with `attempt to add with overflow` at `parser.rs:69`. Fix: `self.col = self.col.saturating_add(1)` (keeps `u16` wire format, no cache version bump). Test asserts parse completes without panic.
- [ ] **Step 2 (PP4):** `walk_dir_generic` calls `compute_logical_path_with_root` (allocating) on every directory only to compute `root_level`. Thread an `is_root_level: bool` through the recursion instead (true only for direct children of the walk root). Existing tests `exclude_dir_patterns_skips_matching_dirs` and `root_resources_skipped_but_common_resources_indexed` must stay green.
- [ ] **Step 3:** `cargo test -p cwtools_parser -p cwtools_file_manager`, fmt+clippy. Commit: `parser: saturate col past u16 max; file_manager: drop per-dir path alloc in walk`

### Task 6: Validation quick perf [sonnet]

**Files:**
- Modify: `crates/validation/src/lib.rs` (~121-148, ~351-352), `crates/validation/src/position.rs` (~114, ~165, ~261-279), `crates/validation/src/resolve.rs`

**Findings:** V1, V4. Corpus-risk none; guard must stay byte-identical.

- [ ] **Step 1 (V1):** `validate_prepared` builds `path_candidates` then calls `find_type_by_path`, which rescans every `ruleset.types` again. Replace with `find_type_from_candidates(&path_candidates, None)` (already imported). Do the same in `rules_at_pos`.
- [ ] **Step 2 (V4):** Extract the duplicated grandchild refinement block (`find_grandchild_type` + `None` -> `type_key_filter` fallback gate) from `validate_wrapper_grandchildren` (lib.rs ~121-148) and `descend_wrapper` (position.rs ~261-279) into one helper in `resolve.rs` (e.g. `refine_grandchild_type(...) -> Option<(&TypeDefinition, &[...])>`, `None` = skip). Both callers delegate.
- [ ] **Step 3:** `cargo test -p cwtools_validation`, fmt+clippy. Commit: `validation: reuse path candidates in dispatch, unify grandchild refinement`

### Task 7: Rules/index perf + config diagnostics [sonnet]

**Files:**
- Modify: `crates/index/src/type_index.rs` (~370-386), `crates/rules/src/rules_types.rs` (~346-379), `crates/rules/src/rules_converter/comment_directives.rs` (~107-127, ~143-152)
- Test: rules crate unit tests

**Findings:** R1, R10 (path-dedup half only), R8.

- [ ] **Step 1 (R1):** `instances_in_file` iterates every bucket of `self.map` filtering by uri. Use the `file_buckets` reverse map (uri -> type names, added by PR #74) to visit only that file's buckets, then filter entries by uri. Preserve result ordering if callers are order-sensitive (check callers; sort if the old iteration order was relied on).
- [ ] **Step 2 (R10):** `reindex` has two byte-identical `paths_lower`/`path_file_lower`/`path_ext_lower` closures (types + complex_enums). Extract one `normalize_path_options(&mut PathOptions)` helper. Skip the merge/lowercase-storage half of R10 (needs a struct change; deferred).
- [ ] **Step 3 (R8):** Malformed `## cardinality` bounds currently `unwrap_or` into silently-wrong semantics and unknown `## severity` values fall through to `None`. Emit a rules diagnostic (whatever `RuleParseError`/load-warning channel `config_validation`/`ruleset_loader` already uses) for an unparseable cardinality bound and an unrecognized severity value. Write tests: `## cardinality = 0..n` with a typo'd bound and `## severity = warn` (invalid; correct is `warning`) each produce one diagnostic and do not change the parsed rule beyond current behavior. Well-formed config parses identically — corpus guard unaffected.
- [ ] **Step 4:** `cargo test -p cwtools_rules -p cwtools_index`, fmt+clippy. Commit: `index: instances_in_file via reverse map; rules: dedup path lowering, diagnose malformed cardinality/severity`

### Phase 1 gate

- [ ] `cargo fmt --all && cargo clippy --workspace --all-targets` clean, `cargo test --workspace` >= 780 pass.
- [ ] Corpus guard: byte-identical vs baseline (sorted diff empty).
- [ ] Commit each task separately (files are disjoint) with `--no-verify`.

---

## Phase 2 — parallel opus tasks (disjoint files)

### Task 8: Cleanup v1 — SuggestedFix engine + `cwtools fix` subcommand [opus]

**Files:**
- Create: fix types module (put `SpanEdit`/`SuggestedFix` next to `SourceRange`, i.e. in `cwtools_parser` — e.g. `crates/parser/src/fix.rs` — since both validation and localization must reference it; add the parser dep to `cwtools_localization` if missing, else re-export)
- Modify: `crates/validation/src/common.rs` (:12-22), `crates/validation/src/rule_core/children.rs` (~101), `crates/validation/src/per_game/hoi4.rs` (~64), `crates/validation/src/per_game/stellaris.rs` (~135), `crates/validation/src/per_game/structural.rs` (~140, ~151), `crates/validation/src/loc_field.rs` (~109), `crates/localization/src/validation.rs` (~38-43, ~93), `crates/localization/src/pipeline.rs` (:19-27), `crates/cli/src/main.rs`
- Test: validation crate tests + a report-inertness guard test

**Findings:** Scout report §4/§5, V9, L3 (engine half).

- [ ] **Step 1 — types:**
```rust
pub struct SpanEdit { pub range: SourceRange, pub replacement: String }  // empty replacement = delete
pub struct SuggestedFix { pub title: String, pub edits: SmallVec<[SpanEdit; 1]> }
```
Add `pub fix: Option<SuggestedFix>` to `ValidationError` (default `None`; keep all existing constructors' signatures by adding a `with_fix(...)` builder), and carry a `fix` through `LocValidationError` -> `LocDiagnostic`.
- [ ] **Step 2 — inertness guard test (write first, must pass before and after):** assert `error_hash` and the CLI `Diag` construction (`crates/cli/src/main.rs` ~485-517) read no fix data; add a test that a `ValidationError` with and without a fix produces identical hash and identical csv/json/cli report rows.
- [ ] **Step 3 — populate tier-1 sites** (each has the AST node, hence the end span, in scope at the emit line):
  - CW253 (`stellaris.rs:135`): rename key token to `set_name`; range = `[block.range.start, start + key_char_len]`.
  - CW282 (`children.rs:101`): delete the leaf (`leaf.pos` full range), title "Remove redundant default".
  - CW280 (`hoi4.rs:64`): delete the block (`block.range`).
  - CW121 / CW281 (`structural.rs:140/:151`): delete the empty block.
  - CW122 (`loc_field.rs:109`): replace the quoted value with unquoted `key_raw`.
  - CW268 (`localization/src/validation.rs:93`): wrap the value in quotes (derive end from `entry.desc` length).
  Each site: smallest correct span, single-line edits only. If a site's node lacks a clean span at the emit point, skip it and note why rather than approximating.
- [ ] **Step 4 — CLI `fix` subcommand:** mirrors `validate`'s connection args plus `--apply` (default dry-run), `--code CWxxx` (repeatable filter). Run `validate_all()` + `loc_project_diagnostics()`, keep diagnostics with `fix.is_some()`, group edits per file, apply sorted by range descending (later-first so offsets do not shift), converting (line, char-col) -> byte offset over the file text (build a line-starts array; columns are char-based so walk chars within the line). Dry-run prints a unified-diff-style preview per file; `--apply` writes files. Skip and warn on overlapping edits within a file. Exit 0 in dry-run; with `--apply`, exit 0 on success.
- [ ] **Step 5 — tests:** for each tier-1 code: fixture text -> validate -> diagnostic carries the expected fix -> applying the fix to the text yields the expected output and revalidation no longer emits that code. Plus one multi-edit-per-file ordering test and one overlap-skip test at the CLI apply layer.
- [ ] **Step 6:** Corpus guard (report output must be byte-identical — the guard test from step 2 backs this). `cargo test --workspace`, fmt+clippy. Commit: `cleanup v1: SuggestedFix payloads on 7 diagnostics, cwtools fix subcommand (dry-run/apply)`

**Interfaces:** Produces `SpanEdit`/`SuggestedFix` types and `ValidationError.fix`/`LocDiagnostic.fix` — T12 (LSP code actions) consumes exactly these. Produces `Commands::Fix` in the CLI.

### Task 9: Parser quoted-key hardening + ParseError cleanup [opus]

**Files:**
- Modify: `crates/parser/src/parser.rs` (~170-227 quoted-key branch, ~331-405 quoted-value for the shared helper, constructors at ~391, ~549, ~599, ~701), `crates/parser/src/ast.rs` (:4-9), consumers `crates/lsp/src/validate.rs` (~241), `crates/driver/src/lib.rs` (~489), `crates/rules/src/ruleset_loader.rs` (~117)
- Test: parser regression tests (mirror `names_file_has_no_false_unclosed_clause`)

**Findings:** PP1 (verified swallow), PP6.

- [ ] **Step 1 (PP1, failing test first):** parsing `"foo\nbar = 1\n" = 5\n` currently yields zero errors and one leaf whose key is a multi-line string, silently swallowing `bar = 1`. Write tests asserting: (a) an unclosed quoted key terminates at end-of-line, (b) an "unclosed quoted string" error is pushed, (c) the following well-formed statement parses as its own leaf. Then port `parse_quoted_value`'s newline-terminates + unclosed-error logic to the quoted-key branch, factoring the shared escape-scanning loop into one helper parameterized by stop-at-newline/error-push so the two copies cannot drift again.
- [ ] **Step 2 (PP1 corpus):** well-formed input must parse identically. Run the corpus guard now (not just at the gate) since this is the parser: byte-identical required.
- [ ] **Step 3 (PP6):** `ParseError::Pos`'s first field is always `""` and every consumer discards it. Drop the field, update the 4 constructors and 3 consumers. Check the derived `Display` format string still renders sensibly (`{0}:{1}: {2}` form).
- [ ] **Step 4:** `cargo test --workspace` (parser changes ripple), fmt+clippy. Commit: `parser: unclosed quoted keys terminate at newline with error; drop dead ParseError filename field`

### Task 10: Cache load bounds validation [opus]

**Files:**
- Modify: `crates/cache/src/convert.rs` (~107-116, and the tautological asserts at ~65, ~77), `crates/cache/src/io.rs` (~64-105) as needed
- Test: cache crate tests with corrupted bytes

**Findings:** PP5.

- [ ] **Step 1 (failing test first):** craft a cached file whose `CachedChild::Leaf(u32)` index exceeds `leaves.len()`; loading currently succeeds and any consumer indexing the arena panics. Test asserts load returns a `CacheError` instead.
- [ ] **Step 2:** After rebuilding children in `archived_to_arena`/`children_from_archived`, validate every `Child::Leaf/LeafValue/Comment` index against the corresponding arena vector length once at the load boundary; return `CacheError` on violation (existing miss/re-parse fallback handles it). Keep it one pass over the child lists — no per-lookup overhead downstream. Replace the tautological `assert_eq!` scaffolding with the real check or delete it.
- [ ] **Step 3:** `cargo test -p cwtools_cache`, fmt+clippy. Commit: `cache: reject out-of-bounds child indices at load instead of panicking downstream`

### Task 11: Index walker dedup [opus]

**Files:**
- Modify: `crates/index/src/collect.rs` (~110-156 vs ~202-238)

**Findings:** R5 (R6 explicitly out of scope for this task — single-walk fusion is deferred).

- [ ] **Step 1:** `collect_skip_root_child` and `walk_instance_node` are near-identical skip-root-key navigators differing only at the leaf (build `TypeInstance` vs invoke callback). Extract one navigation skeleton parameterized by a leaf visitor (closure or small trait); both entry points delegate. No behavior change; existing index tests must pass unchanged.
- [ ] **Step 2:** Corpus guard byte-identical (instance collection feeds validation). `cargo test -p cwtools_index`, fmt+clippy. Commit: `index: unify skip-root-key navigation behind one visitor skeleton`

### Phase 2 gate

- [ ] fmt+clippy clean, `cargo test --workspace` green, corpus guard byte-identical.
- [ ] Per-task commits with `--no-verify`.

---

## Phase 3 — sequenced

### Task 12: LSP quick-fix code actions [opus] (requires T8)

**Files:**
- Modify: `crates/lsp/src/config.rs` (ServerCapabilities ~357), `crates/lsp/src/validate.rs` (`validation_error_to_diagnostic` ~286, `data` currently `None` at ~235), `crates/lsp/src/main.rs` (handler registration)
- Create: `crates/lsp/src/code_action.rs`

**Findings:** Scout §5, L3.

- [ ] **Step 1:** Declare the capability: `code_action_provider: Some(CodeActionProviderCapability::Options(CodeActionOptions { code_action_kinds: Some(vec![CodeActionKind::QUICKFIX]), resolve_provider: Some(false), ..Default::default() }))`.
- [ ] **Step 2:** When `err.fix` is `Some`, serialize `{title, edits: [{range, replacement}]}` into `Diagnostic.data` (both validation and loc paths). Ranges convert with the existing line-1 convention and the negotiated position encoding (`config.rs` ~327-341) — reuse whatever helper hover/rename use for SourceRange -> lsp Range.
- [ ] **Step 3:** `code_action` handler: for each `params.context.diagnostics` entry with a fix payload in `data`, emit `CodeAction { title, kind: QUICKFIX, diagnostics: vec![diag], edit: Some(WorkspaceEdit { changes: {uri -> vec![TextEdit]} }) }`. Model the WorkspaceEdit construction on `rename_impl` in `navigation.rs`.
- [ ] **Step 4 (stretch, only if the above lands cleanly):** a non-edit code action on CW100 diagnostics titled "Generate missing localisation" that invokes the existing `genlocall` command machinery (`config.rs` ~772-824) via `execute_command`.
- [ ] **Step 5:** Test: unit-test the handler mapping (diagnostic-with-data -> CodeAction) directly. `cargo test -p cwtools_lsp`, fmt+clippy. Commit: `lsp: quick-fix code actions from SuggestedFix payloads`

### Task 13: Wire type_key_prefix in instance collection [opus]

**Files:**
- Modify: `crates/index/src/collect.rs` (collector gate, ~124-145 pre-T11 numbering — re-locate after T11's refactor), `crates/rules/src/rules_converter/types.rs` (~121-123, read side only)

**Findings:** R4. HOI4 config has zero uses (verified), so corpus-inert; Stellaris/other configs may use it — this is a correctness fix toward F# parity.

- [ ] **Step 1:** Confirm semantics before coding: in cwtools .cwt spec, `type_key_prefix = X` means instance keys carry a literal prefix `X` (key must start with it; the instance NAME is the key with the prefix intact — check the F#-era behavior via the cwtools guidance docs in the config repos or github.com/cwtools/cwtools docs; if genuinely ambiguous, implement prefix-must-match filtering only, which is the conservative reading, and note the decision).
- [ ] **Step 2:** Apply `key_prefix` in the unified walker's instance gate (from T11) next to `type_key_filter_matches`/`starts_with_matches`. Add a collect test: a type with `type_key_prefix` only collects prefixed keys.
- [ ] **Step 3:** Corpus guard byte-identical (0 HOI4 uses). `cargo test -p cwtools_index -p cwtools_rules`, fmt+clippy. Commit: `index: honor type_key_prefix when collecting instances`

### Task 14 (stretch): children.rs hot-path fusion [opus]

**Files:**
- Modify: `crates/validation/src/rule_core/children.rs` (~221-236, ~650-653)

**Findings:** V3, V7. Only start after T8 is merged (same file). Abandon rather than force if the corpus diff is non-empty.

- [ ] **Step 1 (V7):** `enforce_cardinality` rebuilds `rule_keys_lower` (Vec of lowercased owned Strings) per block from ruleset-static keys. Precompute per rule-list once (at ruleset build or first use) or lowercase into a reused stack buffer during the aggregation. Measure with the existing bench/profiling setup if quick; otherwise rely on the corpus guard plus test suite.
- [ ] **Step 2 (V3):** Fuse `count_children` and `validate_each_child` into one pass that tallies and validates (phase-2 validation does not read phase-1 counts; only `enforce_cardinality` does). Preserve emission content exactly; row order may shift (guard diff is sorted, so only content changes would surface).
- [ ] **Step 3:** Corpus guard byte-identical, `cargo test -p cwtools_validation`, fmt+clippy. Commit: `validation: fuse child count+validate passes, stop rebuilding static lowercased rule keys`

### Task 15: Release chores [sonnet]

**Files:**
- Modify: `CHANGELOG.md` (repo root), `cwtools-rs/Cargo.toml` (workspace version 2.1.0 -> 2.2.0)

- [ ] **Step 1:** Bump workspace version to 2.2.0. `cargo build` to refresh the lockfile.
- [ ] **Step 2:** Changelog entry for 2.2.0 matching the existing heading/voice conventions in CHANGELOG.md: cleanup v1 (`cwtools fix`, LSP quick fixes), new validate flags, loc exit-code change (call out as behavioral), CW256 CSV fix, subtype-membership LSP fix, parser quoted-key hardening, perf and dead-code notes.
- [ ] **Step 3:** Full final gate: fmt, clippy, `cargo test --workspace`, corpus guard. Commit: `v2.2.0: changelog and version bump`

---

## Deferred (needs the maintainer's call — do NOT implement in this sweep)

- **Wire `## error_if_only_match` (V6/R3):** the HOI4 config uses it 69 times; wiring replaces raw only-match errors with custom messages and will visibly move the corpus. Correct per F# parity, but the diff needs eyes-on review. CW272 is its emission home.
- **Delete the multi-mod discovery subsystem (file_manager)::** ~150 lines + tests, zero callers, but it is real F#-parity surface (playset support). Delete or wire — product call.
- **Delete remaining unread directive fields (R3):** `graph_related_types`, `reference_details`, subtype `display_name`/`abbreviation` are spec constructs the engine ignores while completion advertises them. Wiring display_name into completion/hover may be the better move than deletion.
- **End-position plumbing (V8, R9):** `ValidationError.end` would give precise LSP squiggles (~30 emit sites); index `SourceLocation.end` (+ cache version bump) unlocks rename/delete-definition cleanups. Not needed by cleanup v1 (fixes carry their own ranges).
- **R6 single-walk index fusion:** high value, large risk; do as its own corpus-guarded branch.
- **R2 ruleset-load AST pinning**, **L6 revalidate via stored AST**, **L9 inlay hints**, **V10/V11 did-you-mean + related-location diagnostics** (V11 pairs with a small edit-distance helper and upgrades CW262/263/240 fixes), **Ft2 report-type/hash parity for `loc`**, **R10 lowercase-name storage on index entries**.
