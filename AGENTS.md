# Agent instructions

Orientation for coding agents working in this repo. It points at the real docs
rather than repeating them; read the linked file when you need the detail.

`CLAUDE.md` is a symlink to this file, so Claude Code and anything reading
`AGENTS.md` get the same instructions.

## Layout

- `cwtools-rs/` is the Rust workspace and the whole active codebase. Everything
  builds from there.
- `scripts/corpus-guard.sh` and `scripts/corpus-baseline.csv` are the
  diagnostics regression gate.
- `CHANGELOG.md` at the repo root covers both the engine and the release notes.
- The F# tree is gone. Old parity notes that mention it are history, not a plan.

Three sibling checkouts matter, expected next to this repo (override the
location with `CWTOOLS_PROJECTS`):

- `cwtools-hoi4-config` holds the `.cwt` rules. They are not bundled, and a
  behavior change is as likely to belong there as here.
- `Kaiserreich-4-Development` is the pinned corpus the guard validates.
- `cwtools-vscode` is the VS Code extension. A new LSP capability usually needs
  a change there too.

Start with [ARCHITECTURE.md](cwtools-rs/docs/ARCHITECTURE.md) for the crate map
and the CLI-vs-LSP split, and [ERROR_CODES.md](cwtools-rs/docs/ERROR_CODES.md)
for what a CWxxx code means.

## Before you call anything done

From `cwtools-rs/`:

```plaintext
cargo fmt --all
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
```

Then, from the repo root, the corpus guard. Run it for anything that touches the
parser, the rule engine, a validator, or the ruleset types:

```plaintext
./scripts/corpus-guard.sh
```

The test suite proves the code compiles and behaves. The guard proves the
*diagnostics* did not move, which is the thing a "this changes nothing" refactor
is easy to believe and hard to demonstrate. Details, including the flags and the
input revisions, are in [CONTRIBUTING.md](cwtools-rs/CONTRIBUTING.md).

Two things to know about it:

- The committed baseline is pinned to specific corpus and rules revisions,
  recorded in its `#` header and printed on every run. When either checkout has
  moved on you get a diff that has nothing to do with your change. Capture your
  own before-baseline against the current inputs
  (`CWTOOLS_BASELINE=/tmp/before.csv ./scripts/corpus-guard.sh --bless` on a
  clean tree) and compare against that instead.
- Re-blessing the committed baseline is for changes that are *meant* to move
  diagnostics, and the commit message has to say which codes moved and why. A
  re-bless with no explanation reads as a regression someone papered over.

## Pre-commit hooks

`fmt` and `clippy` run on commit, the full suite runs on push. The hook stashes
unstaged files, which breaks a partial commit: either stage everything, or run
the three commands yourself and commit with `--no-verify`. Do not reach for
`--no-verify` to skip a failure.

## Changelog

Every user-visible change gets an entry in the top version section of
`CHANGELOG.md`, under `Features`, `Bug Fixes`, `Improvements`, `Notes` or
`Developer`, with the issue number in parentheses. Behavior changes that will
move someone's diagnostics or baselines go under `Notes` prefixed
`**Behavioral:**`. Write what the reader observes, not what the patch did.

## Performance work

Claim an improvement only with before and after numbers taken the same way. A
rebuild evicts the page cache, so a run straight after `cargo build` is not
comparable to one before it; interleave the two binaries instead of measuring
them in blocks.

Benches live in `crates/*/benches` and run under criterion.
`cargo bench -p cwtools_driver --bench rules_hot` covers the editor hot paths
and needs a rules checkout (`CWTOOLS_RULES`, or `CWTOOLS_PROJECTS`).
[PROFILING.md](cwtools-rs/PROFILING.md) covers the `CWTOOLS_PROFILE`
instrumentation.

## Don't

- Don't silence a lint, a type error or a failing check to get to green. Fix the
  cause, even when it predates your change.
- Don't add a dependency without checking it passes `cargo deny` and is actually
  used (`cargo machete`). Both run in CI.
- Don't widen a public API for one caller. Most crates here are internal to the
  workspace.
