#!/usr/bin/env python3
"""
MELT-style self-learning engine for the momentum bot.

Reads the decision log (every token evaluation) and the labeled outcomes the bot
records 2h after each detection, then answers: which features predict winners vs
rugs, which filters are rejecting profitable tokens, which accepted trades turn
into losers — and recommends concrete scoring/filter changes.

Inputs (produced by the bot when MOMENTUM_DECISION_LOG is set):
  <base>.csv            one row per evaluated token (features + BUY/REJECT)
  <base>_outcomes.csv   one row per token, labeled 2h later (returns + RUG/2X/...)

Usage:
  python3 scripts/learn.py                       # defaults to momentum_decisions
  python3 scripts/learn.py momentum_decisions    # explicit base path
  python3 scripts/learn.py --period weekly       # daily (default) or weekly buckets
"""

import csv, os, sys, math, argparse, datetime as dt
from collections import defaultdict

NUM_FEATURES = [
    "creator_buy", "creator_score", "wallet_score", "insider_score",
    "smart_wallet_count", "holder_concentration", "marketcap", "volume",
    "liquidity", "overall_score", "confidence",
]
WIN_LABELS = {"2X", "5X", "10X", "20X+", "WIN"}
LOSS_LABELS = {"LOSS", "RUG"}


def f(row, k):
    try:
        return float(row.get(k, "") or "nan")
    except (ValueError, TypeError):
        return float("nan")


def load(base):
    dec_path = f"{base}.csv" if not base.endswith(".csv") else base
    base_noext = dec_path[:-4]
    out_path = f"{base_noext}_outcomes.csv"
    decisions = {}
    if os.path.exists(dec_path):
        with open(dec_path, newline="") as fh:
            for r in csv.DictReader(fh):
                ca = r.get("ca")
                if not ca:
                    continue
                # BUY rows win over earlier REJECT rows for the same CA
                if ca not in decisions or r.get("decision") == "BUY":
                    decisions[ca] = r
    outcomes = {}
    if os.path.exists(out_path):
        with open(out_path, newline="") as fh:
            for r in csv.DictReader(fh):
                ca = r.get("ca")
                if ca:
                    outcomes[ca] = r
    return decisions, outcomes, dec_path, out_path


def joined(decisions, outcomes):
    """Tokens that have both an evaluation and a labeled outcome."""
    rows = []
    for ca, o in outcomes.items():
        d = decisions.get(ca, {})
        rows.append({**d, **{f"o_{k}": v for k, v in o.items()}, "ca": ca})
    return rows


def label_of(row):
    return row.get("o_final_label", "") or ""


def is_winner(row):
    return label_of(row) in WIN_LABELS or f(row, "o_final_return") >= 0.15


def is_loser(row):
    return label_of(row) in LOSS_LABELS or f(row, "o_final_return") <= -0.30


def mean(xs):
    xs = [x for x in xs if not math.isnan(x)]
    return sum(xs) / len(xs) if xs else float("nan")


def pct(n, d):
    return 100.0 * n / d if d else 0.0


def hr():
    print("=" * 72)


def section(t):
    print(f"\n{t}")
    print("-" * len(t))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("base", nargs="?", default="momentum_decisions")
    ap.add_argument("--period", choices=["daily", "weekly"], default="daily")
    args = ap.parse_args()

    decisions, outcomes, dec_path, out_path = load(args.base)
    if not decisions:
        sys.stderr.write(f"No decision log at '{dec_path}'. Run the bot with "
                         "MOMENTUM_DECISION_LOG set to generate it.\n")
        sys.exit(1)

    hr()
    print("  MOMENTUM SELF-LEARNING REPORT")
    print(f"  decisions: {dec_path} ({len(decisions)} tokens)")
    print(f"  outcomes : {out_path} ({len(outcomes)} labeled)")
    hr()

    rows = joined(decisions, outcomes)
    if not rows:
        print("\nNo labeled outcomes yet — the bot labels each token ~2h after"
              "\ndetection. Keep it running, then re-run this report.")
        # still show the decision mix
        dec_counts = defaultdict(int)
        for d in decisions.values():
            dec_counts[d.get("decision", "?")] += 1
        section("Decisions so far")
        for k, v in sorted(dec_counts.items()):
            print(f"  {k:8} {v}")
        return

    winners = [r for r in rows if is_winner(r)]
    losers = [r for r in rows if is_loser(r)]
    bought = [r for r in rows if r.get("decision") == "BUY"]
    rejected = [r for r in rows if r.get("decision") == "REJECT"]

    # ---- 5. Headline ----
    section("1. HEADLINE")
    print(f"  labeled tokens     : {len(rows)}")
    print(f"  winners / losers   : {len(winners)} / {len(losers)}  "
          f"(win rate {pct(len(winners), len(winners)+len(losers)):.1f}%)")
    lab = defaultdict(int)
    for r in rows:
        lab[label_of(r) or "?"] += 1
    print("  outcome labels     : " + ", ".join(f"{k}={v}" for k, v in
          sorted(lab.items(), key=lambda x: -x[1])))
    print(f"  avg max-return     : {mean([f(r,'o_max_return') for r in rows]):+.2f}x")
    print(f"  avg final-return   : {mean([f(r,'o_final_return') for r in rows]):+.2f}")

    # ---- 3. Pattern discovery: feature means, winners vs losers ----
    section("2. FEATURE SIGNAL (winners vs losers — does it separate?)")
    print(f"  {'feature':22} {'winners':>10} {'losers':>10} {'gap':>10}")
    feats_ranked = []
    for k in NUM_FEATURES:
        wm = mean([f(r, k) for r in winners])
        lm = mean([f(r, k) for r in losers])
        if math.isnan(wm) or math.isnan(lm):
            continue
        scale = max(abs(wm), abs(lm), 1e-9)
        gap = (wm - lm) / scale
        feats_ranked.append((abs(gap), k, wm, lm, gap))
    feats_ranked.sort(reverse=True)
    for _, k, wm, lm, gap in feats_ranked:
        flag = "  <<" if abs(gap) > 0.25 else ""
        print(f"  {k:22} {wm:>10.3f} {lm:>10.3f} {gap:>+9.0%}{flag}")
    print("  (gap = how differently the feature reads for winners vs losers;")
    print("   '<<' marks the ones actually worth using as signals)")

    # ---- 4. Feature discovery: win-rate by quantile bucket ----
    section("3. FEATURE DISCOVERY (win-rate across each feature's range)")
    for k in NUM_FEATURES:
        vals = sorted(f(r, k) for r in rows if not math.isnan(f(r, k)))
        if len(vals) < 12:
            continue
        lo_cut = vals[len(vals)//3]
        hi_cut = vals[2*len(vals)//3]
        buckets = {"low": [], "mid": [], "high": []}
        for r in rows:
            v = f(r, k)
            if math.isnan(v):
                continue
            b = "low" if v <= lo_cut else ("high" if v >= hi_cut else "mid")
            buckets[b].append(r)
        def wr(rs):
            w = sum(1 for r in rs if is_winner(r)); l = sum(1 for r in rs if is_loser(r))
            return pct(w, w+l), w+l
        (lw, ln), (mw, mn), (hw, hn) = wr(buckets["low"]), wr(buckets["mid"]), wr(buckets["high"])
        spread = abs(hw - lw)
        if spread >= 12 and ln >= 5 and hn >= 5:  # only show features that actually discriminate
            print(f"  {k:22} low {lw:4.0f}% (n={ln:<4}) | high {hw:4.0f}% (n={hn:<4}) | spread {spread:.0f}pts")

    # ---- which filters reject profitable opportunities ----
    section("4. REJECTED WINNERS (filters throwing away profit)")
    by_reason = defaultdict(lambda: {"n": 0, "win": 0, "big": 0})
    for r in rejected:
        reason = (r.get("rejection_reason") or "?").split(":")[0].strip() or "?"
        by_reason[reason]["n"] += 1
        if is_winner(r):
            by_reason[reason]["win"] += 1
        if f(r, "o_max_return") >= 1.0:
            by_reason[reason]["big"] += 1
    if by_reason:
        print(f"  {'rejection reason':32} {'rejected':>9} {'->won':>7} {'->2x+':>7}")
        for reason, d in sorted(by_reason.items(), key=lambda x: -x[1]["big"]):
            print(f"  {reason[:32]:32} {d['n']:>9} {pct(d['win'],d['n']):>6.0f}% {pct(d['big'],d['n']):>6.0f}%")
    else:
        print("  no rejected tokens with outcomes yet")

    # ---- accepted losers ----
    section("5. ACCEPTED LOSERS (buys that went bad)")
    bl = [r for r in bought if is_loser(r)]
    print(f"  bought: {len(bought)} | losers: {len(bl)} ({pct(len(bl),len(bought)):.0f}%) | "
          f"rugs: {sum(1 for r in bought if label_of(r)=='RUG')}")
    if bl:
        print("  shared traits of bought losers (avg):")
        for k in ["insider_score", "holder_concentration", "overall_score", "wallet_score"]:
            print(f"    {k:22} {mean([f(r,k) for r in bl]):.3f}   (winners-bought: "
                  f"{mean([f(r,k) for r in bought if is_winner(r)]):.3f})")

    # ---- period report ----
    section(f"6. {args.period.upper()} BREAKDOWN")
    def bucket_key(r):
        ts = r.get("timestamp", "")[:10]
        if args.period == "weekly" and len(ts) == 10:
            try:
                d = dt.date.fromisoformat(ts)
                return f"{d.isocalendar()[0]}-W{d.isocalendar()[1]:02d}"
            except ValueError:
                return ts
        return ts
    per = defaultdict(list)
    for r in rows:
        per[bucket_key(r)].append(r)
    print(f"  {'period':12} {'tokens':>7} {'winrate':>8} {'avg max':>9}")
    for key in sorted(per):
        rs = per[key]
        w = sum(1 for r in rs if is_winner(r)); l = sum(1 for r in rs if is_loser(r))
        print(f"  {key:12} {len(rs):>7} {pct(w,w+l):>7.0f}% {mean([f(r,'o_max_return') for r in rs]):>+8.2f}x")

    # ---- 5. Recommendations ----
    section("7. RECOMMENDATIONS (data-driven, verify before trusting)")
    recs = []
    # rug predictors
    rugs = [r for r in rows if label_of(r) == "RUG"]
    nonrugs = [r for r in rows if label_of(r) != "RUG"]
    for k, env in [("insider_score", "MOMENTUM_INSIDER_DISTRIB_MAX_SOLD"),
                   ("holder_concentration", "MOMENTUM_MAX_TOP_HOLDER_SHARE")]:
        rm, nm = mean([f(r, k) for r in rugs]), mean([f(r, k) for r in nonrugs])
        if not math.isnan(rm) and not math.isnan(nm) and rm > nm * 1.3 and len(rugs) >= 8:
            recs.append(f"Rugs have much higher {k} ({rm:.2f} vs {nm:.2f}). "
                        f"Consider tightening {env} toward ~{(rm+nm)/2:.2f}.")
    # rejected-winner leakage
    for reason, d in by_reason.items():
        if d["n"] >= 10 and pct(d["big"], d["n"]) >= 25:
            recs.append(f"Filter '{reason}' rejected {d['n']} tokens, {pct(d['big'],d['n']):.0f}% of which "
                        f"hit 2x+. It's leaking winners — loosen it and A/B.")
    # score predictiveness
    ws = mean([f(r, "overall_score") for r in winners])
    ls = mean([f(r, "overall_score") for r in losers])
    if not math.isnan(ws) and not math.isnan(ls):
        if abs(ws - ls) < 3:
            recs.append(f"overall_score barely separates winners ({ws:.1f}) from losers ({ls:.1f}) "
                        "— the entry score is near-noise; lean on exits + the strongest features above.")
        elif ws > ls + 5:
            recs.append(f"overall_score is predictive (winners {ws:.1f} vs losers {ls:.1f}) "
                        "— raising MOMENTUM_ENTRY_SCORE should lift win rate.")
    # best discriminating feature
    if feats_ranked and feats_ranked[0][0] > 0.3:
        _, k, wm, lm, gap = feats_ranked[0]
        recs.append(f"Strongest signal is '{k}' (winners {wm:.2f} vs losers {lm:.2f}). "
                    "Worth adding/raising its weight in the entry score.")
    if not recs:
        recs.append("No statistically clear edge yet — gather more labeled outcomes "
                    "(let it run longer) before changing filters.")
    for i, r in enumerate(recs, 1):
        print(f"  {i}. {r}")

    print()
    hr()
    print("  Caveat: these are correlations on simulated outcomes. Treat every")
    print("  recommendation as a hypothesis to A/B one at a time, not a fact.")
    hr()


if __name__ == "__main__":
    main()
