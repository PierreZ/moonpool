#!/usr/bin/env bash
#
# RUSTC_WRAPPER for selective LLVM SanitizerCoverage instrumentation.
#
# Controlled by SANCOV_CRATES (comma-separated crate whitelist).
# When unset or empty, passes through to rustc unchanged.
#
# Usage:
#   # Instrument example crate only:
#   SANCOV_CRATES=moonpool_explorer_examples cargo run --bin maze_explore
#
#   # Instrument app + framework libraries:
#   SANCOV_CRATES=my_app,moonpool,moonpool_transport cargo run --bin my_app
#
#   # No instrumentation (pass-through):
#   cargo run --bin maze_explore

set -euo pipefail

RUSTC="$1"
shift

# Pass through when SANCOV_CRATES is unset or empty
if [[ -z "${SANCOV_CRATES:-}" ]]; then
    exec "$RUSTC" "$@"
fi

# Parse --crate-name and detect --crate-type proc-macro from args
CRATE_NAME=""
IS_PROC_MACRO=false
PREV=""
for arg in "$@"; do
    if [[ "$PREV" == "--crate-name" ]]; then
        CRATE_NAME="$arg"
    fi
    if [[ "$arg" == "proc-macro" && "$PREV" == "--crate-type" ]]; then
        IS_PROC_MACRO=true
    fi
    PREV="$arg"
done

# Never instrument build scripts or proc-macros
if [[ "$CRATE_NAME" == "build_script_build" ]] || [[ "$IS_PROC_MACRO" == true ]]; then
    exec "$RUSTC" "$@"
fi

# Check if crate is in the whitelist
INSTRUMENT=false
IFS=',' read -ra CRATES <<< "$SANCOV_CRATES"
for crate in "${CRATES[@]}"; do
    if [[ "$crate" == "$CRATE_NAME" ]]; then
        INSTRUMENT=true
        break
    fi
done

if [[ "$INSTRUMENT" == true ]]; then
    exec "$RUSTC" "$@" \
        -Cpasses=sancov-module \
        -Cllvm-args=-sanitizer-coverage-level=3 \
        -Cllvm-args=-sanitizer-coverage-inline-8bit-counters \
        -Ccodegen-units=1
else
    exec "$RUSTC" "$@"
fi
