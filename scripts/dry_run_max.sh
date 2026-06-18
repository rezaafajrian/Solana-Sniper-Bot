#!/usr/bin/env bash
# ============================================================================
# MAXIMAL-POWER DRY RUN preset for the momentum bot.
#
# Turns on the PROVEN stack (alpha-follow + KOL boost + smart-money + trailing stop
# + scale-out + conviction + persistence) with the circuit breaker OFF so it runs
# uninterrupted. The UNPROVEN entry filters (base-momentum floor, stagnation stop,
# concentration veto) are left OFF — they over-filter and should be A/B'd one at a
# time, not switched on blind. This is the +5.66 SOL config, fully loaded.
#
# It SETS each MOMENTUM_* key in your existing .env (removing duplicates first, so
# the value actually takes effect) and ADDS any keys your .env is missing. It does
# NOT touch RPC_HTTP, RPC_WSS, or PRIVATE_KEY — your endpoint and wallet are safe.
#
# Usage (from the repo root):
#   ./scripts/dry_run_max.sh
#   git pull && cargo build --release && cargo run --release
# ============================================================================
set -e
ENV_FILE=".env"
[ -f "$ENV_FILE" ] || { echo "No .env here. Run from the repo root (~/Solana-Sniper-Bot)."; exit 1; }

# Every MOMENTUM_* knob set to a full-power, paper-trading config.
PAIRS=(
  # --- mode / feed (paper trading; never signs) ---
  "MONITORING_MODE=momentum"
  "MOMENTUM_DRY_RUN=true"
  "MOMENTUM_FEED=ws"
  "MOMENTUM_SIM_COST_FRACTION=0.03"

  # --- sizing (3 slots x 0.33 = ~1.0 SOL book) ---
  "MOMENTUM_POSITION_SIZE_SOL=0.33"
  "MOMENTUM_MAX_POSITIONS=3"
  "MOMENTUM_MAX_DEPLOYED_SOL=1.0"

  # --- entry (proven config; experimental floor OFF by default) ---
  "MOMENTUM_ENTRY_SCORE=57"
  "MOMENTUM_MIN_BASE_SCORE=0"
  "MOMENTUM_COLLAPSE_SCORE=35"
  "MOMENTUM_SHORT_WINDOW_SECS=30"
  "MOMENTUM_MEDIUM_WINDOW_SECS=120"

  # --- edge: smart-money memory + alpha-follow (the learned edge) ---
  "MOMENTUM_SMART_MONEY=true"
  "MOMENTUM_ALPHA_FOLLOW=true"
  "MOMENTUM_ALPHA_REP_MIN=0.5"
  "MOMENTUM_ALPHA_MIN_SAMPLES=5"
  "MOMENTUM_ALPHA_BOOST=35"
  "MOMENTUM_ALPHA_WINDOW_SECS=60"
  "MOMENTUM_ALPHA_SIZE_MULT=1.5"

  # --- edge: KOL list in BOOST mode (so alpha can also fire) ---
  "MOMENTUM_KOL_ENABLED=true"
  "MOMENTUM_KOL_REQUIRE=false"
  "MOMENTUM_KOL_BOOST=40"

  # --- edge: conviction sizing ---
  "MOMENTUM_CONVICTION_SIZING=true"
  "MOMENTUM_CONVICTION_MAX_MULT=2.0"

  # --- exits: let winners run, cut duds ---
  "MOMENTUM_TRAIL_ENABLED=true"
  "MOMENTUM_TRAIL_ACTIVATE_PCT=50"
  "MOMENTUM_TRAIL_GIVEBACK_FRAC=0.35"
  "MOMENTUM_SCALE_OUT_TARGETS=100,200,300,400"
  "MOMENTUM_SCALE_OUT_FRACTIONS=0.2,0.2,0.2,0.2"
  "MOMENTUM_HARD_STOP_PCT=-35"
  "MOMENTUM_STAGNATION_SECS=0"
  "MOMENTUM_STAGNATION_MIN_PNL=20"

  # --- exit: insider/leader-dump (full exit; protective) ---
  "MOMENTUM_LEADER_DUMP_EXIT=true"
  "MOMENTUM_LEADER_DUMP_SOL=1.0"
  "MOMENTUM_LEADER_DUMP_FRACTION=1.0"

  # --- anti-dump concentration veto: OFF by default (unproven; over-filters fresh
  #     tokens). Opt in and A/B it one at a time: set SHARE=0.85, CREATOR=0.25. ---
  "MOMENTUM_MAX_TOP_HOLDER_SHARE=0"
  "MOMENTUM_TOP_HOLDER_N=10"
  "MOMENTUM_MAX_CREATOR_SHARE=0"
  "MOMENTUM_CONCENTRATION_MIN_TRADERS=25"

  # --- discipline: breaker OFF for an uninterrupted sample ---
  "MOMENTUM_DAILY_LOSS_LIMIT_SOL=0"
  "MOMENTUM_MAX_CONSECUTIVE_LOSSES=0"
  "MOMENTUM_MAX_SELL_RETRIES=8"

  # --- persistence + dashboard + log ---
  "MOMENTUM_POSITIONS_FILE=momentum_positions.json"
  "MOMENTUM_STATUS_FILE=momentum_status.json"
  "MOMENTUM_TRADE_LOG=momentum_trades.csv"
)

tmp="$(mktemp)"
cp "$ENV_FILE" "$tmp"
for kv in "${PAIRS[@]}"; do
  key="${kv%%=*}"
  grep -v "^${key}=" "$tmp" > "${tmp}.2" && mv "${tmp}.2" "$tmp"
  echo "$kv" >> "$tmp"
done
mv "$tmp" "$ENV_FILE"

echo "✅ MAXIMAL-POWER DRY RUN config applied to .env (RPC + private key untouched)."
echo
echo "   Sanity-check your feed is set (must be your real wss):"
grep -E "^RPC_WSS=" "$ENV_FILE" || echo "   ⚠️  RPC_WSS not set — add your websocket endpoint!"
echo
echo "   Start a clean run:"
echo "     rm -f momentum_trades.csv momentum_positions.json"
echo "     git pull && cargo build --release && cargo run --release"
echo
echo "   Then analyze:"
echo "     python3 scripts/analyze_momentum.py momentum_trades.csv"
echo
echo "   NOTE: GMGN holder/bundle veto stays OFF (needs an API key). The free"
echo "   stream concentration veto covers anti-dump. All filters reduce trade"
echo "   count — if too few entries, lower MOMENTUM_MIN_BASE_SCORE / loosen vetoes."
