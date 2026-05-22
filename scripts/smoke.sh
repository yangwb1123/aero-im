#!/usr/bin/env bash
# Smoke test against a running aero-server. Verifies the P1 happy path:
#  1. register two users
#  2. user A creates a room and invites user B
#  3. user A sends a message via REST? — actually only WS sends messages.
#     For smoke we POST to a hypothetical /api/rooms/:id/messages if added,
#     or just verify history and skip live message send.
#
# Usage: scripts/smoke.sh [host]    default host=http://localhost:3000

set -euo pipefail

HOST="${1:-http://localhost:3000}"
ts=$(date +%s)

say() { printf "\033[1;36m▶ %s\033[0m\n" "$*"; }
ok()  { printf "\033[1;32m✓ %s\033[0m\n" "$*"; }
err() { printf "\033[1;31m✗ %s\033[0m\n" "$*"; exit 1; }

say "Health check"
curl -fsS "$HOST/health" | grep -q ok || err "health endpoint failed"
ok "server is up"

say "Register Alice"
A_RESP=$(curl -fsS -X POST "$HOST/api/auth/register" \
  -H 'content-type: application/json' \
  -d "{\"email\":\"alice+$ts@aero.dev\",\"password\":\"correct horse battery\",\"display_name\":\"Alice\"}")
A_TOK=$(echo "$A_RESP" | python3 -c 'import sys, json; print(json.load(sys.stdin)["access_token"])')
A_PID=$(echo "$A_RESP" | python3 -c 'import sys, json; print(json.load(sys.stdin)["participant"]["id"])')
ok "alice id=$A_PID"

say "Register Bob"
B_RESP=$(curl -fsS -X POST "$HOST/api/auth/register" \
  -H 'content-type: application/json' \
  -d "{\"email\":\"bob+$ts@aero.dev\",\"password\":\"staple horse battery\",\"display_name\":\"Bob\"}")
B_TOK=$(echo "$B_RESP" | python3 -c 'import sys, json; print(json.load(sys.stdin)["access_token"])')
B_PID=$(echo "$B_RESP" | python3 -c 'import sys, json; print(json.load(sys.stdin)["participant"]["id"])')
ok "bob id=$B_PID"

say "Alice: /api/me"
curl -fsS -H "authorization: Bearer $A_TOK" "$HOST/api/me" | grep -q "$A_PID"
ok "me returns alice"

say "Alice creates a group room"
ROOM=$(curl -fsS -X POST "$HOST/api/rooms" \
  -H "authorization: Bearer $A_TOK" \
  -H 'content-type: application/json' \
  -d '{"kind":"group","name":"smoke-test"}' | python3 -c 'import sys,json; print(json.load(sys.stdin)["id"])')
ok "room id=$ROOM"

say "Alice invites Bob"
curl -fsS -o /dev/null -X POST "$HOST/api/rooms/$ROOM/members" \
  -H "authorization: Bearer $A_TOK" \
  -H 'content-type: application/json' \
  -d "{\"participant_id\":\"$B_PID\"}"
ok "bob added"

say "Alice lists rooms"
curl -fsS -H "authorization: Bearer $A_TOK" "$HOST/api/rooms" | grep -q "$ROOM" && ok "room in list"

say "Bob lists rooms"
curl -fsS -H "authorization: Bearer $B_TOK" "$HOST/api/rooms" | grep -q "$ROOM" && ok "room in list"

say "Bob fetches initial history (empty)"
HIST=$(curl -fsS -H "authorization: Bearer $B_TOK" "$HOST/api/rooms/$ROOM/messages")
if [[ "$HIST" == "[]" ]]; then
  ok "empty history"
else
  echo "history was: $HIST"
fi

echo
ok "smoke test passed (REST surface)"
echo "Next: open web/ in two tabs and send messages via WS to validate end-to-end."
