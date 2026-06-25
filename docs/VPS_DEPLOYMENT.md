# VPS Deployment & Hardening Runbook

How to run the bot unattended on a VPS, safely. Follow the sections in order. The
golden rule: **the bot code can't protect a compromised host or a leaked key — host
hygiene and a small hot wallet are what cap your losses.**

> ⚠️ Do NOT skip Section 0. Going live on an un-rotated key or your main wallet is the
> single most expensive mistake available, and no amount of code prevents it.

---

## 0. Pre-flight (DO THIS FIRST — only you can)

- [ ] **Rotate every key that ever touched chat/logs**: Helius, Dune, Vybe, GMGN, the
      Chainstack RPC. They are burned. Generate fresh ones.
- [ ] **Create a DEDICATED hot wallet.** Never your main. Fund it with only what you are
      willing to lose entirely — the bot signs automatically on a network-connected box,
      so assume the key can eventually be exposed and size the blast radius accordingly.
- [ ] Set the safety tripwires in `.env`:
      ```
      MOMENTUM_DRY_RUN=true            # stay in paper mode until you've validated on the VPS
      MOMENTUM_LIVE_CONFIRM=false      # must be flipped true to ever trade real money
      MOMENTUM_MAX_WALLET_SOL=2        # refuse to start live if the wallet holds > this (wrong-wallet guard)
      GMGN_ENABLED=true                # arm the bundle veto (needs the rotated GMGN key)
      GMGN_API_KEY=<rotated key>
      ```
- [ ] Confirm the risk controls are ON for live (these ARE your kill switch when unattended —
      see Section 7): circuit breaker, bankruptcy floor, slippage caps, liquidity caps.

---

## 1. Provision & harden the host

Pick a VPS in a region close to your RPC/validator (latency matters for fills).

```bash
# As root on the fresh box:
adduser --disabled-password --gecos "" solbot     # dedicated non-root service user
mkdir -p /opt/solbot && chown solbot:solbot /opt/solbot
```

**SSH hardening** (`/etc/ssh/sshd_config`):
```
PermitRootLogin no
PasswordAuthentication no        # key-only
```
```bash
systemctl restart ssh
apt-get update && apt-get install -y fail2ban   # throttle SSH brute-force
```

**Firewall** — default-deny inbound, allow only SSH. Outbound stays open (the bot needs
to reach the RPC + landing endpoints). The dashboard is reached via SSH tunnel, NOT an
open port.
```bash
apt-get install -y ufw
ufw default deny incoming
ufw default allow outgoing
ufw allow OpenSSH
ufw enable
```

## 2. Install build deps

```bash
apt-get install -y build-essential pkg-config libssl-dev git curl
# Rust (as the solbot user):
su - solbot -c 'curl --proto "=https" --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y'
```

## 3. Deploy the bot

```bash
su - solbot
cd /opt/solbot
git clone <your repo url> Solana-Sniper-Bot
cd Solana-Sniper-Bot
git checkout claude/code-review-weaknesses-c2c5ny   # or your release branch
~/.cargo/bin/cargo build --release                  # builds target/release/solana-vntr-sniper
```

**Create `.env`** in the repo root (this dir is what `dotenv()` reads, and where runtime
artifacts are written). Then lock it down:
```bash
cp src/env.example .env
nano .env                       # fill in RPC, the ROTATED keys, PRIVATE_KEY (hot wallet)
chmod 600 .env                  # owner-only — the bot warns at startup if this is loose
```

Run once interactively to confirm it starts clean (dry run), then Ctrl-C:
```bash
~/.cargo/bin/cargo run --release
# look for: security preflight OK, feed connected, "Decision log ON … pending outcomes restored"
```

## 4. Run it as a service (always-on + auto-restart)

A ready unit is in `deploy/momentum-bot.service`. It runs as `solbot`, restarts on crash
(crash recovery re-adopts open positions), and is sandboxed (read-only filesystem except
the repo dir, no new privileges, memory cap).

```bash
# As root — edit paths/user in the file if you didn't use /opt/solbot + solbot:
cp /opt/solbot/Solana-Sniper-Bot/deploy/momentum-bot.service /etc/systemd/system/
systemctl daemon-reload
systemctl enable --now momentum-bot
systemctl status momentum-bot
```

## 5. Dashboard — via SSH tunnel only (never expose the port)

Do NOT open 8787 to the internet. Run the dashboard server on the VPS bound to localhost
and tunnel to it from your laptop:
```bash
# On the VPS (as solbot, in the repo dir):
./web/serve.sh                  # serves localhost:8787

# On your laptop:
ssh -L 8787:localhost:8787 solbot@<vps-ip>
# then open http://localhost:8787/dashboard.html in your browser
```

---

## 6. Operations

| Task | Command |
|---|---|
| View live logs | `journalctl -u momentum-bot -f` |
| Stop the bot | `sudo systemctl stop momentum-bot` |
| Start / restart | `sudo systemctl start\|restart momentum-bot` |
| Status | `systemctl status momentum-bot` |
| Update to new code | see below |

**Update flow** (open positions survive the restart via crash recovery):
```bash
sudo systemctl stop momentum-bot
su - solbot -c 'cd /opt/solbot/Solana-Sniper-Bot && git pull && ~/.cargo/bin/cargo build --release && cargo test --release'
sudo systemctl start momentum-bot
```
Always let `cargo test --release` pass before restarting — it guards the money/security logic.

## 7. The safety model (your kill switch when unattended)

An always-on bot can't be babysat, so the **risk controls are the kill switch**. Keep these
ON in live; they're what stop it quietly bleeding:
- **Circuit breaker** — daily loss limit + consecutive-loss cap → halts trading.
- **Bankruptcy floor** — margin-call when the account blows → stops new entries.
- **Slippage caps** (buy tight / sell loose) and **liquidity-aware sizing caps**.
- **Anti-rug stack** — honeypot/authority check, bundle veto, creator-buy discount,
  slow-rug + leader-dump exits.
- **Crash-loop guard** — the systemd unit stops restarting after 10 crashes in 5 min.

## 8. Emergency procedures

- **Stop everything now:** `sudo systemctl stop momentum-bot` (open positions persist to
  `momentum_positions.json` and resume on next start).
- **Flatten manually:** stop the service, then sell the wallet's token holdings from a
  wallet UI (Phantom/Solflare) or `spl-token`. The bot does not currently expose a
  one-command "flatten all".
- **What the halt reasons mean** (in the logs / dashboard):
  - `🛑 HALTED` — circuit breaker tripped (daily loss / loss streak). Resets at the day
    boundary, or restart to clear.
  - `margin call` — bankruptcy floor hit; no new entries for the session.
  - `quarantined` — a position failed to sell too many times (likely illiquid/honeypot);
    surfaced for manual exit.
- **Suspected key compromise:** stop the service, move funds out of the hot wallet
  immediately, rotate the key, investigate the host.

## 9. Remote alerting (Telegram) — set this up so you can walk away

Push alerts fire on: bot online, circuit-breaker halt, margin call, token quarantine, and
manual/external-sell reconcile. Arm it in `.env`:
```
TELEGRAM_BOT_TOKEN=<from @BotFather>
TELEGRAM_CHAT_ID=<your id from @userinfobot>
```
No-op if unset. You'll get a "🟢 bot online" ping at startup confirming alerts are wired.
If you sell a position yourself (from your wallet or a Telegram trading bot), the bot
detects the token left the wallet, reconciles it (frees the slot), keeps running, and
pings you — it does NOT crash or get stuck.

- **gRPC feed** — for lowest latency set `MOMENTUM_FEED=grpc` + a Yellowstone endpoint
  (the default; the ws feed is the no-gRPC fallback).

## 10b. Isolation: research layer vs. the sniper bot

The two systems are deliberately decoupled so neither degrades the other:
- **Separate processes.** The sniper bot is the long-running Rust service (systemd). The
  research/analysis is short-lived Python run by cron (`daily_report.sh`) — a different
  process entirely. They never share a runtime.
- **CPU/IO isolation.** `daily_report.sh` runs every script at `nice -n 19` + `ionice -c3`
  (idle), so the latency-sensitive bot always wins scheduling — the daily analysis burst
  can't steal cycles or disk from live trading.
- **One-way, atomic data hand-off.** The analysis only *reads* the bot's data (decision/
  trade/outcome/wallet CSVs). The single file the bot reads back — `runner_patterns.json`
  (the research→watchlist bridge) — is written atomically (temp + rename), and the bot's
  reader fails safe (a bad/partial parse keeps the previous patterns, never crashes).
- **Decision-logic isolation.** The research layer NEVER touches the bot's live entry/exit/
  risk logic. Its only influence is advisory: it writes the validated-pattern file the
  watchlist *scores* against — and even that only affects "Ones to Watch" (alert-only by
  default), never the safety controls, sizing, or exits.

For *maximum* isolation you can run the analysis on a different box (rsync the bot's CSVs
to it nightly) — but on one VPS the priority + atomic-handoff setup above is enough that
the bot is unaffected.

## 10. Daily self-learning report (every 24h)

`scripts/daily_report.sh` runs `learn.py` over all accumulated data, saves a timestamped
report to `reports/learn_<date>.txt`, and pushes the **executive synthesis** (strengths,
weaknesses, risk factors, hidden patterns, missed opportunities, and hypotheses to test)
to Telegram. Schedule it every 24h with cron (as the `solbot` user — `crontab -e`):
```
0 9 * * * cd /opt/solbot/Solana-Sniper-Bot && ./scripts/daily_report.sh >> reports/cron.log 2>&1
```
Run it any time by hand: `./scripts/daily_report.sh`. (Telegram push needs
`TELEGRAM_BOT_TOKEN` / `TELEGRAM_CHAT_ID` in `.env`; without them it just saves the file.)
The report is most useful once outcomes have matured — give the bot a couple of days of
continuous running so there's enough labeled data for the out-of-sample validation.
