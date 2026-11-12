#!/bin/sh
# Settings-overlay probe: needs network and a write outside the workspace.
# Usage: net-probe.sh TAG   (writes ~/.leadhand-probe/net-probe-TAG, prints the HTTP code)
tag=${1:-marker}
dir="$HOME/.leadhand-probe"
code=$(curl -sS -o /dev/null -w '%{http_code}' https://api.github.com/zen) || code="curl-failed:$code"
if mkdir -p "$dir" && printf '%s\n' "$code" > "$dir/net-probe-$tag"; then
  wrote=yes
else
  wrote=no
fi
echo "http_code=$code marker_written=$wrote"
