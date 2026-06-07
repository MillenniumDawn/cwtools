# Cruft audit (F#-era leftovers)

Inventory of what predates the Rust rewrite. The end goal is deleting the F#
stack (issue #6). The "delete now" tier is done (see below); the rest is the
keep / move list to work from.

## The live dependency to break first

The Rust CLI still delegates to F# when run with `--engine fsharp`
(`cwtools-rs/crates/cli/src/main.rs`, `run_fsharp_engine` / `locate_fsharp_cli`),
shelling out to `CWToolsCLI.dll` via `dotnet`. So "delete F#" is gated on:
1. Rust reaching parity for whatever the F# engine is still used to cross-check.
2. Removing the `fsharp` engine option from the CLI and the extension
   (`cwtools.engine` setting in cwtools-vscode still offers `fsharp`).

Until that, keep the F# build working but stop investing in it.

## Deleted

- ~~**`.vscode_ext_extension.ts`, `.vscode_ext_executable.ts`** (repo root)~~ — stale
  TypeScript templates. Were already gitignored (never tracked); removed.
- ~~**`CWToolsCSTests/`**~~ — C# tests against the F# lib. Removed, along with its
  entries in `cwtools.slnx` and `build/Program.fs`.
- ~~**`CWToolsDocs/`**~~ — F# API-doc generator (superseded by `cwtools-rs/docs/`).
  Removed, along with its `cwtools.slnx` / `build/Program.fs` entries. Its
  `testconfig/cwtools-ir-config/` had been the fixture for the Rust
  `load_ruleset_dir` test; that test now loads the real `cwtools-hoi4-config`
  repo instead (a sibling checkout, or `CWTOOLS_HOI4_CONFIG`). Bundling a dead
  Imperator config inside the engine repo was the wrong place for it — the whole
  point of `.cwt` is that the config lives in its own repo.

  Note: the first attempt deleted these two without removing their `.slnx` /
  build refs and silently broke the Rust test (it skips when the path is absent).
  Always grep `cwtools.slnx` + `build/Program.fs` before deleting an F#/C# project.

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

1. `.vscode_ext_*.ts`, `CWToolsCSTests/`, `CWToolsDocs/` are gone (the latter two
   with their `.slnx` / `build/Program.fs` refs). Before deleting any other F#/C#
   project, grep `cwtools.slnx` and `build/Program.fs` first — solution membership
   was missed once and those two had to be restored before deleting them properly.
2. Resolve the root `.cwt` files into the config repo or remove them.
3. Drive Rust parity (the open `enhancement` issues) until the `fsharp` engine is
   no longer needed.
4. Remove the `fsharp` engine option (CLI + extension), then delete the F#/C#
   tree wholesale under #6.
