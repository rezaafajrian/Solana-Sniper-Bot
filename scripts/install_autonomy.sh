#!/usr/bin/env bash
# ============================================================================
# INSTALL AUTONOMY — make the research/learning layer self-sustaining.
#
# Schedules the nightly research pass (scripts/daily_report.sh) so the knowledge
# base — runner_patterns.json, viral_dna.json, smart-money set, the daily learn.py
# synthesis — rebuilds itself every night with ZERO manual steps. The bot already
# hot-reloads runner_patterns.json every ~60s, so a fresh pass sharpens the live
# watchlist WITHOUT a restart.
#
# With --with-bot it also installs the bot itself as an always-on service, so the
# whole system is autonomous: bot auto-restarts + re-adopts open positions on crash
# or reboot, research self-refreshes nightly, memory compounds across runs.
#
# Picks the best scheduler available, in order:
#   1. system systemd   (run as root / sudo)      — survives reboot, fully headless
#   2. user systemd     (non-root, systemctl --user) — + lingering so it runs logged-out
#   3. cron                                          — universal fallback
#
# ZERO hand-editing: the repo path and run-user are detected from where you run this.
#
# Usage:
#   ./scripts/install_autonomy.sh                      # schedule nightly research
#   ./scripts/install_autonomy.sh --with-bot           # ALSO install the always-on bot service
#   ./scripts/install_autonomy.sh --with-market-watch  # ALSO run the whole-market Birdeye watch (isolated)
#   ./scripts/install_autonomy.sh --at 03:30           # nightly run time (default 09:00, local tz)
#   ./scripts/install_autonomy.sh --uninstall          # remove everything this installed
# ============================================================================
set -euo pipefail
cd "$(dirname "$0")/.."
REPO="$(pwd -P)"
RUNUSER="$(id -un)"
RUNGROUP="$(id -gn)"
BIN="$REPO/target/release/solana-vntr-sniper"
AT="09:00"
WITH_BOT=false
WITH_MARKET=false
UNINSTALL=false

while [ $# -gt 0 ]; do
  case "$1" in
    --with-bot)          WITH_BOT=true ;;
    --with-market-watch) WITH_MARKET=true ;;
    --uninstall)         UNINSTALL=true ;;
    --at)         AT="${2:-09:00}"; shift ;;
    --at=*)       AT="${1#--at=}" ;;
    -h|--help)    grep '^#' "$0" | grep -v '^#!' | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unknown arg: $1 (try --help)"; exit 1 ;;
  esac
  shift
done

# HH:MM -> components (for OnCalendar + cron)
HH="${AT%%:*}"; MM="${AT##*:}"
case "$HH:$MM" in [0-2][0-9]:[0-5][0-9]) : ;; *) echo "❌ --at must be HH:MM (24h), got '$AT'"; exit 1 ;; esac

echo "════════════════════════════════════════════════════════════"
echo "  AUTONOMY INSTALLER"
echo "  repo : $REPO"
echo "  user : $RUNUSER   nightly research at $AT (local time)"
$WITH_BOT && echo "  + always-on bot service (auto-restart, re-adopts positions)"
$WITH_MARKET && echo "  + whole-market watch (Birdeye, ISOLATED from the sniper)"
echo "════════════════════════════════════════════════════════════"

# --- detect which scheduler we can use -------------------------------------
have(){ command -v "$1" >/dev/null 2>&1; }
MODE=""
if have systemctl && [ "$(id -u)" = "0" ]; then
  MODE="system"
elif have systemctl && systemctl --user show-environment >/dev/null 2>&1; then
  MODE="user"
elif have crontab; then
  MODE="cron"
else
  echo "❌ No systemd or cron found. Run scripts/daily_report.sh from your own scheduler."
  exit 1
fi
echo "  scheduler: $MODE"
echo

# ===========================================================================
# systemd (system or user)
# ===========================================================================
if [ "$MODE" = "system" ] || [ "$MODE" = "user" ]; then
  if [ "$MODE" = "system" ]; then
    UNIT_DIR="/etc/systemd/system"; SC(){ systemctl "$@"; }; USER_LINE="User=$RUNUSER"$'\n'"Group=$RUNGROUP"
  else
    UNIT_DIR="$HOME/.config/systemd/user"; SC(){ systemctl --user "$@"; }; USER_LINE=""
    mkdir -p "$UNIT_DIR"
  fi

  if $UNINSTALL; then
    SC disable --now momentum-research.timer 2>/dev/null || true
    SC disable --now momentum-bot.service 2>/dev/null || true
    SC disable --now momentum-market-watch.service 2>/dev/null || true
    rm -f "$UNIT_DIR/momentum-research.service" "$UNIT_DIR/momentum-research.timer"
    rm -f "$UNIT_DIR/momentum-bot.service" "$UNIT_DIR/momentum-market-watch.service"
    SC daemon-reload 2>/dev/null || true
    echo "✅ removed systemd units ($MODE)."
    exit 0
  fi

  # --- research oneshot + timer ---
  cat > "$UNIT_DIR/momentum-research.service" <<EOF
[Unit]
Description=Solana bot nightly research/learning pass (oneshot, idle priority)
After=network-online.target
Wants=network-online.target

[Service]
Type=oneshot
$USER_LINE
WorkingDirectory=$REPO
ExecStart=/usr/bin/env bash $REPO/scripts/daily_report.sh
# never steal CPU/IO from the latency-sensitive bot
Nice=19
IOSchedulingClass=idle
CPUSchedulingPolicy=idle
# defense-in-depth: this job only reads the bot's data and writes advisory files
ProtectSystem=strict
ReadWritePaths=$REPO
NoNewPrivileges=true
PrivateTmp=true
EOF

  cat > "$UNIT_DIR/momentum-research.timer" <<EOF
[Unit]
Description=Nightly Solana bot research/learning pass

[Timer]
OnCalendar=*-*-* $HH:$MM:00
Persistent=true
RandomizedDelaySec=300

[Install]
WantedBy=timers.target
EOF

  # --- optional: always-on bot service ---
  if $WITH_BOT; then
    if [ ! -x "$BIN" ]; then echo "  ⚠️  $BIN not built yet — run: cargo build --release"; fi
    cat > "$UNIT_DIR/momentum-bot.service" <<EOF
[Unit]
Description=Solana momentum trading bot
After=network-online.target
Wants=network-online.target
StartLimitIntervalSec=300
StartLimitBurst=10

[Service]
Type=simple
$USER_LINE
WorkingDirectory=$REPO
ExecStart=$BIN
Restart=always
RestartSec=5
SyslogIdentifier=momentum-bot
NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=strict
ReadWritePaths=$REPO
ProtectKernelTunables=true
ProtectKernelModules=true
ProtectControlGroups=true
RestrictSUIDSGID=true
LockPersonality=true
MemoryMax=2G
TasksMax=512

[Install]
WantedBy=$([ "$MODE" = system ] && echo multi-user.target || echo default.target)
EOF
  fi

  # --- optional: whole-market watch (Birdeye) — ISOLATED from the sniper ---
  if $WITH_MARKET; then
    grep -qE '^BIRDEYE_API_KEY=.+' "$REPO/.env" 2>/dev/null || echo "  ⚠️  BIRDEYE_API_KEY not set in .env — market-watch needs it (birdeye.so)."
    cat > "$UNIT_DIR/momentum-market-watch.service" <<EOF
[Unit]
Description=Solana whole-market watch (Birdeye, isolated from the sniper)
After=network-online.target
Wants=network-online.target

[Service]
Type=simple
$USER_LINE
WorkingDirectory=$REPO
ExecStart=/usr/bin/env python3 $REPO/scripts/market_watch.py
Restart=always
RestartSec=15
Nice=15
IOSchedulingClass=idle
SyslogIdentifier=market-watch
NoNewPrivileges=true
PrivateTmp=true
ProtectSystem=strict
ReadWritePaths=$REPO

[Install]
WantedBy=$([ "$MODE" = system ] && echo multi-user.target || echo default.target)
EOF
  fi

  SC daemon-reload
  SC enable --now momentum-research.timer
  $WITH_BOT && SC enable --now momentum-bot.service || true
  $WITH_MARKET && SC enable --now momentum-market-watch.service || true

  # for user-mode: make it run even when logged out
  if [ "$MODE" = "user" ] && have loginctl; then
    loginctl enable-linger "$RUNUSER" 2>/dev/null \
      && echo "  ✓ lingering enabled (runs while you're logged out)" \
      || echo "  ⚠️  could not enable-linger — research runs only while you're logged in. Fix: sudo loginctl enable-linger $RUNUSER"
  fi

  echo
  echo "✅ Autonomy installed ($MODE systemd)."
  echo "   next research run : $(SC list-timers momentum-research.timer --no-pager 2>/dev/null | awk 'NR==2{print $1,$2,$3}')"
  echo "   watch research    : journalctl $([ "$MODE" = user ] && echo --user) -u momentum-research -f"
  $WITH_BOT && echo "   watch the bot     : journalctl $([ "$MODE" = user ] && echo --user) -u momentum-bot -f"
  $WITH_MARKET && echo "   watch the market  : journalctl $([ "$MODE" = user ] && echo --user) -u market-watch -f"
  echo "   run research now  : systemctl $([ "$MODE" = user ] && echo --user) start momentum-research.service"
  exit 0
fi

# ===========================================================================
# cron fallback
# ===========================================================================
if [ "$MODE" = "cron" ]; then
  MARKER="# momentum-research (installed by install_autonomy.sh)"
  MARKETMARK="# momentum-market-watch (installed by install_autonomy.sh)"
  CRON_LINE="$MM $HH * * * cd $REPO && ./scripts/daily_report.sh >> $REPO/reports/cron.log 2>&1 $MARKER"
  # cron can't keep a loop alive, so run the scanner one-shot every 3 minutes instead
  MARKET_LINE="*/3 * * * * cd $REPO && python3 scripts/market_watch.py --once >> $REPO/reports/market_watch.log 2>&1 $MARKETMARK"
  CUR="$(crontab -l 2>/dev/null || true)"
  # always strip our previous lines first (idempotent)
  NEW="$(printf '%s\n' "$CUR" | grep -vF "$MARKER" | grep -vF "$MARKETMARK" || true)"

  if $UNINSTALL; then
    printf '%s\n' "$NEW" | crontab -
    echo "✅ removed the cron entries. (The bot itself isn't managed by cron — stop it however you started it.)"
    exit 0
  fi

  { printf '%s\n%s\n' "$NEW" "$CRON_LINE"; $WITH_MARKET && printf '%s\n' "$MARKET_LINE"; } | sed '/^$/d' | crontab -
  echo "✅ Autonomy installed (cron). Nightly research at $AT → logs in reports/cron.log"
  $WITH_MARKET && echo "   market watch: every 3 min → reports/market_watch.log (needs BIRDEYE_API_KEY)"
  if $WITH_BOT; then
    echo
    echo "  ⚠️  cron can schedule the research, but NOT keep the bot alive across reboots."
    echo "     On this box, run the bot under a process you control, e.g.:"
    echo "       nohup ./target/release/solana-vntr-sniper >> reports/bot.log 2>&1 &"
    echo "     (A systemd box is strongly preferred for a truly always-on bot.)"
  fi
  echo "   run research now : ./scripts/daily_report.sh"
  exit 0
fi
