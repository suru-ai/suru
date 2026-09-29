#!/bin/sh
# Rewrites stored Sessions and Worktree preparation intents written in formats
# Suru no longer reads into the shape the current build writes. Run it before
# upgrading past the build that dropped the read-time fallbacks for them, or
# the Sessions affected list as unreadable.
#
# Each old shape is one UPDATE whose WHERE matches only rows still in it, so
# the script is idempotent: a second run migrates nothing. A few old shapes
# cannot be rewritten in SQL, or never had a fallback; those are counted and
# left alone.
#
# Runs on macOS and Linux with the sqlite3 CLI (JSON functions built in since
# SQLite 3.38). Stop the Suru server for the Channel first: a running server
# holds Sessions in memory and would write its own copies back.

set -eu

usage() {
    cat <<'EOF'
Usage: migrate-legacy-storage.sh [--dry-run] [--channel NAME | --db PATH]

  --dry-run       Migrate a throwaway copy and report what would change.
  --channel NAME  The Channel whose database to migrate (default: $SURU_CHANNEL,
                  else release). Honors SURU_DATA_DIR and SURU_STATE_DIR.
  --db PATH       Migrate this suru.db, and the Worktree preparations beside
                  it, instead of a Channel's.
EOF
}

die() {
    printf 'migrate-legacy-storage: %s\n' "$*" >&2
    exit 1
}

dry_run=false
channel=${SURU_CHANNEL:-release}
database=
while [ $# -gt 0 ]; do
    case $1 in
        --dry-run) dry_run=true ;;
        --channel) [ $# -ge 2 ] || die "--channel needs a name"; channel=$2; shift ;;
        --db) [ $# -ge 2 ] || die "--db needs a path"; database=$2; shift ;;
        -h | --help) usage; exit 0 ;;
        *) usage >&2; exit 2 ;;
    esac
    shift
done

command -v sqlite3 >/dev/null 2>&1 || die "sqlite3 is not installed"
[ "$(sqlite3 :memory: "SELECT json_insert('{}', '\$.a', 1);" 2>/dev/null)" = '{"a":1}' ] ||
    die "this sqlite3 lacks the JSON functions; install SQLite 3.38 or later"

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

if [ -z "$database" ]; then
    database="$(channel_root "$data_base")/suru.db"
    runtime="$(channel_root "$state_base")/runtime.json"
    if ! $dry_run && [ -f "$runtime" ]; then
        pid=$(sed -n 's/.*"pid":\([0-9][0-9]*\).*/\1/p' "$runtime")
        if [ -n "$pid" ] && kill -0 "$pid" 2>/dev/null; then
            die "the $channel Suru server (pid $pid) is running; stop it first"
        fi
    fi
fi
[ -f "$database" ] || die "no database at $database"

# The Session migration. Every UPDATE is followed by a report row counting what
# it changed; rows counted with migrated = 0 are old shapes left in place.
migration() {
    cat <<'SQL'
.bail on
CREATE TEMP TABLE report (label TEXT NOT NULL, rows INTEGER NOT NULL, migrated INTEGER NOT NULL);
BEGIN IMMEDIATE;

-- sessions.workspace: Session metadata

UPDATE sessions
SET workspace = json_insert(workspace, '$.execution_directory',
    json_object('path', json_extract(workspace, '$.path')))
WHERE json_type(workspace, '$.execution_directory') IS NULL;
INSERT INTO report VALUES ('sessions: execution_directory absent -> {"path": $.path}', changes(), 1);

UPDATE sessions SET workspace = json_insert(workspace, '$.checkout', NULL)
WHERE json_type(workspace, '$.checkout') IS NULL;
INSERT INTO report VALUES ('sessions: checkout absent -> null', changes(), 1);

UPDATE sessions SET workspace = json_insert(workspace, '$.approval_posture', NULL)
WHERE json_type(workspace, '$.approval_posture') IS NULL;
INSERT INTO report VALUES ('sessions: approval_posture absent -> null', changes(), 1);

UPDATE sessions SET workspace = json_insert(workspace, '$.approval_posture.application', 'applied')
WHERE json_type(workspace, '$.approval_posture') = 'object'
    AND json_type(workspace, '$.approval_posture.application') IS NULL;
INSERT INTO report VALUES ('sessions: approval_posture.application absent -> "applied"', changes(), 1);

UPDATE sessions SET workspace = json_insert(workspace, '$.workspace.icon', NULL)
WHERE json_type(workspace, '$.workspace') = 'object'
    AND json_type(workspace, '$.workspace.icon') IS NULL;
INSERT INTO report VALUES ('sessions: workspace.icon absent -> null', changes(), 1);

UPDATE sessions
SET workspace = json_insert(workspace, '$.workspace.repository.capabilities.rename_branch',
    json('{"status":"unsupported","reason":"This source control operation is not implemented"}'))
WHERE json_type(workspace, '$.workspace.repository.capabilities') = 'object'
    AND json_type(workspace, '$.workspace.repository.capabilities.rename_branch') IS NULL;
INSERT INTO report VALUES ('sessions: repository.capabilities.rename_branch absent -> unsupported', changes(), 1);

UPDATE sessions
SET workspace = json_insert(workspace, '$.checkout.recovery_revision', NULL, '$.checkout.reclaim', NULL)
WHERE json_type(workspace, '$.checkout') = 'object'
    AND (json_type(workspace, '$.checkout.recovery_revision') IS NULL
        OR json_type(workspace, '$.checkout.reclaim') IS NULL);
INSERT INTO report VALUES ('sessions: checkout.recovery_revision/reclaim absent -> null', changes(), 1);

-- A path-only record derives its Workspace ID from a blake3 hash of the path,
-- which SQLite cannot compute.
INSERT INTO report SELECT 'sessions: path-only record (unreadable, workspace needs a blake3 ID)', count(*), 0
FROM sessions WHERE json_type(workspace, '$.workspace') IS NULL;

-- prompts.payload and messages.payload: Skill Invocations

UPDATE prompts SET payload = json_insert(payload, '$.skill_invocations', json('[]'))
WHERE json_type(payload, '$.skill_invocations') IS NULL;
INSERT INTO report VALUES ('prompts: skill_invocations absent -> []', changes(), 1);

UPDATE messages SET payload = json_insert(payload, '$.skill_invocations', json('[]'))
WHERE json_type(payload, '$.skill_invocations') IS NULL;
INSERT INTO report VALUES ('messages: skill_invocations absent -> []', changes(), 1);

UPDATE prompts
SET payload = json_set(payload, '$.skill_invocations', (
    SELECT json_group_array(CASE
        WHEN json_type(value, '$.marker') IS NULL THEN json(value)
        ELSE json_set(json_remove(value, '$.marker'), '$.span', json(json_extract(value, '$.marker')))
    END)
    FROM json_each(prompts.payload, '$.skill_invocations')))
WHERE EXISTS (SELECT 1 FROM json_each(prompts.payload, '$.skill_invocations')
    WHERE json_type(value, '$.marker') IS NOT NULL);
INSERT INTO report VALUES ('prompts: skill_invocations[].marker -> span', changes(), 1);

UPDATE messages
SET payload = json_set(payload, '$.skill_invocations', (
    SELECT json_group_array(CASE
        WHEN json_type(value, '$.marker') IS NULL THEN json(value)
        ELSE json_set(json_remove(value, '$.marker'), '$.span', json(json_extract(value, '$.marker')))
    END)
    FROM json_each(messages.payload, '$.skill_invocations')))
WHERE EXISTS (SELECT 1 FROM json_each(messages.payload, '$.skill_invocations')
    WHERE json_type(value, '$.marker') IS NOT NULL);
INSERT INTO report VALUES ('messages: skill_invocations[].marker -> span', changes(), 1);

-- turns.payload: timing, usage, and cost recorded after the Turn format began

UPDATE turns
SET payload = json_insert(payload,
    '$.started_at', NULL, '$.settled_at', NULL, '$.last_output_at', NULL,
    '$.usage', NULL, '$.cost', NULL, '$.cost_basis', NULL, '$.cost_details', NULL)
WHERE json_type(payload, '$.started_at') IS NULL
    OR json_type(payload, '$.settled_at') IS NULL
    OR json_type(payload, '$.last_output_at') IS NULL
    OR json_type(payload, '$.usage') IS NULL
    OR json_type(payload, '$.cost') IS NULL
    OR json_type(payload, '$.cost_basis') IS NULL
    OR json_type(payload, '$.cost_details') IS NULL;
INSERT INTO report VALUES ('turns: timing/usage/cost absent -> null', changes(), 1);

-- activities.payload

UPDATE activities
SET payload = json_insert(payload, '$.detail_truncated', json('false'), '$.follow_up_error', NULL)
WHERE json_extract(payload, '$.kind') = 'approval'
    AND (json_type(payload, '$.detail_truncated') IS NULL
        OR json_type(payload, '$.follow_up_error') IS NULL);
INSERT INTO report VALUES ('activities: approval detail_truncated/follow_up_error absent -> false/null', changes(), 1);

UPDATE activities
SET payload = json_insert(payload, '$.model', NULL, '$.brokered', json('false'))
WHERE json_extract(payload, '$.kind') = 'subagent'
    AND (json_type(payload, '$.model') IS NULL OR json_type(payload, '$.brokered') IS NULL);
INSERT INTO report VALUES ('activities: subagent model/brokered absent -> null/false', changes(), 1);

-- Old shapes that never had a fallback: no default would say what they
-- recorded.

INSERT INTO report SELECT 'messages: truncated absent (unreadable, no fallback)', count(*), 0
FROM messages WHERE json_type(payload, '$.truncated') IS NULL;

INSERT INTO report SELECT 'activities: command output_truncated absent (unreadable, no fallback)', count(*), 0
FROM activities WHERE json_extract(payload, '$.kind') = 'command'
    AND json_type(payload, '$.output_truncated') IS NULL;

INSERT INTO report SELECT 'activities: questionnaire combine_freeform absent (unreadable, no fallback)', count(*), 0
FROM activities WHERE json_extract(payload, '$.kind') = 'questionnaire'
    AND EXISTS (SELECT 1 FROM json_each(activities.payload, '$.questionnaire.questions')
        WHERE json_type(value, '$.combine_freeform') IS NULL);

COMMIT;

SELECT printf('%8d  %s', rows, label) FROM report WHERE migrated = 1;
SELECT printf('%8d  %s', rows, label) FROM report WHERE migrated = 0 AND rows > 0;
SELECT printf('pending=%d', total(rows)) FROM report WHERE migrated = 1;
SQL
}

scratch=$(mktemp -d "${TMPDIR:-/tmp}/suru-migrate.XXXXXX")
trap 'rm -rf "$scratch"' EXIT

# Worktree preparation intents are JSON files beside the database. An intent
# without persisted_at predates it and was read as already old, which the
# epoch says as well; one without rename_branch predates that capability.
preparations="$(dirname "$database")/checkout-preparations"
sql_string() {
    printf "'%s'" "$(printf '%s' "$1" | sed "s/'/''/g")"
}
preparation_sql() {
    cat <<SQL
SELECT json_type(intent, '\$.persisted_at') IS NULL,
    json_type(intent, '\$.repository.capabilities') = 'object'
        AND json_type(intent, '\$.repository.capabilities.rename_branch') IS NULL,
    json_insert(intent, '\$.persisted_at', 0,
        '\$.repository.capabilities.rename_branch',
        json('{"status":"unsupported","reason":"This source control operation is not implemented"}'))
FROM (SELECT CAST(readfile($(sql_string "$1")) AS TEXT) AS intent);
SQL
}

# Rewrites each intent still in an old shape unless this is a dry run, and
# reports how many carried each.
migrate_preparations() {
    stamped=0
    capable=0
    [ -d "$preparations" ] || { report_preparations; return; }
    for intent in "$preparations"/*.json; do
        [ -f "$intent" ] || continue
        preparation_sql "$intent" | sqlite3 -separator '|' :memory: >"$scratch/intent" ||
            die "cannot read $intent"
        missing_stamp=$(cut -d'|' -f1 "$scratch/intent")
        missing_rename=$(cut -d'|' -f2 "$scratch/intent")
        [ "$missing_stamp" = 1 ] && stamped=$((stamped + 1))
        [ "$missing_rename" = 1 ] && capable=$((capable + 1))
        if ! $dry_run && { [ "$missing_stamp" = 1 ] || [ "$missing_rename" = 1 ]; }; then
            # Rewritten in place so the intent keeps its owner-only mode.
            cut -d'|' -f3- "$scratch/intent" | tr -d '\n' >"$intent"
        fi
    done
    report_preparations
}
report_preparations() {
    printf '%8d  %s\n' "$stamped" 'preparations: persisted_at absent -> 0 (already old)'
    printf '%8d  %s\n' "$capable" 'preparations: repository.capabilities.rename_branch absent -> unsupported'
}

# Runs the migration against a database, printing the report and leaving the
# count of rows it migrated in $pending.
migrate() {
    migration | sqlite3 "$1" >"$scratch/report" || die "migration failed; $1 is unchanged"
    grep -v '^pending=' "$scratch/report" || true
    pending=$(sed -n 's/^pending=\([0-9]*\).*/\1/p' "$scratch/report")
}

if $dry_run; then
    sqlite3 "$database" ".backup '$scratch/suru.db'"
    printf 'Dry run against a copy of %s:\n' "$database"
    migrate "$scratch/suru.db"
    migrate_preparations
    exit 0
fi

backup="$database.bak-$(date -u +%Y%m%dT%H%M%S)"
sqlite3 "$database" ".backup '$backup'"
printf 'Backed up %s to %s\n' "$database" "$backup"
if [ -d "$preparations" ]; then
    cp -Rp "$preparations" "$preparations.bak-${backup##*.bak-}"
    printf 'Backed up %s to %s\n' "$preparations" "$preparations.bak-${backup##*.bak-}"
fi
migrate "$database"
migrate_preparations

# Idempotence doubles as verification: nothing the migration covers remains.
sqlite3 "$database" ".backup '$scratch/verify.db'"
migration | sqlite3 "$scratch/verify.db" >"$scratch/report"
pending=$(sed -n 's/^pending=\([0-9]*\).*/\1/p' "$scratch/report")
[ "$pending" = 0 ] || die "$pending rows still in an old shape after migrating; restore $backup"
dry_run=true
migrate_preparations >/dev/null
[ $((stamped + capable)) = 0 ] || die "Worktree preparations still in an old shape after migrating"
printf 'Migrated %s; a second pass finds nothing left.\n' "$database"
