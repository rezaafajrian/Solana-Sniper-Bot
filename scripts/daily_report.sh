#!/usr/bin/env bash
# Daily self-learning report. Runs learn.py over all accumulated data, saves a
# timestamped report to reports/, and pushes the executive synthesis to Telegram
# (if TELEGRAM_BOT_TOKEN / TELEGRAM_CHAT_ID are set in .env).
#
# Run it manually any time:   ./scripts/daily_report.sh
# Schedule every 24h via cron (see docs/VPS_DEPLOYMENT.md), e.g. at 09:00:
#   0 9 * * * cd /opt/solbot/Solana-Sniper-Bot && ./scripts/daily_report.sh >> reports/cron.log 2>&1
set -e
cd "$(dirname "$0")/.."
python3 scripts/learn.py momentum_decisions --report-dir reports --telegram "$@"
