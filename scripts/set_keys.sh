#!/usr/bin/env bash
# Simple, safe way to drop your RPC + Helius keys into .env.
#
#   ./scripts/set_keys.sh
#
# It prompts for each value, pastes it into .env safely (replaces the old line
# instead of duplicating it, no hand-editing = no mangled lines), and can run a
# quick connectivity check at the end. Press ENTER on any prompt to keep the
# current value unchanged.

set -euo pipefail
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
ENV_FILE="$ROOT/.env"
[ -f "$ENV_FILE" ] || { echo "no .env at $ENV_FILE — copy src/env.example first"; exit 1; }

# set_kv KEY VALUE  — replace `KEY=...` line in-place, or append if missing.
set_kv() {
  local key="$1" val="$2" tmp
  tmp="$(mktemp)"
  # Replace the key IN PLACE if it already exists (preserves its line position so
  # it never gets buried below a malformed line, which halts dotenv parsing).
  # Only append at the end when the key is genuinely new.
  if grep -qE "^[[:space:]]*#?[[:space:]]*${key}=" "$ENV_FILE"; then
    awk -v k="$key" -v v="$val" '
      !done && $0 ~ "^[[:space:]]*#?[[:space:]]*" k "=" { print k "=" v; done=1; next }
      { print }
    ' "$ENV_FILE" > "$tmp"
    # Drop any further duplicate definitions after the first.
    awk -v k="$key" '
      $0 ~ "^[[:space:]]*#?[[:space:]]*" k "=" { if (seen++) next }
      { print }
    ' "$tmp" > "${tmp}.2" && mv "${tmp}.2" "$tmp"
  else
    cp "$ENV_FILE" "$tmp"
    printf '%s=%s\n' "$key" "$val" >> "$tmp"
  fi
  mv "$tmp" "$ENV_FILE"
  echo "  ✓ ${key} set"
}

# current KEY — print existing value (for the "keep current" hint), masked.
current() { grep -E "^[[:space:]]*${1}=" "$ENV_FILE" 2>/dev/null | head -1 | cut -d= -f2- ; }

mask() { local v="$1"; [ -z "$v" ] && { echo "(unset)"; return; }; echo "${v:0:18}…[hidden]"; }

ask() {  # ask VAR "prompt" "KEY"
  local __var="$1" prompt="$2" key="$3" cur input
  cur="$(current "$key")"
  printf '%s\n    current: %s\n  > ' "$prompt" "$(mask "$cur")"
  IFS= read -r input || true
  if [ -n "$input" ]; then printf -v "$__var" '%s' "$input"; else printf -v "$__var" '%s' "$cur"; fi
}

echo "=== Fill in your keys (ENTER = keep current) ==="
echo
echo "--- Chainstack (the bot's RPC: feed + blockhash + tx) ---"
ask CS_HTTP "Chainstack HTTPS endpoint  (https://solana-mainnet.core.chainstack.com/<KEY>)" RPC_HTTP
ask CS_WSS  "Chainstack WSS endpoint    (wss://solana-mainnet.core.chainstack.com/<KEY>)"  RPC_WSS
echo
echo "--- Helius free tier (smart-money watcher only — optional) ---"
ask HELIUS  "Helius API key             (just the key, not a URL)" HELIUS_API_KEY
echo

[ -n "${CS_HTTP:-}" ] && set_kv RPC_HTTP "$CS_HTTP"
[ -n "${CS_WSS:-}" ]  && set_kv RPC_WSS  "$CS_WSS"
[ -n "${HELIUS:-}" ]  && set_kv HELIUS_API_KEY "$HELIUS"
# Make sure the feed uses the websocket (Chainstack works without gRPC).
set_kv MOMENTUM_FEED ws

echo
echo "Done. Run the connectivity check?  ./scripts/check_rpc.sh"
