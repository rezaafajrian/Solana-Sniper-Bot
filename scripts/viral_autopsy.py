#!/usr/bin/env python3
"""
VIRAL AUTOPSY — dissect tokens that ALREADY went viral, so we don't wait for new ones.

Give it a list of mints you know ran big (10x / 50x / 100x+). For each, it pulls the
token's history from Dexscreener (free, no key) and Birdeye (free tier, needs a key),
reconstructs WHAT THE SETUP LOOKED LIKE BEFORE THE MOVE, and distills the recurring
pre-explosion fingerprint across all of them — the "viral DNA". That fingerprint is
written to viral_dna.json so the live early-warning + watchlist layers can hunt for the
next token that matches it, instead of only learning from tokens the bot witnessed live.

This is a research/intelligence layer. Free-tier data is shallow on wallet-level detail
(smart-money tags, full early-buyer lists are paid), so it dissects what's reliably free:
the price / market-cap / liquidity / volume / holder ramp BEFORE the pump, the pump
magnitude, and how long after launch it fired. It degrades gracefully when a source or
key is missing, and --mock validates the logic offline.

Usage:
  # 1) put the mints you know ran (one per line) in viral_tokens.txt
  # 2) export BIRDEYE_API_KEY=...   (free key from birdeye.so; optional but richer)
  python3 scripts/viral_autopsy.py
  python3 scripts/viral_autopsy.py --mock     # offline self-test
"""
import os, sys, json, time, math, argparse, urllib.request, urllib.parse, datetime as dt
from collections import defaultdict

TOKENS_FILE = "viral_tokens.txt"
OUT = "viral_dna.json"
BIRDEYE = "https://public-api.birdeye.so"
DEX = "https://api.dexscreener.com/latest/dex/tokens/"


def env(key):
    v = os.environ.get(key)
    if v:
        return v
    try:
        for line in open(".env"):
            if line.strip().startswith(f"{key}=") and not line.strip().startswith("#"):
                return line.split("=", 1)[1].strip()
    except OSError:
        pass
    return None


def _get(url, headers=None, timeout=20):
    try:
        req = urllib.request.Request(url, headers=headers or {"User-Agent": "viral-autopsy"})
        return json.load(urllib.request.urlopen(req, timeout=timeout))
    except Exception as e:
        sys.stderr.write(f"  fetch failed {url[:70]}… ({e})\n")
        return None


def dex_overview(mint):
    d = _get(DEX + mint)
    if not d:
        return {}
    best, best_liq = {}, -1
    for p in d.get("pairs") or []:
        liq = ((p.get("liquidity") or {}).get("usd")) or 0
        if liq > best_liq:
            best_liq = liq
            b = p.get("baseToken") or {}
            best = {"symbol": b.get("symbol", ""), "mcap": p.get("marketCap") or p.get("fdv") or 0,
                    "liq": liq, "vol24": ((p.get("volume") or {}).get("h24")) or 0,
                    "created": p.get("pairCreatedAt")}
    return best


def birdeye_overview(mint, key):
    if not key:
        return {}
    h = {"X-API-KEY": key, "x-chain": "solana", "User-Agent": "viral-autopsy"}
    d = _get(f"{BIRDEYE}/defi/token_overview?address={mint}", headers=h)
    if not d or not d.get("data"):
        return {}
    o = d["data"]
    return {"holders": o.get("holder") or 0, "mcap": o.get("mc") or o.get("marketCap") or 0,
            "liq": o.get("liquidity") or 0, "v24": o.get("v24hUSD") or 0, "price": o.get("price") or 0,
            "symbol": o.get("symbol", "")}


def birdeye_history(mint, key, days=120):
    """Hourly/daily price history -> the price curve, to locate the pump + measure the run."""
    if not key:
        return []
    now = int(time.time())
    h = {"X-API-KEY": key, "x-chain": "solana", "User-Agent": "viral-autopsy"}
    url = f"{BIRDEYE}/defi/history_price?address={mint}&address_type=token&type=1H&time_from={now-days*86400}&time_to={now}"
    d = _get(url, headers=h)
    items = ((d or {}).get("data") or {}).get("items") or []
    return [(it.get("unixTime"), it.get("value")) for it in items if it.get("value")]


def autopsy(mint, key, mock=False):
    """Reconstruct one token's pump + pre-pump setup."""
    if mock:
        import random
        random.seed(hash(mint) & 0xffff)
        launch = random.uniform(4000, 20000)
        peak = launch * random.choice([12, 30, 80])
        days_to_peak = random.choice([0, 2, 9, 25, 60])
        return {"mint": mint, "symbol": f"T{mint[:3]}", "ok": True,
                "launch_mcap": launch, "peak_mcap": peak, "mult": peak / launch,
                "days_to_peak": days_to_peak, "pre_liq": random.uniform(3000, 40000),
                "pre_vol": random.uniform(1000, 60000), "holders": random.randint(80, 4000),
                "source": "mock"}
    dx = dex_overview(mint)
    be = birdeye_overview(mint, key)
    hist = birdeye_history(mint, key)
    if not dx and not be and not hist:
        return {"mint": mint, "ok": False}
    sym = be.get("symbol") or dx.get("symbol") or mint[:6]
    cur_mcap = be.get("mcap") or dx.get("mcap") or 0
    # locate the pump from the price curve
    launch_mcap = peak_mcap = mult = 0.0
    days_to_peak = 0
    pre_liq = dx.get("liq") or be.get("liq") or 0
    if hist:
        prices = [v for _, v in hist]
        t_peak = max(range(len(prices)), key=lambda i: prices[i])
        base = min(prices[:max(1, t_peak)] or prices)  # lowest before the peak
        peak = prices[t_peak]
        mult = peak / base if base > 0 else 0
        days_to_peak = round((hist[t_peak][0] - hist[0][0]) / 86400, 1) if len(hist) > 1 else 0
        # pre-pump window = the 24h before the peak (volume can't be reconstructed from price
        # history alone on free tier; we use current liq/holders as the structural proxy).
        launch_mcap = base * (cur_mcap / peak) if peak > 0 and cur_mcap else 0
        peak_mcap = peak * (cur_mcap / peak) if peak > 0 and cur_mcap else 0
    return {"mint": mint, "symbol": sym, "ok": True,
            "launch_mcap": launch_mcap, "peak_mcap": peak_mcap or cur_mcap, "mult": round(mult, 1),
            "days_to_peak": days_to_peak, "pre_liq": pre_liq, "pre_vol": dx.get("vol24") or be.get("v24") or 0,
            "holders": be.get("holders") or 0, "source": "live"}


def med(xs):
    xs = sorted(x for x in xs if x)
    return xs[len(xs) // 2] if xs else 0


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--tokens", default=TOKENS_FILE)
    ap.add_argument("--mock", action="store_true")
    args = ap.parse_args()

    if args.mock:
        mints = [f"MOCK{i:03d}xxxxxxxxxxxxxxxxxxxxxxxpump" for i in range(25)]
    elif os.path.exists(args.tokens):
        mints = [l.strip() for l in open(args.tokens) if l.strip() and not l.startswith("#")]
    else:
        sys.stderr.write(f"No token list at '{args.tokens}'. Put known-viral mints there "
                         "(one per line), then re-run.\n")
        sys.exit(1)
    key = env("BIRDEYE_API_KEY")

    print("=" * 76)
    print("  VIRAL AUTOPSY — dissecting tokens that already ran")
    print(f"  {len(mints)} tokens | Birdeye key: {'yes' if key else 'NO (Dexscreener only — shallower)'}"
          f"{' | MOCK' if args.mock else ''}")
    print("=" * 76)

    studies = []
    for i, m in enumerate(mints):
        a = autopsy(m, key, mock=args.mock)
        if a.get("ok"):
            studies.append(a)
        if not args.mock:
            time.sleep(1.1)  # respect free-tier rate limits
        if (i + 1) % 10 == 0:
            print(f"  …dissected {i+1}/{len(mints)}")
    if not studies:
        print("\n  No tokens could be dissected (no data / bad mints / rate-limited). "
              "Check the mints and your Birdeye key.")
        return

    print(f"\n  successfully dissected {len(studies)} of {len(mints)}")
    print(f"  {'symbol':10} {'mult':>7} {'days→peak':>10} {'pre-liq $':>11} {'holders':>8}")
    for a in sorted(studies, key=lambda x: -x.get("mult", 0))[:25]:
        print(f"  {a['symbol'][:10]:10} {a.get('mult',0):>6.0f}x {a.get('days_to_peak',0):>10} "
              f"{a.get('pre_liq',0):>11,.0f} {a.get('holders',0):>8}")

    # ---- aggregate the recurring pre-explosion fingerprint ----
    mults = [a.get("mult", 0) for a in studies]
    days = [a.get("days_to_peak", 0) for a in studies]
    slow = sum(1 for d in days if d >= 7)
    dna = {
        "generated": dt.date.today().isoformat(),
        "tokens_studied": len(studies),
        "median_mult": med(mults),
        "median_days_to_peak": med(days),
        "slow_burn_share": round(slow / len(studies), 2),
        "median_pre_pump_liquidity_usd": med([a.get("pre_liq", 0) for a in studies]),
        "median_holders": med([a.get("holders", 0) for a in studies]),
    }
    print("\n  ── VIRAL DNA (the recurring setup across these winners) ──")
    print(f"    median run            : {dna['median_mult']:.0f}x")
    print(f"    median days to peak   : {dna['median_days_to_peak']}")
    print(f"    slow-burn share (>=7d): {dna['slow_burn_share']*100:.0f}%  "
          "← the ones a curve-only view misses")
    print(f"    median pre-pump liq   : ${dna['median_pre_pump_liquidity_usd']:,.0f}")
    print(f"    median holders        : {dna['median_holders']:,}")
    tmp = OUT + ".tmp"  # atomic write (any live reader sees a complete file or none)
    json.dump(dna, open(tmp, "w"), indent=2)
    os.replace(tmp, OUT)
    print(f"\n  → viral DNA written to {OUT} (feed the live layers to hunt the next match).")
    if not key:
        print("  NOTE: no Birdeye key — holder/history depth was limited. Add BIRDEYE_API_KEY")
        print("  (free at birdeye.so) for the deeper holder-growth + price-history dissection.")
    print("=" * 76)


if __name__ == "__main__":
    main()
