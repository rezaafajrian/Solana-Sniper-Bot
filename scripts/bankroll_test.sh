#!/usr/bin/env bash
# ============================================================================
# MULTI-DAY BANKROLL TEST preset.
#
# Turns the dry run into a REAL, finite, compounding account and leaves it alone:
# start 1 SOL, 0.33 per position, the account grows on profit (funding more
# concurrent positions) and MARGIN-CALLS if it blows the capital. Breaker OFF so
# nothing halts it but a real bankruptcy. Only the PROVEN stack is on — every
# unproven entry filter is OFF (no overfitting, no stalls).
#
# The point: run it for DAYS, untouched, and watch one thing on the dashboard —
# does equity grind UP across thousands of trades, or chop/bleed? That answers
# whether there's a durable edge.
#
# It SETS each MOMENTUM_* key in your existing .env (no duplicates) and ADDS any
# missing ones. It does NOT touch RPC_HTTP, RPC_WSS, or PRIVATE_KEY.
#
# Usage:
#   ./scripts/bankroll_test.sh
#   rm -f momentum_trades.csv momentum_positions.json
#   git pull && cargo build --release && cargo run --release
# ============================================================================
set -e
ENV_FILE=".env"
[ -f "$ENV_FILE" ] || { echo "No .env here. Run from the repo root (~/Solana-Sniper-Bot)."; exit 1; }

PAIRS=(
  # --- mode / feed (paper trading) ---
  "MONITORING_MODE=momentum"
  "MOMENTUM_DRY_RUN=true"
  "MOMENTUM_FEED=ws"
  "MOMENTUM_SIM_COST_FRACTION=0.03"

  # --- REAL bankroll: 1 SOL, 0.33/position, capital-scaled concurrency ---
  "MOMENTUM_START_CAPITAL_SOL=1.0"
  "MOMENTUM_POSITION_SIZE_SOL=0.33"
  "MOMENTUM_MAX_POSITIONS=12"
  "MOMENTUM_MAX_DEPLOYED_SOL=1.0"

  # --- entry: proven config, NO experimental floor ---
  "MOMENTUM_ENTRY_SCORE=57"
  "MOMENTUM_MIN_BASE_SCORE=0"
  "MOMENTUM_COLLAPSE_SCORE=35"
  "MOMENTUM_SHORT_WINDOW_SECS=30"
  "MOMENTUM_MEDIUM_WINDOW_SECS=120"

  # --- proven edges ON ---
  "MOMENTUM_SMART_MONEY=true"
  "MOMENTUM_ALPHA_FOLLOW=true"
  "MOMENTUM_ALPHA_REP_MIN=0.5"
  "MOMENTUM_ALPHA_MIN_SAMPLES=5"
  "MOMENTUM_ALPHA_BOOST=35"
  "MOMENTUM_ALPHA_SIZE_MULT=1.5"
  "MOMENTUM_KOL_ENABLED=true"
  "MOMENTUM_KOL_REQUIRE=false"
  "MOMENTUM_KOL_BOOST=40"
  "MOMENTUM_CONVICTION_SIZING=true"
  "MOMENTUM_CONVICTION_MAX_MULT=2.0"

  # --- exits: let winners run, cut losers ---
  "MOMENTUM_TRAIL_ENABLED=true"
  "MOMENTUM_TRAIL_ACTIVATE_PCT=50"
  "MOMENTUM_TRAIL_GIVEBACK_FRAC=0.35"
  "MOMENTUM_SCALE_OUT_TARGETS=100,200,300,400"
  "MOMENTUM_SCALE_OUT_FRACTIONS=0.2,0.2,0.2,0.2"
  "MOMENTUM_HARD_STOP_PCT=-35"
  "MOMENTUM_LEADER_DUMP_EXIT=true"
  "MOMENTUM_LEADER_DUMP_SOL=1.0"
  "MOMENTUM_LEADER_DUMP_FRACTION=1.0"

  # --- experimental entry filters: OFF (unproven; A/B separately later) ---
  "MOMENTUM_STAGNATION_SECS=0"
  "MOMENTUM_MAX_TOP_HOLDER_SHARE=0"
  "MOMENTUM_MAX_CREATOR_SHARE=0"

  # --- breaker OFF: only a real margin call ends the session ---
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

echo "✅ BANKROLL TEST config applied (RPC + private key untouched)."
echo "   start 1.0 SOL | 0.33/position | up to 12 concurrent | breaker OFF | margin call ON"
echo
grep -E "^RPC_WSS=" "$ENV_FILE" >/dev/null || echo "   ⚠️  RPC_WSS not set — add your websocket endpoint!"
echo "   Start a CLEAN long run:"
echo "     rm -f momentum_trades.csv momentum_positions.json"
echo "     git pull && cargo build --release && cargo run --release"
echo
echo "   Watch the dashboard CAPITAL panel: equity should grind UP over days."
echo "   Leave it alone — no tuning. Analyze later:"
echo "     python3 scripts/analyze_momentum.py momentum_trades.csv"
