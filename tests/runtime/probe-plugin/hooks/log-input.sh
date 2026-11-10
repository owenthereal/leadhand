#!/bin/sh
# PreToolUse hook for the lhprobe plugin: append one JSON line to $PROBE_LOG
# with the hook's stdin JSON, LEADHAND_RUN_ID and the working directory.
# Never blocks the tool call.
[ -n "${PROBE_LOG:-}" ] || exit 0
python3 -c '
import json, os, sys
raw = sys.stdin.read()
try:
    data = json.loads(raw)
except ValueError:
    data = {"unparsed": raw}
line = json.dumps({
    "hook_input": data,
    "LEADHAND_RUN_ID": os.environ.get("LEADHAND_RUN_ID"),
    "pwd": sys.argv[1],
})
with open(os.environ["PROBE_LOG"], "a") as f:
    f.write(line + "\n")
' "$(pwd)" || true
exit 0
