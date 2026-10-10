#!/usr/bin/env bash
# Backup and restore, end to end, in a throwaway Compose project
# (scrobblr-backuptest, its own volumes and port): seeds users, scrobbles,
# labels, weights and uploads, backs up, checks that failures are loud,
# restores onto empty volumes and over a used database, and compares every
# public table and every upload with the originals. Passwords hold
# / @ # ? $ on purpose. Tears everything down at the end, but builds and
# keeps the scrobblr-backend and scrobblr-backup images, as `up --build`
# does.
#
#   backup/test.sh            (from the repository root; needs Docker, curl, python3)

set -euo pipefail
cd "$(dirname "$0")/.."

project=scrobblr-backuptest
work=$(mktemp -d)
port=$((20000 + RANDOM % 20000))
token=metrics-$(head -c 12 /dev/urandom | od -An -tx1 | tr -d ' \n')
env=$work/env
cat > "$env" << EOF
POSTGRES_USER=scrobblr
POSTGRES_PASSWORD='p@ss/w#rd?\$x'
POSTGRES_DB=scrobblr
REDIS_PASSWORD='r3d/s@#?\$y'
API_PORT=$port
PUBLIC_BASE_URL=http://127.0.0.1:$port
BACKUP_DIR=$work/backups
BACKUP_RCLONE_DIR=$work/rclone
BACKUP_REMOTE=/config/rclone/remote
BACKUP_KEEP_DAILY=3
BACKUP_KEEP_WEEKLY=2
METRICS_TOKEN=$token
EOF

dc() { docker compose -p "$project" --env-file "$env" "$@"; }
api() { curl -fsS "http://127.0.0.1:$port$1" "${@:2}"; }
step() { echo; echo "=== $*"; }
fail() { echo "FAIL: $*" >&2; exit 1; }

cleanup() {
    status=$?
    if [ "$status" -ne 0 ]; then dc logs --tail 30 backup restore api worker 2> /dev/null || true; fi
    dc --profile restore down -v --remove-orphans > /dev/null 2>&1 || true
    # The containers wrote these as root.
    docker run --rm -v "$work:/w" --entrypoint rm scrobblr-backup:latest -rf /w/backups /w/rclone \
        > /dev/null 2>&1 || true
    rm -rf "$work"
    exit "$status"
}
trap cleanup EXIT

# Every public table and view, with a row count and an md5 of its rows.
fingerprint() {
    dc exec -T db psql -XAt -U scrobblr -d scrobblr -v ON_ERROR_STOP=1 << 'SQL'
SELECT format(
    'SELECT %L, count(*), md5(coalesce(string_agg(x::text, %L ORDER BY x::text), %L)) FROM public.%I x',
    c.relname, '|', '', c.relname)
FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace
WHERE n.nspname = 'public' AND c.relkind IN ('r', 'v', 'm', 'p')
ORDER BY c.relname
\gexec
SQL
    dc run --rm --no-deps -T --entrypoint sh backup -c \
        'cd /uploads && find . -path ./.tmp -prune -o -type f -exec stat -c "%u:%g %a %n" {} + | sort && find . -path ./.tmp -prune -o -type f -exec sha256sum {} + | sort'
}

count_rows() {
    dc exec -T db psql -XAt -U scrobblr -d scrobblr -c "SELECT count(*) FROM $1"
}

step "build every image, start db, redis, migrate, api"
dc build
dc up -d --wait api

step "seed: a synthetic population, its labels and weights"
dc exec -T db psql -XAq -U scrobblr -d scrobblr -v users=30 -v days=21 -v artists=300 \
    < scripts/synthetic/population.sql > /dev/null
dc run --rm --no-deps -T worker worker classify reclassify > /dev/null
dc run --rm --no-deps -T worker worker rank recompute > /dev/null

step "seed: uploads through the API"
python3 - "$work/image.png" << 'PY'
import struct, sys, zlib
w, h = 64, 48
rows = b"".join(b"\0" + b"".join(bytes((x * 4 % 256, y * 5 % 256, 120)) for x in range(w)) for y in range(h))
chunk = lambda t, d: struct.pack(">I", len(d)) + t + d + struct.pack(">I", zlib.crc32(t + d) & 0xFFFFFFFF)
png = b"\x89PNG\r\n\x1a\n" + chunk(b"IHDR", struct.pack(">IIBBBBB", w, h, 8, 2, 0, 0, 0)) + chunk(b"IDAT", zlib.compress(rows)) + chunk(b"IEND", b"")
open(sys.argv[1], "wb").write(png)
PY
session=$(api /v1/auth/register -H 'Content-Type: application/json' \
    -d '{"username":"backupper","email":"backupper@example.com","password":"Backup-Test-Passw0rd!"}' |
    python3 -c 'import json,sys; print(json.load(sys.stdin)["token"])')
api /v1/user/me/avatar -H "Authorization: Bearer $session" -F "image=@$work/image.png" > /dev/null
artist=$(dc exec -T db psql -XAt -U scrobblr -d scrobblr -c "SELECT min(id) FROM artists")
api "/v1/artist/$artist/image" -H "Authorization: Bearer $session" -F "image=@$work/image.png" > /dev/null
for table in users scrobbles scrobble_classification_days scrobble_flags ranking_daily image_candidates; do
    n=$(count_rows "$table")
    echo "$table: $n rows"
    if [ "$table" != scrobble_flags ] && [ "$n" -eq 0 ]; then fail "no $table seeded"; fi
done

step "back up while the API and worker run"
dc up -d --wait worker
dc run --rm backup run
api /health | grep -qx ok || fail "/health"

step "back up a quiet stack, and fingerprint it"
dc stop api worker
before=$(fingerprint)
dc run --rm backup run
name=$(dc run --rm --no-deps -T backup list | head -1 | cut -d' ' -f1)
echo "latest backup: $name"
dc run --rm --no-deps -T backup check
inside() { dc run --rm --no-deps -T --entrypoint sh backup -c "$1"; }
inside "cmp /backups/$name/SHA256SUMS /config/rclone/remote/$name/SHA256SUMS" || fail "no remote copy of $name"
inside "stat -c '%a %n' /backups/$name/*" | grep -v '^600 ' && fail "backup files readable by others"

step "failures are loud: wrong password"
if dc run --rm -e PGPASSWORD=wrong backup run; then fail "a backup with a wrong password succeeded"; fi
if dc run --rm --no-deps -T backup check; then fail "check passed after a failed backup"; fi
inside '! ls -d /backups/.partial-* 2> /dev/null' || fail "a partial backup was left behind"
# Retention keeps a day's newest backup only.
[ "$(dc run --rm --no-deps -T backup list | cut -d' ' -f1)" = "$name" ] || fail "the failed backup was counted"

step "failures are loud: a truncated dump"
dc run --rm --no-deps -T --entrypoint sh backup -c \
    "cp -r /backups/$name /backups/scrobblr-20000101T000000Z && \
     head -c 4096 /backups/$name/scrobblr.dump > /backups/scrobblr-20000101T000000Z/scrobblr.dump"
if dc run --rm --no-deps -T backup verify scrobblr-20000101T000000Z; then fail "a truncated dump verified"; fi
dc run --rm --no-deps -T --entrypoint sh backup -c \
    "head -c 4096 /backups/$name/scrobblr.dump > /tmp/d && cd /backups/scrobblr-20000101T000000Z && \
     cp /tmp/d scrobblr.dump && sha256sum scrobblr.dump uploads.tar manifest > SHA256SUMS && \
     ! pg_restore -f /dev/null scrobblr.dump" || fail "pg_restore read a truncated dump"
if dc run --rm --no-deps -T backup verify scrobblr-20000101T000000Z; then
    fail "a truncated dump with matching checksums verified"
fi
dc run --rm --no-deps -T --entrypoint rm backup -rf /backups/scrobblr-20000101T000000Z

step "retention: the newest of the latest days and ISO weeks that have backups"
kept=$(printf '%s\n' scrobblr-20261012T033000Z scrobblr-20261011T120000Z scrobblr-20261011T033000Z \
    scrobblr-20261010T033000Z scrobblr-20261008T033000Z scrobblr-20261004T033000Z \
    scrobblr-20260927T033000Z | dc run --rm --no-deps -T -e BACKUP_KEEP_WEEKLY=3 backup _keep | tr '\n' ' ')
[ "$kept" = "scrobblr-20261012T033000Z scrobblr-20261011T120000Z scrobblr-20261010T033000Z scrobblr-20261004T033000Z " ] \
    || fail "kept $kept"
# shellcheck disable=SC2016 # expanded in the container
dc run --rm --no-deps -T --entrypoint sh backup -c \
    'for d in 20200101 20200102 20200103 20200110 20200117; do mkdir /backups/scrobblr-${d}T033000Z; done'
dc run --rm backup run
remaining=$(dc run --rm --no-deps -T backup list | cut -d' ' -f1 | tr '\n' ' ')
echo "kept: $remaining"
name=${remaining%% *}
# Today's newest, then the two latest days with backups; the same in the
# remote copy.
[ "$remaining" = "$name scrobblr-20200117T033000Z scrobblr-20200110T033000Z " ] || fail "retention kept $remaining"
remote=$(inside 'ls /config/rclone/remote')
[ "$remote" = "$name" ] || fail "remote retention kept $remote"
# The data hasn't changed since the fingerprint: backups only read it.

step "restore onto empty volumes"
dc down -v
dc up -d --wait db redis
dc run --rm restore "$name"
after=$(fingerprint)
if [ "$before" != "$after" ]; then
    diff <(echo "$before") <(echo "$after") || true
    fail "the restored data differs"
fi
echo "identical: $(echo "$before" | grep -c '|') tables and views, $(echo "$before" | grep -c '^[0-9a-f]\{64\}') uploads"

step "the restored stack runs"
dc up -d --wait api worker
api /health | grep -qx ok || fail "/health after restore"
for _ in $(seq 30); do
    if api /health/worker > /dev/null 2>&1; then break; fi
    sleep 2
done
api /health/worker | grep -qx ok || fail "/health/worker after restore"
dc exec -T worker worker status | tail -1
api /metrics -H "Authorization: Bearer $token" | grep -E '^scrobblr_(up|worker_healthy)'
api "/uploads/$(dc exec -T db psql -XAt -U scrobblr -d scrobblr -c "SELECT image_url FROM users WHERE username = 'backupper'")" \
    -o /dev/null || fail "the restored avatar isn't served"

step "restore refuses while the API is connected, and onto data without --replace"
if dc run --rm restore "$name" --replace; then fail "restored under a running API"; fi
dc stop api worker
if dc run --rm restore "$name"; then fail "restored over data without --replace"; fi

step "restore --replace over a used database"
dc exec -T db psql -XAq -U scrobblr -d scrobblr -c "DELETE FROM scrobble_flags; UPDATE users SET bio = 'changed'"
dc run --rm restore "$name" --replace
after=$(fingerprint)
[ "$before" = "$after" ] || fail "the data restored with --replace differs"

step "the scheduled service catches up at startup and stops promptly"
inside 'rm /backups/.last-success'
dc up -d backup
for _ in $(seq 30); do
    if dc exec -T backup scrobblr-backup check > /dev/null 2>&1; then break; fi
    sleep 2
done
dc exec -T backup scrobblr-backup check || fail "the scheduled service didn't catch up"
started=$(date +%s)
dc stop backup
[ $(($(date +%s) - started)) -lt 8 ] || fail "the scheduled service took too long to stop"

echo
echo "PASS"
