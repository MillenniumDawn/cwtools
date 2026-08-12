#!/usr/bin/env bash

set -euo pipefail

: "${COVERAGE_THRESHOLD:=85}"

if ! command -v cargo-llvm-cov >/dev/null; then
	echo "cargo-llvm-cov is required. Install it with: cargo install cargo-llvm-cov" >&2
	exit 1
fi

script_dir=$(CDPATH='' cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)
repo_root=$(dirname -- "$script_dir")
workspace=${CWTOOLS_RS:-"$repo_root/cwtools-rs"}

cd "$workspace"
mkdir -p target/coverage

cargo llvm-cov \
	--workspace \
	--all-features \
	--lcov \
	--output-path target/coverage/lcov.info \
	--fail-under-lines "$COVERAGE_THRESHOLD"

cargo llvm-cov report --summary-only
