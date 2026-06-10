# Contributing

The active codebase is the Rust workspace in `cwtools-rs/`.

## Pre-commit hooks

We use [pre-commit](https://pre-commit.com) to run the same checks CI does, before
they leave your machine. Config is `.pre-commit-config.yaml` at the repo root.

Install once per clone:

```sh
pipx install pre-commit          # or: pip install --user pre-commit
pre-commit install --hook-type pre-commit --hook-type pre-push
```

What runs:

- **on commit**: `cargo fmt --all -- --check` and `cargo clippy --workspace --all-targets --all-features -- -D warnings`
- **on push**: `cargo test --workspace --all-features --no-fail-fast`

fmt and clippy keep commits fast; the full test suite gates the push. All three
mirror `.github/workflows/test.yml`, so a green local run means a green CI lint/test.

Bypass in a pinch with `git commit --no-verify` / `git push --no-verify`, but don't
make a habit of it. CI runs the same checks plus `cargo machete` and `cargo deny`.

## Running checks by hand

From `cwtools-rs/`:

```sh
cargo fmt --all
cargo clippy --workspace --all-targets --all-features -- -D warnings
cargo test --workspace
```
