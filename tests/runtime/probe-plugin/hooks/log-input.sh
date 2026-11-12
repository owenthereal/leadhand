#!/bin/sh
# PreToolUse hook for the lhprobe plugin: append one JSON line to $PROBE_LOG
# with the hook's stdin JSON, LEADHAND_RUN_ID and the working directory.
# Never blocks the tool call, except: with PROBE_DENY_UNSANDBOXED=1, a Bash
# call that sets dangerouslyDisableSandbox is denied (and logged with "denied").
[ -n "${PROBE_LOG:-}" ] || exit 0
python3 -c '
import json, os, sys
raw = sys.stdin.read()
try:
    data = json.loads(raw)
except ValueError:
    data = {"unparsed": raw}
tool_input = data.get("tool_input") if isinstance(data, dict) else None
deny = (os.environ.get("PROBE_DENY_UNSANDBOXED") == "1"
        and isinstance(tool_input, dict)
        and bool(tool_input.get("dangerouslyDisableSandbox")))
entry = {
    "hook_input": data,
    "LEADHAND_RUN_ID": os.environ.get("LEADHAND_RUN_ID"),
    "pwd": sys.argv[1],
}
if deny:
    entry["denied"] = True
with open(os.environ["PROBE_LOG"], "a") as f:
    f.write(json.dumps(entry) + "\n")
if deny:
    print(json.dumps({"hookSpecificOutput": {
        "hookEventName": "PreToolUse",
        "permissionDecision": "deny",
        "permissionDecisionReason": "lhprobe: dangerouslyDisableSandbox is not allowed in this probe; do not retry.",
    }}))
' "$(pwd)" || true
exit 0
