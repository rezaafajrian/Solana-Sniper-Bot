# Complete Setup Guide (0 → 9) — Enable Everything, No Guessing

Follow top to bottom. Golden rule: **dry run first → validate → only then live.**
Commands assume Ubuntu (VPS) or macOS. Run from the repo root unless told otherwise.

---

## API / KEY REFERENCE — what you need and where to get it

| What | Required? | Cost | Where to get it | Goes in `.env` as |
|---|---|---|---|---|
| **Solana RPC (websocket)** | **YES** | free tier ok | helius.dev · chainstack.com · quicknode.com | `RPC_WSS` |
| **Wallet private key** | **YES** | — | Phantom/Solflare "export private key" (base58) | `PRIVATE_KEY` |
| **Telegram bot** | strongly rec. | free | @BotFather (token) + @userinfobot (chat id) | `TELEGRAM_BOT_TOKEN`, `TELEGRAM_CHAT_ID` |
| **GMGN** | optional | free key | gmgn.ai → API/AI section | `GMGN_API_KEY` |
| **Birdeye** | optional | free tier | birdeye.so → Dashboard → API keys | `BIRDEYE_API_KEY` |
| **Helius** | optional | free tier | helius.dev → API keys + webhooks | `HELIUS_API_KEY` |
| **Yellowstone gRPC** | optional (live speed) | paid | helius.dev (LaserStream) / triton.one | `YELLOWSTONE_GRPC_HTTP`, `YELLOWSTONE_GRPC_TOKEN` |
| **Dexscreener** | auto | free, no key | — (used automatically) | — |

You can run the entire dry run with just the first two. Everything else adds capability.

---

## STEP 0 — Safety + a throwaway wallet

**0a. Rotate every key** that ever appeared in chat/logs (Helius, Dune, Vybe, GMGN,
Chainstack). Old ones are compromised — regenerate them in each provider's dashboard.

**0b. Make a THROWAWAY wallet for the dry run** (the bot needs *a* key to start, even in
paper mode — never put your real key on the box). Easiest way to get a base58 key:
- Install **Phantom** (or Solflare) → create a **new** wallet → Settings → Security →
  **Export Private Key** → copy the base58 string. **Do not fund it.** That string is your
  dry-run `PRIVATE_KEY`.

CLI alternative:
```bash
solana-keygen new --no-bip39-passphrase --outfile dryrun.json
python3 -c "import json,base58;print(base58.b58encode(bytes(json.load(open('dryrun.json')))).decode())"
# (pip install base58 if needed). The printed string is PRIVATE_KEY.
```

## STEP 1 — Install dependencies & build

**Ubuntu VPS:**
```bash
sudo apt update && sudo apt install -y build-essential pkg-config libssl-dev git curl python3
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
source "$HOME/.cargo/env"
# if the box has <4GB RAM, add swap so the build doesn't OOM:
sudo fallocate -l 4G /swapfile && sudo chmod 600 /swapfile && sudo mkswap /swapfile && sudo swapon /swapfile
```
**macOS:** `xcode-select --install` then install Rust with the same `rustup` line.

**Get the code & build:**
```bash
git clone <your repo url> Solana-Sniper-Bot && cd Solana-Sniper-Bot   # or: cd existing + git pull
git checkout claude/code-review-weaknesses-c2c5ny
cargo build --release      # produces target/release/solana-vntr-sniper
cargo test --release       # MUST print "20 passed" before you trust a run
```

## STEP 2 — Get a Solana RPC (websocket) endpoint  [REQUIRED]

The bot's data source. It must support **`blockSubscribe`** over websocket.
1. Go to **helius.dev** (or chainstack.com) → sign up → create a **Solana Mainnet** endpoint.
2. Copy the **WebSocket URL** (looks like `wss://mainnet.helius-rpc.com/?api-key=...` or
   `wss://...core.chainstack.com/<token>`).
3. That URL is `RPC_WSS` in STEP 3.

## STEP 3 — Configure `.env` (the bot core)
```bash
cp src/env.example .env
nano .env
```
Set these (dry-run values). Everything not listed has a sensible default — don't touch it:
```ini
MONITORING_MODE=momentum
MOMENTUM_DRY_RUN=true
MOMENTUM_LIVE_CONFIRM=false
MOMENTUM_FEED=ws
RPC_WSS=wss://<your-rpc-endpoint>          # from STEP 2
PRIVATE_KEY=<throwaway base58 key>          # from STEP 0b
MOMENTUM_DECISION_LOG=momentum_decisions    # REQUIRED — the learning layer reads this
MOMENTUM_STATUS_FILE=momentum_status.json
MOMENTUM_POSITIONS_FILE=momentum_positions.json
```
Lock the file so the key isn't world-readable:
```bash
chmod 600 .env
```
**Already ON by default (no action):** structure gate, creator self-buy discount, authority
honeypot check, anti-tamper program guard, co-buy clustering + bundler-cluster veto, exit
ladder (scale-out/trailing/leader-dump), circuit breaker, position persistence, zombie
reapers, liquidity caps, early-warning pump alerts, runner-DNA bridge, log secret-redaction.

**Optional — turn ON when ready** (default off):
```ini
MOMENTUM_SLOW_RUG_SOL=2.0        # cumulative-insider-dump exit (≈2× your leader-dump threshold)
```

## STEP 4 — Telegram alerts  [strongly recommended]
1. In Telegram, message **@BotFather** → `/newbot` → follow prompts → it gives you a
   **token** like `123456789:ABCdef...`.
2. Message **@userinfobot** → it replies with your numeric **chat id**.
3. In `.env`:
```ini
TELEGRAM_BOT_TOKEN=123456789:ABCdef...
TELEGRAM_CHAT_ID=987654321
```
This arms: bot-online ping, ⚡ pre-pump early-warnings, halt/margin-call/quarantine alerts,
manual-sell reconcile, and the daily research digest.

## STEP 5 — GMGN bundle/rug veto  [optional]
1. Go to **gmgn.ai** → API/AI section → generate an API key.
2. In `.env`:
```ini
GMGN_ENABLED=true
GMGN_API_KEY=<your gmgn key>
GMGN_MAX_BUNDLE_RATE=0.20        # default; rejects >20% bundled supply
```
Skip if you have no key — the feed-only anti-fake layers still run.

## STEP 6 — Start the bot & verify
```bash
cargo run --release
```
You should see, in order:
- `🔐 Security preflight OK` (or a `.env` permission warning to fix)
- `📡 Websocket feed connected` (or gRPC)
- `📲 Telegram alerts ARMED` (if you did STEP 4) + a Telegram "🟢 bot online" message
- `📒 Decision log ON … N pending outcomes restored`
- `🧬 Runner-DNA bridge armed` (0 patterns yet — expected)

Leave it running. If you see a flood of `🛑` rejects and almost no buys, the structure gate
is too tight — in `.env` lower `MOMENTUM_STRUCT_MIN_BUYERS` (e.g. 3) and/or
`MOMENTUM_STRUCT_MIN_GENUINE` (e.g. 0.45), then restart.

**Run it always-on (VPS):** see `docs/VPS_DEPLOYMENT.md` for the `systemd` unit
(`deploy/momentum-bot.service`) so it survives reboots and auto-restarts.

## STEP 7 — Dashboard on your phone
Second terminal, repo root:
```bash
./web/serve.sh          # serves http://localhost:8787/dashboard.html
```
Reach it from your phone WITHOUT exposing the port:
- **Tailscale (easiest):** install on the VPS and your phone → open
  `http://<tailscale-ip>:8787/dashboard.html`.
- **SSH tunnel:** on your laptop `ssh -L 8787:localhost:8787 user@<vps>` then browse
  `http://localhost:8787/dashboard.html`.

## STEP 8 — Research layer (daily, automated)
Optional richer inputs:
```bash
# tokens you KNOW ran (feeds viral_autopsy) — one mint per line:
echo "8Jx8AAHj86wbQgUTjGuj6GTTL5Ps3cqxKRTvpaJApump" >> viral_tokens.txt
```
```ini
# .env — free Birdeye key (birdeye.so) for holder/price-history depth:
BIRDEYE_API_KEY=<your birdeye key>
```
Schedule the daily pass (runs learn.py + runner_research + lifecycle + autopsy + smart-money
sync, all at idle priority so the bot is unaffected):
```bash
crontab -e
# add this line (adjust path):
0 9 * * * cd ~/Solana-Sniper-Bot && ./scripts/daily_report.sh >> reports/cron.log 2>&1
```
Or run by hand any time: `./scripts/daily_report.sh`. Needs ~2h+ of running and some
labeled outcomes before the reports have content.

## STEP 9a — Real-time smart-money watch  [optional, separate process]
Follow proven wallets market-wide via Helius webhooks.
1. **helius.dev** → API keys → copy your key.
2. You need a **publicly reachable URL** for the receiver (your VPS IP + an open port).
```ini
# .env:
HELIUS_API_KEY=<your helius key>
SMART_MONEY_WEBHOOK_URL=https://<your-vps-public-ip>:8899
```
```bash
sudo ufw allow 8899                                   # open the receiver port
python3 scripts/smart_money_watch.py --sync           # registers the webhook (daily report re-syncs)
python3 scripts/smart_money_watch.py --serve --port 8899   # the receiver — run under tmux/systemd
```
Pings Telegram the instant a tracked proven wallet buys anything.

## STEP 9b — GO LIVE  (only after the dry run validates the edge)
Pre-flight: edge positive across 2+ dry-run windows, `learn.py` shows labeled outcomes,
all keys rotated, a DEDICATED hot wallet funded with a SMALL amount.
```ini
# .env — replace the throwaway PRIVATE_KEY with your funded hot-wallet key, then:
MOMENTUM_DRY_RUN=false
MOMENTUM_LIVE_CONFIRM=true            # required, or it refuses to start live
MOMENTUM_MAX_WALLET_SOL=2             # tripwire: refuses to start if wallet holds more
MOMENTUM_PRESEND_SIMULATE=true        # honeypot/sell-revert check for the first cautious runs
MOMENTUM_FEED=grpc                    # for speed; also set the two lines below
YELLOWSTONE_GRPC_HTTP=<grpc endpoint>
YELLOWSTONE_GRPC_TOKEN=<grpc token>
```
```bash
chmod 600 .env
cargo run --release        # or restart the systemd service
```
Start tiny (0.02–0.05 SOL/trade), watch BUY_ACTUAL/SELL_ACTUAL slippage in the trade log,
and only scale once real fills match the sim.

---

## Verify checklist
- [ ] `cargo test --release` → 20 passed
- [ ] startup shows preflight + feed connected + (Telegram armed) + decision log
- [ ] Telegram received the "🟢 bot online" ping
- [ ] dashboard loads on your phone (tables scroll sideways, links tap through)
- [ ] after 2h+: `python3 scripts/learn.py` shows labeled outcomes > 0
- [ ] `cargo audit` clean · `.env` is `chmod 600` · keys rotated

## Troubleshooting
- **Exits instantly "missing PRIVATE_KEY"** → set `PRIVATE_KEY` (STEP 0b/3).
- **"Refusing to start LIVE…"** → you set live without `MOMENTUM_LIVE_CONFIRM=true`, or the
  wallet exceeds `MOMENTUM_MAX_WALLET_SOL` (that's the safety tripwire working).
- **Feed never connects** → wrong `RPC_WSS`, or the RPC doesn't support `blockSubscribe`.
- **No buys, lots of 🛑** → loosen the structure gate (STEP 6).
- **learn.py says 0 outcomes** → it labels ~2h after detection; let it run longer.
- **Telegram silent** → token/chat id wrong, or the bot must be started by you messaging it once.
