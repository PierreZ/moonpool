#!/usr/bin/env bash
#
# Run a command (a nextest run) under a watchdog that captures the stacks of
# hung test processes before nextest's slow-timeout kills them.
#
# nextest reports a timed-out test with nothing but "(test timed out)": the
# process is killed before it can say where it was stuck. The watchdog polls
# for test binaries (anything running out of target/*/deps/) that have been
# alive longer than HANG_TRACE_AFTER seconds and samples every thread of each
# one once: `sample` on macOS, `gdb` on Linux. Forked children of a test (the
# explorer's workers) run the same binary and are sampled too.
#
# Stacks land in $CI_TRACE_DIR (default target/ci-traces) for the workflow to
# print and upload on failure. The command's exit status is preserved.
#
# Usage: scripts/ci/with-hang-traces.sh <command> [args...]

set -uo pipefail

trace_dir="${CI_TRACE_DIR:-target/ci-traces}"
threshold="${HANG_TRACE_AFTER:-7}"
mkdir -p "$trace_dir"

# Elapsed seconds from `ps -o etime=` ([[dd-]hh:]mm:ss), portable to macOS.
etime_seconds() {
  local etime="$1" days=0 h=0 m=0 s=0
  if [[ "$etime" == *-* ]]; then
    days="${etime%%-*}"
    etime="${etime#*-}"
  fi
  IFS=: read -r -a parts <<<"$etime"
  case "${#parts[@]}" in
    3) h="${parts[0]}" m="${parts[1]}" s="${parts[2]}" ;;
    2) m="${parts[0]}" s="${parts[1]}" ;;
    1) s="${parts[0]}" ;;
  esac
  echo $((10#$days * 86400 + 10#$h * 3600 + 10#$m * 60 + 10#$s))
}

# Sample every thread of `pid` into `out`.
capture() {
  local pid="$1" out="$2"
  case "$(uname -s)" in
    Darwin)
      sample "$pid" 2 -file "$out" >/dev/null 2>&1 ||
        sudo -n sample "$pid" 2 -file "$out" >/dev/null 2>&1
      ;;
    *)
      local gdb
      gdb="$(command -v gdb || true)"
      [[ -n "$gdb" ]] || { echo "gdb not found; no stack for $pid" >"$out"; return; }
      # ubuntu runners restrict ptrace to ancestors (ptrace_scope=1). The
      # redirect stays unprivileged on purpose: the runner user owns the file.
      # shellcheck disable=SC2024
      sudo -n "$gdb" -p "$pid" -batch -nx \
        -iex "set auto-load safe-path /" -ex "set pagination off" -ex "thread apply all bt" >"$out" 2>&1 ||
        "$gdb" -p "$pid" -batch -nx \
          -iex "set auto-load safe-path /" -ex "set pagination off" -ex "thread apply all bt" >"$out" 2>&1
      ;;
  esac
}

watchdog() {
  local seen=" "
  while sleep 1; do
    while read -r pid etime cmd; do
      [[ -n "${pid:-}" ]] || continue
      # Only the executable counts, not a shell whose arguments mention it.
      [[ "${cmd%% *}" == */target/*/deps/* ]] || continue
      [[ "$seen" == *" $pid "* ]] && continue
      (($(etime_seconds "$etime") >= threshold)) || continue
      seen+="$pid "
      local name out
      name="$(basename "${cmd%% *}")"
      out="$trace_dir/hang-${name}-${pid}.txt"
      {
        echo "# pid $pid, alive ${etime}, command: $cmd"
        echo "# captured at $(date -u +%Y-%m-%dT%H:%M:%SZ)"
      } >"$out.head"
      capture "$pid" "$out.body"
      cat "$out.head" "$out.body" >"$out" 2>/dev/null
      rm -f "$out.head" "$out.body"
    done < <(ps -axo pid=,etime=,command= 2>/dev/null)
  done
}

watchdog &
watchdog_pid=$!
trap 'kill "$watchdog_pid" 2>/dev/null' EXIT

"$@"
status=$?
kill "$watchdog_pid" 2>/dev/null
wait "$watchdog_pid" 2>/dev/null
exit "$status"
