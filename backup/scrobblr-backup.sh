#!/bin/sh
# Backups of Scrobblr's database and uploaded images (docs/operations.md).
#
#   scrobblr-backup schedule      back up daily at BACKUP_TIME (UTC); the default
#   scrobblr-backup run           back up now
#   scrobblr-backup check         exit 1 unless the last backup succeeded, recently
#   scrobblr-backup list          the backups kept
#   scrobblr-backup verify NAME   check every file of a backup reads back whole
#   scrobblr-backup restore NAME [--replace]
#
# A backup is a directory scrobblr-YYYYMMDDTHHMMSSZ in /backups holding
# scrobblr.dump (pg_dump -Fc), uploads.tar, manifest and SHA256SUMS. It is
# written as .partial-NAME and renamed once verified, so a directory with a
# backup's name is complete. Postgres settings come from PG* variables.
# It runs on the image's busybox ash.
# shellcheck shell=busybox

set -eu
set -o pipefail
# Backups hold password hashes and encrypted tokens.
umask 077

ROOT=/backups
UPLOADS=/uploads
NAME_RE='^scrobblr-[0-9]{8}T[0-9]{6}Z$'

log() { echo "$(date -u +%Y-%m-%dT%H:%M:%SZ) backup: $*"; }

die() {
    log "FAILED: $*" >&2
    if [ -n "${FAIL_FILE:-}" ]; then echo "$*" > "$FAIL_FILE"; fi
    exit 1
}

# NAME VALUE DEFAULT MIN: a whole number of at least MIN; blank is DEFAULT.
number() {
    v=${2:-$3}
    case $v in '' | *[!0-9]*) die "$1=$2: expected a whole number" ;; esac
    [ "$v" -ge "$4" ] || die "$1=$2: expected at least $4"
    echo "$v"
}

config() {
    BACKUP_TIME=${BACKUP_TIME:-03:30}
    case $BACKUP_TIME in
        [01][0-9]:[0-5][0-9] | 2[0-3]:[0-5][0-9]) ;;
        *) die "BACKUP_TIME=$BACKUP_TIME: expected HH:MM (UTC)" ;;
    esac
    KEEP_DAILY=$(number BACKUP_KEEP_DAILY "${BACKUP_KEEP_DAILY:-}" 7 1)
    KEEP_WEEKLY=$(number BACKUP_KEEP_WEEKLY "${BACKUP_KEEP_WEEKLY:-}" 4 0)
    MAX_AGE_HOURS=$(number BACKUP_MAX_AGE_HOURS "${BACKUP_MAX_AGE_HOURS:-}" 26 1)
    REMOTE=${BACKUP_REMOTE:-}
    REMOTE=${REMOTE%/}
    PING_URL=${BACKUP_PING_URL:-}
    [ -d "$ROOT" ] || die "$ROOT is missing: mount the backup directory there"
}

# Backup names, newest first.
backups() {
    # shellcheck disable=SC2010 # names are ours, and checked
    ls -1 "$ROOT" | grep -E "$NAME_RE" | sort -r || true
}

# Reads backup names, newest first; prints those to keep: the newest of each
# of the KEEP_DAILY latest days and of the KEEP_WEEKLY latest ISO weeks.
keep() {
    days=0 weeks=0 last_day='' last_week=''
    while read -r candidate; do
        d=${candidate#scrobblr-}
        d=${d%%T*}
        week=$(date -u -d "${d:0:4}-${d:4:2}-${d:6:2} 12:00" +%G-W%V)
        kept=
        if [ "$d" != "$last_day" ] && [ "$days" -lt "$KEEP_DAILY" ]; then
            days=$((days + 1)) last_day=$d kept=1
        fi
        if [ "$week" != "$last_week" ] && [ "$weeks" -lt "$KEEP_WEEKLY" ]; then
            weeks=$((weeks + 1)) last_week=$week kept=1
        fi
        if [ -n "$kept" ]; then echo "$candidate"; fi
    done
}

# Variables are global: these loops must not reuse `name`.
prune_local() {
    kept=$(backups | keep)
    for old in $(backups); do
        if ! echo "$kept" | grep -qx "$old"; then
            log "deleting $old (retention)"
            rm -rf "${ROOT:?}/$old"
        fi
    done
}

# Without a config file, rclone says so on every call; remotes may then be
# given whole in BACKUP_REMOTE (`:sftp,host=…:path`).
rclone() {
    if [ -f "${RCLONE_CONFIG:-}" ]; then
        command rclone "$@"
    else
        command rclone --config "" "$@"
    fi
}

# Checks the files of the backup in DIR: their checksums, the dump's table
# of contents and every byte of its data, and the upload count.
verify_dir() {
    dir=$1
    (cd "$dir" && sha256sum -c -s SHA256SUMS) || die "$dir: checksums don't match"
    toc=$(pg_restore --list "$dir/scrobblr.dump") || die "$dir: the dump's table of contents can't be read"
    echo "$toc" | grep -q "TABLE DATA public users " || die "$dir: the dump has no users table"
    pg_restore -f /dev/null "$dir/scrobblr.dump" || die "$dir: the dump can't be read in full"
    expected=$(sed -n 's/^uploads=//p' "$dir/manifest")
    entries=$(tar -tf "$dir/uploads.tar") || die "$dir: uploads.tar can't be read"
    found=$(echo "$entries" | grep -vc -e '/$' -e '^$' || true)
    [ "$found" = "$expected" ] || die "$dir: uploads.tar holds $found files, the manifest says $expected"
}

backup() {
    exec 9> "$ROOT/.lock"
    flock -n 9 || die "another backup is running"
    name=scrobblr-$(date -u +%Y%m%dT%H%M%SZ)
    work=$ROOT/.partial-$name
    rm -rf "$ROOT"/.partial-*
    trap 'rm -rf "$work"' EXIT
    mkdir "$work"

    log "dumping database ${PGDATABASE:?}"
    # Waits at most a minute for its share locks rather than queue behind
    # a migration holding an exclusive one.
    started=$(date -u +%s)
    pg_dump -Fc --lock-wait-timeout=60000 -f "$work/scrobblr.dump" || die "pg_dump failed"
    [ -s "$work/scrobblr.dump" ] || die "pg_dump wrote an empty file"
    log "dumped in $(($(date -u +%s) - started)) s"

    # After the dump: an image is stored before a row refers to it, so the
    # archive has every file the dump names.
    log "archiving uploads"
    (cd "$UPLOADS" && find . -path ./.tmp -prune -o -type f -print | sed 's|^\./||' | sort) \
        > "$work/uploads.list" || die "can't list $UPLOADS"
    uploads=$(wc -l < "$work/uploads.list")
    if [ "$uploads" -eq 0 ]; then
        # An empty archive, which busybox tar refuses to write.
        head -c 1024 /dev/zero > "$work/uploads.tar"
    else
        tar -cf "$work/uploads.tar" -C "$UPLOADS" -T "$work/uploads.list" || die "tar failed"
    fi
    rm "$work/uploads.list"

    {
        echo "name=$name"
        echo "database=$PGDATABASE"
        echo "server_version=$(psql -XAtc 'SHOW server_version')"
        echo "timescaledb_version=$(psql -XAtc "SELECT extversion FROM pg_extension WHERE extname = 'timescaledb'")"
        echo "schema_version=$(psql -XAtc 'SELECT max(version) FROM schema_migrations')"
        echo "uploads=$uploads"
    } > "$work/manifest" || die "can't write the manifest"
    (cd "$work" && sha256sum scrobblr.dump uploads.tar manifest > SHA256SUMS) || die "sha256sum failed"

    log "verifying"
    verify_dir "$work"
    mv "$work" "$ROOT/$name"
    trap - EXIT
    log "$name: database $(du -h "$ROOT/$name/scrobblr.dump" | cut -f1), $uploads uploads ($(du -h "$ROOT/$name/uploads.tar" | cut -f1))"
    prune_local

    if [ -n "$REMOTE" ]; then
        log "copying $name to the remote"
        rclone copy --checksum "$ROOT/$name" "$REMOTE/$name" || die "copying to the remote failed"
        rclone check --one-way "$ROOT/$name" "$REMOTE/$name" || die "the remote copy doesn't match"
        remote=$(rclone lsf --dirs-only "$REMOTE" | sed 's|/$||' | grep -E "$NAME_RE" | sort -r) \
            || die "listing the remote failed"
        kept=$(echo "$remote" | keep)
        for old in $remote; do
            if ! echo "$kept" | grep -qx "$old"; then
                log "deleting $old from the remote (retention)"
                rclone purge "$REMOTE/$old" || die "deleting $old from the remote failed"
            fi
        done
    fi
    echo "$name" > "$FAIL_FILE.name"
}

ping() {
    if [ -n "$PING_URL" ]; then
        wget -q -T 10 -O /dev/null "$1" || log "warning: pinging the monitor failed"
    fi
}

# One backup in a process of its own, so `set -e` holds and a failure
# anywhere ends it; records the outcome for `check`.
run() {
    config
    fail=$ROOT/.failure.tmp
    rm -f "$fail" "$fail.name"
    if FAIL_FILE=$fail "$0" _backup; then
        echo "$(date -u +%s) $(cat "$fail.name")" > "$ROOT/.last-success"
        rm -f "$ROOT/.last-failure" "$fail.name"
        log "done"
        ping "$PING_URL"
    else
        reason=$(cat "$fail" 2> /dev/null || echo "exited without saying why")
        echo "$(date -u +%s) $reason" > "$ROOT/.last-failure"
        rm -f "$fail" "$fail.name"
        ping "$PING_URL/fail"
        return 1
    fi
}

# Epoch of the last success or failure, 0 for none.
last() {
    if [ -f "$ROOT/.last-$1" ]; then cut -d' ' -f1 "$ROOT/.last-$1"; else echo 0; fi
}

check() {
    config
    if [ -f "$ROOT/.last-failure" ]; then
        echo "last backup failed: $(cut -d' ' -f2- "$ROOT/.last-failure")"
        exit 1
    fi
    success=$(last success)
    if [ "$success" -eq 0 ]; then
        echo "no backup yet"
        exit 1
    fi
    age=$(($(date -u +%s) - success))
    if [ "$age" -gt $((MAX_AGE_HOURS * 3600)) ]; then
        echo "last backup is $((age / 3600)) h old (more than BACKUP_MAX_AGE_HOURS=$MAX_AGE_HOURS)"
        exit 1
    fi
    echo "last backup $(cut -d' ' -f2 "$ROOT/.last-success"), $((age / 60)) min ago"
}

# The latest BACKUP_TIME at or before now, in epoch seconds.
last_slot() {
    slot=$(date -u -d "$(date -u +%Y-%m-%d) $BACKUP_TIME" +%s)
    if [ "$slot" -gt "$1" ]; then slot=$((slot - 86400)); fi
    echo "$slot"
}

# Backs up once a day at BACKUP_TIME, and at once when the latest slot was
# missed (the service was down); a failed backup is retried hourly.
schedule() {
    config
    trap 'log "stopping"; exit 0' TERM INT
    log "daily at $BACKUP_TIME UTC, keeping $KEEP_DAILY daily and $KEEP_WEEKLY weekly backups${REMOTE:+, copied to the remote}"
    while :; do
        now=$(date -u +%s)
        slot=$(last_slot "$now")
        failure=$(last failure)
        if [ "$(last success)" -lt "$slot" ] && [ $((now - failure)) -ge 3600 ]; then
            run || true
            continue
        fi
        wake=$((slot + 86400))
        if [ "$(last success)" -lt "$slot" ] && [ $((failure + 3600)) -lt "$wake" ]; then
            wake=$((failure + 3600))
        fi
        sleep $((wake - now > 0 ? wake - now : 1)) &
        wait $! || true
    done
}

list() {
    config
    for name in $(backups); do
        echo "$name  $(du -sh "$ROOT/$name" | cut -f1)  $(sed -n 's/^uploads=//p' "$ROOT/$name/manifest" 2> /dev/null || echo ?) uploads"
    done
}

# psql on the maintenance database, with the target's name as :db.
admin_sql() {
    psql -X -v ON_ERROR_STOP=1 -v db="$PGDATABASE" -d postgres -At
}

restore() {
    [ $# -ge 1 ] || die "usage: restore NAME [--replace]"
    name=$1
    replace=
    if [ "${2:-}" = --replace ]; then replace=1; fi
    dir=$ROOT/$name
    echo "$name" | grep -qE "$NAME_RE" && [ -d "$dir" ] || die "no backup $name (scrobblr-backup list)"
    [ -d "$UPLOADS" ] && [ -w "$UPLOADS" ] || die "$UPLOADS must be mounted writable"

    log "verifying $name"
    verify_dir "$dir"
    version=$(sed -n 's/^timescaledb_version=//p' "$dir/manifest")
    offered=$(echo "SELECT version FROM pg_available_extension_versions WHERE name = 'timescaledb' AND version = '$version'" | admin_sql)
    [ -n "$offered" ] || die "the backup needs TimescaleDB $version, which this database image doesn't have: restore with the image the backup was made with"

    clients=$(echo "SELECT count(*) FROM pg_stat_activity WHERE datname = :'db' AND backend_type = 'client backend'" | admin_sql)
    [ "$clients" -eq 0 ] || die "$clients connections to $PGDATABASE: stop the api and worker first (docker compose stop api worker backup)"
    exists=$(echo "SELECT count(*) FROM pg_database WHERE datname = :'db'" | admin_sql)
    if [ "$exists" -eq 1 ]; then
        used=$(psql -X -At -c "SELECT to_regclass('public.users') IS NOT NULL")
        if [ "$used" = t ] && [ -z "$replace" ]; then
            die "$PGDATABASE already has data: add --replace to drop it and its uploads"
        fi
        log "dropping database $PGDATABASE"
        echo 'DROP DATABASE :"db" WITH (FORCE)' | admin_sql > /dev/null || die "dropping $PGDATABASE failed"
    fi
    echo 'CREATE DATABASE :"db" TEMPLATE template0' | admin_sql > /dev/null || die "creating $PGDATABASE failed"
    psql -X -v ON_ERROR_STOP=1 -q -c "CREATE EXTENSION timescaledb VERSION '$version'" \
        -c "SELECT timescaledb_pre_restore()" > /dev/null || die "preparing $PGDATABASE failed"

    log "restoring the database"
    if ! pg_restore --no-owner --exit-on-error -d "$PGDATABASE" "$dir/scrobblr.dump"; then
        psql -X -q -c "SELECT timescaledb_post_restore()" > /dev/null || true
        die "pg_restore failed; $PGDATABASE is incomplete: fix the cause and restore again with --replace"
    fi
    psql -X -v ON_ERROR_STOP=1 -q -c "SELECT timescaledb_post_restore()" -c "ANALYZE" > /dev/null \
        || die "finishing the restore failed"

    log "restoring uploads"
    if [ -n "$replace" ]; then
        find "$UPLOADS" -mindepth 1 -delete || die "emptying $UPLOADS failed"
    fi
    tar -xf "$dir/uploads.tar" -C "$UPLOADS" || die "unpacking uploads failed"
    chown -R 10001:10001 "$UPLOADS" || die "giving the uploads to the API's user failed"
    log "restored $name; start the stack: docker compose up -d"
}

case "${1:-schedule}" in
    schedule) schedule ;;
    run) run ;;
    _backup) config && backup ;;
    _keep) config && keep ;;
    check) check ;;
    list) list ;;
    verify)
        config
        [ $# -ge 2 ] && [ -d "$ROOT/$2" ] || die "usage: verify NAME (scrobblr-backup list)"
        verify_dir "$ROOT/$2"
        log "$2 is complete and readable"
        ;;
    restore)
        shift
        config
        restore "$@"
        ;;
    *)
        sed -n '2,9s/^# \{0,1\}//p' "$0"
        exit 2
        ;;
esac
