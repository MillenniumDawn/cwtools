# Profiling

See `docs/ARCHITECTURE.md` for the loc system architecture and `BUILD.md`
for build instructions.

## Runtime profiling

The workspace is instrumented with [`tracing`](https://docs.rs/tracing). The
subscriber is off by default and only turns on when `RUST_LOG` is set, so normal
runs stay quiet.

## Run a profiled validate

```plaintext
RUST_LOG=info cargo run --release -p cwtools_cli -- \
  validate --game hoi4 --directory <mod> --rules <config>
```

With `RUST_LOG=info` the subscriber prints a span-close line for every
instrumented hot path, with its busy/idle time. The instrumented paths today:

- `parse_string` (parser) — one span per file parsed
- `collect_type_instances` (index) — one span per file indexed
- `post_process` (rules) — the single ruleset post-processing pass
- `validate_ast_with_loc` / `validate_prepared` (validation) — one span per file validated

Filter to a single crate to cut noise:

```plaintext
RUST_LOG=cwtools_validation=info cargo run --release -p cwtools_cli -- validate ...
RUST_LOG=cwtools_index=info,cwtools_rules=info cargo run --release -p cwtools_cli -- validate ...
```

(Diagnostics go to stdout; the trace output goes to stderr, so redirect with
`2> trace.log` to capture timings separately.)

## Add a new hot path

Put `#[tracing::instrument(skip_all)]` on the function (use `skip_all` so large
args aren't formatted), and make sure the crate has `tracing` in its
`Cargo.toml` (`tracing = { workspace = true }`). It shows up under
`RUST_LOG=<crate>=info` automatically.

## What to look for (runtime)

- A `parse_string` or `validate_ast_with_loc` span that dominates total time points at a
  pathological file (huge or deeply nested).
- `post_process` time scales with ruleset size; it runs once, so a large number
  there is a one-off cost, not per-file.
- `collect_type_instances` adds up across files; if indexing is slow, that's the
  span to drill into.

## Per-workspace ignore globs

The LSP workspace walk consults three lists, layered in this order:

1. **Engine baseline (always on)**: toolchain junk (`.git`, `target`, `.vs`, `node_modules`, `bin`,
   `obj`, `out`, `dist`, `.idea`, `.vscode`, `resources`)
   and free-form text files (`Changelog.txt`, `README.txt`, `LICENSE.txt`,
   `README.md`, `LICENSE.md`, `*.md`). These are hard-coded and cannot be
   disabled per-workspace — they exist because matching them otherwise
   wastes validator time on files that almost never contain script.
2. **User file globs**: forwarded by the extension from
   `cwtools.ignore.filePatterns` (in `settings.json`) into
   `initializationOptions.ignoreFilePatterns`. Re-read on every
   `workspace/didChangeConfiguration`.
3. **User directory globs**: same as above, key
   `cwtools.ignore.directories` → `initializationOptions.ignoreDirectories`.

Both user lists default to empty and extend (not replace) the engine
baseline. Patterns use `*` and `?` only (no `**`).

The CLI exposes the same two lists as repeatable flags:
`--ignore-file GLOB` and `--ignore-dir GLOB` on `validate`.

## Workspace parse cache (status)

Two pieces:

**3a (in-memory pass-through) — shipped.** The full-workspace scan runs two
passes over every file. Pass 1 parses to populate the type index; pass 2
re-parses and validates. Pass 1 used to throw away the AST and pass 2 used
to re-parse from disk. With the loc service now scoped to an inner block
(so peak RSS is bounded) we keep `Vec<Option<ParsedFile>>` between passes
and drop it before the profile/RSS-summary block. Net effect: ~4-6s
shaved off the scan, steady-state RSS unchanged.

**3b (on-disk persisted) — deferred.** The `ParsedFile` AST uses the LSP's
process-wide `StringTable` for every key, exposed as `StringTokens`
indices. Caching the AST to disk and reloading in a new process requires
the AST to be self-contained (owned `String` keys, no interner). That
crosses a parser/validation/info/LSP boundary and is too big to ride
along on a perf-fix branch.

When the time comes, the design is:

- **Storage**: `<state.cache_dir>/workspaces/<workspace-fingerprint>/`
  - `settings.sig` — 8-byte FNV-1a of (engine version, ruleset signature,
    user globs, workspace exclude dirs). Changing this invalidates the
    whole workspace dir.
  - `<file-blob-hash>.cwb` — one per file, keyed by FNV-1a of
    `(absolute_path, mtime_unix_nanos, file_size)`. rkyv+zstd, same crate
    the vanilla `cwtools_cache` uses.
- **Lookup order in pass 2**: in-memory (3a) → on-disk blob → re-parse.
- **Prerequisite refactor**: parser outputs a self-contained AST variant
  (or `rkyv::Archive` is derived directly after the `StringTable` is
  peeled back to a `HashMap<String, u32>` lookup). Either way, the
  `StringTable` becomes an optional layer on top of the AST, not baked
  into the type.
