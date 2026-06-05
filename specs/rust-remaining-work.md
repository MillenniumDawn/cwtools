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

### Dropped / deferred (decided 2026-06-04)

**Icon/filepath existence checks** — dropped. HOI4 config uses these in ~0/2
rules; disk checks risk false positives on vanilla assets. Revisit for non-HOI4
games only.

**Strict variable/value-scope validation** — dropped for HOI4. Pure noise for
MD; zero value unless `var:` scope errors actually appear.

**Mod overwrite tracking** — deferred. Only relevant for layered submods; MD is
a single mod.

**Embedded docs/setup.log loading** — deferred. Superseded by the vanilla cache
for the CW500 case.

**clicksound/subtype CW203 noise** — deferred. Root cause not pinned; fixing
subtype-required-field semantics risks regressing cardinality work. Revisit only
with heavy F#-diff gating.

### Perf + cosmetics

These are "no behavior change" and must be verified by identical diagnostics on
MD plus a green `cargo test --workspace`. The tracing profiler already landed;
the rest:

- **rules/info perf:** H6 hoist `replace_single_aliases`
  clone, H7 in-place `iter_mut` in the inline/colour/ignore passes, H9
  `precompute_comments` (kills an O(N^2) scan in `rules_converter`), H10
  precomputed lowercased path patterns, M14 `type_by_name` index in `reindex()`.
  Note: H3, M6, H4 already landed.
- **LSP perf:** H5 `type_by_name` for `scan_use_sites`.
  Note: H1 (yield_now), H2 (parking_lot Mutex), H8 (cached modifier_keys) landed.
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

1. Remaining rules/info perf (H6, H7, H9, H10, M14) — gate each with identical
   diagnostics on MD.
2. Idiomatic cleanups and comment cleanup — low regression risk, do last.
3. LSP graph/code-actions — only if a concrete editor need appears.

## 5. This is the only spec

This file is the single live backlog. The earlier `rust-fsharp-parity-gaps.md`
and `rust-rewrite-cleanup.md` specs have been folded in here and deleted; recover
their detail from git history if needed. Delete this file once the backlog is
empty.

## 6. Verification

For every change: `cargo test --workspace` green, no new clippy warnings in the
touched crates, and the F#-diff ground-truth on a representative MD subtree
confirming the diagnostic delta is intended (real new signal, not noise).
