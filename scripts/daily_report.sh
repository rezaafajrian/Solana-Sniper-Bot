#!/usr/bin/env bash
# Daily research/analysis pass — runs the self-learning + research layers over the data
# the bot has produced, saves reports/, and pushes the synthesis to Telegram.
#
# ISOLATION: this is a SEPARATE process from the sniper bot and must never degrade it.
# Everything here runs at the lowest CPU + idle I/O priority, so the latency-sensitive bot
# always wins scheduling. The only file the bot reads back (runner_patterns.json) is written
# atomically by runner_research.py, so the bot never sees a partial file. The analysis only
# ever READS the bot's data and writes advisory files — it never touches the bot's live
# decision/risk/exit logic.
#
# Run manually:   ./scripts/daily_report.sh
# Schedule (cron, e.g. 09:00):
#   0 9 * * * cd /opt/solbot/Solana-Sniper-Bot && ./scripts/daily_report.sh >> reports/cron.log 2>&1
set -e
cd "$(dirname "$0")/.."
mkdir -p reports   # log + report target (also used by the scheduler's cron.log)

# Stamp each run so the cron/journal log is readable when this runs unattended.
echo "──── research pass $(date '+%Y-%m-%d %H:%M:%S %z') ────"

# Lowest-priority wrapper so the analysis can't steal CPU/IO from the bot.
LOWPRIO="nice -n 19"
command -v ionice >/dev/null 2>&1 && LOWPRIO="ionice -c3 $LOWPRIO"

# 1. Daily self-learning report (entries, exits, edge, OOS-validated recs) + Telegram.
$LOWPRIO python3 scripts/learn.py momentum_decisions --report-dir reports --telegram "$@" || true
# 2. Runner DNA research layer (studies the 10x+ winners, refines the knowledge base).
$LOWPRIO python3 scripts/runner_research.py momentum_decisions --report-dir reports || true
# 3. Lifecycle tracker — keep watching tokens after graduation (find the slow-burn winners).
$LOWPRIO python3 scripts/lifecycle_tracker.py || true
# 4. Viral autopsy — re-dissect your known-winner list & refresh the viral DNA (if present).
[ -f viral_tokens.txt ] && $LOWPRIO python3 scripts/viral_autopsy.py || true
# 5. Refresh the proven smart-money wallet set (re-registers the Helius webhook if keys set).
$LOWPRIO python3 scripts/smart_money_watch.py --sync || true
