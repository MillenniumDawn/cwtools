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

- F# was itself noisy on MD: ~80K diagnostics with the cwtools-hoi4-config
  (37,838 CW240, 21,984 CW100, ...). Rust is ~51K, already below it. The F#
  source tree has been removed from this repo; to reproduce an F# baseline,
  check out a pre-removal commit (or upstream cwtools/cwtools) and
  `dotnet build CWToolsCLI/CWToolsCLI.fsproj -c Release`. Day to day, measure
  changes against a fresh Rust run on the same MD subtree instead.
  Run full-mod validations sequentially, never in parallel.
- Goal is to aim BELOW F# (low noise), so prefer fixes that remove false
  positives over additions that introduce them.
- The workspace is rustfmt- and clippy-clean (`-D warnings`); keep it that way.
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

**clicksound/subtype CW203 noise** — FIXED. F# never enforces min cardinality
for rules inside subtype blocks; `checkCardinality` is called on the parent
`SubtypeRule` entries which hit the wildcard case. Fix: set `min=0` when merging
subtype rules into the flat list (both Path A and Path B in `validate_with_type`).
Result: 36,826 fewer warnings (38,111 → 1,285) on MD, zero error regression.

**ValueClauseRule in rule files** — FIXED. Anonymous `{…}` blocks in cwt rule
definitions (e.g. `colors = { {float} }`) were parsed as `LeafValueRule{SpecificField("")}`
instead of `ValueClauseRule`. Fix: in `children_to_rules`, detect `Child::LeafValue`
with `Value::Clause` and `Child::ValueClause` and create `ValueClauseRule` from
their children. Removed 818 spurious CW201/CW203 warnings from MD_ribbons.txt.
Final MD tally: 13,458 errors, 945 warnings.

### All perf/idiomatic/cosmetic items — DONE

All H/M-class perf items and idiomatic cleanups have landed. Summary:
- H1 yield_now, H2 parking_lot, H3 dead-loop drop, H4 single-pass indexing,
  H5 type_by_name in scan_use_sites, H6 replace_single_aliases no-clone,
  H7 expand_ignore in-place + colour fast-path, H8 cached modifier_keys,
  H9 precompute_comments, H10 paths_lower, M6 is_any_instance O(1),
  M14 type_by_name + enum_by_name in reindex, M15 position.rs deleted
  (callers use element_at_position from info/lib.rs).
- loc_string.rs: Vec<char> allocations removed; all scanning uses &str + bytes.
- NOT-PORTED block trimmed. F# preambles in yaml_parser/scope_validation gone.

### LSP editor features (deferred)

Graph panel, code-actions, pre-trigger refactor — large, low HOI4 value.
The `lsp/main.rs` NOT-PORTED comment points at F# `LanguageFeatures.fs`.

## 4. What's left

1. **LSP graph/code-actions** — only if a concrete editor need appears.
2. Delete this file once resolved or permanently dropped.

## 5. This is the only spec

This file is the single live backlog. The earlier `rust-fsharp-parity-gaps.md`
and `rust-rewrite-cleanup.md` specs have been folded in here and deleted; recover
their detail from git history if needed. Delete this file once the backlog is
empty.

## 6. Verification

For every change: `cargo test --workspace` green, no new clippy warnings in the
touched crates, and the F#-diff ground-truth on a representative MD subtree
confirming the diagnostic delta is intended (real new signal, not noise).
