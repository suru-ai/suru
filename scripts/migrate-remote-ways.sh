#!/bin/sh
# Rewrites the Remotes in remotes.json written before a Remote was reached by
# ways (#486, #493) into the shape the current build reads. A Server that finds
# an old Remote there refuses to start with
#   read Pairing records ".../remotes.json": missing field `ways`
#
#   addresses: ["1.2.3.4:7777", ...]  -> ways: [{"direct": "1.2.3.4:7777"}, ...]
#   last_good_address: "1.2.3.4:7777" -> answered: [{"direct": "1.2.3.4:7777"}]
#   last_answered: {...}              -> answered: [{...}]
#
# Only Remotes still in an old shape change, so the script is idempotent: a
# second run migrates nothing. Runs on macOS and Linux with jq. Stop the Suru
# server for the Channel first: a running server holds its Remotes in memory
# and would write its own copy back.

set -eu

usage() {
    cat <<'EOF'
Usage: migrate-remote-ways.sh [--dry-run] [--channel NAME | --file PATH]

  --dry-run       Report what would change without writing anything.
  --channel NAME  The Channel whose remotes.json to migrate (default:
                  $SURU_CHANNEL, else release). Honors SURU_DATA_DIR and
                  SURU_STATE_DIR.
  --file PATH     Migrate this remotes.json instead of a Channel's.
EOF
}

die() {
    printf 'migrate-remote-ways: %s\n' "$*" >&2
    exit 1
}

dry_run=false
channel=${SURU_CHANNEL:-release}
remotes=
while [ $# -gt 0 ]; do
    case $1 in
        --dry-run) dry_run=true ;;
        --channel) [ $# -ge 2 ] || die "--channel needs a name"; channel=$2; shift ;;
        --file) [ $# -ge 2 ] || die "--file needs a path"; remotes=$2; shift ;;
        -h | --help) usage; exit 0 ;;
        *) usage >&2; exit 2 ;;
    esac
    shift
done

command -v jq >/dev/null 2>&1 || die "jq is not installed"

# Mirrors the roots main.rs resolves through the dirs crate: macOS keeps state
# beside data, and a non-release Channel lives in a subdirectory of each.
case $(uname -s) in
    Darwin)
        data_base="$HOME/Library/Application Support/suru"
        state_base=$data_base
        ;;
    *)
        data_base="${XDG_DATA_HOME:-$HOME/.local/share}/suru"
        state_base="${XDG_STATE_HOME:-$HOME/.local/state}/suru"
        ;;
esac
data_base=${SURU_DATA_DIR:-$data_base}
state_base=${SURU_STATE_DIR:-$state_base}
channel_root() {
    if [ "$channel" = release ]; then printf '%s\n' "$1"; else printf '%s/%s\n' "$1" "$channel"; fi
}

if [ -z "$remotes" ]; then
    remotes="$(channel_root "$data_base")/remotes.json"
    runtime="$(channel_root "$state_base")/runtime.json"
    if ! $dry_run && [ -f "$runtime" ]; then
        pid=$(sed -n 's/.*"pid":\([0-9][0-9]*\).*/\1/p' "$runtime")
        if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
            die "the $channel Suru server (pid $pid) is running; stop it first"
        fi
    fi
fi
if [ ! -f "$remotes" ]; then
    printf 'No remotes.json at %s; nothing to migrate.\n' "$remotes"
    exit 0
fi

old='has("addresses") or has("last_good_address") or has("last_answered")'
migration="map(
    if has(\"addresses\") then .ways = [.addresses[] | {direct: .}] | del(.addresses) else . end
    | if has(\"last_answered\") then
        .answered = ([.last_answered | select(. != null)] + (.answered // [])) | del(.last_answered)
      else . end
    | if has(\"last_good_address\") then
        .answered = ([.last_good_address | select(. != null) | {direct: .}] + (.answered // []))
        | del(.last_good_address)
      else . end
)"

pending=$(jq "[.[] | select($old)] | length" "$remotes") || die "cannot read $remotes"
printf '%8d  remotes: addresses/last_good_address/last_answered -> ways/answered\n' "$pending"
if $dry_run || [ "$pending" = 0 ]; then
    exit 0
fi

scratch=$(mktemp -d "${TMPDIR:-/tmp}/suru-migrate.XXXXXX")
trap 'rm -rf "$scratch"' EXIT

backup="$remotes.bak-$(date -u +%Y%m%dT%H%M%S)"
cp -p "$remotes" "$backup"
printf 'Backed up %s to %s\n' "$remotes" "$backup"

jq -c "$migration" "$remotes" >"$scratch/remotes.json" || die "migration failed; $remotes is unchanged"
# Rewritten in place so the file keeps its owner-only mode.
cat "$scratch/remotes.json" >"$remotes"

# Idempotence doubles as verification: nothing the migration covers remains.
left=$(jq "[.[] | select($old)] | length" "$remotes")
[ "$left" = 0 ] || die "$left Remotes still in an old shape after migrating; restore $backup"
printf 'Migrated %s; a second pass finds nothing left.\n' "$remotes"
