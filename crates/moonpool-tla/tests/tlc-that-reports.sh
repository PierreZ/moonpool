#!/usr/bin/env bash
# TLC, with a progress line after each line that is one printed string.
#
# TLC prints a progress line from a thread of its own (the first after a
# few seconds, then one each minute), so in a long run that line can fall
# between any two prints of the spec. This launcher puts it there on each
# run: after each print of a lone string, as `PrintT("...")` prints it.
# It runs the TLC that `MOONPOOL_TLC` names, or `tlc`.
set -o pipefail
"${MOONPOOL_TLC:-tlc}" "$@" 2>&1 | while IFS= read -r line; do
  printf '%s\n' "$line"
  case "$line" in
    \"*\") echo 'Progress(123) at 2026-11-23 10:04:37: 9 states generated, 4 distinct states found, 0 states left on queue.' ;;
  esac
done
