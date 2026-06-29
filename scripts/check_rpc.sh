#!/usr/bin/env bash
# Quick connectivity + latency check for the endpoints in .env.
#
#   ./scripts/check_rpc.sh
#
# - RPC_HTTP : real getLatestBlockhash call x5, reports round-trip latency
#              (this is the number that matters — and the call that 429'd)
# - RPC_WSS  : TLS reachability of the websocket host (full feed test is at bot start)
# - HELIUS   : getHealth, confirms the key works
# Nothing here sends a transaction or spends anything.

set -uo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ENV_FILE="$ROOT/.env"
val() { grep -E "^[[:space:]]*${1}=" "$ENV_FILE" 2>/dev/null | head -1 | cut -d= -f2- ; }
host_of() { echo "$1" | sed -E 's#^[a-z]+://([^/?]+).*#\1#'; }

RPC_HTTP="$(val RPC_HTTP)"; RPC_WSS="$(val RPC_WSS)"; HELIUS="$(val HELIUS_API_KEY)"

echo "=== RPC_HTTP — getLatestBlockhash x5 (latency that matters) ==="
if [ -z "$RPC_HTTP" ] || [[ "$RPC_HTTP" == *test-rpc* ]]; then
  echo "  ✗ RPC_HTTP not set (or still the test- placeholder)"
else
  body='{"jsonrpc":"2.0","id":1,"method":"getLatestBlockhash","params":[{"commitment":"processed"}]}'
  ok=0
  for i in 1 2 3 4 5; do
    out="$(curl -sS -m 8 -w '\n%{http_code} %{time_total}' -X POST -H 'content-type: application/json' \
           --data "$body" "$RPC_HTTP" 2>/dev/null)" || { echo "  attempt $i: connection error"; continue; }
    code="$(echo "$out" | tail -1 | awk '{print $1}')"
    t="$(echo "$out" | tail -1 | awk '{print $2}')"
    if [ "$code" = "200" ] && echo "$out" | grep -q '"blockhash"'; then
      printf '  attempt %d: 200 OK   %sms\n' "$i" "$(awk "BEGIN{printf \"%.0f\", $t*1000}")"
      ok=$((ok+1))
    else
      printf '  attempt %d: HTTP %s  %s\n' "$i" "$code" "$(echo "$out" | head -c 120)"
    fi
  done
  [ "$ok" -eq 5 ] && echo "  ✓ HTTP healthy (5/5)" || echo "  ⚠ HTTP $ok/5 ok — check key / rate limit"
fi

echo
echo "=== RPC_WSS — websocket host TLS reachability ==="
if [ -z "$RPC_WSS" ] || [[ "$RPC_WSS" == *test-wss* ]]; then
  echo "  ✗ RPC_WSS not set (or still the test- placeholder)"
else
  H="$(host_of "$RPC_WSS")"
  if echo | timeout 8 openssl s_client -connect "${H}:443" -servername "$H" >/dev/null 2>&1; then
    echo "  ✓ ${H}:443 TLS handshake OK (logsSubscribe is verified live when the bot starts)"
  else
    echo "  ✗ ${H}:443 unreachable — check the URL"
  fi
fi

echo
echo "=== HELIUS_API_KEY — getHealth (smart-money watcher) ==="
if [ -z "$HELIUS" ]; then
  echo "  – not set (optional; smart-money watch stays off)"
else
  out="$(curl -sS -m 8 -X POST -H 'content-type: application/json' \
         --data '{"jsonrpc":"2.0","id":1,"method":"getHealth"}' \
         "https://mainnet.helius-rpc.com/?api-key=${HELIUS}" 2>/dev/null)"
  if echo "$out" | grep -qi '"ok"\|"result"'; then echo "  ✓ Helius key works"; else echo "  ✗ Helius: $(echo "$out" | head -c 160)"; fi
fi
echo
echo "Done."
