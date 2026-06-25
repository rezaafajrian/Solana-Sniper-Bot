#!/usr/bin/env python3
"""
LIFECYCLE TRACKER — monitor every token across its WHOLE life, not just the bonding curve.

The trading bot only sees tokens on the pump.fun curve and forgets them at graduation.
But the biggest winners often go viral WEEKS or MONTHS after launch, long after they've
left the curve. This layer keeps watching them: it maintains a registry of every token
the bot ever saw, polls each one's live market state (mcap / volume / liquidity) via
Dexscreener — including long after graduation — and records the full time-series so we can
study what eventually turns a quiet launch into a major winner.

It is a research/intelligence layer, NOT a trade signal: a token that goes viral months
later can't be latency-sniped, it can only be understood (and, if you choose, accumulated
and held — a different strategy). Its job is to find and dissect the slow-burn winners.

Data source: Dexscreener (free, no key, batched up to 30 mints/call). Post-graduation
prices live on Raydium, which the pump.fun feed can't see — this is the bridge to them.

Usage:
  python3 scripts/lifecycle_tracker.py            # seed from the decision log + poll + report
  python3 scripts/lifecycle_tracker.py --mock     # offline self-test (no network)
  python3 scripts/lifecycle_tracker.py --report-only   # just the report from stored data
"""
import csv, os, sys, json, time, math, argparse, datetime as dt, urllib.request
from collections import defaultdict

REGISTRY = "token_registry.json"     # {mint: {...lifecycle state...}}
LIFECYCLE_CSV = "token_lifecycle.csv"  # append-only time-series snapshots
DEX_BATCH = 30                        # Dexscreener tokens-per-request cap
VIRAL_MULT = 10.0                     # peak / launch mcap that counts as "went viral"


def today():
    return dt.date.today().isoformat()


def load_registry():
    if os.path.exists(REGISTRY):
        try:
            return json.load(open(REGISTRY))
        except Exception:
            return {}
    return {}


def save_registry(reg):
    tmp = REGISTRY + ".tmp"
    json.dump(reg, open(tmp, "w"), indent=1)
    os.replace(tmp, REGISTRY)


def seed_from_decisions(reg, base="momentum_decisions.csv"):
    """Register every mint the bot evaluated, with its launch (detection) mcap."""
    if not os.path.exists(base):
        return 0
    added = 0
    for r in csv.DictReader(open(base, newline="")):
        mint = r.get("ca")
        if not mint or mint in reg:
            continue
        try:
            launch_mc = float(r.get("marketcap", "") or "0")
        except ValueError:
            launch_mc = 0.0
        reg[mint] = {
            "first_seen": (r.get("timestamp", "") or today())[:10],
            "launch_mcap": launch_mc,        # SOL mcap at detection (bonding curve)
            "symbol": "", "peak_mcap": 0.0, "peak_date": "", "last_mcap": 0.0,
            "last_seen": "", "snapshots": 0, "graduated": False, "viral": False,
        }
        added += 1
    return added


def dex_fetch(mints):
    """Query Dexscreener for a batch of mints. Returns {mint: {mcap_usd, liq, vol24, sym, price}}."""
    out = {}
    url = "https://api.dexscreener.com/latest/dex/tokens/" + ",".join(mints)
    try:
        req = urllib.request.Request(url, headers={"User-Agent": "lifecycle-tracker"})
        data = json.load(urllib.request.urlopen(req, timeout=20))
    except Exception as e:
        sys.stderr.write(f"  dexscreener fetch failed ({e}); skipping batch\n")
        return out
    for p in data.get("pairs") or []:
        base = p.get("baseToken") or {}
        mint = base.get("address")
        if not mint:
            continue
        liq = ((p.get("liquidity") or {}).get("usd")) or 0.0
        prev = out.get(mint)
        # keep the deepest-liquidity pair for the token (the real market)
        if prev and prev["liq"] >= liq:
            continue
        out[mint] = {
            "mcap_usd": p.get("marketCap") or p.get("fdv") or 0.0,
            "liq": liq,
            "vol24": ((p.get("volume") or {}).get("h24")) or 0.0,
            "sym": base.get("symbol") or "",
            "price": p.get("priceUsd") or "",
        }
    return out


def mock_market(mints):
    """Offline synthetic market state, so the logic is testable without network."""
    import random
    random.seed(len(mints))
    out = {}
    for i, m in enumerate(mints):
        late_viral = (i % 7 == 0)  # some tokens bloom big
        mc = random.uniform(5000, 40000) * (random.choice([20, 50]) if late_viral else 1)
        out[m] = {"mcap_usd": mc, "liq": random.uniform(2000, 80000),
                  "vol24": random.uniform(0, mc), "sym": f"TKN{i}", "price": "0.0001"}
    return out


def poll(reg, mock=False, max_tokens=600):
    """Poll current market state for tracked tokens; update registry + append time-series."""
    # Prioritise: always re-check tokens that ever showed life; sample the long tail.
    alive = [m for m, r in reg.items() if r["last_mcap"] > 0 or r["snapshots"] < 2]
    rest = [m for m in reg if m not in set(alive)]
    targets = (alive + rest)[:max_tokens]
    if not targets:
        return 0
    fetched = {}
    fetcher = mock_market if mock else dex_fetch
    for i in range(0, len(targets), DEX_BATCH):
        batch = targets[i:i + DEX_BATCH]
        fetched.update(fetcher(batch))
        if not mock:
            time.sleep(0.4)  # be polite to the free API
    new_header = not os.path.exists(LIFECYCLE_CSV)
    f = open(LIFECYCLE_CSV, "a", newline="")
    wr = csv.writer(f)
    if new_header:
        wr.writerow(["date", "mint", "symbol", "mcap_usd", "liquidity_usd", "vol24_usd", "price_usd"])
    d = today()
    updated = 0
    for mint, mk in fetched.items():
        r = reg.get(mint)
        if not r:
            continue
        mc = float(mk["mcap_usd"] or 0)
        r["symbol"] = mk["sym"] or r["symbol"]
        r["last_mcap"] = mc
        r["last_seen"] = d
        r["snapshots"] += 1
        r["graduated"] = True  # has a DEX pair => migrated off the curve
        if mc > r["peak_mcap"]:
            r["peak_mcap"] = mc
            r["peak_date"] = d
        # viral = big expansion vs the launch mcap (bonding curve, in SOL — convert roughly
        # is unnecessary; we use the USD peak vs a USD-equivalent floor heuristic).
        if r["peak_mcap"] >= 50000 and r["peak_mcap"] / max(1.0, r.get("launch_usd_floor", 3000)) >= VIRAL_MULT:
            r["viral"] = True
        wr.writerow([d, mint, r["symbol"], f"{mc:.0f}", f"{mk['liq']:.0f}", f"{mk['vol24']:.0f}", mk["price"]])
        updated += 1
    f.close()
    return updated


def days_between(a, b):
    try:
        return (dt.date.fromisoformat(b) - dt.date.fromisoformat(a)).days
    except Exception:
        return 0


def report(reg):
    print("=" * 76)
    print("  TOKEN LIFECYCLE — launch → graduation → beyond")
    print("=" * 76)
    n = len(reg)
    grad = [r for r in reg.values() if r["graduated"]]
    seen_live = [r for r in reg.values() if r["snapshots"] > 0]
    viral = [r for r in reg.values() if r["viral"]]
    print(f"  tracked: {n} tokens | graduated (have a DEX market): {len(grad)} | "
          f"polled at least once: {len(seen_live)} | went viral: {len(viral)}")

    # Late bloomers — peaked well AFTER launch (the whole point).
    bloomers = []
    for m, r in reg.items():
        if r["viral"] and r["peak_date"] and r["first_seen"]:
            age = days_between(r["first_seen"], r["peak_date"])
            bloomers.append((age, r["peak_mcap"], r.get("symbol") or m[:8], m))
    print("\n  LATE BLOOMERS (went viral, sorted by days from launch to peak):")
    if bloomers:
        print(f"  {'days→peak':>9} {'peak mcap $':>13} {'symbol':>10}  mint")
        for age, peak, sym, m in sorted(bloomers, reverse=True)[:20]:
            tag = "  ⟵ SLOW BURN" if age >= 7 else ""
            print(f"  {age:>9} {peak:>13,.0f} {sym:>10}  {m[:16]}…{tag}")
        slow = sum(1 for a, *_ in bloomers if a >= 7)
        print(f"\n  {slow}/{len(bloomers)} of the viral tokens peaked 7+ days AFTER launch —")
        print("  these are exactly the winners the pre-graduation-only view would miss.")
    else:
        print("  none yet — needs more polling history (tokens are still young / quiet).")

    # Age-at-peak distribution (when does virality actually happen?).
    if bloomers:
        buckets = defaultdict(int)
        for age, *_ in bloomers:
            b = "0-1d" if age <= 1 else "2-7d" if age <= 7 else "8-30d" if age <= 30 else "30d+"
            buckets[b] += 1
        print("\n  WHEN DO THEY GO VIRAL?  " +
              " | ".join(f"{k}: {buckets.get(k,0)}" for k in ["0-1d", "2-7d", "8-30d", "30d+"]))

    print("\n  This is the data the runner-research layer needs to learn the FULL-lifecycle")
    print("  winner DNA — including the slow burns. Keep polling (daily) to grow it.")
    print("=" * 76)


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--mock", action="store_true", help="offline self-test (no network)")
    ap.add_argument("--report-only", action="store_true", help="report from stored data, no poll")
    ap.add_argument("--decisions", default="momentum_decisions.csv")
    ap.add_argument("--max-tokens", type=int, default=600)
    args = ap.parse_args()

    reg = load_registry()
    added = seed_from_decisions(reg, args.decisions)
    if added:
        print(f"  registered {added} new token(s) from {args.decisions}")
    if not args.report_only:
        updated = poll(reg, mock=args.mock, max_tokens=args.max_tokens)
        print(f"  polled market state for {updated} token(s){' (MOCK)' if args.mock else ''}")
        save_registry(reg)
    report(reg)


if __name__ == "__main__":
    main()
