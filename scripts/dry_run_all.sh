#!/usr/bin/env bash
# ============================================================================
# DRY RUN — EVERYTHING TOGETHER, one command.
#   • sniper bot   (paper trading, via clean_dry_run.sh)        — foreground
#   • market watch (SAFE/post-bonded Birdeye scanner, ISOLATED) — background
#   • smart-money  (Helius poll mode — proven wallets' buys)    — background
#   • dashboard    (http://localhost:8787/dashboard.html)       — background
#
# Ctrl-C stops all of them. Market watch needs BIRDEYE_API_KEY; smart-money needs
# HELIUS_API_KEY — each starts only if its key is present.
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
  pkill -f 'scripts/market_watch.py'      2>/dev/null || true
  pkill -f 'scripts/smart_money_watch.py' 2>/dev/null || true
  pkill -f "http.server ${PORT}"          2>/dev/null || true
}
trap cleanup EXIT INT TERM

# --- 1. market watch (isolated, self-paced) — background, only if a Birdeye key is present ---
if grep -qE '^BIRDEYE_API_KEY=.+' .env 2>/dev/null; then
  echo "🌐 market watch  → self-paced background scanner (log: reports/market_watch.log)"
  python3 scripts/market_watch.py >> reports/market_watch.log 2>&1 &
  PIDS+=($!)
  # surface the first verdict inline (key OK? budget? early runners?) without an extra scan
  sleep 6
  grep -hE '✅|⛔|⚠️|budget|scanned|EARLY' reports/market_watch.log 2>/dev/null | tail -3 | sed 's/^/   /' || true
else
  echo "⚠️  no BIRDEYE_API_KEY in .env — running WITHOUT market watch."
  echo "    add it:  printf 'BIRDEYE_API_KEY=YOURKEY\\n' >> .env   (then re-run)"
fi

# --- 1b. smart-money watcher (poll mode — laptop-friendly, no public URL) ---
if grep -qE '^HELIUS_API_KEY=.+' .env 2>/dev/null; then
  echo "🐋 smart money  → poll mode in background (log: reports/smart_money.log)"
  python3 scripts/smart_money_watch.py --poll >> reports/smart_money.log 2>&1 &
  PIDS+=($!)
else
  echo "   (no HELIUS_API_KEY — smart-money watcher off; add it for proven-wallet buy alerts)"
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
