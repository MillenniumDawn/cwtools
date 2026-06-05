# Spec: Remaining F#-vs-Rust parity and cleanup work

**Status:** Draft for review
**Branch:** `refactor/rust-rewrite`
**Workspace:** `cwtools-rs/`
**Spec path:** `specs/rust-remaining-work.md`

## 1. Context

A session on 2026-06-04 closed the high-value, low-risk parity gaps. This spec
captures what's left, with a value/risk note on each so it can be prioritized
rather than run top to bottom. The driving goal is a working, low-noise HOI4
tool, measured against a fresh F# run (see below), not against the older memory
numbers.

Landed this session (all build + test verified on Millennium Dawn, MD now sits
at 13,336 errors / 37,996 warnings):

- `.gui`/`.gfx` discovery + validation, and the sprite-name unquote fix (CW500 on
  sprites 19,469 -> 1,322; the rest are genuine base-game sprites).
- Case-insensitive + per-key cardinality counting (warnings 106K -> 38K).
- Pre-generated vanilla cache: `cwtools cache-vanilla` + `validate --vanilla-cache`,
  shared loader in `cwtools_info::vanilla_cache`, also wired into the LSP via a
  `vanillaCache` init option.
- Alias-body deep validation: inline whole-body `single_alias_right[...]` and
  resolve aliases case-insensitively (catches real nested-effect errors).
- CLI error-hash suppression (`--ignore-hashes`/`--output-hashes`) and CSV/JSON
  reports (`--report-type`, `--output-file`).
- Tracing profiler (RUST_LOG-gated; see `cwtools-rs/PROFILING.md`).

## 2. Ground truth and conventions

- F# is itself noisy on MD: ~80K diagnostics with the cwtools-hoi4-config
  (37,838 CW240, 21,984 CW100, ...). Rust is ~51K, already below it. Measure each
  change against a fresh F# run, not the stale "~5K" memory figure: build the F#
  CLI once (`dotnet build CWToolsCLI/CWToolsCLI.fsproj -c Release`), run it and
  `target/release/cwtools validate` on the same MD subtree with
  `--rulespath /mnt/Linux/github-projects/cwtools-hoi4-config/Config`, and diff.
  Run full-mod validations sequentially, never in parallel.
- Goal is to aim BELOW F# (low noise), so prefer fixes that remove false
  positives over additions that introduce them.
- Do NOT run global `cargo fmt`: the baseline has ~541 hunks of rustfmt drift, so
  a global format buries every feature diff. Hand-format only changed lines.
- The lsp crate has ~22 pre-existing clippy warnings; the baseline is not
  clippy-clean. Don't add new warnings; don't chase the old ones blanket-style.
- Per CLAUDE.md: no em-dashes, terse prose, no Claude attribution in commits.

## 3. Remaining items, by value

### Lower value for HOI4 (consider dropping)

**Icon/filepath existence checks.** `IconField`/`FilepathField` accept any string
(`validation/src/lib.rs` ~1871-1874). F# checks the file exists. But the HOI4
config uses `IconField` in ~0 rules and `FilepathField` in 2, and the icon
validation that matters (`icon = <sprite>`) already works via TypeField. Disk
checks need the file-set threaded into `validate_ast` for ~2 rules and risk
false positives on vanilla/missing assets. Recommend dropping unless a non-HOI4
game needs it.

**Strict variable/value-scope validation.** `variable_get`/`variable_set`
(`lib.rs` ~1877) and `value_scope_field` (~1880) blanket-accept. F# matches
against known var defs. Adding this is pure noise-add for low HOI4 value. The one
contained, safe sub-part is the `var:`/`variable:` scope prefix in
`game/src/scope_engine.rs` (~291) — but verify MD actually uses bare `var:` in
scope position first; if scope errors are ~0 today it's zero value.

**Mod overwrite tracking.** `file_manager` doesn't track which mod's file wins
across layers (F# `FileManager.fs:91-147`). Real for layered submods, but the
user tests a single mod (MD), so it's unexercised today. Defer.

**Embedded docs/setup.log loading.** Wire `game/src/docs_parser.rs` + a
`--vanilla-dir`-style log source so effect/trigger/modifier DBs come from a real
install. Largely superseded by the vanilla cache for the CW500 case; revisit only
if undocumented effects/modifiers surface.

### Delicate (high regression risk)

**Reduce clicksound/subtype CW203 noise (~36K).** `subtype[spriteType]` requires
`clicksound` (comment-less field -> defaults to 1..1) where F# never flags it (F#
emits ~37,838 CW240 on the same sprites instead). Fixing means changing
subtype-required-field semantics, which risks regressing the cardinality work
just landed. Magnitude already tracks F#, so not urgent. If attempted: heavy
F#-diff gating, and back off on any regression. Root cause not yet pinned (F#
appears not to enforce comment-less subtype fields the way Rust does).

### Perf + cosmetics

These are "no behavior change" and must be verified by identical diagnostics on
MD plus a green `cargo test --workspace`. The tracing profiler already landed;
the rest:

- **rules/info perf:** H4 per-file summary, H6 hoist `replace_single_aliases`
  clone, H7 in-place `iter_mut` in the inline/colour/ignore passes, H9
  `precompute_comments` (kills an O(N^2) scan in `rules_converter`), H10
  precomputed lowercased path patterns, M14 `type_by_name` index in `reindex()`.
  Note: H3 and M6 already landed.
- **LSP perf:** H1 `spawn_blocking`/`yield_now` in `validate_entire_workspace`,
  H2 `parking_lot` + single-lock-per-handler, H5 `type_by_name` for
  `scan_use_sites`, H8 cache `modifier_keys` instead of per-file rebuild.
- **Idiomatic:** the medium/low cleanups across `rules_converter`, `info/lib.rs`,
  `loc_string.rs`, `lsp/main.rs`, plus merging `position::find_at_position` with
  `info::find_pos_in_children` (watch the `<` vs `<=` end-column divergence).
- **Comment cleanup:** trim verbose F#-port comment blocks (e.g. the NOT-PORTED
  block in `lsp/main.rs`, the F# preambles in `yaml_parser.rs` /
  `scope_validation.rs`). Cosmetic; do last. The NOT-PORTED block is useful design
  context if LSP graph/code-actions get ported.

Value note: the LSP already validates the ~6,877-file mod in ~6.4s, so these are
marginal for the user; weigh the regression risk of rewriting hot files against
the gain before doing them. The full per-file H/M/L findings list lived in the
old `rust-rewrite-cleanup.md`; recover it from git history (`git log -- specs/`)
if a detailed line-by-line reference is needed.

### LSP editor features (nice-to-have)

**Graph / code-actions / metadata / progress notifications**
(`lsp/src/main.rs` ~109-118 NOT-PORTED block, F# `LanguageFeatures.fs`). The
contained, useful sub-part is the server->client progress notifications
(`loadingBar`, `updateFileList`) so the extension's file explorer populates; the
graph panel and Stellaris pre-trigger code-actions are large and low HOI4 value.

## 4. Suggested order

1. The contained safe wins if wanted: `var:` scope prefix (verify usage first),
   LSP progress notifications.
2. PR3 then PR2 perf, if the LSP responsiveness is worth the regression risk
   (gate each with identical-diagnostics on MD).
3. PR4, PR1 cosmetics.
4. Drop or defer: icon/filepath, strict variable validation, mod overwrite,
   embedded docs, graph/code-actions — unless a concrete need appears.
5. clicksound noise only with a careful, gated attempt.

## 5. This is the only spec

This file is the single live backlog. The earlier `rust-fsharp-parity-gaps.md`
and `rust-rewrite-cleanup.md` specs have been folded in here and deleted; recover
their detail from git history if needed. Delete this file once the backlog is
empty.

## 6. Verification

For every change: `cargo test --workspace` green, no new clippy warnings in the
touched crates, and the F#-diff ground-truth on a representative MD subtree
confirming the diagnostic delta is intended (real new signal, not noise).
