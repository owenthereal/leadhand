#!/bin/sh
# Fake dispatch runner for leadhand's Task 3 lifecycle probe. It stands in for
# the detached runner that will own a Codex worker.
#
# Usage: fake-runner.sh [--detach] [DIR]     (DIR defaults to $FAKE_RUNNER_DIR)
#
# Writes into DIR:
#   ids          pid, pgid, ppid, mode and start time (written first)
#   pid          its pid (the probe waits for this file)
#   heartbeat    one epoch-seconds line every $FAKE_RUNNER_INTERVAL s (default 5)
#   signals      one line per HUP/INT/TERM it caught, before it exits
#   result.json  after $FAKE_RUNNER_SECONDS s (default 120), written atomically
#
# Without --detach it runs in the foreground, in the caller's process group.
# With --detach it starts a copy of itself in a new session and returns as soon
# as that copy has written its pid. macOS has no setsid binary, so the detach is
# python3: fork, os.setsid() in the child, then exec /bin/sh on this script.
set -u

DETACH_METHOD="python3 fork + os.setsid + os.execv"

detach=no
child=no
if [ "${1:-}" = "--detach" ]; then detach=yes; shift; fi
if [ "${1:-}" = "--child" ]; then child=yes; shift; fi
dir=${1:-${FAKE_RUNNER_DIR:-}}
if [ -z "$dir" ]; then
  echo "usage: fake-runner.sh [--detach] DIR   (or set FAKE_RUNNER_DIR)" >&2
  exit 2
fi
secs=${FAKE_RUNNER_SECONDS:-120}
interval=${FAKE_RUNNER_INTERVAL:-5}
mkdir -p "$dir" || exit 1

if [ "$detach" = yes ]; then
  rm -f "$dir/pid"
  python3 -c '
import os, sys
if os.fork():
    os._exit(0)
os.setsid()
os.execv(sys.argv[1], sys.argv[1:])
' /bin/sh "$0" --child "$dir" </dev/null >>"$dir/runner.log" 2>&1
  i=0
  while [ ! -s "$dir/pid" ] && [ "$i" -lt 100 ]; do
    sleep 0.1
    i=$((i + 1))
  done
  if [ -s "$dir/pid" ]; then
    echo "fake-runner: detached pid=$(cat "$dir/pid") method=$DETACH_METHOD"
    exit 0
  fi
  echo "fake-runner: detached runner wrote no pid within 10 s" >&2
  exit 1
fi

if [ "$child" = yes ]; then
  mode=detached
  method=$DETACH_METHOD
else
  mode=attached
  method=none
fi

on_signal() {
  echo "$1 $(date +%s)" >>"$dir/signals"
  exit "$2"
}
trap 'on_signal HUP 129' HUP
trap 'on_signal INT 130' INT
trap 'on_signal TERM 143' TERM

started=$(date +%s)
# python3, not ps: ps may be blocked inside a sandboxed Bash tool.
pgid=$(python3 -c 'import os, sys; print(os.getpgid(int(sys.argv[1])))' "$$" 2>/dev/null)
printf 'pid=%s pgid=%s ppid=%s mode=%s detach_method=%s started=%s\n' \
  "$$" "$pgid" "$PPID" "$mode" "$method" "$started" >"$dir/ids"
echo "$$" >"$dir/pid.tmp" && mv "$dir/pid.tmp" "$dir/pid"

beats=0
while [ $(($(date +%s) - started)) -lt "$secs" ]; do
  date +%s >>"$dir/heartbeat"
  beats=$((beats + 1))
  sleep "$interval"
done

finished=$(date +%s)
printf '{"pid": %s, "mode": "%s", "started": %s, "finished": %s, "heartbeats": %s}\n' \
  "$$" "$mode" "$started" "$finished" "$beats" >"$dir/result.json.tmp" &&
  mv "$dir/result.json.tmp" "$dir/result.json"
echo "fake-runner: done pid=$$ mode=$mode heartbeats=$beats"
