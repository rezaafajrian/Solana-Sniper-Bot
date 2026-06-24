#!/usr/bin/env python3
"""
Wallet scoring report — recency-weighted, confidence-adjusted (matches the bot).

Reads the bot's learned wallet reputation (momentum_wallet_rep.csv, which now
stores each wallet's recent win/loss window) and produces the scoring spec:

  final_score = 0.50*recent50_winrate + 0.30*recent200_winrate + 0.20*lifetime_winrate
  then shrunk toward 0.50 by sample-size confidence = n/(n+30), x100.

Recent performance dominates, small samples are penalized, and a formerly-good
wallet drops fast when its recent trades sour.

Rules: < 5 trades = IGNORE. < 20 trades = LOW_CONFIDENCE.
Tiers: 90+ ELITE / 75-89 STRONG / 60-74 WATCHLIST / 40-59 WEAK / <40 AVOID.

Usage:
  python3 scripts/wallet_scores.py                      # ranked report
  python3 scripts/wallet_scores.py --min-tier WATCHLIST # only WATCHLIST and up
  python3 scripts/wallet_scores.py --csv > scores.csv   # machine-readable
"""
import argparse, csv, sys

CONF_K = 30.0
TIER_ORDER = {"AVOID": 0, "WEAK": 1, "WATCHLIST": 2, "STRONG": 3, "ELITE": 4}


def winrate(bits, n=None):
    """Win rate over the most recent n outcomes of a '0'/'1' string (newest last)."""
    b = bits[-n:] if n else bits
    if not b:
        return None
    return b.count("1") / len(b)


def score_wallet(row):
    try:
        samples = int(row.get("samples", "0") or "0")
        wins = int(row.get("wins", "0") or "0")
    except ValueError:
        return None
    recent = "".join(c for c in (row.get("recent", "") or "") if c in "01")
    lifetime_wr = wins / samples if samples else 0.0
    # richer profile fields (default 0 for older rep files)
    def _f(k):
        try: return float(row.get(k, "0") or "0")
        except ValueError: return 0.0
    def _i(k):
        try: return int(float(row.get(k, "0") or "0"))
        except ValueError: return 0
    roi_sum = _f("roi_sum")
    avg_roi = roi_sum / samples if samples else 0.0
    rugs_bought = _i("rugs_bought")
    rugs_created = _i("rugs_created")
    tokens_followed = _i("tokens_followed")
    hold_secs_sum = _f("hold_secs_sum")
    hold_samples = _i("hold_samples")
    avg_hold = (hold_secs_sum / hold_samples) if hold_samples else 0.0
    first_seen = _i("first_seen")
    cluster_id = _i("cluster_id")
    r50 = winrate(recent, 50)
    r200 = winrate(recent, 200)
    # fall back to lifetime where the rolling window is empty (old data)
    r50 = lifetime_wr if r50 is None else r50
    r200 = lifetime_wr if r200 is None else r200
    raw = 0.50 * r50 + 0.30 * r200 + 0.20 * lifetime_wr
    conf = samples / (samples + CONF_K)
    final = (conf * raw + (1 - conf) * 0.5) * 100.0
    if samples < 5:
        tier = "IGNORE"
    elif final >= 90: tier = "ELITE"
    elif final >= 75: tier = "STRONG"
    elif final >= 60: tier = "WATCHLIST"
    elif final >= 40: tier = "WEAK"
    else: tier = "AVOID"
    return {
        "wallet_address": row.get("wallet", ""),
        "lifetime_trades": samples,
        "lifetime_winrate": round(lifetime_wr, 4),
        "recent_50_winrate": round(r50, 4),
        "recent_200_winrate": round(r200, 4),
        "confidence_score": round(conf, 4),
        "final_wallet_score": round(final, 2),
        "smart_money_score": round(final, 2),
        "avg_roi": round(avg_roi, 3),
        "avg_hold_secs": int(avg_hold),
        "rugs_bought": rugs_bought,
        "rugs_created": rugs_created,
        "tokens_followed": tokens_followed,
        "first_seen": first_seen,
        "cluster_id": cluster_id,
        "tier": tier,
        "flag": "LOW_CONFIDENCE" if samples < 20 else "",
    }


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("rep_csv", nargs="?", default="momentum_wallet_rep.csv")
    ap.add_argument("--min-tier", default="WEAK", choices=list(TIER_ORDER))
    ap.add_argument("--csv", action="store_true", help="emit CSV instead of a table")
    args = ap.parse_args()

    try:
        rows = list(csv.DictReader(open(args.rep_csv, newline="")))
    except FileNotFoundError:
        sys.stderr.write(f"No rep file at '{args.rep_csv}'. Run the bot so it can learn.\n")
        sys.exit(1)

    scored = [s for s in (score_wallet(r) for r in rows) if s and s["tier"] != "IGNORE"]
    scored.sort(key=lambda s: s["final_wallet_score"], reverse=True)
    keep = [s for s in scored if TIER_ORDER[s["tier"]] >= TIER_ORDER[args.min_tier]]

    cols = ["wallet_address", "lifetime_trades", "lifetime_winrate", "recent_50_winrate",
            "recent_200_winrate", "confidence_score", "final_wallet_score", "smart_money_score",
            "avg_roi", "avg_hold_secs", "rugs_bought", "rugs_created", "tokens_followed",
            "first_seen", "cluster_id", "tier", "flag"]
    if args.csv:
        w = csv.DictWriter(sys.stdout, fieldnames=cols)
        w.writeheader()
        for s in keep:
            w.writerow(s)
        return

    print("=" * 96)
    print(f"  WALLET SCORES  (recency-weighted, confidence-adjusted) — {args.rep_csv}")
    print(f"  scored {len(scored)} wallets (>=5 trades); showing {len(keep)} >= {args.min_tier}")
    print("=" * 96)
    by_tier = {}
    for s in scored:
        by_tier[s["tier"]] = by_tier.get(s["tier"], 0) + 1
    print("  tiers: " + " · ".join(f"{t}={by_tier.get(t,0)}" for t in ["ELITE","STRONG","WATCHLIST","WEAK","AVOID"]))
    print(f"\n  {'wallet':14} {'tier':10} {'score':>6} {'win50':>6} {'avgROI':>7} {'n':>5} {'follow':>6} {'rugB':>5} {'rugC':>5}  flag")
    for s in keep[:60]:
        w = s["wallet_address"]
        print(f"  {w[:12]+'…':14} {s['tier']:10} {s['final_wallet_score']:>6.1f} "
              f"{s['recent_50_winrate']*100:>5.0f}% {s['avg_roi']*100:>6.0f}% "
              f"{s['lifetime_trades']:>5} {s['tokens_followed']:>6} {s['rugs_bought']:>5} {s['rugs_created']:>5}  {s['flag']}")
    print("\n  win50 = recent-50 win rate · avgROI = mean forward return · follow = distinct tokens")
    print("  rugB = rugs bought · rugC = rugs CREATED (a creator of rugs — hard avoid).")
    print("  Follow ELITE/STRONG; WATCHLIST = promising; WEAK/AVOID = don't follow.")
    print("  (avg_hold_secs + cluster_id are in --csv output; cluster_id needs indexer data.)")


if __name__ == "__main__":
    main()
