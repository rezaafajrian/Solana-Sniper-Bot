#!/usr/bin/env bash
# ============================================================================
# DRY RUN — EVERYTHING TOGETHER, one command.
#   • sniper bot   (paper trading, via clean_dry_run.sh)        — foreground
#   • market watch (whole-market Birdeye scanner, ISOLATED)     — background
#   • dashboard    (http://localhost:8787/dashboard.html)       — background
#
# Ctrl-C stops all three. The market watch only starts if BIRDEYE_API_KEY is set.
#
#   ./scripts/dry_run_all.sh
# ============================================================================
set -e
cd "$(dirname "$0")/.."
PORT="${1:-8787}"
mkdir -p reports

PIDS=()
cleanup(){
  echo
  echo "🛑 stopping background services…"
  for p in "${PIDS[@]}"; do kill "$p" 2>/dev/null || true; done
  pkill -f 'scripts/market_watch.py'     2>/dev/null || true
  pkill -f "http.server ${PORT}"          2>/dev/null || true
}
trap cleanup EXIT INT TERM

# --- 1. market watch (isolated) — background, only if a Birdeye key is present ---
if grep -qE '^BIRDEYE_API_KEY=.+' .env 2>/dev/null; then
  echo "🌐 market watch  → background (log: reports/market_watch.log)"
  python3 scripts/market_watch.py >> reports/market_watch.log 2>&1 &
  PIDS+=($!)
else
  echo "⚠️  no BIRDEYE_API_KEY in .env — running WITHOUT market watch."
  echo "    add it:  printf 'BIRDEYE_API_KEY=YOURKEY\\n' >> .env   (then re-run)"
fi

# --- 2. dashboard — background ---
echo "📊 dashboard     → http://localhost:${PORT}/dashboard.html  (log: reports/dashboard.log)"
./web/serve.sh "$PORT" >> reports/dashboard.log 2>&1 &
PIDS+=($!)
sleep 1

# --- 3. sniper dry run — FOREGROUND (this blocks; Ctrl-C ends everything) ---
echo "🟢 sniper dry run → starting (paper trading). Ctrl-C stops all three."
echo "────────────────────────────────────────────────────────────"
./scripts/clean_dry_run.sh
