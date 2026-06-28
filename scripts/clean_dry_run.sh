#!/usr/bin/env bash
# ============================================================================
# CLEAN DRY RUN — the validation window before going live.
#
# Archives the previous run's data so the new window's PnL / win-rate is HONEST
# (not mixed with old tests), rebuilds, sanity-checks, and launches paper trading.
# Learned memory (wallet reputation, avoidance, runner patterns) is KEPT by default
# so the bot validates in the state you'd actually go live in. Use --fresh-memory to
# wipe that too and validate a cold start.
#
# SAFETY: refuses to run unless MOMENTUM_DRY_RUN=true — it can never wipe data or
# start a live run by accident.
#
# Usage:
#   ./scripts/clean_dry_run.sh                 # archive window data, keep memory, launch
#   ./scripts/clean_dry_run.sh --fresh-memory  # also wipe wallet_rep / avoidance / patterns
# ============================================================================
set -e
cd "$(dirname "$0")/.."

# FORCE the critical flags — this is the momentum PAPER-TRADE launcher. Exporting them here
# overrides any stale `export MONITORING_MODE=copy` / `MOMENTUM_DRY_RUN=false` left in your
# shell (dotenv can't override a shell var — that's why the bot kept booting into copy-trading
# and then refusing as "LIVE"). This makes the dry run bulletproof: momentum + paper, always.
export MONITORING_MODE=momentum
export MOMENTUM_DRY_RUN=true
export MOMENTUM_LIVE_CONFIRM=false
export MOMENTUM_FEED=ws            # websocket feed (logsSubscribe on RPC_WSS) — no paid Yellowstone gRPC needed

FRESH_MEMORY=false
[ "$1" = "--fresh-memory" ] && FRESH_MEMORY=true

[ -f .env ] || { echo "❌ No .env here. Set it up first (see docs/ENABLE_ALL.md)."; exit 1; }

# --- safety: paper-trading only ---
if ! grep -qE '^MOMENTUM_DRY_RUN=true' .env; then
  echo "❌ MOMENTUM_DRY_RUN is not 'true' in .env."
  echo "   This script only runs paper trades — refusing so it can't wipe data or go live by accident."
  exit 1
fi
# --- required keys present ---
grep -qE '^RPC_WSS=.+'     .env || { echo "❌ RPC_WSS is empty in .env (your wss:// endpoint). See docs/ENABLE_ALL.md STEP 2."; exit 1; }
grep -qE '^PRIVATE_KEY=.+' .env || { echo "❌ PRIVATE_KEY is empty in .env (throwaway wallet). See docs/ENABLE_ALL.md STEP 0b."; exit 1; }

# --- archive the previous window's data (never just delete) ---
STAMP="backups/$(date +%Y%m%d-%H%M%S)"
mkdir -p "$STAMP"
echo "🧹 Archiving previous window → $STAMP/"
for f in momentum_trades.csv momentum_positions.json momentum_positions.json.tmp \
         momentum_decisions.csv momentum_decisions.jsonl momentum_decisions_outcomes.csv \
         momentum_decisions_pending.json momentum_decisions_signals.csv momentum_status.json; do
  [ -f "$f" ] && mv "$f" "$STAMP/" 2>/dev/null || true
done
if [ "$FRESH_MEMORY" = true ]; then
  echo "   --fresh-memory: also wiping learned memory (cold-start validation)"
  for f in momentum_wallet_rep.csv momentum_avoidance.csv runner_patterns.json; do
    [ -f "$f" ] && mv "$f" "$STAMP/" 2>/dev/null || true
  done
else
  echo "   learned memory KEPT (wallet_rep / avoidance / runner_patterns) — validating the bot as-is"
fi

# --- update + build + sanity check ---
echo "🔨 Updating + building (this can take a few minutes)…"
git pull --ff-only 2>/dev/null || echo "   (git pull skipped — not a fast-forward or offline)"
cargo build --release
echo "🧪 Sanity check…"
if cargo test --release >/dev/null 2>&1; then echo "   ✅ tests passed"; else echo "   ⚠️  tests FAILED — investigate before trusting any result"; fi

cat <<'EOF'

🟢 Launching CLEAN dry run — paper trading, ZERO real money at risk.

   • Leave it running for DAYS, untouched. Watch via the dashboard / Telegram.
   • After 2h+, read the results:
       python3 scripts/analyze_momentum.py momentum_trades.csv
       python3 scripts/learn.py
   • Stress test before trusting it: set MOMENTUM_SIM_COST_FRACTION=0.08 in .env
     and run another window — if still green at 8% cost, the edge has real margin.

   Ctrl-C to stop. (On a VPS, prefer the systemd service so it survives reboots —
   see docs/VPS_DEPLOYMENT.md.)

EOF
cargo run --release
