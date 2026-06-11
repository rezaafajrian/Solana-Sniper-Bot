#!/usr/bin/env python3
"""
Analyze the momentum bot's trade log and answer the only question that matters:
does this configuration have an edge?

Usage:
    python3 scripts/analyze_momentum.py [path/to/momentum_trades.csv]

Reads the append-only CSV written by src/processor/momentum.rs and reports:
  1. Total realized PnL after costs (the edge, in one number).
  2. Per-token results and the biggest winners / losers.
  3. Score-vs-outcome: do higher-scored entries actually do better?
  4. Cost drag (how much fees + tip + slippage ate) when on-chain actuals exist.

Realized PnL accounting:
  - LIVE: each sell writes an estimate row (SELL_PARTIAL/SELL_FULL) and, once
    confirmed, a SELL_ACTUAL row with true on-chain proceeds. We prefer the
    actual and fall back to the estimate only if no actual landed.
  - DRY RUN: there are no SELL_ACTUAL rows; the SELL_PARTIAL/SELL_FULL rows
    already include the simulated cost and ARE the truth.
"""

import csv
import sys
from collections import defaultdict


def load(path):
    with open(path, newline="") as f:
        return list(csv.DictReader(f))


def f(row, key):
    try:
        return float(row[key])
    except (KeyError, ValueError):
        return 0.0


def realized_rows(rows):
    """Yield (mint, realized_sol, source_row) counting each sell exactly once,
    preferring on-chain actuals over estimates (matched by signature)."""
    actual_sigs = {r["signature"] for r in rows if r["event"] == "SELL_ACTUAL"}
    for r in rows:
        ev = r["event"]
        if ev == "SELL_ACTUAL":
            yield r["mint"], f(r, "est_realized_pnl_sol"), r
        elif ev in ("SELL_PARTIAL", "SELL_FULL"):
            sig = r["signature"]
            # Skip estimates that have a reconciled actual (live mode).
            if sig in actual_sigs and sig not in ("", "DRY_RUN"):
                continue
            yield r["mint"], f(r, "est_realized_pnl_sol"), r


def categorize(reason):
    r = reason.lower()
    if "insider" in r or "leader" in r:
        return "insider/leader-dump exit"
    if "hard stop" in r:
        return "hard stop"
    if "collapse" in r:
        return "momentum collapse"
    if "scale-out" in r:
        return "scale-out (profit)"
    return "other"


def summarize(rows):
    """Compute the headline metrics for a trade log into a flat dict.
    Shared by the single-log report and the A/B comparison helper."""
    per_token = defaultdict(float)
    total = 0.0
    sells = 0
    cat_pnl = defaultdict(float)
    for mint, pnl, row in realized_rows(rows):
        per_token[mint] += pnl
        total += pnl
        sells += 1
        cat_pnl[categorize(row["reason"])] += pnl

    closed = dict(per_token)
    winners = [v for v in closed.values() if v > 0]
    losers = [v for v in closed.values() if v < 0]
    buys = sum(1 for r in rows if r["event"] == "BUY")
    n = len(closed)
    return {
        "total": total,
        "buys": buys,
        "sells": sells,
        "tokens": n,
        "avg_per_token": total / n if n else 0.0,
        "win_rate": 100.0 * len(winners) / n if n else 0.0,
        "winners_sum": sum(winners),
        "losers_sum": sum(losers),
        "cat_pnl": dict(cat_pnl),
        "dry": any(r["signature"] == "DRY_RUN" for r in rows),
    }


def main():
    path = sys.argv[1] if len(sys.argv) > 1 else "momentum_trades.csv"
    try:
        rows = load(path)
    except FileNotFoundError:
        print(f"No trade log at '{path}'. Run the bot (dry run is fine) to generate it.")
        sys.exit(1)

    if not rows:
        print(f"'{path}' is empty — no trades recorded yet.")
        sys.exit(0)

    dry = any(r["signature"] == "DRY_RUN" for r in rows)
    buys = [r for r in rows if r["event"] == "BUY"]

    # ---- Per-token realized PnL ----
    per_token = defaultdict(float)
    total_realized = 0.0
    sells = 0
    for mint, pnl, _ in realized_rows(rows):
        per_token[mint] += pnl
        total_realized += pnl
        sells += 1

    # Entry score per token (average over its buys).
    score_sum = defaultdict(float)
    score_n = defaultdict(int)
    for b in buys:
        score_sum[b["mint"]] += f(b, "score")
        score_n[b["mint"]] += 1
    avg_score = {m: score_sum[m] / score_n[m] for m in score_sum if score_n[m]}

    # ---- Cost drag (estimate vs actual, matched by signature) ----
    est_by_sig = {}
    for r in rows:
        if r["event"] in ("SELL_PARTIAL", "SELL_FULL") and r["signature"] not in ("", "DRY_RUN"):
            est_by_sig[r["signature"]] = f(r, "est_realized_pnl_sol")
    drag = 0.0
    drag_n = 0
    for r in rows:
        if r["event"] == "SELL_ACTUAL" and r["signature"] in est_by_sig:
            drag += est_by_sig[r["signature"]] - f(r, "est_realized_pnl_sol")
            drag_n += 1

    # ---- Win/loss split ----
    closed = {m: v for m, v in per_token.items()}
    winners = {m: v for m, v in closed.items() if v > 0}
    losers = {m: v for m, v in closed.items() if v < 0}

    mode = "DRY RUN (simulated)" if dry else "LIVE"
    print("=" * 64)
    print(f"  MOMENTUM TRADE ANALYSIS  —  {mode}")
    print(f"  source: {path}")
    print("=" * 64)

    print("\n1. THE EDGE (realized PnL after costs)")
    print(f"   Total realized PnL : {total_realized:+.4f} SOL")
    print(f"   Buys / sells       : {len(buys)} / {sells}")
    print(f"   Tokens traded      : {len(closed)}")
    if closed:
        avg = total_realized / len(closed)
        print(f"   Avg PnL per token  : {avg:+.4f} SOL")
        verdict = "POSITIVE — worth tuning/scaling carefully" if total_realized > 0 else "NEGATIVE — no edge at these settings"
        print(f"   Verdict            : {verdict}")

    print("\n2. WIN / LOSS DISTRIBUTION")
    n = len(closed)
    if n:
        wr = 100.0 * len(winners) / n
        print(f"   Win rate           : {wr:.1f}%  ({len(winners)} win / {len(losers)} lose)")
        if winners:
            print(f"   Total from winners : {sum(winners.values()):+.4f} SOL  (avg {sum(winners.values())/len(winners):+.4f})")
        if losers:
            print(f"   Total from losers  : {sum(losers.values()):+.4f} SOL  (avg {sum(losers.values())/len(losers):+.4f})")
        biggest = sorted(closed.items(), key=lambda kv: kv[1], reverse=True)
        print("   Top 5 winners:")
        for m, v in biggest[:5]:
            print(f"      {m[:44]:<44} {v:+.4f} SOL  (entry score {avg_score.get(m, 0):.0f})")
        print("   Top 5 losers:")
        for m, v in biggest[-5:][::-1]:
            print(f"      {m[:44]:<44} {v:+.4f} SOL  (entry score {avg_score.get(m, 0):.0f})")

    print("\n3. SCORE vs OUTCOME (is the signal predictive?)")
    win_scores = [avg_score[m] for m in winners if m in avg_score]
    lose_scores = [avg_score[m] for m in losers if m in avg_score]
    if win_scores and lose_scores:
        aw = sum(win_scores) / len(win_scores)
        al = sum(lose_scores) / len(lose_scores)
        print(f"   Avg entry score, winners : {aw:.1f}")
        print(f"   Avg entry score, losers  : {al:.1f}")
        if aw > al + 2:
            print("   -> Higher scores DO predict better outcomes. Consider raising MOMENTUM_ENTRY_SCORE.")
        elif al > aw + 2:
            print("   -> Higher scores predict WORSE outcomes. The signal may be inverted/noise — rethink weights.")
        else:
            print("   -> Score barely separates winners from losers. Signal is weak at this threshold.")
    else:
        print("   Not enough closed winners and losers yet to judge.")

    print("\n4. EXIT REASONS (is each edge earning its keep?)")
    # Categorize every counted sell by reason, summing realized PnL per category.
    cat_pnl = defaultdict(float)
    cat_n = defaultdict(int)
    for _mint, pnl, row in realized_rows(rows):
        r = row["reason"].lower()
        if "insider" in r or "leader" in r:
            cat = "insider/leader-dump exit"
        elif "hard stop" in r:
            cat = "hard stop"
        elif "collapse" in r:
            cat = "momentum collapse"
        elif "scale-out" in r:
            cat = "scale-out (profit)"
        else:
            cat = "other"
        cat_pnl[cat] += pnl
        cat_n[cat] += 1
    if cat_n:
        for cat in sorted(cat_pnl, key=lambda c: cat_pnl[c], reverse=True):
            print(f"   {cat:<26} {cat_n[cat]:>4} sells   {cat_pnl[cat]:+.4f} SOL")
    else:
        print("   No sells recorded yet.")

    # Smart-money reputation snapshot, if present.
    print("\n4b. DISCIPLINE / DRAWDOWN (did the process hold?)")
    # Chronological running PnL -> peak, max drawdown, worst losing streak.
    cum = 0.0
    peak = 0.0
    max_dd = 0.0
    streak = 0
    worst_streak = 0
    for _mint, pnl, row in realized_rows(rows):
        cum += pnl
        peak = max(peak, cum)
        max_dd = max(max_dd, peak - cum)
        if row["event"] == "SELL_FULL":
            if pnl < 0:
                streak += 1
                worst_streak = max(worst_streak, streak)
            else:
                streak = 0
    print(f"   Peak cumulative PnL    : {peak:+.4f} SOL")
    print(f"   Max drawdown           : -{max_dd:.4f} SOL")
    print(f"   Worst losing streak    : {worst_streak} consecutive full exits")
    print("   (Set MOMENTUM_DAILY_LOSS_LIMIT_SOL ~ your max tolerable drawdown and")
    print("    MOMENTUM_MAX_CONSECUTIVE_LOSSES above normal streaks but below a blow-up.)")

    print("\n4c. KOL ATTRIBUTION (which KOLs actually make money?)")
    # Map mint -> KOL label from BUY reasons like "momentum entry [KOL:xyz]".
    import re
    mint_kol = {}
    for r in rows:
        if r["event"] == "BUY":
            m = re.search(r"\[KOL:([^\]]+)\]", r["reason"])
            if m:
                mint_kol[r["mint"]] = m.group(1)
    if not mint_kol:
        print("   No KOL-tagged trades (KOL tracking off, or no KOL buys this run).")
    else:
        kol_pnl = defaultdict(float)
        kol_tok = defaultdict(set)
        for mint, pnl, _ in realized_rows(rows):
            if mint in mint_kol:
                kol_pnl[mint_kol[mint]] += pnl
                kol_tok[mint_kol[mint]].add(mint)
        print(f"   {'KOL':<20} {'tokens':>7} {'realized SOL':>14}")
        for kol in sorted(kol_pnl, key=lambda k: kol_pnl[k], reverse=True):
            print(f"   {kol[:20]:<20} {len(kol_tok[kol]):>7} {kol_pnl[kol]:>+14.4f}")
        print("   -> Keep the green KOLs, cut the red ones from kol_wallets.txt. That's the edge loop.")

    print("\n5. SMART-MONEY MEMORY")
    rep_path = "momentum_wallet_rep.csv"
    try:
        rep = load(rep_path)
        scored = [(r["wallet"], float(r["score"]), int(r["samples"])) for r in rep]
        ranked = sorted([w for w in scored if w[2] >= 3], key=lambda w: w[1], reverse=True)
        print(f"   Wallets with reputation : {len(scored)} ({len(ranked)} with >=3 samples)")
        if ranked:
            print("   Top smart-money wallets:")
            for w, s, n in ranked[:5]:
                print(f"      {w[:44]:<44} rep {s:+.3f}  ({n} samples)")
    except FileNotFoundError:
        print(f"   No reputation file at '{rep_path}' yet (builds as the bot runs).")

    print("\n6. COST DRAG (estimate minus on-chain actual)")
    if dry:
        print("   Dry run — costs are already baked into MOMENTUM_SIM_COST_FRACTION.")
    elif drag_n:
        print(f"   Reconciled sells   : {drag_n}")
        print(f"   Total drag         : {drag:+.4f} SOL  (estimate overstated realized by this much)")
    else:
        print("   No reconciled (SELL_ACTUAL) rows yet — run longer or check RPC get_transaction access.")

    print("\n" + "=" * 64)
    if total_realized <= 0:
        print("  Bottom line: no demonstrated edge yet. Keep dry-running and tune,")
        print("  or accept it doesn't work at these settings. Do NOT add real money.")
    else:
        print("  Bottom line: positive in this sample. Validate over MORE trades and a")
        print("  fresh time window before trusting it — small samples lie.")
    print("=" * 64)


if __name__ == "__main__":
    main()
