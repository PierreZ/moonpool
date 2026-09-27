#!/usr/bin/env bash
#
# Print every stack scripts/ci/with-hang-traces.sh captured, one collapsible
# group per process, so a hung test can be read straight from the job log.

set -uo pipefail

trace_dir="${CI_TRACE_DIR:-target/ci-traces}"
shopt -s nullglob
traces=("$trace_dir"/hang-*.txt)
if ((${#traces[@]} == 0)); then
  echo "No hung test was sampled (nothing ran past ${HANG_TRACE_AFTER:-7}s)."
  exit 0
fi
for trace in "${traces[@]}"; do
  echo "::group::$(head -n 1 "$trace")"
  cat "$trace"
  echo "::endgroup::"
done
