#!/usr/bin/env bash
# Test a claude-gateway token end to end.
#   ./test-api.sh <token> [base-url]
#   ./test-api.sh sk-gw-... https://proxyx.online/claude/api
#   ./test-api.sh sk-gw-... http://127.0.0.1:8443
set -u
TOKEN="${1:?usage: $0 <token> [base-url]}"
BASE="${2:-https://proxyx.online/claude/api}"
BASE="${BASE%/}"
MODEL="${MODEL:-claude-haiku-5-5}"
pass=0; fail=0
ok()  { echo "  ✓ $*"; pass=$((pass + 1)); }
bad() { echo "  ✗ $*"; fail=$((fail + 1)); }

echo "Testing $BASE  (model $MODEL)"

echo "1. Health (no token)"
code=$(curl -s -o /dev/null -w '%{http_code}' -m 15 "$BASE/health")
[ "$code" = 200 ] && ok "GET /health -> 200" || bad "GET /health -> $code (is the route/proxy set up?)"

echo "2. Requests without a token are rejected"
code=$(curl -s -o /dev/null -w '%{http_code}' -m 15 "$BASE/v1/models")
[ "$code" = 401 ] && ok "no token -> 401" || bad "no token -> $code (expected 401)"

echo "3. Token is accepted"
code=$(curl -s -o /dev/null -w '%{http_code}' -m 15 "$BASE/v1/models" -H "x-api-key: $TOKEN")
[ "$code" = 200 ] && ok "GET /v1/models with token -> 200" || bad "GET /v1/models with token -> $code"

echo "4. Send a message"
resp=$(curl -s -m 180 "$BASE/v1/messages" -H "x-api-key: $TOKEN" -H "content-type: application/json" \
  -d "{\"model\":\"$MODEL\",\"max_tokens\":100,\"messages\":[{\"role\":\"user\",\"content\":\"Reply with exactly: gateway ok\"}]}")
if out=$(printf '%s' "$resp" | python3 -c '
import json, sys
d = json.load(sys.stdin)
if d.get("type") == "error":
    sys.exit(d["error"]["type"] + ": " + d["error"]["message"])
u = d["usage"]
total_in = u.get("input_tokens", 0) + u.get("cache_creation_input_tokens", 0) + u.get("cache_read_input_tokens", 0)
text, out = d["content"][0]["text"], u["output_tokens"]
print(f"reply: {text!r}  |  tokens in: {total_in}, out: {out}")
' 2>&1); then ok "$out"; else bad "POST /v1/messages failed: $out ${resp:0:200}"; fi

echo "5. Streaming"
events=$(curl -sN -m 180 "$BASE/v1/messages" -H "Authorization: Bearer $TOKEN" -H "content-type: application/json" \
  -d "{\"model\":\"$MODEL\",\"max_tokens\":50,\"stream\":true,\"messages\":[{\"role\":\"user\",\"content\":\"Say hi\"}]}" \
  | grep -c '^event: content_block_delta')
[ "${events:-0}" -gt 0 ] && ok "stream delivered $events text chunks (Bearer auth)" || bad "no streamed text"

echo "6. A wrong token is rejected"
code=$(curl -s -o /dev/null -w '%{http_code}' -m 15 "$BASE/v1/models" -H "x-api-key: ${TOKEN}x")
[ "$code" = 401 ] && ok "wrong token -> 401" || bad "wrong token -> $code (expected 401)"

echo
echo "$pass passed, $fail failed"
[ "$fail" = 0 ]
