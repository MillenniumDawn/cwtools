# Cruft audit (F#-era leftovers)

Read-only inventory of what predates the Rust rewrite. The end goal is deleting
the F# stack (issue #6), but nothing here is deleted yet. This is the keep / move
/ delete list to work from. 216 F#/C# files are tracked today.

## The live dependency to break first

The Rust CLI still delegates to F# when run with `--engine fsharp`
(`cwtools-rs/crates/cli/src/main.rs`, `run_fsharp_engine` / `locate_fsharp_cli`),
shelling out to `CWToolsCLI.dll` via `dotnet`. So "delete F#" is gated on:
1. Rust reaching parity for whatever the F# engine is still used to cross-check.
2. Removing the `fsharp` engine option from the CLI and the extension
   (`cwtools.engine` setting in cwtools-vscode still offers `fsharp`).

Until that, keep the F# build working but stop investing in it.

## Delete (superseded, nothing depends on them)

- **`.vscode_ext_extension.ts`, `.vscode_ext_executable.ts`** (repo root) — stale
  TypeScript templates. The real extension is `cwtools-vscode/client/extension/*.ts`.
  Safe to delete now; confirm nothing in this repo imports them (nothing does).
- **`CWToolsCSTests/`** (5 files) — old C# tests against the F# library. Dead once
  F# goes; no Rust path uses them.
- **`CWToolsDocs/`** (3 files) — F# API docs. Superseded by `cwtools-rs/docs/`.

## Move / reconcile

- **Root `.cwt` files** (`effects.cwt`, `triggers.cwt`, `list_effects.cwt`,
  `list_triggers.cwt`, `links.cwt`) — game-agnostic rule files. **Not referenced
  by any Rust crate** (the Rust path loads rules from the config repo via the
  `rulesCache` init option). Either delete as legacy, or, if they're a canonical
  copy, move them into the config repo (`cwtools-hoi4-config`) so there's one
  source of truth. Decide before #13 (config-as-source-of-truth) lands.
- **`CSharpHelpers/`, `Shared/`** (10 + 6 files) — C# helpers feeding the F#/C#
  build. Audit which, if any, the F# library still needs; the Rust path needs none.

## Keep until F# is retired (then delete with #6)

- **`CWTools/`** (99 `.fs`) — the F# library. Still the `--engine fsharp` backend
  and the parity oracle. The reference for un-ported behavior.
- **`CWToolsCLI/`** (4) — builds `CWToolsCLI.dll`, the binary the Rust CLI shells
  out to. Delete together with the `fsharp` engine option.
- **`CWToolsTests/`** (5) — F# test suite; useful as a parity reference while
  porting checks, then delete.
- **`CWToolsPerformanceCLI/`** (6) — F# perf harness. The Rust side now has its
  own profiling (`cwtools-rs/crates/profiling`, `CWTOOLS_PROFILE=1`), so this is
  redundant once F# is gone.

## Suggested order

1. Delete the three "Delete" items now (zero risk).
2. Resolve the root `.cwt` files into the config repo or remove them.
3. Drive Rust parity (the open `enhancement` issues) until the `fsharp` engine is
   no longer needed.
4. Remove the `fsharp` engine option (CLI + extension), then delete the F#/C#
   tree wholesale under #6.
