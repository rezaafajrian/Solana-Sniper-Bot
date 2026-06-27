#!/usr/bin/env bash
# Launch the live dashboard. Serves the repo's web/ dir + the bot's status JSON.
#
# Usage (from the repo root, while the bot is running):
#   ./web/serve.sh
# then open http://localhost:8787 in your browser.
#
# The bot writes momentum_status.json to the repo root (MOMENTUM_STATUS_FILE).
# This server exposes the dashboard and symlinks that file so the page can poll it.

set -e
PORT="${1:-8787}"
ROOT="$(cd "$(dirname "$0")/.." && pwd)"
cd "$ROOT"

# Make the live status (written to repo root) reachable from web/ as momentum_status.json.
if [ -f momentum_status.json ]; then
  ln -sf ../momentum_status.json web/momentum_status.json 2>/dev/null || cp momentum_status.json web/momentum_status.json
else
  # No live file yet — preview with the bundled sample so the UI renders.
  [ -f web/momentum_status.json ] || cp web/momentum_status.sample.json web/momentum_status.json
  echo "ℹ️  No live momentum_status.json yet — showing the sample. Start the bot to go live."
fi

echo "🌐 Dashboard:  http://localhost:${PORT}/dashboard.html"
echo "   (Ctrl-C to stop)"
# Keep web/momentum_status.json (+ the isolated market_watch.json) fresh as they update.
( while true; do
    if [ -f momentum_status.json ]; then cp -f momentum_status.json web/momentum_status.json 2>/dev/null || true; fi
    if [ -f market_watch.json ]; then cp -f market_watch.json web/market_watch.json 2>/dev/null || true; fi
    sleep 2
  done ) &
COPYPID=$!
trap "kill $COPYPID 2>/dev/null" EXIT
cd web
python3 -m http.server "$PORT"
