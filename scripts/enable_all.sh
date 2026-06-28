#!/usr/bin/env bash
# ============================================================================
# ENABLE ALL — turn ON every feature for a CLEAN DRY RUN (paper trading).
#
# Edits .env in place (backs it up first) and PRESERVES your RPC / PRIVATE_KEY /
# BIRDEYE / TELEGRAM keys. Forces dry-run safety so it can never go live by accident.
# Most intelligence (anti-rug vetoes, bundler/insider detection, runner-DNA bridge,
# creator/pattern avoidance, leader-dump exit, trailing stop, sizing auto-calibration)
# is ON by default — this just flips the opt-in features and tunes for a clean window.
#
#   ./scripts/enable_all.sh        # then:  ./scripts/dry_run_all.sh
# ============================================================================
set -e
cd "$(dirname "$0")/.."
[ -f .env ] || { echo "❌ no .env — run ./scripts/setup_env.sh first."; exit 1; }
cp .env ".env.bak.$(date +%s)" && echo "  (backed up your .env)"
set_kv(){ grep -v "^$1=" .env > .env.tmp && mv .env.tmp .env; printf '%s=%s\n' "$1" "$2" >> .env; }

# --- SAFETY: paper trading only (can never go live from here) ---
set_kv MONITORING_MODE        momentum
set_kv MOMENTUM_DRY_RUN       true
set_kv MOMENTUM_LIVE_CONFIRM  false
set_kv MOMENTUM_FEED          ws
set_kv MOMENTUM_DECISION_LOG  momentum_decisions

# --- ACCOUNT MODEL: start from 1 SOL, day rolls over at 00:00 WIB (UTC+7) ---
set_kv MOMENTUM_START_CAPITAL_SOL            1
set_kv MOMENTUM_DAILY_RESET_UTC_OFFSET_HOURS 7

# --- SIZING: adaptive, capital-preservation-first + self-calibrating from learn.py ---
set_kv MOMENTUM_ADAPTIVE_SIZING       true
set_kv MOMENTUM_KELLY_SIZING          false   # adaptive supersedes flat Kelly
set_kv MOMENTUM_SIZING_AUTOCALIBRATE  true

# --- INTELLIGENCE: sniper watch (pre-bonding) with floors tuned for fresh tokens ---
set_kv MOMENTUM_WATCH_ENABLED      true
set_kv MOMENTUM_WATCH_MIN_LIQ_SOL  2
set_kv MOMENTUM_WATCH_MIN_VOL_SOL  1
set_kv MOMENTUM_WATCH_WHALE_SOL    2

# --- MARKET WATCH: SAFE mode (post-bonded graduates, RugCheck rug-verified) ---
set_kv MARKET_SAFE_MODE            true

# --- RUNNER CAPTURE: keep 40% riding the trail (already the code default; pinned here) ---
set_kv MOMENTUM_LEADER_DUMP_FRACTION 0.6
set_kv MOMENTUM_TRAIL_GIVEBACK_FRAC  0.45

# --- HONEST COST: realistic round-trip so the dry-run PnL means something ---
set_kv MOMENTUM_SIM_COST_FRACTION  0.03
set_kv MOMENTUM_BUY_COST_FRACTION  0.015

# --- BREAKER: wide enough that a normal losing streak doesn't truncate the window ---
set_kv MOMENTUM_DAILY_LOSS_LIMIT_SOL    0.5
set_kv MOMENTUM_MAX_CONSECUTIVE_LOSSES  30   # wide so a dry run gathers the full distribution, never halt-truncates

chmod 600 .env

echo
echo "════════════════════════════════════════════════════════════"
echo "  ✅ ALL FEATURES ON — clean dry run (paper trading, zero risk)"
echo "════════════════════════════════════════════════════════════"
echo "  on by default too: anti-rug vetoes · bundler/insider detection ·"
echo "  runner-DNA bridge · creator/pattern avoidance · leader-dump exit ·"
echo "  trailing stop · scale-out · sizing auto-calibration"
echo
# what's required vs optional
ok(){ grep -qE "^$1=.+" .env; }
ok RPC_WSS        && echo "  RPC ✓"            || echo "  ❌ RPC_WSS missing — the sniper needs it (setup_env.sh)"
ok PRIVATE_KEY    && echo "  wallet ✓"          || echo "  ❌ PRIVATE_KEY missing — throwaway wallet (setup_env.sh)"
ok BIRDEYE_API_KEY&& echo "  🌐 market watch (SAFE/post-bonded, RugCheck) ✓" || echo "  ⚠️  market watch OFF — add BIRDEYE_API_KEY for the whole-market radar"
ok HELIUS_API_KEY && echo "  🐋 smart-money poll ✓" || echo "  (optional) HELIUS_API_KEY for the smart-money watcher (poll mode, no public url needed)"
ok TELEGRAM_BOT_TOKEN && echo "  📲 telegram alerts ✓" || echo "  (optional) TELEGRAM_BOT_TOKEN + TELEGRAM_CHAT_ID for phone alerts"
ok GMGN_API_KEY   && echo "  GMGN security veto ✓" || echo "  (optional) GMGN_API_KEY for the honeypot/security veto"
echo
echo "  ▶ start everything:   ./scripts/dry_run_all.sh"
echo "    dashboard:          http://localhost:8787/dashboard.html"
echo "════════════════════════════════════════════════════════════"
