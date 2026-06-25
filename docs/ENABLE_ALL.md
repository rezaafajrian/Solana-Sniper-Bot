# Enable Everything — Ordered Setup Guide

Follow these in order. The golden rule: **dry run first, validate, then live.** Most
defenses are already ON by default — this is mostly keys, a few flags, and sequence.

---

## STEP 0 — Safety first (before anything else)
- [ ] **Rotate every key** that ever hit chat/logs (Helius, Dune, Vybe, GMGN, Chainstack RPC).
- [ ] For the **dry run**, generate a **throwaway empty wallet** — never your real key on the box.
- [ ] You only fund a real (small, dedicated) hot wallet at STEP 9, not before.

## STEP 1 — Get the code & build
```bash
cd ~/Solana-Sniper-Bot         # or /opt/solbot/... on the VPS
git pull
git checkout claude/code-review-weaknesses-c2c5ny
cargo build --release
cargo test --release           # must pass (20 tests) before you trust a run
```

## STEP 2 — Configure `.env` (the bot core)
```bash
cp src/env.example .env        # if you don't have one yet
nano .env
chmod 600 .env                 # owner-only — the bot warns if this is loose
```
Set these (dry-run values shown):
```
MONITORING_MODE=momentum
MOMENTUM_DRY_RUN=true            # paper trade — no real money
MOMENTUM_LIVE_CONFIRM=false      # stays false until STEP 9
MOMENTUM_FEED=ws                 # ws now; grpc later (STEP 9) for speed
RPC_WSS=<your websocket RPC>     # Chainstack/Helius ws endpoint (must support blockSubscribe)
PRIVATE_KEY=<THROWAWAY wallet>   # empty wallet for the dry run
MOMENTUM_DECISION_LOG=momentum_decisions   # REQUIRED — the whole learning layer reads this
MOMENTUM_STATUS_FILE=momentum_status.json  # for the dashboard
MOMENTUM_POSITIONS_FILE=momentum_positions.json   # crash recovery
```
Already ON by default (no action needed): structure gate, creator self-buy discount,
authority honeypot check, anti-tamper program guard, co-buy clustering + bundler-cluster
veto, momentum/alpha/KOL/convergence edges, exit ladder (scale-out + trailing + leader-dump),
circuit breaker, position persistence, zombie reapers, liquidity caps, early-warning pump
alerts, runner-DNA bridge, secret-redaction in logs.

Optional knobs you may want ON (default OFF — enable when ready):
```
MOMENTUM_SLOW_RUG_SOL=2.0        # cumulative-insider-dump exit (0=off); ~2x your leader-dump
MOMENTUM_MAX_WALLET_SOL=2        # SECURITY tripwire — refuse to start LIVE above this (set at STEP 9)
```

## STEP 3 — Telegram alerts + daily report
Get a bot token from **@BotFather** and your chat id from **@userinfobot**, then in `.env`:
```
TELEGRAM_BOT_TOKEN=123456:ABC...
TELEGRAM_CHAT_ID=<your numeric id>
```
This arms: bot-online ping, ⚡ pre-pump early-warnings, halt/margin-call/quarantine alerts,
manual-sell reconcile, and the daily research digest.

## STEP 4 — GMGN bundle/rug veto (recommended)
The funded-bundler-cluster detector. In `.env` (use the ROTATED key):
```
GMGN_ENABLED=true
GMGN_API_KEY=<rotated gmgn key>
GMGN_MAX_BUNDLE_RATE=0.20        # already the default; reject >20% bundled supply
```
Skip if you don't have a key — the feed-only anti-fake layers still run.

## STEP 5 — Start the bot & verify
```bash
cargo run --release
```
Confirm these lines appear at startup:
- `🔐 Security preflight OK` (or the `.env` permission warning)
- feed connected (ws/grpc)
- `📲 Telegram alerts ARMED`
- `📒 Decision log ON … N pending outcomes restored`
- `🧬 Runner-DNA bridge armed` (no patterns yet — that's expected)
Then **leave it running.** Watch for a flood of `🛑` rejects — if entries dry up, loosen
`MOMENTUM_STRUCT_MIN_BUYERS` / `MOMENTUM_STRUCT_MIN_GENUINE` (the structure gate is the
likeliest over-filter).

## STEP 6 — Dashboard on your phone
In a second terminal: `./web/serve.sh` → open `http://localhost:8787/dashboard.html`.
From your phone: use **Tailscale** (install on phone + box) or an SSH tunnel
(`ssh -L 8787:localhost:8787 user@vps`). Don't expose 8787 to the internet.

## STEP 7 — Research layer (daily, automated)
Optional inputs for deeper research:
```
# viral_tokens.txt  — one mint per line of tokens you KNOW ran (feeds viral_autopsy)
echo "8Jx8AAHj86wbQgUTjGuj6GTTL5Ps3cqxKRTvpaJApump" >> viral_tokens.txt
# .env: free Birdeye key for holder/history depth (lifecycle + autopsy)
BIRDEYE_API_KEY=<free key from birdeye.so>
```
Schedule the daily pass (runs learn.py + runner_research + lifecycle + autopsy + smart-money
sync, all at idle priority so the bot is unaffected). `crontab -e`:
```
0 9 * * * cd ~/Solana-Sniper-Bot && ./scripts/daily_report.sh >> reports/cron.log 2>&1
```
Or run it by hand any time: `./scripts/daily_report.sh`. (Needs ~2h+ of running and some
labeled outcomes before the reports have anything to say.)

## STEP 8 — Real-time smart-money watch (optional, separate process)
Follow proven wallets market-wide via Helius. In `.env`:
```
HELIUS_API_KEY=<free key from helius.dev>
SMART_MONEY_WEBHOOK_URL=https://<your-vps-public>:8899
```
```bash
python3 scripts/smart_money_watch.py --sync          # register the webhook (daily report re-syncs)
python3 scripts/smart_money_watch.py --serve --port 8899   # the receiver (open 8899 in ufw)
```
Run `--serve` under its own systemd unit / tmux (it's long-running, like the bot).

## STEP 9 — Going LIVE (only after the dry run validates the edge)
Pre-flight: edge looks real across a couple of dry-run windows, `learn.py` has labeled
outcomes, keys rotated. Then:
```
# fund a DEDICATED hot wallet with a small amount, put its key in .env, then:
MOMENTUM_DRY_RUN=false
MOMENTUM_LIVE_CONFIRM=true        # required, or it refuses to start live
MOMENTUM_MAX_WALLET_SOL=2         # tripwire — refuses to start if the wallet holds more
MOMENTUM_PRESEND_SIMULATE=true    # honeypot/sell-revert check for the first cautious runs
MOMENTUM_FEED=grpc                # + YELLOWSTONE_GRPC_HTTP / _TOKEN for the fast feed
chmod 600 .env
```
Start tiny (0.02–0.05 SOL/trade), watch the BUY_ACTUAL/SELL_ACTUAL slippage, and only
scale once live fills match the sim.

---

## Quick verify checklist
- [ ] `cargo test --release` passes
- [ ] startup shows security preflight + feed + Telegram armed + decision log
- [ ] dashboard reachable from phone (tables scroll, links tap through)
- [ ] Telegram gets the bot-online ping
- [ ] after 2h+: `python3 scripts/learn.py` shows labeled outcomes (not 0)
- [ ] `cargo audit` is clean (deps), `.env` is `chmod 600`

## Operating model (so the two systems don't fight)
The **bot** is the always-on Rust service; the **research** is cron'd Python at idle
priority. They share files one-way (atomic hand-off), and research never touches live
trade/risk logic — see `docs/VPS_DEPLOYMENT.md` §10b.
