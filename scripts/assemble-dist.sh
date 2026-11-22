#!/bin/sh
# Assembles, or checks, the plugin tree the release workflow publishes on the dist branch:
# the plugin files and the three platform binaries, installable on its own.
#
#   assemble-dist.sh <version> <out dir> <binaries dir>
#       Refuses unless <version> is the version in Cargo.toml, .claude-plugin/plugin.json,
#       .codex-plugin/plugin.json and .claude-plugin/marketplace.json's leadhand entry, and
#       unless <binaries dir> holds <target>/leadhand for each target (darwin-arm64,
#       linux-x86_64, linux-arm64). Then copies the plugin files into <out dir> (new or
#       empty), the binaries as bin/<target>/leadhand with mode 0755, points the marketplace's
#       leadhand entry at "./", and runs --check on the result.
#   assemble-dist.sh --check <dir>
#       Checks an assembled tree, changing nothing: every plugin path is there and nothing
#       else, each bin/<target>/leadhand and bin/leadhand is executable, every manifest is
#       valid JSON at one version, and the marketplace's leadhand source is "./".
#
# The sources are the package this script belongs to (its parent folder). Needs jq.
set -eu

# What a dist tree holds besides the binaries.
plugin_paths='.claude-plugin .codex-plugin agents skills presets bin/leadhand docs README.md LICENSE'
targets='darwin-arm64 linux-x86_64 linux-arm64'
manifests='.claude-plugin/plugin.json .codex-plugin/plugin.json .claude-plugin/marketplace.json'

usage() {
    echo "usage: assemble-dist.sh <version> <out dir> <binaries dir>" >&2
    echo "       assemble-dist.sh --check <dir>" >&2
    echo "<binaries dir> holds <target>/leadhand for each of: $targets" >&2
    exit 2
}

fail() {
    echo "assemble-dist: $*" >&2
    exit 1
}

command -v jq >/dev/null 2>&1 || fail "needs jq, to read and write the JSON manifests; install jq"

# The version of a manifest: of the plugin, or of the marketplace's single leadhand entry.
# Prints nothing, and fails, when the file isn't valid JSON or has no such version.
manifest_version() {
    case $1 in
        */marketplace.json)
            jq -er '[.plugins[]? | select(.name == "leadhand")]
                | if length == 1 then .[0].version | strings else empty end' "$1" 2>/dev/null
            ;;
        *) jq -er '.version | strings' "$1" 2>/dev/null ;;
    esac
}

# The names in folder $1, hidden ones included, one per line.
names() {
    for entry in "$1"/* "$1"/.[!.]* "$1"/..?*; do
        if [ -e "$entry" ] || [ -L "$entry" ]; then
            echo "${entry##*/}"
        fi
    done
}

default_ifs=$IFS
newline='
'

# Reports each name in folder $1 that isn't one of the names after $2, as
# "unexpected $2<name>". Each name is compared whole: not split, and not expanded as a glob.
only() {
    folder=$1 prefix=$2
    shift 2
    list=$(names "$folder")
    set -f
    IFS=$newline
    for entry in $list; do
        allowed=no
        for name in "$@"; do
            if [ "$entry" = "$name" ]; then
                allowed=yes
            fi
        done
        if [ "$allowed" = no ]; then
            problem "unexpected $prefix$entry"
        fi
    done
    IFS=$default_ifs
    set +f
}

problems=0
problem() {
    echo "assemble-dist: $dir: $*" >&2
    problems=$((problems + 1))
}

# --check <dir>: prints every problem, then fails if there was one.
check() {
    dir=$1
    [ -d "$dir" ] || fail "no such folder: $dir"

    for path in $plugin_paths; do
        if [ ! -e "$dir/$path" ]; then
            problem "missing $path"
        elif [ -d "$dir/$path" ] && [ -z "$(names "$dir/$path")" ]; then
            problem "$path is empty"
        fi
    done
    if [ -e "$dir/bin/leadhand" ] && ! { [ -f "$dir/bin/leadhand" ] && [ -x "$dir/bin/leadhand" ]; }; then
        problem "bin/leadhand is not an executable file"
    fi

    # Nothing else: at the top, the plugin paths (and .git, in a checkout of the branch); in
    # bin/, the script and the targets.
    # shellcheck disable=SC2086 # the lists are words
    only "$dir" '' $plugin_paths bin .git
    if [ -d "$dir/bin" ]; then
        # shellcheck disable=SC2086
        only "$dir/bin" bin/ leadhand $targets
    fi

    for target in $targets; do
        bin=bin/$target/leadhand
        if [ ! -f "$dir/$bin" ]; then
            problem "missing $bin"
        elif [ ! -x "$dir/$bin" ]; then
            problem "$bin is not executable"
        fi
    done

    version='' first=''
    for manifest in $manifests; do
        if [ ! -f "$dir/$manifest" ]; then
            problem "missing $manifest"
        elif ! jq empty "$dir/$manifest" >/dev/null 2>&1; then
            problem "$manifest is not valid JSON"
        elif ! v=$(manifest_version "$dir/$manifest"); then
            problem "$manifest has no leadhand version"
        elif [ -z "$version" ]; then
            version=$v first=$manifest
        elif [ "$v" != "$version" ]; then
            problem "$manifest is at version $v, but $first is at version $version"
        fi
    done

    marketplace=$dir/.claude-plugin/marketplace.json
    if [ -f "$marketplace" ] && jq empty "$marketplace" >/dev/null 2>&1; then
        if ! jq -e '[.plugins[]? | select(.name == "leadhand")]
                | length == 1 and .[0].source == "./"' "$marketplace" >/dev/null; then
            problem ".claude-plugin/marketplace.json's leadhand source is not \"./\""
        fi
    fi

    [ "$problems" -eq 0 ] || fail "$dir is not a complete dist tree ($problems problems)"
    echo "assemble-dist: $dir is a complete dist tree for leadhand $version"
}

if [ "${1-}" = --check ]; then
    [ $# -eq 2 ] || usage
    check "$2"
    exit 0
fi
[ $# -eq 3 ] || usage
version=$1 out=$2 bins=$3

src=$(CDPATH='' cd -- "$(dirname -- "$0")/.." && pwd -P)

# The version gate: <version> must be the package's version in all four places.
mismatches=0
mismatch() {
    echo "assemble-dist: version '$version' is not $1's version ($2)" >&2
    mismatches=$((mismatches + 1))
}
cargo_version=$(awk '
    /^\[/ { in_package = ($0 == "[package]"); next }
    in_package && /^version *= *"/ { sub(/^version *= *"/, ""); sub(/".*/, ""); print; exit }
' "$src/Cargo.toml")
[ "$version" = "$cargo_version" ] || mismatch Cargo.toml "$cargo_version"
for manifest in $manifests; do
    v=$(manifest_version "$src/$manifest") || v='none, or not valid JSON'
    [ "$version" = "$v" ] || mismatch "$manifest" "$v"
done
[ "$mismatches" -eq 0 ] ||
    fail "refused: a release v<version> must match the version in Cargo.toml and every manifest"

for target in $targets; do
    [ -f "$bins/$target/leadhand" ] || fail "no binary at $bins/$target/leadhand"
done
if [ -e "$out" ] && [ -n "$(names "$out")" ]; then
    fail "$out is not empty; assemble into a new or empty folder"
fi

mkdir -p "$out/bin"
for path in $plugin_paths; do
    cp -R "$src/$path" "$out/$path"
done
for target in $targets; do
    mkdir "$out/bin/$target"
    cp "$bins/$target/leadhand" "$out/bin/$target/leadhand"
    chmod 0755 "$out/bin/$target/leadhand"
done

# The dist tree is its own marketplace's plugin.
marketplace=$out/.claude-plugin/marketplace.json
jq '(.plugins[] | select(.name == "leadhand") | .source) = "./"' "$marketplace" >"$marketplace.new"
mv "$marketplace.new" "$marketplace"

check "$out"
