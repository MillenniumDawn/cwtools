# Spec: Close F#-vs-Rust parity gaps in the Rust rewrite

**Status:** Draft for review
**Branch:** `refactor/rust-rewrite`
**Workspace:** `cwtools-rs/`
**Spec path:** `specs/rust-fsharp-parity-gaps.md`

## 1. Context

The Rust rewrite under `cwtools-rs/` validates a HOI4 Millennium Dawn mod with the false-positive
count driven from ~2M to ~37K over eight passes. That work suppressed noise by making validators
permissive. It did not reach feature parity with the F# original. This spec catalogs the verified
gaps and proposes an order to close them.

A fresh audit (three agents across validation/parser/rules, game/scope/loc/file, and CLI/LSP, every
high-impact claim checked against source) found the items below. Findings are evidence-backed with
`file:line`. The companion hunt report is at
`/home/kmccormick/.claude/plans/can-you-hunt-for-glittery-boot.md`.

Out of scope: VIC3/EU5 scope catalogs (commented out in F# too), and anything that strictly needs a
vanilla game install (tracked separately as embedded-data loading).

## 2. Constraints and conventions

Per `CLAUDE.md`:

- No em-dashes. Terse prose, short lines, plain words.
- No "authoritative / canonical / seamless / robust" tells. No Claude attribution in commits or PRs.
- Use `file_path:line_number` when referencing code.
- Match existing comment style in touched files.

Ground-truth verification method (reuse for every gap): build the F# CLI once
(`dotnet build CWToolsCLI/CWToolsCLI.fsproj -c Release`), run it and
`target/release/cwtools validate` on the same MD subtree with
`--rulespath /mnt/Linux/github-projects/cwtools-hoi4-config/Config`, and diff the diagnostics. Run
full-mod validations sequentially, never in parallel, or the comparison corrupts.

## 3. Findings summary

| # | Gap | Rust status | File:line | Impact |
|---|---|---|---|---|
| 1 | Alias-block contents unchecked | `AliasField` clause `=> true` | `validation/src/lib.rs:1889-1893` | High |
| 2 | Icon-file existence not checked | `IconField => true` | `validation/src/lib.rs:1873-1874` | Med |
| 3 | Filepath existence not checked | `FilepathField => true` | `validation/src/lib.rs:1871` | Med |
| 4 | `variable_get`/`variable_set` blanket-accept | `=> true` | `validation/src/lib.rs:1877-1878` | Med |
| 5 | `value_scope_field` blanket-accept | `=> true` | `validation/src/lib.rs:1880-1884` | Low |
| 6 | `var:`/`variable:` scope prefix unhandled | only `event_target:`/`parameter:`/`scope:` | `game/src/scope_engine.rs:291-299` | Med (verify) |
| 7 | CLI error-hash suppression | absent | `cli/src/main.rs` | High |
| 8 | CLI CSV/JSON report + `--OutputFile` | absent | `cli/src/main.rs` | Med |
| 9 | CLI `list` / `format` subcommands | absent | `cli/src/main.rs` | Low |
| 10 | CLI `--Languages` filter | absent | `cli/src/main.rs` | Low |
| 11 | LSP graph / code-actions / metadata / progress | stubbed | `lsp/src/main.rs:105-114` | Low |
| 12 | Mod overwrite-order tracking | not tracked | `file_manager/src/file_manager.rs` | Med |
| 13 | Vanilla/embedded data loading | parser exists, not wired | `game/src/docs_parser.rs`, no `--vanilla-dir` | Med |

Plain `VariableField` is range-checked (`validation/src/lib.rs:1850-1862`) and is NOT a gap.

## 4. Implementation plan

Independently mergeable units, ordered by leverage. Each lists changes, conflict risk, verification.

### Unit 1: Alias-block deep validation (finding 1)

The broadest correctness gap. Today the alias name resolves via the alias index but the block body
is never recursed, so trigger/effect contents go unchecked.

**Touches:** `crates/validation/src/lib.rs`.

**Changes:**

- At `lib.rs:1889-1893`, replace the `AliasField(_), Value::Clause(_) => true` shortcut with a
  recursion: resolve the alias overloads for the category (reuse `validate_alias_usage` /
  `alias_exact` + `alias_categories` from `RuleSet::reindex()`), then validate the clause's children
  against the resolved alias body via the existing `validate_children` path.
- Keep the disjunction behavior already used for overloaded aliases: try all overloads, accept on
  first clean match, else report the fewest-errors candidate. Do not regress the `find()`-first bug
  fixed in the third pass.
- F# reference: `FieldValidators.fs:915-926`.

**Conflict risk:** medium. `validation/src/lib.rs` is a hot file. `git log --since=2.weeks -- crates/validation/src/lib.rs` first; if the other agent touched it, ask the user.

**Verify:** add a regression test under `crates/validation/tests/` with an alias block containing a
known-bad inner field; confirm it now flags. Run the F#-diff method on an MD effect-heavy subtree
and confirm no new false-positive flood (this is the risk: deep validation can resurface noise).

### Unit 2: CLI error-hash suppression (finding 7)

Highest-value CLI gap. With ~37K residual MD diagnostics there is no way to baseline and see only
new errors.

**Touches:** `crates/cli/src/main.rs`, and wherever the CLI builds its diagnostic list.

**Changes:**

- Add `--output-hashes [FILE]` and `--ignore-hashes [FILE]` to the `Validate` subcommand.
- Hash each diagnostic stably (match F#: file logical path + code + message + maybe line; check
  `Validator.fs:75-86` and `Reporters.fs` for the exact hash input so baselines are portable).
- On `--ignore-hashes`, drop diagnostics whose hash is in the file before reporting. On
  `--output-hashes`, write the surviving set.

**Conflict risk:** low. New flags, isolated to the CLI.

**Verify:** run validate twice on the same MD subtree, write hashes the first time, ignore them the
second, confirm zero reported. Add a CLI-level test if the harness supports it.

### Unit 3: Asset existence checks (findings 2, 3)

**Touches:** `crates/validation/src/lib.rs`, and file_manager for path lookups.

**Changes:**

- `IconField` (`lib.rs:1873-1874`): build `folder/key.dds` (folder from the rule) and check existence
  against the indexed file set. F# ref `FieldValidators.fs:519-544`.
- `FilepathField` (`lib.rs:1871`): honor prefix/suffix + extension, check existence. F# ref
  `FieldValidators.fs:492-517`.
- Both need the file_manager's known-files index threaded into validation. Gate on the index being
  present so a partial workspace does not flood (mirror the SAFE strict-TypeField pattern: only flag
  when we actually have the file set).

**Conflict risk:** medium (touches lib.rs and file_manager wiring).

**Verify:** test mod with a deliberately missing icon and a missing texturefile; confirm each flags.
F#-diff to confirm parity, watch for false positives from `gfx`/vanilla assets not in the mod.

### Unit 4: variable_get/set + value_scope_field + var: prefix (findings 4, 5, 6)

**Touches:** `crates/validation/src/lib.rs`, `crates/game/src/scope_engine.rs`.

**Changes:**

- `scope_engine.rs:291-299`: add `var:` and `variable:` (and `variable:from:`) prefix stripping,
  per-game where F# differs (`simpleVarPrefixFun`/`complexVarPrefixFun`, `Scopes.fs:115-130`,
  `HOI4Scopes.fs:40`). Verify MD actually uses bare `var:` in scope position before investing here.
- `variable_get`/`variable_set` (`lib.rs:1877-1878`): match against known variable defs with
  longest-prefix fallback, downgrade misses to warnings (F# `FieldValidators.fs:436-471`).
- `value_scope_field` (`lib.rs:1880-1884`): resolve value-or-scope with `static_values` fallback
  (F# `checkValueScopeField`, `FieldValidators.fs:704-757`).

**Conflict risk:** low-medium.

**Verify:** F#-diff. These are the items most likely to reintroduce false positives, so confirm the
delta is genuinely new real errors, not noise.

### Unit 5: CLI report formats and subcommands (findings 8, 9, 10)

Lower urgency, ergonomic.

**Touches:** `crates/cli/src/main.rs`, a new printer for `format`.

**Changes:**

- `--report-type CLI|CSV|JSON` + `--output-file` on `Validate` (F# `Reporters.fs`).
- `--languages` filter on `Validate`.
- `list` subcommand: folders / files / scripted triggers / scripted effects / localisation
  (F# `CWToolsCLI.fs:297-354`).
- `format` subcommand: needs a pretty-printer in `crates/parser/` (none exists; F# `Printer.fs`).

**Conflict risk:** low.

**Verify:** snapshot tests of CSV/JSON output on a tiny fixture; round-trip `format` on a sample file.

### Unit 6 (large, separate): mod overwrite tracking + embedded data (findings 11, 12, 13)

These are bigger subsystems, listed for completeness, not for this pass.

- Overwrite-order tracking in `file_manager` (F# `FileManager.fs:91-147`): record
  Overwritten/OverwrittenBy across mod layers so layered MD submods resolve correctly.
- Vanilla/embedded data loading: wire `docs_parser.rs` + a `--vanilla-dir` flag so effect/trigger/
  modifier DBs come from a real install. Clears the ~5K CW500 refs-to-vanilla-objects residual.
- LSP graph / code-actions / `getEmbeddedMetadata` / progress notifications: defer until the
  extension side needs them.

## 5. Verification summary

For every unit: add regression tests under `crates/validation/tests/` (or the relevant crate), run
`cargo test --workspace` and `cargo clippy --workspace` clean, then run the F#-diff ground-truth
method on a representative MD subtree and confirm the diagnostic delta is intended. Keep full-mod
runs sequential.
