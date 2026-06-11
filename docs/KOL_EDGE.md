# KOL Edge — sourcing wallets from Dune (API) and keeping the list live

Your edge is a curated, evolving list of wallets that buy winners early. The bot
treats a KOL buy as a leading entry signal; momentum / LP / anti-fake / rug
checks confirm; the insider-dump exit protects. Here's the full loop.

## 1. Get a Dune API key + query IDs
- API key: dune.com → Settings → API.
- Query ID: on the dashboard, click a table's title to open its query. The URL
  is `dune.com/queries/<QUERY_ID>/...` — use that number. (Dashboards aren't
  directly queryable; you pull the individual queries behind them.)

## 2. Pull wallets from the Dune API → kol_wallets.txt
```bash
export DUNE_API_KEY=xxxxx
# one or more query ids; weight by a pnl column if present:
python3 scripts/dune_api_to_kol.py <QUERY_ID_1> <QUERY_ID_2> --weight-col pnl > kol_wallets.txt
```
- Auto-detects the wallet column, validates Solana addresses, de-dups, and
  normalizes any pnl column into a 1.0–3.0 weight multiplier.
- No API key? Use the CSV path instead: download the table CSV from Dune and run
  `scripts/dune_to_kol.py file.csv > kol_wallets.txt`.

## 3. Keep it live (optional)
Cron the fetch and let the bot hot-reload the file:
```bash
# crontab: refresh the KOL list every 30 min
*/30 * * * * cd /path/to/Solana-Sniper-Bot && DUNE_API_KEY=xxxxx \
  python3 scripts/dune_api_to_kol.py <QUERY_ID> --weight-col pnl > kol_wallets.txt
```
In `.env`: `MOMENTUM_KOL_RELOAD_SECS=300` so the bot reloads the file every 5 min.

## 4. Turn it on
```env
MOMENTUM_KOL_ENABLED=true
MOMENTUM_KOL_FILE=kol_wallets.txt
MOMENTUM_KOL_REQUIRE=false   # true = only enter tokens a KOL bought
MOMENTUM_KOL_BOOST=40
```

## 5. Prune — this is the actual edge
After a run, `python3 scripts/analyze_momentum.py momentum_trades.csv` prints
**realized PnL per KOL**. Keep the green wallets, cut the red ones. Public Dune
lists are a starting seed (everyone copies them); your narrowed, self-verified
list — and the private alt-wallets you trace from the proven ones — is what's
actually defensible.

> Reminder: this raises selectivity, it doesn't guarantee profit. Validate in dry
> run, A/B with `compare_momentum.py`, and only scale on proven results.
