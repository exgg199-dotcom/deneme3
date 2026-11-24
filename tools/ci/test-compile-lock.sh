#!/usr/bin/env bash
# Serialize the two large test compiles, preserving the configured compiler cache wrapper.
set -euo pipefail
# Cargo retries E2BIG with newline-delimited response files; inspect without rewriting argv.
classify_args=()
for arg in "$@"; do
  if [[ "$arg" == @* && -f "${arg#@}" ]]; then
    while IFS= read -r line || [ -n "$line" ]; do
      classify_args+=("$line")
    done < "${arg#@}"
  else
    classify_args+=("$arg")
  fi
done
crate_name= previous= test_unit=false
for arg in "${classify_args[@]}"; do
  if [ "$previous" = --crate-name ]; then crate_name=$arg; fi
  case "$arg" in
    --crate-name=*) crate_name=${arg#--crate-name=} ;;
    --test) test_unit=true ;;
  esac
  previous=$arg
done
command=("$@")
if [ -n "${HORIZON_TEST_RUSTC_WRAPPER:-}" ]; then
  command=("$HORIZON_TEST_RUSTC_WRAPPER" "$@")
fi
if $test_unit && { [ "$crate_name" = horizon_client ] || [ "$crate_name" = horizon_client_lib ]; }; then
  # Keep the lock in flock's parent, not in compiler/cache daemon descendants.
  lock="${RUNNER_TEMP:?}/horizon-lib-test-compile.lock"
  if command -v flock >/dev/null 2>&1; then
    exec flock -x -o "$lock" "${command[@]}"
  fi
  # macOS has fcntl locks but no flock command. The supervisor owns the lock,
  # and never lends it to compiler/cache-daemon descendants.
  exec python3 "$(dirname "$0")/test_compile_lock.py" "$lock" "${command[@]}"
fi
exec "${command[@]}"
