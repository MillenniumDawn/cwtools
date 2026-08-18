# Agent instructions

Orientation for coding agents working in this repo. It points at the real docs
rather than repeating them; read the linked file when you need the detail.

`CLAUDE.md` is a symlink to this file, so Claude Code and anything reading
`AGENTS.md` get the same instructions.

## Layout

- `cwtools-rs/` is the Rust workspace and the whole active codebase. Everything
  builds from there.
- `scripts/corpus-guard.sh` and `scripts/corpus-baseline.csv` are the
  diagnostics regression gate, and `scripts/md-guard.sh` with
  `scripts/md-baseline.csv` is the same gate over a second real mod.
  `scripts/vanilla-guard.sh` and `scripts/vanilla-baseline.csv` are the tier
  over a committed synthetic base game, for the checks that need one (CW113,
  CW222, CW227, CW229, CW250, CW500).
- `CHANGELOG.md` at the repo root covers both the engine and the release notes.
- The F# tree is gone. Old parity notes that mention it are history, not a plan.

Four sibling checkouts matter, expected next to this repo (override the
location with `CWTOOLS_PROJECTS`):

- `cwtools-hoi4-config` holds the `.cwt` rules. They are not bundled, and a
  behavior change is as likely to belong there as here.
- `Kaiserreich-4-Development` and `Millennium-Dawn` are the pinned corpora the
  guards validate. Neither one substitutes for the other; each reports codes
  the other never does.
- `cwtools-vscode` is the VS Code extension. A new LSP capability usually needs
  a change there too.

The workspace shape: `parser` builds the AST, `rules` loads the `.cwt` config,
`index` builds the cross-file lookups, `validation` is the rule engine plus the
per-game validators, and `driver` is the shared load-and-validate pipeline both
binaries call. The `cwtools` CLI is batch (one `Session` per run); the
`cwtools-server` LSP keeps its own incremental state and does not use
`Session`. Run the CLI from source with
`cargo run -p cwtools_cli -- validate --game hoi4 --directory <mod> --rules <rules>/Config`.

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

While iterating, scope tests down: `cargo test -p cwtools_validation`, or
`cargo test -p cwtools_parser <substring>` for one test. Packages are named
`cwtools_*`; the short directory names under `crates/` won't resolve.

Then, from the repo root, the corpus guards. Run both for anything that touches
the parser, the rule engine, a validator, or the ruleset types:

```plaintext
./scripts/corpus-guard.sh
./scripts/md-guard.sh
```

The test suite proves the code compiles and behaves. The guards prove the
*diagnostics* did not move, which is the thing a "this changes nothing" refactor
is easy to believe and hard to demonstrate. Details, including the flags and the
input revisions, are in [CONTRIBUTING.md](cwtools-rs/CONTRIBUTING.md).

Three things to know about them:

- Each committed baseline is pinned to specific corpus and rules revisions,
  recorded in its `#` header and printed on every run. When either checkout has
  moved on you get a diff that has nothing to do with your change. Capture your
  own before-baseline against the current inputs
  (`CWTOOLS_BASELINE=/tmp/before.csv ./scripts/corpus-guard.sh --bless` on a
  clean tree) and compare against that instead.
- Re-blessing a committed baseline is for changes that are *meant* to move
  diagnostics, and the commit message has to say which codes moved and why. A
  re-bless with no explanation reads as a regression someone papered over.
- The two corpora are not a second opinion on each other. Kaiserreich is the
  only one reporting CW122, CW248, CW251 and CW280, Millennium Dawn the only
  one reporting CW105, CW255, CW262 and CW268, and the codes they share come
  out in nothing like the same proportions. A change that moves one baseline
  and not the other is usually telling you something.

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

## Releasing

A release is a tag. `.github/workflows/release.yml` fires on `v*`, builds the
Linux x86_64, macOS arm64 and Windows x86_64 archives, runs the release-profile
suite alongside them, and publishes the archives plus a `SHA256SUMS` as the
GitHub release. Nothing else ships the binaries. That tag is the only
authorization the workflow gets, so most of the process is about making it worth
trusting.

Cutting X.Y.Z:

1. Rename the top section of `CHANGELOG.md` from `# unreleased` to `# X.Y.Z`.
   The headings inside a version section are `##`.
2. Bump `version` under `[workspace.package]` in `cwtools-rs/Cargo.toml`. It is
   the only place the number lives; all 15 crates inherit it with
   `version.workspace = true`. Run any cargo command afterwards so `Cargo.lock`
   picks up the 15 member entries, and stage the lockfile with the manifest.
   Leaving it behind ships binaries that name the previous release.
3. Land both through a PR. `main` is protected, and the tag has to point into it.
4. Tag the merged commit, signed and annotated, and push the tag:

```plaintext
git tag -s -a vX.Y.Z -m vX.Y.Z
git push origin vX.Y.Z
```

The workflow refuses a lightweight tag, and refuses one whose commit is not an
ancestor of `origin/main`, both before anything builds. That is defense in
depth. What actually stops a bad tag is the `release-tags` ruleset, which holds
`refs/tags/v*` to signed tags and restricts creation, update, deletion and
non-fast-forward. So the tag has to be signed, not merely annotated, and that
needs `user.signingkey` set locally with the key registered on the GitHub
account as a signing key. SSH counts: `gpg.format ssh` with the public key path
as the signing key. A repo admin can bypass the ruleset, which is worth knowing
about and not worth using.

Dry run before the tag. `gh workflow run release.yml --ref <branch>` builds all
three platforms and runs the release tests off a branch, and publishing is gated
on a `refs/tags/` ref, so a branch dispatch cannot ship anything. Dispatching off
a tag does publish, which is the re-run path; `gh release upload --clobber` makes
that idempotent.

Rollback is delete, never move. `gh release delete vX.Y.Z --cleanup-tag` takes
the release and the tag down together, and the next push of that tag is a clean
run. The same ruleset restricts tag deletion, so that step needs the account
that can bypass it. Repointing a live tag at a new commit is the thing to avoid:
anyone who already pulled it keeps the old binaries under the same version
number. Cut X.Y.Z+1 instead.

Then the downstream one. `cwtools-vscode` bundles this engine as the
`submodules/cwtools` submodule, and its release refuses an untagged engine
(`git describe --tags --exact-match`), so the extension cannot ship until that
pointer sits on a release tag. Dependabot opens the bump PR and
`automerge-engine.yml` lands it once Test passes, but only forward: a bump to an
older tag is refused. Check the PR arrived rather than assuming it did.

## Performance work

Claim an improvement only with before and after numbers taken the same way. A
rebuild evicts the page cache, so a run straight after `cargo build` is not
comparable to one before it; interleave the two binaries instead of measuring
them in blocks.

Benches live in `crates/*/benches` and run under criterion.
`cargo bench -p cwtools_driver --bench rules_hot` covers the editor hot paths
and needs a rules checkout (`CWTOOLS_RULES`, or `CWTOOLS_PROJECTS`).
`cargo bench -p cwtools_driver --bench validate_hot` covers the batch
validation inner loop. `validate_prepared/fixture` always runs against an
in-repo script. `validate_prepared/scripted_effects` needs the same rules
checkout as `rules_hot` plus a corpus checkout (`CWTOOLS_CORPUS`,
`CWTOOLS_PROJECTS`, or a sibling of this repo).
[PROFILING.md](cwtools-rs/PROFILING.md) covers the `CWTOOLS_PROFILE`
instrumentation.

## Don't

- Don't silence a lint, a type error or a failing check to get to green. Fix the
  cause, even when it predates your change.
- Don't add a dependency without checking it passes `cargo deny` and is actually
  used (`cargo machete`). Both run in CI.
- Don't widen a public API for one caller. Most crates here are internal to the
  workspace.
- Don't hardcode a Unix-style absolute path (`Path::new("/ws")`, a bare
  `"/foo/bar"`) in a test that reaches `Url::from_file_path` or
  `Path::is_absolute`. A leading `/` with no drive letter isn't absolute on
  Windows, so CI fails there even though Linux/macOS are fine. Build the
  fixture with the `abs()` helper already in `crates/cli/src/report.rs` /
  `crates/cli/src/config.rs` (or add an equivalent local one).
- Don't assert an exact result from `Url::to_file_path` or `std::fs::canonicalize`
  without checking it holds on Windows. `to_file_path` yields a path only when
  the first segment is a drive letter (`http://localhost/etc/passwd` converts
  on Unix, not Windows; `http://localhost/C:/Windows` converts on both), and
  `canonicalize` collapses a `dir/..` pair even when `dir` doesn't exist, so
  a climbing `..` reports `OutsideWorkspace` there where Linux reports
  `Unresolvable`. Pick a fixture that converts on both platforms, assert the
  shared guarantee (`is_err()`, containment) rather than a platform-specific
  variant, or gate the assert `#[cfg(unix)]` with a comment saying why (see
  `crates/lsp/src/access.rs`). CI runs the full test suite on
  `windows-latest`, so a Unix-only assumption here turns every PR red.
- Don't change a type that gets serialized into a cache without bumping the
  format version next to it (`FORMAT_VERSION` in `crates/cache/src/io.rs` for
  `.cwb` and `ERRORS_FORMAT_VERSION` for its `.cwe` sidecar, `CACHE_VERSION` in
  `crates/index/src/vanilla_cache.rs` for `.cwv`, `CACHE_VERSION` in
  `crates/cache/src/workspace.rs` for the parse cache's fingerprint). The bump
  is what turns an old cache into a clean miss instead of a load error.
