# Live Dashboard

A real-time web UI for the momentum bot: session/today PnL, open positions with
live PnL bars, simulated buys/sells streaming in a feed, KOL hits, capital usage,
and the circuit-breaker state. Dark, glowing, auto-refreshing every 2s.

## How it works
The bot writes a snapshot to `momentum_status.json` (repo root) every ~3 seconds
(`MOMENTUM_STATUS_FILE`). The dashboard (`web/dashboard.html`) polls that file and
renders it. No build step, no dependencies beyond Python (for a static server).

## Run it
While the bot is running (dry run or live), in a second terminal:

```bash
cd ~/Solana-Sniper-Bot
./web/serve.sh                 # serves on http://localhost:8787
```
Then open **http://localhost:8787/dashboard.html**.

- If the bot hasn't produced `momentum_status.json` yet, the server shows the
  bundled **sample** so you can see the layout immediately.
- Once the bot is live, the dashboard switches to real data automatically.

### Just want to preview the look (no bot)?
```bash
cd ~/Solana-Sniper-Bot/web
cp momentum_status.sample.json momentum_status.json
python3 -m http.server 8787
# open http://localhost:8787/dashboard.html
```

## What you see
- **Hero**: session realized PnL (big, green/red glow), today's PnL, buy/sell
  counts, loss streak; open-position count + capital-deployed bar; size/mode and
  KOL mode.
- **Tiles**: tokens tracked, KOL wallets loaded, KOLs hot right now, GMGN watch,
  total buys/sells.
- **Open Positions**: one row per held token — shortened mint, entry→current
  market cap, live PnL% with a colored bar, % still held, scale-out rungs hit,
  peak PnL, age, and a ★ KOL badge if a KOL triggered the entry. Rows being sold
  glow red.
- **Trade Feed**: newest-first stream of BUY / SCALE / EXIT events with the
  reason and realized SOL.

A red **HALTED** badge appears if the circuit breaker trips.

> It's read-only — the dashboard only displays the bot's state, it never places
> or changes trades.
