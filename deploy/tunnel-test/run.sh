#!/usr/bin/env bash
# shellcheck disable=SC2015 # `pass` never fails
# Tunnel mode end to end, with a stand-in for Cloudflare's edge in place of
# cloudflared: which client the API counts, the web app's view of it, and
# the live stream. Builds the backend and starts a project of its own,
# which it removes afterwards, volumes and backend image included.
#
#   deploy/tunnel-test/run.sh
set -uo pipefail
cd "$(dirname "$0")/../.." || exit 1

export COMPOSE_PROJECT_NAME=scrobblr-tunnel-test
export COMPOSE_FILE=docker-compose.yml:docker-compose.tunnel.yml:deploy/tunnel-test/compose.yml
export COMPOSE_PROFILES=caddy
export TUNNEL_SUBNET_PREFIX=10.213.8
export POSTGRES_PASSWORD=tunnel-test REDIS_PASSWORD=tunnel-test
export WEB_DOMAIN=scrobblr.app API_DOMAIN=api.scrobblr.app UPLOADS_DOMAIN=cdn.scrobblr.app
image=scrobblr-backend:tunnel-test
caddy=$TUNNEL_SUBNET_PREFIX.3

dc() { docker compose --env-file /dev/null "$@"; }
cleanup() {
  dc --profile web down -v --remove-orphans >/dev/null 2>&1
  docker image rm "$image" >/dev/null 2>&1
}
trap cleanup EXIT

failures=0
pass() { echo "PASS $*"; }
fail() { echo "FAIL $*"; failures=$((failures + 1)); }

# A curl from a container at address IP on NETWORK.
client() {
  local ip=$1 net=$2
  shift 2
  docker run --rm --network "${COMPOSE_PROJECT_NAME}_$net" --ip "$ip" "$image" curl -s "$@"
}
# A visitor of HOST at address IP, through the edge.
visit() {
  local ip=$1 host=$2 path=$3
  shift 3
  client "$ip" internet -H "Host: $host" "$@" "http://198.18.0.2$path"
}
redis() { dc exec -T redis redis-cli "$@"; }
counted() { redis --scan --pattern 'rl:*' | sed 's/:[0-9]*$//; s/^rl://' | sort -u | tr '\n' ' '; }
forget() { dc exec -T redis sh -c "redis-cli --scan --pattern 'rl:*' | xargs -r redis-cli del" >/dev/null; }
expect_counted() {
  local got
  got=$(counted)
  [ "$got" = "$2 " ] && pass "$1: counted as $2" || fail "$1: counted as [$got], not $2"
  forget
}

echo "Building and starting..."
dc up -d --build >/dev/null 2>&1 || { dc up -d --build; exit 1; }
for _ in $(seq 90); do
  [ "$(docker inspect -f '{{.State.Health.Status}}' "$COMPOSE_PROJECT_NAME-api-1" 2>/dev/null)" = healthy ] && break
  sleep 2
done

published=$(docker ps --filter "label=com.docker.compose.project=$COMPOSE_PROJECT_NAME" --format '{{.Names}} {{.Ports}}' | grep -- '->')
[ -z "$published" ] && pass "nothing published on the host" || fail "published: $published"

answer=$(visit 198.18.0.20 "$WEB_DOMAIN" / -w ' %{http_code}')
[[ $answer == *"isn't running"*503 ]] && pass "no web app: the web host answers 503" || fail "no web app: $answer"
[ "$(visit 198.18.0.20 "$API_DOMAIN" /docs -o /dev/null -w '%{http_code}')" = 200 ] \
  && pass "the API answers without the web app" || fail "the API doesn't answer"

forget
visit 198.18.0.10 "$API_DOMAIN" /v1/user/nobody -o /dev/null
expect_counted "visitor" 198.18.0.10
visit 198.18.0.11 "$API_DOMAIN" /v1/user/nobody -o /dev/null \
  -H 'CF-Connecting-IP: 6.6.6.6' -H 'X-Forwarded-For: 6.6.6.6, 7.7.7.7' -H 'X-Real-IP: 8.8.8.8'
expect_counted "visitor forging CF-Connecting-IP and X-Forwarded-For" 198.18.0.11
client "$TUNNEL_SUBNET_PREFIX.5" tunnel -o /dev/null -H "Host: $API_DOMAIN" \
  -H 'CF-Connecting-IP: 6.6.6.6' -H 'X-Forwarded-For: 6.6.6.6' "http://$caddy/v1/user/nobody"
expect_counted "another container on the tunnel network, forging" "$TUNNEL_SUBNET_PREFIX.5"
caddy_default=$(docker inspect -f "{{(index .NetworkSettings.Networks \"${COMPOSE_PROJECT_NAME}_default\").IPAddress}}" "$COMPOSE_PROJECT_NAME-caddy-1")
other=$(docker run --rm --network "${COMPOSE_PROJECT_NAME}_default" "$image" sh -c \
  "curl -s -o /dev/null -H 'Host: $API_DOMAIN' -H 'CF-Connecting-IP: 6.6.6.6' http://$caddy_default/v1/user/nobody; hostname -i")
expect_counted "a container on the default network going to Caddy, forging" "$other"

# 60 requests a minute: start early in a window, so the burst stays in one.
while [ "$(date +%-S)" -gt 40 ]; do sleep 1; done
forget
codes=$(docker run --rm --network "${COMPOSE_PROJECT_NAME}_internet" --ip 198.18.0.10 "$image" sh -c \
  "for i in \$(seq 61); do curl -s -o /dev/null -w '%{http_code}\n' -H 'Host: $API_DOMAIN' http://198.18.0.2/v1/user/nobody; done" \
  | sort | uniq -c | tr -s ' ' | tr '\n' ',')
[ "$codes" = " 60 404, 1 429," ] && pass "visitor A: 60 answers, then 429" || fail "visitor A: $codes"
code=$(visit 198.18.0.10 "$API_DOMAIN" /v1/user/nobody -o /dev/null -w '%{http_code}' \
  -H 'CF-Connecting-IP: 6.6.6.6' -H 'X-Forwarded-For: 6.6.6.6')
[ "$code" = 429 ] && pass "visitor A forging headers: still 429" || fail "visitor A forging headers: $code"
code=$(visit 198.18.0.12 "$API_DOMAIN" /v1/user/nobody -o /dev/null -w '%{http_code}')
[ "$code" = 404 ] && pass "visitor B meanwhile: answered" || fail "visitor B meanwhile: $code"
forget

COMPOSE_PROFILES=caddy,web dc up -d web >/dev/null 2>&1
sleep 2
seen=$(visit 198.18.0.13 "$WEB_DOMAIN" / -H 'X-Forwarded-For: 1.1.1.1, 2.2.2.2' -H 'CF-Connecting-IP: 3.3.3.3')
[[ $seen == "xff=[198.18.0.13] "* ]] && pass "the web app gets one address: $seen" || fail "the web app gets: $seen"

token=$(visit 198.18.0.20 "$API_DOMAIN" /v1/auth/register -X POST -H 'content-type: application/json' \
  -d '{"username":"tunneltest","email":"tunneltest@example.com","password":"Correct-Horse-9-Battery"}' \
  | sed -n 's/.*"token":"\([^"]*\)".*/\1/p')
stream() { # IP PORT SECONDS: each line of the stream, after the time it arrived
  client "$1" internet -N --max-time "$3" -H "Host: $API_DOMAIN" "http://198.18.0.2:$2/v1/user/tunneltest/live" \
    | while IFS= read -r line; do [ -n "$line" ] && echo "$(date +%s.%N) $line"; done
  echo "$(date +%s.%N) end"
}
dir=$(mktemp -d)
stream 198.18.0.30 81 45 > "$dir/kept" &
stream 198.18.0.31 82 45 > "$dir/control" &
sleep 5
posted=$(date +%s.%N)
visit 198.18.0.20 "$API_DOMAIN" /v1/now-playing -o /dev/null -X POST -H "Authorization: Bearer $token" \
  -H 'content-type: application/json' -d '{"track":"Teardrop","artist":"Massive Attack","duration_ms":330000}'
wait
calc() { awk "BEGIN { print ($*) }"; }
time_of() { grep -m1 "$1" "$2" | cut -d' ' -f1; }
lasted() { calc "$(tail -1 "$1" | cut -d' ' -f1) - $(head -1 "$1" | cut -d' ' -f1)"; }
first=$(time_of Teardrop "$dir/kept")
delay=$(calc "${first:-1e9} - $posted")
[ "$(calc "$delay < 2")" = 1 ] && pass "now playing arrived $(calc "int($delay * 1000)") ms after it was posted" \
  || fail "now playing didn't arrive promptly"
lasted=$(lasted "$dir/kept")
[ "$(calc "$lasted > 40")" = 1 ] \
  && pass "behind a 20 s idle limit, the stream lasted $(calc "int($lasted)") s ($(grep -c ' :$' "$dir/kept") keepalives)" \
  || fail "behind a 20 s idle limit, the stream lasted $(calc "int($lasted)") s"
lasted=$(lasted "$dir/control")
[ "$(calc "$lasted < 30")" = 1 ] && pass "control: a 10 s idle limit closed the stream after $(calc "int($lasted)") s" \
  || fail "control: a 10 s idle limit didn't close the stream"
rm -r "$dir"

echo
[ "$failures" = 0 ] && echo "All passed." || { echo "$failures failed."; exit 1; }
