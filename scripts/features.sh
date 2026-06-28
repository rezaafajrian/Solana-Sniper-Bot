#!/usr/bin/env bash
# ============================================================================
# FEATURES — show which features are ON for your current .env. Read-only.
#   ./scripts/features.sh
# ============================================================================
set -e
cd "$(dirname "$0")/.."
[ -f .env ] || { echo "❌ no .env — run ./scripts/setup_env.sh first."; exit 1; }

# value of KEY from .env (tolerant of spaces/quotes/export), else the given default
val(){ local v; v=$(grep -E "^[[:space:]]*(export[[:space:]]+)?$1[[:space:]]*=" .env 2>/dev/null | tail -1 \
        | sed -E "s/^[^=]*=[[:space:]]*//; s/^[\"']//; s/[\"'][[:space:]]*$//" | tr -d '[:space:]'); echo "${v:-$2}"; }
has(){ grep -qE "^[[:space:]]*(export[[:space:]]+)?$1[[:space:]]*=.+" .env 2>/dev/null; }
flag(){ [ "$(val "$1" "$2")" = "true" ] && echo "✅ ON " || echo "⬜ off"; }
keyok(){ has "$1" && echo "✅ ON " || echo "⬜ off"; }

echo "════════════════════════════════════════════════════════════"
echo "  FEATURE STATUS  (from .env — what the next run will use)"
echo "════════════════════════════════════════════════════════════"
mode=$(val MONITORING_MODE copy)
[ "$mode" = momentum ] && m="✅ momentum" || m="⚠️  $mode  (NOT momentum — will run copy-trading!)"
printf "  %-30s %s\n" "engine mode"                 "$m"
printf "  %-30s %s\n" "dry run (paper, zero risk)"  "$(flag MOMENTUM_DRY_RUN false)"
echo "  ── sizing ─────────────────────────────────────────────────"
printf "  %-30s %s\n" "adaptive sizing"             "$(flag MOMENTUM_ADAPTIVE_SIZING false)"
printf "  %-30s %s\n" "sizing auto-calibration"     "$(flag MOMENTUM_SIZING_AUTOCALIBRATE true)"
printf "  %-30s %s\n" "start capital (SOL)"         "$(val MOMENTUM_START_CAPITAL_SOL 0)"
echo "  ── edge / exits ───────────────────────────────────────────"
printf "  %-30s %s\n" "runner capture (dump frac)"  "$(val MOMENTUM_LEADER_DUMP_FRACTION 0.6)  (<1.0 = riding)"
printf "  %-30s %s\n" "trailing giveback"           "$(val MOMENTUM_TRAIL_GIVEBACK_FRAC 0.45)"
printf "  %-30s %s\n" "entry liquidity floor (SOL)" "$(val MOMENTUM_STRUCT_MIN_LIQ_SOL 1.5)"
printf "  %-30s %s\n" "daily reset offset (hrs)"    "$(val MOMENTUM_DAILY_RESET_UTC_OFFSET_HOURS 0)  (7 = WIB)"
echo "  ── intelligence layers ────────────────────────────────────"
printf "  %-30s %s\n" "sniper watch (pre-bonding)"  "$(flag MOMENTUM_WATCH_ENABLED true)"
printf "  %-30s %s\n" "market watch (Birdeye)"      "$(keyok BIRDEYE_API_KEY)"
printf "  %-30s %s\n" "  └ SAFE/post-bonded mode"   "$(flag MARKET_SAFE_MODE false)"
printf "  %-30s %s\n" "  └ RugCheck rug-safety"     "$(flag MARKET_SAFE_RUGCHECK true)"
printf "  %-30s %s\n" "smart-money watch (Helius)"  "$(keyok HELIUS_API_KEY)"
printf "  %-30s %s\n" "GMGN security veto"          "$(keyok GMGN_API_KEY)"
printf "  %-30s %s\n" "telegram alerts"             "$(keyok TELEGRAM_BOT_TOKEN)"
echo "  ── always on (code defaults, no flag) ─────────────────────"
echo "  ✅ anti-rug vetoes · bundler/insider detection · runner-DNA bridge ·"
echo "  ✅ creator/pattern avoidance · structure gate · circuit breaker"
echo "════════════════════════════════════════════════════════════"
[ "$mode" = momentum ] || echo "  ⚠️  FIX MODE FIRST:  ./scripts/enable_all.sh"
echo "  launch the lot:  ./scripts/dry_run_all.sh"
