#!/usr/bin/env bash
# ============================================================================
# SETUP — build your .env by answering a few questions. No text editor needed.
# Paste each value when asked; press Enter to skip the optional ones.
# Sets everything up for a DRY RUN (paper trading, zero risk).
#   ./scripts/setup_env.sh
# ============================================================================
set -e
cd "$(dirname "$0")/.."

echo "════════════════════════════════════════════════════════════"
echo "  BOT SETUP — I'll ask for each value and write the .env for you."
echo "  (Press Enter to skip the optional ones.)"
echo "════════════════════════════════════════════════════════════"
echo

# back up any existing .env, then start from the template (keeps all the safe defaults)
[ -f .env ] && cp .env ".env.bak.$(date +%s)" && echo "  (your old .env was backed up)"
cp src/env.example .env

set_kv(){ grep -v "^$1=" .env > .env.tmp && mv .env.tmp .env; printf '%s=%s\n' "$1" "$2" >> .env; }

# ---- RPC (required) ----
echo "1) RPC — paste your Helius API key, OR paste your full https:// RPC link."
printf "   > "; read -r RPC_IN
if [ -z "$RPC_IN" ]; then echo "   ❌ RPC is required. Re-run when you have it (helius.dev)."; exit 1; fi
if [ "${RPC_IN#http}" != "$RPC_IN" ]; then
  RPC_HTTP="$RPC_IN"
  echo "   now paste your wss:// websocket link:"
  printf "   > "; read -r RPC_WSS
else
  RPC_HTTP="https://mainnet.helius-rpc.com/?api-key=$RPC_IN"
  RPC_WSS="wss://mainnet.helius-rpc.com/?api-key=$RPC_IN"
  echo "   ✓ built Helius http + wss links from your key"
fi
set_kv RPC_HTTP "$RPC_HTTP"
set_kv RPC_WSS "$RPC_WSS"

# ---- wallet (required) ----
echo
echo "2) WALLET — paste your Phantom PRIVATE KEY (the long string, NOT the 12 words)."
echo "   Use a THROWAWAY wallet with no money for the dry run."
printf "   > "; read -r PRIV
if [ -z "$PRIV" ]; then echo "   ❌ PRIVATE_KEY is required. Export it from Phantom and re-run."; exit 1; fi
set_kv PRIVATE_KEY "$PRIV"

# ---- optional keys ----
echo
echo "3) TELEGRAM bot token (from @BotFather) — or Enter to skip:"
printf "   > "; read -r TG_TOKEN
echo "   Telegram chat id (from @userinfobot) — or Enter to skip:"
printf "   > "; read -r TG_CHAT
[ -n "$TG_TOKEN" ] && set_kv TELEGRAM_BOT_TOKEN "$TG_TOKEN"
[ -n "$TG_CHAT" ] && set_kv TELEGRAM_CHAT_ID "$TG_CHAT"

echo
echo "4) GMGN API key (gmgn.ai) — or Enter to skip:"
printf "   > "; read -r GMGN_KEY
if [ -n "$GMGN_KEY" ]; then set_kv GMGN_ENABLED true; set_kv GMGN_API_KEY "$GMGN_KEY"; fi

echo
echo "5) BIRDEYE API key (birdeye.so) — or Enter to skip:"
printf "   > "; read -r BIRDEYE_KEY
[ -n "$BIRDEYE_KEY" ] && set_kv BIRDEYE_API_KEY "$BIRDEYE_KEY"

# ---- lock to dry-run mode + the learning log ----
set_kv MONITORING_MODE momentum
set_kv MOMENTUM_DRY_RUN true
set_kv MOMENTUM_LIVE_CONFIRM false
set_kv MOMENTUM_FEED ws
set_kv MOMENTUM_DECISION_LOG momentum_decisions
# Account model: start from 1 SOL so the dashboard's "total SOL" is meaningful and PnL
# compounds against a real bankroll (paper money in dry run). Change to taste.
set_kv MOMENTUM_START_CAPITAL_SOL 1
chmod 600 .env

echo
echo "════════════════════════════════════════════════════════════"
echo "  ✅ .env written and locked. Dry-run mode (paper trading)."
echo "  Configured: RPC ✓  wallet ✓"
[ -n "$TG_TOKEN" ] && echo "              Telegram ✓"
[ -n "$GMGN_KEY" ] && echo "              GMGN ✓"
[ -n "$BIRDEYE_KEY" ] && echo "              Birdeye ✓"
echo
echo "  Now start the bot:"
echo "      ./scripts/clean_dry_run.sh"
echo "════════════════════════════════════════════════════════════"
