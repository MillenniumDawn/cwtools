# Building cwtools-rs

## Prerequisites

- **Rust toolchain** — stable, installed via [rustup](https://rustup.rs/).
  The workspace pins a minimum version in `rust-toolchain.toml`.
- **Windows**: the `rust-lld` component (LLVM linker) is recommended for
  faster LTO linking. Install it after rustup:

  ```plaintext
  rustup component add rust-lld
  ```

## Build

```plaintext
cd cwtools-rs
cargo build --release
```

Produces two binaries in `target/release/`:

| Binary | Purpose |
|---|---|
| `cwtools` | CLI validator (`validate`, `cache-vanilla`, etc.) |
| `cwtools-server` | LSP server (used by the VS Code extension) |

## Build all targets (debug + tests)

```plaintext
cargo build --workspace --all-targets
cargo test --workspace --all-features --no-fail-fast
```

## Release profile

The workspace uses `lto = "thin"` (not fat) and the default 16 codegen units.
Thin LTO parallelizes well and links much faster than fat LTO, especially on
Windows where MSVC `link.exe` is the bottleneck. The binary is ~5-10% larger
than fat LTO + `codegen-units = 1`, but `strip = true` keeps it small enough
that antivirus false positives are not a concern.

See `PROFILING.md` for build profiling and runtime tracing.

## Platform notes

### Windows

- The `.cargo/config.toml` sets `rust-lld.exe` as the linker for
  `x86_64-pc-windows-msvc`. This replaces MSVC `link.exe` with LLVM's
  linker, which is much faster at LTO linking.
- Without `rust-lld`, the build falls back to MSVC `link.exe` and is
  significantly slower. Install it with `rustup component add rust-lld`.

### macOS

- The system linker (`ld64`) is used. No special setup needed.

### Linux

- The system linker (`lld` or `gold`) is used. No special setup needed.

## CI

The `build-bench` workflow (`.github/workflows/build-bench.yml`) measures clean
release-build time across all three platforms. Run it manually from the Actions
tab or push to the `perf/build-time-improvements` branch.

The `release` workflow (`.github/workflows/release.yml`) builds and archives
binaries for all three platforms on `workflow_dispatch`.
