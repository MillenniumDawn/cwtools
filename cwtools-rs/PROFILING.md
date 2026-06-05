# Profiling

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
- `collect_type_instances` (info) — one span per file indexed
- `post_process` (rules) — the single ruleset post-processing pass
- `validate_ast` (validation) — one span per file validated

Filter to a single crate to cut noise:

```plaintext
RUST_LOG=cwtools_validation=info cargo run --release -p cwtools_cli -- validate ...
RUST_LOG=cwtools_info=info,cwtools_rules=info cargo run --release -p cwtools_cli -- validate ...
```

(Diagnostics go to stdout; the trace output goes to stderr, so redirect with
`2> trace.log` to capture timings separately.)

## Add a new hot path

Put `#[tracing::instrument(skip_all)]` on the function (use `skip_all` so large
args aren't formatted), and make sure the crate has `tracing` in its
`Cargo.toml` (`tracing = { workspace = true }`). It shows up under
`RUST_LOG=<crate>=info` automatically.

## What to look for

- A `parse_string` or `validate_ast` span that dominates total time points at a
  pathological file (huge or deeply nested).
- `post_process` time scales with ruleset size; it runs once, so a large number
  there is a one-off cost, not per-file.
- `collect_type_instances` adds up across files; if indexing is slow, that's the
  span to drill into.
