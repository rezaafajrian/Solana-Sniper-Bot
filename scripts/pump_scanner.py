#!/usr/bin/env python3
"""
Whole-market pump scanner + autopsy (Birdeye).

The sniper only sees fresh pump.fun launches. This scans the ENTIRE Solana
memecoin market for what's pumping right now, logs each mover's fingerprint over
time, and (in analyze mode) reverse-engineers what the biggest pumpers had in
common early — so you can learn to ride the next one.

It is intentionally SEPARATE from the bot: a periodic poller, not a hot path.
Run it on the VPS alongside the bot.

Setup:
  export BIRDEYE_API_KEY=your_key            # birdeye.so -> dashboard -> API key
  python3 scripts/pump_scanner.py scan                 # one scan, append observations
  python3 scripts/pump_scanner.py scan --loop 300      # scan every 300s forever
  python3 scripts/pump_scanner.py analyze              # autopsy: what the pumpers shared
  python3 scripts/pump_scanner.py scan --mock          # offline demo (no API/key needed)

Data:
  pump_scanner_observations.csv   one row per (token, scan time) with its metrics.
                                  The same token across rows = its trajectory; the
                                  analyzer reconstructs each token's max pump from
                                  first-seen and fingerprints the big winners.
"""

import argparse, csv, json, os, sys, time, urllib.request, urllib.parse
from collections import defaultdict
from datetime import datetime, timezone

BIRDEYE_BASE = "https://public-api.birdeye.so"
OBS_PATH = "pump_scanner_observations.csv"
FIELDS = ["ts", "address", "symbol", "price", "mc", "liquidity", "vol24h",
          "price1h_pct", "price24h_pct", "holders", "trades24h", "rank"]


def _num(v):
    try:
        return float(v)
    except (ValueError, TypeError):
        return ""


def birdeye_trending(api_key, limit=50):
    """Top trending Solana tokens (the 'what's pumping' feed)."""
    url = f"{BIRDEYE_BASE}/defi/token_trending?sort_by=rank&sort_type=asc&offset=0&limit={limit}"
    req = urllib.request.Request(url, headers={
        "X-API-KEY": api_key, "x-chain": "solana", "accept": "application/json",
    })
    with urllib.request.urlopen(req, timeout=15) as r:
        return json.loads(r.read().decode())


def extract_tokens(payload):
    """Defensively pull the token list from Birdeye's response (shape varies)."""
    data = payload.get("data", payload) if isinstance(payload, dict) else {}
    toks = data.get("tokens") or data.get("items") or data.get("updateUnixTime") and [] or []
    if not toks and isinstance(data, list):
        toks = data
    return toks or []


def row_from_token(t):
    g = lambda *ks: next((t[k] for k in ks if k in t and t[k] is not None), "")
    return {
        "ts": datetime.now(timezone.utc).isoformat(),
        "address": g("address", "mint"),
        "symbol": g("symbol"),
        "price": _num(g("price")),
        "mc": _num(g("marketcap", "mc", "fdv")),
        "liquidity": _num(g("liquidity")),
        "vol24h": _num(g("volume24hUSD", "v24hUSD", "volume24h")),
        "price1h_pct": _num(g("price1hChangePercent", "priceChange1hPercent")),
        "price24h_pct": _num(g("price24hChangePercent", "priceChange24hPercent")),
        "holders": _num(g("holder", "holders")),
        "trades24h": _num(g("trade24h", "trades24h")),
        "rank": _num(g("rank")),
    }


def append_rows(rows, path):
    need_header = not os.path.exists(path)
    with open(path, "a", newline="") as f:
        w = csv.DictWriter(f, fieldnames=FIELDS)
        if need_header:
            w.writeheader()
        for r in rows:
            w.writerow(r)


MOCK = {"data": {"tokens": [
    {"address": "So1aXXX...pump", "symbol": "WIF2", "price": 0.0042, "marketcap": 820000,
     "liquidity": 95000, "volume24hUSD": 1900000, "price1hChangePercent": 31.2,
     "price24hChangePercent": 240.0, "holder": 5400, "trade24h": 21000, "rank": 1},
    {"address": "Boop...pump", "symbol": "BOOP", "price": 0.0009, "marketcap": 310000,
     "liquidity": 41000, "volume24hUSD": 450000, "price1hChangePercent": 8.1,
     "price24hChangePercent": 55.0, "holder": 1800, "trade24h": 6400, "rank": 7},
    {"address": "Rug...pump", "symbol": "RUGZ", "price": 0.00001, "marketcap": 60000,
     "liquidity": 5000, "volume24hUSD": 90000, "price1hChangePercent": -22.0,
     "price24hChangePercent": -40.0, "holder": 320, "trade24h": 1500, "rank": 33},
]}}


def do_scan(args):
    if args.mock:
        payload = MOCK
    else:
        key = args.api_key or os.environ.get("BIRDEYE_API_KEY", "")
        if not key:
            sys.stderr.write("No BIRDEYE_API_KEY (env or --api-key). Use --mock to test offline.\n")
            sys.exit(1)
        try:
            payload = birdeye_trending(key, args.limit)
        except Exception as e:
            sys.stderr.write(f"Birdeye request failed: {e}\n")
            sys.exit(2)
    toks = extract_tokens(payload)
    rows = []
    for t in toks:
        r = row_from_token(t)
        if not r["address"]:
            continue
        # Only log genuine movers with real liquidity (skip dust/noise).
        if r["liquidity"] != "" and r["liquidity"] < args.min_liq:
            continue
        if r["price24h_pct"] != "" and r["price24h_pct"] < args.min_change and \
           (r["price1h_pct"] == "" or r["price1h_pct"] < args.min_change):
            continue
        rows.append(r)
    append_rows(rows, args.out)
    sys.stderr.write(f"[{datetime.now(timezone.utc).strftime('%H:%M:%S')}] "
                     f"scanned {len(toks)} trending, logged {len(rows)} movers -> {args.out}\n")
    return len(rows)


def do_analyze(args):
    if not os.path.exists(args.out):
        sys.stderr.write(f"No observations at '{args.out}'. Run 'scan' first.\n")
        sys.exit(1)
    # Reconstruct each token's trajectory: first-seen metrics + max price reached.
    first, last, peak = {}, {}, {}
    with open(args.out, newline="") as f:
        for r in csv.DictReader(f):
            a = r["address"]
            if not a:
                continue
            p = _num(r.get("price"))
            if a not in first:
                first[a] = r
            last[a] = r
            if isinstance(p, float):
                peak[a] = max(peak.get(a, p), p)

    rows = []
    for a, fr in first.items():
        p0 = _num(fr.get("price"))
        if not isinstance(p0, float) or p0 <= 0:
            continue
        pk = peak.get(a, p0)
        x_from_first = pk / p0  # multiple from first observation
        rows.append((x_from_first, a, fr))
    if not rows:
        print("No usable price trajectories yet — keep scanning over time.")
        return

    rows.sort(reverse=True)
    print("=" * 70)
    print(f"  PUMP AUTOPSY  ({len(first)} tokens tracked from {args.out})")
    print("=" * 70)
    print("\nTop movers since first seen:")
    print(f"  {'symbol':10} {'x-from-first':>12} {'mc@first':>12} {'liq@first':>11}")
    for x, a, fr in rows[:15]:
        print(f"  {(fr.get('symbol') or a[:8]):10} {x:>11.2f}x {_num(fr.get('mc')) or 0:>12,.0f} {_num(fr.get('liquidity')) or 0:>11,.0f}")

    # Fingerprint: what did the big pumpers (>=2x) look like at FIRST sighting vs rest?
    big = [fr for x, a, fr in rows if x >= args.pump_x]
    rest = [fr for x, a, fr in rows if x < args.pump_x]
    print(f"\nFingerprint of >= {args.pump_x}x pumpers ({len(big)}) vs the rest ({len(rest)}), at first sighting:")
    def avg(group, k):
        vals = [_num(r.get(k)) for r in group]
        vals = [v for v in vals if isinstance(v, float)]
        return sum(vals) / len(vals) if vals else float("nan")
    print(f"  {'metric':16} {'pumpers':>14} {'others':>14}")
    for k in ["mc", "liquidity", "vol24h", "price1h_pct", "price24h_pct", "holders", "trades24h"]:
        bm, rm = avg(big, k), avg(rest, k)
        print(f"  {k:16} {bm:>14,.2f} {rm:>14,.2f}")
    print("\n  -> features where pumpers differ most are your early 'next pump' signals.")
    print("     This sharpens as you accumulate days of scans. Verify before trusting.")


def main():
    ap = argparse.ArgumentParser(description="Whole-market pump scanner + autopsy (Birdeye)")
    sub = ap.add_subparsers(dest="cmd", required=True)

    s = sub.add_parser("scan", help="poll Birdeye top movers and log them")
    s.add_argument("--api-key", default="")
    s.add_argument("--limit", type=int, default=50)
    s.add_argument("--min-liq", type=float, default=10000.0, help="skip tokens below this liquidity (USD)")
    s.add_argument("--min-change", type=float, default=20.0, help="log tokens up at least this % (1h or 24h)")
    s.add_argument("--loop", type=int, default=0, help="scan every N seconds (0 = once)")
    s.add_argument("--mock", action="store_true", help="use bundled sample data (offline test)")
    s.add_argument("--out", default=OBS_PATH)

    a = sub.add_parser("analyze", help="autopsy: what the biggest pumpers had in common")
    a.add_argument("--pump-x", type=float, default=2.0, help="x-from-first to count as a 'pumper'")
    a.add_argument("--out", default=OBS_PATH)

    args = ap.parse_args()
    if args.cmd == "scan":
        if args.loop > 0:
            sys.stderr.write(f"Scanning every {args.loop}s — Ctrl-C to stop.\n")
            while True:
                do_scan(args)
                time.sleep(args.loop)
        else:
            do_scan(args)
    elif args.cmd == "analyze":
        do_analyze(args)


if __name__ == "__main__":
    main()
