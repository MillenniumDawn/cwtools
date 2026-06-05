# Spec: `refactor/rust-rewrite` Cleanup, Simplification, and Profiling

**Status:** Draft for review
**Branch:** `refactor/rust-rewrite` (30 commits ahead of `origin/master`, 20,021 LOC added across 67 files)
**Workspace:** `cwtools-rs/`
**Spec path:** `specs/rust-rewrite-cleanup.md`

## 1. Goals

Three concurrent concerns, in order of priority:

1. Make the Rust rewrite easier to read by trimming verbose and F#-port-only comments, while keeping the public-API doc comments.
2. Fix the high-severity hot paths found in the new code (LSP server responsiveness, ruleset post-processing, info indexing) without changing observable behavior.
3. Wire up a real profiler using the workspace's already-declared but unused `tracing` dependency, so future regressions are visible at a glance.

Out of scope: behavior changes, new validation rules, cache format changes, F# compatibility work.

## 2. Constraints and conventions

Per `CLAUDE.md` (project memory at `/home/kmccormick/.claude/CLAUDE.md`):

- No em-dashes. Use periods, commas, or parens.
- Terse prose. Short lines, plain words. Cut hedging and filler.
- No "authoritative / canonical / seamless / robust" AI tells.
- No "Generated with Claude Code" footers. No `Co-Authored-By: Claude`.
- Match existing comment style in the touched files. The `// ── Section ──` dividers in `info/lib.rs` and `post_process.rs` are good navigation aids. Keep them.
- Use `file_path:line_number` when referencing code locations.

Coordination with the other agent:

- Before each PR, run `git log --since=2.weeks -- <file>` for every file in the PR's touch set.
- If the other agent has committed to any of those files in the last two weeks, stop and ask the user before proceeding.
- Avoid editing the body of `children_to_rules` in `cwtools-rs/crates/rules/src/rules_converter.rs` and the rewrite logic in `cwtools-rs/crates/rules/src/post_process.rs` while the other agent is working, since these are the most likely conflict zones.

## 3. Findings summary

The research agent produced a categorized list of 10 High, 15 Medium, and 30 Low findings. Top items per file:

| File | H | M | L | Top issue |
|---|---|---|---|---|
| `crates/lsp/src/main.rs` | 4 | 2 | 5 | Sync work on the async runtime; O(N×M) `scan_use_sites` |
| `crates/rules/src/post_process.rs` | 2 | 1 | 2 | Double deep-clone of `single_aliases` per loop iteration |
| `crates/info/src/lib.rs` | 2 | 3 | 5 | Dead loop in `collect_defined_variables`; O(N) `is_any_instance` |
| `crates/rules/src/rules_converter.rs` | 1 | 5 | 5 | Quadratic comment-before-child scan |
| `crates/localization/src/loc_string.rs` | 0 | 1 | 4 | `Vec<char>` allocation for `parse_loc_elements` |
| Other crates | 0 | 2 | 8 | Mostly idiomatic cleanups |

Full findings list is in section 9.

## 4. PR plan

Five PRs, independently mergeable. Each lists files, conflict risk, and verification.

### PR 1: Comment cleanup (no logic change)

**Touches:** `rules/src/rules_converter.rs`, `rules/src/rules_types.rs`, `rules/src/post_process.rs`, `info/src/lib.rs`, `info/src/inline_expansion.rs`, `lsp/src/main.rs`, `localization/src/yaml_parser.rs`, `localization/src/scope_validation.rs`, `localization/src/loc_string.rs`, `validation/src/lib.rs`.

**Changes:**

- Replace multi-line F# cross-reference headers with one-line equivalents.
- `crates/rules/src/rules_converter.rs:5-7` (float sentinels): `// ±1e12 sentinel for unranged float; 1e6 was too narrow (build costs, populations).`
- Drop the 24-line `crates/lsp/src/main.rs:103-114` "NOT PORTED" block to a single short note pointing at the relevant doc.
- Trim `crates/localization/src/yaml_parser.rs:1-14` and `crates/localization/src/scope_validation.rs:1-28` F# preamble blocks.
- Collapse `// ── Item N: ... ──` headers in `crates/info/src/lib.rs` to terse single-line `// Item N: ...` where the F# name is no longer useful.
- Keep all `///` doc comments on public items. Keep section dividers that aid navigation.

**Conflict risk:** near zero. Pure comment edits.

**Verify:** `cargo check --workspace`, `cargo clippy --workspace`, `cargo fmt --check`. No behavior change. Spot-check a few comments by eye against `CLAUDE.md` style rules.

### PR 2: LSP hot-path fixes (H1, H2, H5, H8)

**Touches:** `crates/lsp/src/main.rs` (mostly), `crates/lsp/Cargo.toml`, workspace `Cargo.toml`.

**Changes:**

**H1, async runtime hygiene:**

- `main.rs:1696-1840` (`validate_entire_workspace`): wrap `walk_dir` and per-file `parse_string` calls in `tokio::task::spawn_blocking`. Add `tokio::task::yield_now().await` between files.
- `main.rs:1847` (`index_document`): strip the `async` keyword; the only `.await` is `client.log_message`, which can stay at the LSP layer.

**H2, lock contention:**

- Add `parking_lot = "0.12"` to `Cargo.toml` workspace deps.
- Switch LSP `Backend` mutexes to `parking_lot::Mutex` (no poisoning, smaller, faster).
- In each handler (`hover`, `completion`, `goto_definition`, `references`, `prepare_rename`, `rename`, `parse_and_validate`), take one lock, snapshot the needed data into local variables, drop the guard, then do the work.
- Combine the double-locks in `completion` (`main.rs:1088`, `main.rs:1096-1098`, `main.rs:1127, 1148`) into a single early lock.

**H5, `scan_use_sites` quadratic:**

- `main.rs:2079-2211` and `main.rs:2173-2202` (`is_type_ref_leaf`).
- Add `type_by_name: HashMap<String, &TypeDefinition>` to `RuleSet`, built in `reindex()` (see PR 3, M14). Pass it into `is_type_ref_leaf` so the inner `ruleset.types.iter().find(...)` becomes O(1).
- Cache `(leaf_key, logical_path) -> bool` per-ruleset, since neither side changes during a session.

**H8, `build_modifier_keys` per file:**

- `main.rs:1985`. Add `modifier_keys: Arc<parking_lot::RwLock<HashSet<String>>>` to the `Backend` state. Recompute when `ruleset` changes. Pass `&modifier_keys` into `validate_parsed`.

**Conflict risk:** medium. The other agent may be editing `lsp/src/main.rs`. Run `git log` first; stop and ask the user on conflict.

**Verify:** `cargo test -p cwtools_lsp`. Manual: `cargo run -p cwtools_cli -- validate` on a small mod. Confirm LSP `didOpen` and `completion` still respond on a representative mod.

### PR 3: Rules + Info hot-path fixes (H3, H4, H6, H7, H9, H10, M6, M14)

**Touches:** `crates/rules/src/post_process.rs`, `crates/rules/src/rules_converter.rs`, `crates/rules/src/rules_types.rs`, `crates/info/src/lib.rs`.

**Changes:**

**H3, dead loop in `collect_defined_variables`:**

- `info/src/lib.rs:331-551`. Delete the `for (_, rules) in &ruleset.values { let _ = rules; }` block at lines 384-387. If `collect_defined_variables` is unused outside the dead block, delete the function entirely. Otherwise, reduce it to the `@var` collection pass only.

**H4, repeated AST walks:**

- `info/src/lib.rs` (`index_file_with_path`, `find_pos_in_children`, `enclosing_key_path`).
- Introduce a `FileSummary { top_keys, at_vars, type_refs, saved_event_targets }` struct built once at index time and stored on `FileInfo`. Position queries become lookups, not recursive walks.

**H6, `replace_single_aliases` deep clone:**

- `post_process.rs:26-56`. Hoist the `map: Vec<(String, NewRule)>` outside the loop. Replace the `before` deep-clone with a `mut changed: bool` flag set by the inner recursion.

**H7, post-process rebuilds on no-op:**

- `post_process.rs:80-141, 188-194, 350-368` (`inline_rules_list`, `expand_colour_in_list`, `expand_ignore_in_list`).
- Replace `std::mem::take(rules)` + unconditional push with an in-place `iter_mut` pass that only allocates when a rewrite is actually needed. Use `Vec::retain_mut` or a manual pending-vec pattern.

**H9, quadratic comment scan:**

- `rules_converter.rs:13-35` (`collect_comments_before_child`).
- Add `precompute_comments(children: &[Child], ast: &ParsedFile) -> Vec<Option<Vec<String>>>` helper. Replace per-child `collect_comments_before_child` calls with O(1) index lookups at every call site (`ast_to_ruleset`, `children_to_rules`, `process_type_node`, `parse_localisation_block`, `parse_modifiers_block`).

**H10, `check_path_dir` lowercases per call:**

- `info/src/lib.rs:107-138`. Add a `precompute: fn(&mut PathOptions)` (or extend `RuleSet::reindex`) to lowercase patterns once.

**M6, `is_any_instance` linear scan:**

- `info/src/lib.rs:70-72`. Add `name_to_types: HashMap<String, HashSet<String>>` to `TypeIndex`. Update on `merge` and `remove_file`. Lookup is `name_to_types.get(name).is_some()`.

**M14, linear `ruleset.types` scan:**

- `info/src/lib.rs:263-313`, `lsp/main.rs:340`, `lsp/main.rs:2189`. Add `type_by_name: HashMap<String, &TypeDefinition>` to `RuleSet`, built in `reindex()`. This also unblocks H5.

**Conflict risk:** medium-high on `rules_converter.rs` and `info/lib.rs`. Run `git log` on those files before starting. Stop and ask the user on conflict.

**Verify:** `cargo test -p cwtools_rules -p cwtools_info`, plus the roundtrip tests at `crates/cache/tests/roundtrip.rs`. Confirm `cargo run -p cwtools_cli -- validate` produces identical diagnostics on a small mod before and after.

### PR 4: Idiomatic simplifications (M1-M5, M7-M13, M15, L1-L30)

**Touches:** all the remaining Medium and Low items, batched by file:

- `crates/rules/src/rules_converter.rs`: L1-L5, L16-L17, L24-L25
- `crates/info/src/lib.rs`: M7, L5, L14, L17, L22, L27
- `crates/info/src/inline_expansion.rs`: M8
- `crates/lsp/src/main.rs`: M12, M13, L18-L21, L23, L26
- `crates/lsp/src/position.rs`: M15 (merge with `info::find_pos_in_children`)
- `crates/localization/src/loc_string.rs`: M9, L7, L8, L28, L29
- `crates/localization/src/scope_validation.rs`: M10, L30
- `crates/localization/src/yaml_parser.rs`: L30

**Highlights:**

- `rules_converter.rs:833` (`extract_bracket_content`): use `strip_prefix` + char check instead of `format!`.
- `rules_converter.rs:1514-1600` (`options_from_comments`): single pass building a `CommentBits` struct. This is also a perf win, since the function runs per leaf.
- `info/lib.rs:12-21` (`leaf_value_string`): return `Cow<'_, str>` to avoid per-call `String` allocation when the caller can borrow.
- `rules_converter.rs:1097-1189` (`parse_localisation_block`, `parse_modifiers_block`): dedupe via a `LocalField` enum.
- `localization/loc_string.rs:54-101` (`parse_loc_elements`): use `char_indices` instead of `Vec<char>` allocation.
- `lsp/main.rs:575-580` (`enum_values_for`): HashMap added in `RuleSet::reindex`.
- `lsp/main.rs:1428-1450, 1452-1475` (`SymbolInformation` blocks): extract a `make_symbol` helper.
- `lsp/main.rs` repeated `Location { uri: file_uri.parse().unwrap_or_else(...) }`: extract a `parse_uri` helper.

**Conflict risk:** medium. Same protocol as PR 3.

**Verify:** `cargo test --workspace`, `cargo clippy --workspace`, `cargo fmt --check`.

### PR 5: Profiler via tracing

**Touches:** workspace `Cargo.toml` (add `tracing-subscriber`). New file: `crates/cli/examples/md_bench_traced.rs`. New file: `cwtools-rs/PROFILING.md`. Optional: `cwtools-rs/scripts/bench.sh`.

**Changes:**

1. **Wire `tracing`:**
   - Add `tracing-subscriber = { version = "0.3", features = ["env-filter"] }` to workspace deps.
   - Add a `tracing_init()` helper in `cwtools-cli` that sets up a layered subscriber with `env-filter` from `RUST_LOG`. Default filter: `info`.
   - Call it from `cwtools-cli` `main` and from `cwtools-lsp` `Backend::new` (only when `RUST_LOG` is set, to keep the default quiet).

2. **Instrument the hot paths** found in PRs 2-3 with `#[tracing::instrument]` and `info_span!`:
   - `info::index_file_with_path`, `info::clear_file`
   - `rules::post_process::post_process_ruleset` and its three sub-passes
   - `lsp::parse_and_validate`, `lsp::validate_entire_workspace`, `lsp::scan_use_sites`
   - `validation::validate_parsed`
   - `parser::parse_string`

3. **Instrumented benchmark** at `crates/cli/examples/md_bench_traced.rs`:
   - Same workload as `md_bench` (Millennium Dawn directories).
   - Wraps each phase in an `info_span!` and prints a hierarchical timing summary at the end.
   - Honors `RUST_LOG=cwtools_info=info,cwtools_rules=info,cwtools_lsp=info` for filtering.
   - Default output: one line per phase with `time/file` and `time/leaf` so we can spot regressions at a glance.

4. **Document** at `cwtools-rs/PROFILING.md`:
   - How to run the traced bench (`RUST_LOG=info cargo run --example md_bench_traced -p cwtools_cli`).
   - The expected hot path hierarchy (the H1-H10 items).
   - How to add `#[tracing::instrument]` to a new hot path.
   - A short "what to look for" checklist.

5. **Lightweight perf-test script** (optional, no CI yet) at `cwtools-rs/scripts/bench.sh`:
   - Runs the traced bench, captures the timing summary, and prints `OK` if no phase regressed by more than X% vs a baseline file. Manual until we have a real baseline.

**Conflict risk:** low. Mostly new files plus a small workspace-dep add.

**Verify:**

- `RUST_LOG=info cargo run --example md_bench_traced -p cwtools_cli` on a small mod produces a timing tree.
- `cargo test --workspace` is unaffected.
- `cargo clippy --workspace` is clean.

## 5. Verification strategy

After each PR, run from the repo root:

```
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
```

End-to-end smoke:

```
cargo run -p cwtools_cli -- validate --ruleset <path> <mod>
```

Before/after timing check (using the bench from PR 5 once it lands):

```
RUST_LOG=info cargo run --example md_bench_traced -p cwtools_cli
```

## 6. Risk and rollback

- Each PR is independent and self-contained. Any PR can be reverted without affecting the others.
- PRs 2-3 are performance-only with no expected behavior change. The smoke `validate` run on a representative mod is the regression check.
- PR 4's merge of `position::find_at_position` and `info::find_pos_in_children` (M15) is the one spot with a small behavior risk. The note in the research report says they diverge in the end-column check (`<` vs `<=`). Pick one and call it out in the PR description so the other agent is aware.

## 7. Open questions for the implementer

These were raised with the user and answered. Recording here for the next agent:

- **Conflict protocol:** Stop and ask the user on any recent commit in the touch set. Don't rebase around the other agent.
- **Mutex choice:** Add `parking_lot = "0.12"` to workspace deps. Use it for LSP `Backend` state.
- **First PR to ship:** PR 1 (comments) is the recommended starting point. Lowest conflict risk, unblocks review of the rest.

## 8. File map

```
cwtools-rs/
├── Cargo.toml                                              (PR 2, PR 5)
├── PROFILING.md                                            (PR 5, new)
├── scripts/
│   └── bench.sh                                            (PR 5, new)
└── crates/
    ├── cli/
    │   └── examples/
    │       └── md_bench_traced.rs                          (PR 5, new)
    ├── info/
    │   └── src/
    │       ├── lib.rs                                      (PR 1, 3, 4)
    │       └── inline_expansion.rs                        (PR 1, 4)
    ├── lsp/
    │   ├── Cargo.toml                                      (PR 2)
    │   └── src/
    │       ├── main.rs                                     (PR 1, 2, 4)
    │       └── position.rs                                 (PR 4)
    ├── localization/
    │   └── src/
    │       ├── loc_string.rs                               (PR 1, 4)
    │       ├── scope_validation.rs                         (PR 1, 4)
    │       └── yaml_parser.rs                              (PR 1, 4)
    ├── rules/
    │   └── src/
    │       ├── post_process.rs                             (PR 1, 3)
    │       ├── rules_converter.rs                          (PR 1, 3, 4)
    │       └── rules_types.rs                              (PR 1, 3)
    └── validation/
        └── src/
            └── lib.rs                                      (PR 1)
```

## 9. Findings list (full)

The numbered list below is the full output of the research pass. Each entry has a file, line range, severity, and a one-line summary. The PR sections above describe the chosen fix.

### High severity

**H1. LSP workspace validation runs heavy sync work on the async runtime.**
File: `cwtools-rs/crates/lsp/src/main.rs:1696-1840` (`validate_entire_workspace`).
The full-workspace loop performs filesystem walks (synchronous), full file reads, parsing via `parse_string`, indexing, validation, and per-file `publish_diagnostics` `await`s on the multi-thread runtime. The work is dispatched into a `tokio::spawn` at line 905 but executes inline. On a mod of any size this can starve other LSP requests (hover, completion). The function never uses `tokio::task::spawn_blocking` for the CPU-bound passes; reads happen via blocking `std::fs::read_to_string`; `index_document` (line 1847) is marked `async` only to use `.await` on `client.log_message`. Fix: wrap the two passes in `tokio::task::spawn_blocking`, or split the loop into chunks and `tokio::task::yield_now().await` between files. At minimum, run `walk_dir` and the `parse_string` calls in `spawn_blocking`.

**H2. Mutex held across await points is impossible (good), but `lock().unwrap()` is called many times in sequence and double-locks the same mutex in a single handler.**
File: `cwtools-rs/crates/lsp/src/main.rs`.
The handlers `hover` (lines 993-1062), `completion` (lines 1064-1181), `goto_definition` (lines 1183-1276), `references` (lines 1278-1410), `prepare_rename` (lines 1536-1575), and `rename` (lines 1577-1663) all lock the same `Mutex`es in close succession. `rename` re-locks `documents`, `ruleset`, and `workspace_uri` inside two separate scopes back to back. `parse_and_validate` (line 1901) locks `ruleset` four times within one call (lines 1875, 1877, 1882, 1967, 1977). `unwrap()` is used everywhere. Lock contention: `completion` locks `info_service` (line 1088) and then locks `documents + ruleset + info_service` again in the `else` branch (lines 1096-1098). Fix: replace `Mutex<HashMap<...>>` with `RwLock<...>` (or `DashMap`) and acquire once per handler, with the work performed via local variables. Replace `.unwrap()` with `.expect("mutex poisoned")` and switch to `parking_lot::Mutex` which doesn't poison. Snapshot a `Vec<(uri, doc)>` from `documents` once at the top of each handler.

**H3. `collect_defined_variables` and `collect_defined_variables_from_rules` duplicate work and the first is dead-feeling.**
File: `cwtools-rs/crates/info/src/lib.rs:331-551`.
`collect_defined_variables` (line 331) walks the AST collecting `@var` entries and *then has a dead `for` loop* (lines 384-387) that iterates `ruleset.values` doing nothing (the body only has `let _ = rules;`). This is recomputed on every file open. `collect_defined_variables_from_rules` (line 413) is what `InfoService::index_file_with_path` actually uses (line 977), so `collect_defined_variables` is effectively unused. `scan_node_for_varset` (line 490) iterates `rules` per child for every Node child (line 542), meaning O(N×M) for a file with N leaves and M rules. Fix: delete the dead `for (_, rules) in &ruleset.values { let _ = rules; }` block. If `collect_defined_variables` is still wanted, reduce to a thin wrapper that does only the `@var` collection. In `scan_node_for_varset`, pre-compute the set of rules that contain any `VariableSetField` once outside the per-file loop.

**H4. Recursive AST walkers do not short-circuit when only the index wants a summary.**
File: `cwtools-rs/crates/info/src/lib.rs`.
`index_child_heuristic` (line 1086) and `collect_event_targets_rec` (line 576) and `collect_at_vars` (line 456) each do their own full walk. `InfoService::index_file_with_path` (line 951) runs the heuristic walk *and then* `collect_type_instances` *and then* `collect_defined_variables_from_rules`, *and then* the saved event targets walk. The parsed AST is walked 3-4 times per file. `find_pos_in_children` (line 703) is also a full recursive walk. Fix: introduce a single pre-computed per-file summary (top-level keys, @vars, type references, saved event targets) computed once at index time, indexed by `(line, col)` ranges so the position queries become O(log n) or O(1) lookups.

**H5. `scan_use_sites` is O(files × leaves × root_rules) on every find-references and rename.**
File: `cwtools-rs/crates/lsp/src/main.rs:2079-2211`.
`scan_use_sites` (line 2079) walks every leaf of every open document. For each leaf with a matching value, it calls `is_type_ref_leaf` (line 2173) which scans *all* `root_rules` and for each one, does a `ruleset.types.iter().find(|t| t.name == name)` (line 2189), a linear scan per root rule per leaf per file. On a mod with 10K leaves and 500 root rules, that's 5M operations per find-references call. Fix: use the pre-built `ruleset.alias_exact` / `ruleset.alias_categories` HashMap (rules_types.rs:18,21) where possible. Pre-compute, for each `leaf_key`, whether it resolves to a `TypeField(type_name)` for a given `logical_path`. Cache the result per `(leaf_key, logical_path)` since neither changes often.

**H6. `replace_single_aliases` clones the entire single_aliases Vec on every iteration.**
File: `cwtools-rs/crates/rules/src/post_process.rs:26-56`.
Two full clones of `single_aliases` per iteration, up to 10 iterations. `single_aliases` contains `(String, (RuleType, Options))` and the `RuleType::NodeRule` and friends contain nested `Vec<NewRule>`, `Vec<NewField::ScopeField(Vec<String>)>`, etc., so each clone is a deep tree copy. Fix: build the lookup once outside the loop. The `map` is constant during the loop, so hoist it out and drop the `before` clone, using a `bool` set by the recursion to detect fixpoint.

**H7. `inline_rules_list` and `expand_colour_in_list` and `expand_ignore_in_list` use `std::mem::take` + push, which is correct but expensive for the success case.**
File: `cwtools-rs/crates/rules/src/post_process.rs:80-141, 188-194, 350-368`.
These three functions all `std::mem::take(rules)` (drain), iterate, and `push` back the original (often with no transformation) and the rewritten. On a large ruleset, this rebuilds the full `Vec<NewRule>` for *every* rules list in the ruleset, even rules that don't contain any `SingleAliasField`/`ColourField`/`IgnoreMarkerField`. The hot case is: most rule lists don't need rewriting. Fix: do an in-place `iter_mut` pass that only allocates when a rewrite happens. Use `Vec::retain_mut` or a manual loop that mutates existing entries and only allocates a fresh `Vec` on rewrite.

**H8. `validate_parsed` is called per-file, holding `info_service` lock while building modifier_keys every call.**
File: `cwtools-rs/crates/lsp/src/main.rs:1985`.
In `parse_and_validate` (line 1901), `build_modifier_keys` is called *per file* (line 1985), but the function is on a single `&self` and rebuilds the entire `HashSet<String>` from scratch each time. The `validate_entire_workspace` pass at line 1800 already shows the correct pattern: build the set *once* and pass it in. `parse_and_validate` reverts to the per-file pattern, costing O(modifiers × instances) per file open. Fix: cache `modifier_keys` in `DocumentState` (it's invalidated only when the ruleset or the type index changes), and pass `&modifier_keys` into `validate_parsed`.

**H9. `collect_comments_before_child` walks backwards linearly per child.**
File: `cwtools-rs/crates/rules/src/rules_converter.rs:13-35`.
For a child at index `idx` in a `Vec<Child>`, this walks from `idx-1` down. In a file with N children, the total work is O(N^2); for the K-th child we do K steps. This is called from `ast_to_ruleset`, `children_to_rules`, `process_type_node`, `parse_localisation_block`, `parse_modifiers_block` (lines 478, 883, 1100, 1165), all of which iterate `children.iter().enumerate()`, so each of these loops is O(N^2) in the size of its children list. Fix: precompute a `Vec<Option<Vec<String>>>` of "comments-before-this-child" for each children list once at the top of the function, then index into it.

**H10. `check_path_dir` lowercases the directory and every path pattern on every call.**
File: `cwtools-rs/crates/info/src/lib.rs:107-138`.
`dir.to_lowercase()` (line 118) and `pat.to_lowercase()` (line 123) inside the per-pattern loop. `collect_type_instances` (line 263) calls this once per type definition; `find_pos_in_children` and `classify_node_key` (line 783) also call it. For every type (often hundreds) and every file open, every path pattern is re-lowercased. Fix: pre-compute lowercased patterns once at ruleset load time, e.g. an `Option<(Vec<String>, bool)>` on `PathOptions` that's filled in by `RuleSet::reindex()` or a new `precompute()` method.

### Medium severity

**M1. `rules_converter.rs` `field_from_string` performs `format!` + String allocations for known static prefix strings.**
File: `cwtools-rs/crates/rules/src/rules_converter.rs`.
`extract_bracket_content` (line 833) does `let expected = format!("{}[", prefix);` *and* `full[expected.len()..]` every call. Called many times per file. Fix: use `full.strip_prefix(prefix)` and check the next char, or precompute the prefix length with a `let prefix_len = prefix.len() + 1;` after asserting the prefix matches. `get_setting_from_string` (line 1503) has the same pattern. The function is ~440 lines and has 30+ `unwrap_or_default()` calls (all returning a default-constructed `String`). For purely-`String` returns, `&str` would suffice at the call site. Consider an internal `Cow<str>` API.

**M2. `options_from_comments` does 6 separate linear scans of the same `comments` slice.**
File: `cwtools-rs/crates/rules/src/rules_converter.rs:1514-1600`.
`comments.iter().find(...)` is called for `cardinality`, `push_scope`, `severity`, `outgoingReferenceLabel`, `incomingReferenceLabel`, `error_if_only_match`, and again inside `parse_replace_scopes_from_comments` and `parse_required_scopes` and `extract_description_from_comments`. 9 linear scans per Options construction. `Options` is constructed for every leaf and node in every .cwt file (often thousands). Fix: do a single pass over `comments` once at the top, collecting matches into a `CommentBits` struct with `Option<String>` for each known key. The result is one walk instead of nine.

**M3. `build_subtype` clones every child via `iter().filter().cloned().collect()`.**
File: `cwtools-rs/crates/rules/src/rules_converter.rs:1260-1273`.
`filtered_children: Vec<Child> = children.iter().filter(...).cloned().collect();` allocates a fresh Vec of every surviving `Child` (which is just a `u32` index, cheap), but the point of this filter is to drop a `type_key_field` leaf. Since `Child::Leaf(idx)` is just a `u32`, the clone is cheap, but the `collect` allocates a new `Vec` that could be avoided by passing `&filtered_children` to a downstream function that takes `&[Child]` (already does at line 1276, good), and the intermediate `Vec` could be a smallvec. Low priority.

**M4. `parse_localisation_block` and `parse_modifiers_block` rebuild the same `&str` slices.**
File: `cwtools-rs/crates/rules/src/rules_converter.rs:1097-1189`.
The two functions are 90% identical. They could share most of the body. Both do the same `find('$')` -> split into prefix/suffix logic, both do the same `##` description extraction, both recurse into subtype blocks via duplicated `parse_subtype_localisation` / `parse_subtype_modifiers` (lines 1145 and 1191) which are also nearly identical. Fix: parameterise on a `LocalField` enum and share the loop, or extract a `parse_named_subblock` helper.

**M5. `Value::Bool(true) => "yes".to_string()` and friends in `value_to_string`.**
File: `cwtools-rs/crates/rules/src/rules_converter.rs:1473-1490`.
For non-String variants, the function always allocates a new `String`. For a leaf with a Bool/Int/Float value, the function is called many times. The LSP `build_hover_markdown` (line 144) and `find_rule_description` are also called per request and often read from the same leaves. Fix: an internal helper returning `Cow<'_, str>` would skip the allocations for the String case. Acceptable as-is if the callers don't form hot loops.

**M6. `TypeIndex::is_any_instance` is O(total instances).**
File: `cwtools-rs/crates/info/src/lib.rs:70-72`.
`pub fn is_any_instance(&self, name: &str) -> bool { self.map.values().any(|v| v.iter().any(|(_, ti)| ti.name == name)) }`. Called from `validation/src/lib.rs:1300` per key check. For a mod with 10K type instances, this is 10K string compares per key. Fix: add a secondary index `name_to_types: HashMap<String, HashSet<String>>` updated on `merge` and `remove_file`. Lookup becomes O(1) for "is this name a known instance".

**M7. `TypeIndex::remove_file` walks all values.**
File: `cwtools-rs/crates/info/src/lib.rs:93-98` and `InfoService::clear_file` (lines 1016-1060) which scans `self.files.values()` four separate times for `saved_event_targets`, `defined_variables`, `inline_scripts` removal checks. Each `any()` scan is O(all files), so removing one file is O(files × data). On a workspace of 1000 files removing one, it's 1M operations. Fix: track per-file "exclusive" ownership of a global symbol (refcount or simply skip-and-sweep the touched symbols at low cost because the `still_exists` is local), or maintain reverse maps from symbol -> set of file URIs so removal is O(contributors).

**M8. `inline_expansion::clone_tokens` always lowers the substituted text.**
File: `cwtools-rs/crates/info/src/inline_expansion.rs:385-394`.
`intern_both` is called for every text token in every expanded script, even when the substituted value is identical to the source value. The `to_lowercase()` plus intern allocations add up for large scripts with many leaves. Fix: if `text` is unchanged after `substitute_params`, reuse the original `StringTokens` (just return `tokens` cloned), but that would need a Cow-ish API on the table.

**M9. `loc_string.rs` `parse_loc_elements` collects `Vec<char>` and iterates by index.**
File: `cwtools-rs/crates/localization/src/loc_string.rs:54-101`.
`let chars: Vec<char> = s.chars().collect();` then iterates `chars[i]`. For long loc strings, this allocates a fresh `Vec<char>` (4 bytes each) and indexes it. The function is called once per entry by `parse_loc_text` (yaml_parser.rs:238). Fix: use `char_indices` and `s.len()` with `str::char_indices()` to get `&str` slices directly, or use a `Peekable<Chars<'_>>` borrowed from `s`. Avoid the full `Vec<char>` allocation.

**M10. `loc_validation.rs` `is_bypass_prefix` lowercases the whole command on every segment.**
File: `cwtools-rs/crates/localization/src/scope_validation.rs:138-144`.
`cmd.to_ascii_lowercase()` allocates a new String for the entire command. For a chain like `THIS.Owner.capital.GetName`, called once per segment. Same `is_terminal_command` (line 273) does `seg.to_ascii_lowercase()` for the "get" check. Fix: use `eq_ignore_ascii_case` and `starts_with` patterns that work on `&[u8]`, or compare with `char::to_ascii_lowercase` lazily.

**M11. `rules_converter.rs::children_to_rules` clones the entire `rules` Vec via `inline_rules_list`.**
File: `cwtools-rs/crates/rules/src/post_process.rs:80`.
`std::mem::take(rules)` is fine for the rewriting case, but combined with `inline_single_alias_rule`'s recursion (line 60), each level of the rules tree re-allocates. The depth can be 5+ for deeply nested scripts. Combined with H6, this is the dominant cost of post-processing. Fix: batch the rewrites: traverse once with a `&mut Vec<NewRule>` and a `pending: Vec<NewRule>` accumulator; only `mem::take` when a rewrite is actually going to happen.

**M12. `lsp/main.rs` rebuilds `info_guard` lookups per completion.**
File: `cwtools-rs/crates/lsp/src/main.rs:1095-1180`.
In `completion`, `info_guard` is locked, then a sub-block may re-lock `info_service` (line 1148). The fallback at line 1125+ also re-locks. The locked-guard pattern is leaking into control flow. Fix: take one snapshot of the relevant `InfoService` data up front, drop the lock, do the work.

**M13. `enum_values_for` does a linear scan.**
File: `cwtools-rs/crates/lsp/src/main.rs:575-580`.
`if let Some(e) = ruleset.enums.iter().find(|e| e.key == enum_name) { ... }`. Called from `completions_from_rules` (lines 459, 503, 640) and `root_type_snippets`. With dozens of enums and a few completions per request, this is small but compounds with H5. Fix: add `HashMap<String, Vec<String>>` to `RuleSet::reindex()` (mirroring `alias_exact`).

**M14. `info/lib.rs::collect_type_instances` doesn't use `reindex`-built indexes.**
File: `cwtools-rs/crates/info/src/lib.rs:263-313`.
The function iterates `ruleset.types` linearly. Since types are looked up by name in many places (e.g. `rules_for_context` in `lsp/main.rs:340`, `is_type_ref_leaf` in `lsp/main.rs:2189`), a `HashMap<&str, &TypeDefinition>` would help. Fix: precompute a `name_to_type: HashMap<&str, &TypeDefinition>` in `RuleSet::reindex()` (or a new `precompute` method).

**M15. `lsp/main.rs` `position.rs` and `lsp/main.rs` `find_pos_in_children` are two parallel implementations.**
File: `cwtools-rs/crates/info/src/lib.rs:703-780` and `cwtools-rs/crates/lsp/src/position.rs:5-69`.
Both do the same thing: walk the AST to find the deepest element at a position. The info version is the more complete one (it classifies with rules). `position::find_at_position` is the fallback. They diverge subtly (e.g. `pos_in_range` differs by `<` vs `<=` in end-column check). Merging them removes the dead duplicate.

### Low severity

**L1. `rules_converter.rs:53-85, 387-572, etc.` long match arms could be helper methods.**
`children_to_rules` (line 470-575) has a 100-line `match child` with three cases (Leaf, Node, LeafValue) each doing their own subtype/colour/options handling. Extracting `process_leaf`, `process_node`, `process_leaf_value` would shrink the function and make it testable.

**L2. `field_from_string` is a 220-line `match` ladder.**
File: `cwtools-rs/crates/rules/src/rules_converter.rs:214-432`. Could be a `match` table on `&str` or a small enum + table. Many of the arms return constants and could be a static `HashMap<&str, NewField>` for the simple cases, with the bracket forms handled separately.

**L3. `parse_replace_scopes_from_comments` has 8 near-identical `fromfrom`/`prevprev` arms.**
File: `cwtools-rs/crates/rules/src/rules_converter.rs:1637-1644`. The four `from*` and four `prev*` arms are pure duplication differing only in the index `1,2,3,4`. A helper `fn nth(s: &str, idx: usize)` (or just a small `match` on `tokens[ti]` with a counter) would compress this.

**L4. Repeated `comments.iter().find(...).and_then(... s.find('=') ...)` pattern.**
File: `cwtools-rs/crates/rules/src/rules_converter.rs:1546-1582, 1063-1075, 1083-1095, 1300-1312`. A small `fn find_key_value<'a>(comments: &'a [String], key: &str) -> Option<&'a str>` returning `Option<&str>` (or `Option<String>` for owned) would deduplicate all six call sites.

**L5. `info/lib.rs` `leaf_value_string` returns `String`; not `Cow`.**
File: `cwtools-rs/crates/info/src/lib.rs:12-21`. For the `Value::String` / `Value::QString` case, the function could return a `Cow<'_, str>` to avoid allocation when the caller can borrow. There are many call sites (lines 16, 18, 191, 366, 502, 503, 587, 737, 1144, 1167) where the `String` is only used for comparison.

**L6. `info/lib.rs` `is_any_instance` could be a `HashSet<&str>`.**
See M6 above for the fix; mentioning again under idiomatic since `HashSet::contains` is the obvious shape.

**L7. `loc_string.rs` `parse_bracket` could use a slice index over `char_indices`.**
File: `cwtools-rs/crates/localization/src/loc_string.rs:140-177`. The function walks `chars` by index and slices `chars[..]` / `chars[start..i]`. The whole thing is a hand-rolled recursive descent over a `Vec<char>`. With `&str` and `char_indices()` (or `s.char_indices()`) most of the `chars[c].iter().collect::<String>()` lines would go away.

**L8. `loc_string.rs` `parse_jomini` does the same.**
File: `cwtools-rs/crates/localization/src/loc_string.rs:185-229`. Same `Vec<char>` + index pattern. `current.clone()` on line 197 / 205 happens on every dot and could be `std::mem::take(&mut current)`.

**L9. Comment quality, F# port notes.**
`rules_converter.rs:5-7`, `rules_converter.rs:213` (`/// Matches F# processKey (RulesParser.fs:371-567)`), `rules_converter.rs:707` (`/// Parse values = { value[name] = { ... } } top-level block (F# RulesParser.fs:1298-1321)`), `rules_converter.rs:1353-1357` (F# behavior comment), `post_process.rs:3-4`, `post_process.rs:22-26`, `post_process.rs:155-158` (Pass 2 comment), `info/lib.rs:101-106` (F# CheckPathDir), `info/lib.rs:140` (F# skiprootkey), `info/lib.rs:262-263` (F# getTypesFromDefinitions), `info/lib.rs:316-318` (Item 2 header), `info/lib.rs:553-555` (Item 3 header), `info/lib.rs:615-617` (Item 4 header), `info/lib.rs:619-627` (F# limitation note), `lsp/main.rs:103-114` (large F# port-todo block), `yaml_parser.rs:1-14` (extensive F# block), `scope_validation.rs:1-28` (F# module block). These are all factual and useful, but a few could be shortened. Example: `rules_converter.rs:5-7`. The "F# RulesParserConstants" reference is implementation-irrelevant to Rust code; the rest is useful. Could be: `// +/-1e12 sentinel for unranged float (1e6 was too narrow for build costs/pop).` The 24-line `lsp/main.rs:103-114` "NOT PORTED" block is great as design context but won't be re-read by people modifying the Rust.

**L10. `rules_converter.rs:13-35` `collect_comments_before_child` is exported and well-commented.**
The 5-line doc-comment is fine. No change.

**L11. `rules_converter.rs:8-12` and `:842-871` could be `const` for sentinels.**
`FLOAT_MAX`, `FLOAT_MIN`, `INT_MAX`, `INT_MIN` (lines 8-11) are already `const`. Good. `Options::default()` (rules_types.rs:236-256) has `min: 0, max: 1000`, runtime constants. Could be `pub const DEFAULT_MIN: i32 = 0;` and used directly.

**L12. `inline_expansion.rs:373` good fix-comment.**
```rust
/// Intern a string and its lowercase form so that `tokens.lower` really is the
/// lowercase intern.  The original code called `intern` once and reused the
/// `.normal` id for both, which broke case-insensitive lookups for mixed-case
/// substitution results.
```
Useful, keep as-is.

**L13. `post_process.rs:386, 453, 490, 521` section dividers in tests.**
The `// --- Pass 1: ... ---` comments in the test module are fine. They mirror the F# pass names which is useful for cross-referencing.

**L14. `info/lib.rs:1014-1082` `clear_file` and `find_references` could be iterator chains.**
`find_references` (lines 1068-1082) is a 14-line manual `for` loop with `result.push((uri.clone(), *loc))`. An iterator chain would be a wash on perf but more idiomatic. Optional.

**L15. `info/lib.rs:1-7` `pub mod inline_expansion;` is a single export.**
Fine.

**L16. `rules_converter.rs:1475-1489` `value_to_string` mixes stripping and formatting.**
The `if s.starts_with('"') && s.ends_with('"') && s.len() >= 2` then `s[1..s.len() - 1].to_string()` (lines 1478-1480) is a manual unquote. Could use `s.strip_prefix('"').and_then(|s| s.strip_suffix('"'))`. Minor.

**L17. `rules_converter.rs:1109-1111` triple `.iter().any()` for the same comments.**
```rust
let required = child_comments.iter().any(|s| s.contains("required"));
let optional = child_comments.iter().any(|s| s.contains("optional"));
let primary = child_comments.iter().any(|s| s.contains("primary"));
```
Three separate linear scans. Same pattern in `parse_modifiers_block` (lines 1173-1177) and the `options_from_comments` issue (M2).

**L18. `lsp/main.rs:118-122` empty async fn.**
```rust
async fn on_did_focus_file(&self, _params: Value) {
    // C->S: accept silently.
}
```
Could be `async fn on_did_focus_file(&self, _params: Value) {}`. Trivial.

**L19. `lsp/main.rs:1190-1191` repeated `logical_path` computation.**
Many handlers (`hover`, `completion`, `goto_definition`, `references`, `prepare_rename`, `rename`) compute `logical_path` via `logical_path_from_uri(uri, &ws_uri)`. This is cheap but called on every request. Could be cached per URI in the `documents` map or precomputed when the doc is opened. Minor.

**L20. `lsp/main.rs:1428-1450` and `:1442-1475` large literal `Location` constructors.**
The `#[allow(deprecated)] SymbolInformation { ... }` blocks at lines 1428-1450 and 1452-1475 are nearly identical in structure. A small helper `fn make_symbol(name, kind, loc, container) -> SymbolInformation` would remove the boilerplate. Same for `Location { uri: file_uri.parse().unwrap_or_else(...) }` (appears 6+ times at lines 1224, 1262, 1325, 1343, 1377, 1386, 1396, 1644). A `fn parse_uri(s: &str, fallback: &Url) -> Url` would dedupe.

**L21. `lsp/main.rs:9-18` unused imports?**
`use cwtools_parser::ast::{ParsedFile, ParseError};`. `ParseError` is used in `parse_error_to_diagnostic`. `ParsedFile` used. `PositionElement`, `ReferenceHint` used. All clean.

**L22. `info/lib.rs:888-912` `FileInfo::default()` is used (line 959).**
The 12-field struct has a `Default` derive; that's fine. The 4 fields `type_definitions`, `type_references`, `defined_variables`, `inline_scripts` are pre-computed by the heuristic indexer and never populated by the rule-driven path; they could be `#[allow(dead_code)]` or removed if the LSP is migrated off heuristics.

**L23. `lsp/main.rs:1087` `.yml` / `.yaml` extension check.**
```rust
if uri.ends_with(".yml") || uri.ends_with(".yaml") {
```
Could be `Path::extension(...).is_some_and(|e| e == "yml" || e == "yaml")` to handle query strings and case insensitivity properly. Cosmetic.

**L24. `post_process.rs:115-118` and similar `inline_rules_list` pushes back `other` literally.**
The function name says "walk" but the comment says "walk a `Vec<NewRule>` in place, replacing SingleAliasField entries". The function does NOT walk in place; it always rebuilds. Rename to `rewrite_single_aliases_in_list` to match reality.

**L25. `rules_converter.rs:1648-1650` `ti += 1;` for unknown tokens.**
The while-loop on `tokens` increments `ti` by 1 when the next token is not "=". For long token lists without "=" at position 1, this is a near-O(N) scan. Not hot, but worth noting.

**L26. `lsp/main.rs:2060-2062` nested function `walk_dir` inside `validate_entire_workspace`.**
The nested `fn walk_dir` is OK in Rust 2024 edition but uses a function. Could be replaced with a `walkdir` crate or `rayon::par_iter` for parallel walking. The walk itself is I/O bound, so `rayon` over `read_dir` doesn't help much without `spawn_blocking`.

**L27. `info/lib.rs:71-72` `is_any_instance` linear scan.**
See M6, listed again as it's also a code-clarity issue (returning early would be clearer than nested `any().any()`).

**L28. `loc_string.rs:64-72, 80-87, 90-95` three near-identical "lone special char" branches.**
```rust
'$' => { ... }
'[' => { ... }
_ => { ... }
```
All three do: `let start = i; i += 1; while i < chars.len() && !['$', '[', ']'].contains(&chars[i]) { i += 1; } elements.push(LocElement::Chars(chars[start..i].iter().collect()));`. A `fn consume_chars_until_special(chars: &[char], i: &mut usize) -> LocElement` would dedupe.

**L29. `loc_string.rs:107-137` `parse_ref` allocates `chars[content_start..i].iter().collect::<String>()`.**
Use `s[content_start..i].to_string()` directly (you're already on a `&[char]`), or work on `&str` from the start. Avoids the iterator+collect round-trip.

**L30. `scope_validation.rs:194-200` diagnostic format does extra `format!` per diags.**
```rust
command: format!("{} (in {})", command, cmd),
```
Allocates a String just to wrap `command` and `cmd`. Could store both fields separately. Minor.

## 10. References

- Branch: `refactor/rust-rewrite`
- Diff: `git diff origin/master...HEAD` (20,021 LOC added, 67 files)
- Workspace: `cwtools-rs/` (Rust 2024 edition, 11 crates)
- Project memory: `/home/kmccormick/.claude/CLAUDE.md`
