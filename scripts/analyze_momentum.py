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

    print("\n4. COST DRAG (estimate minus on-chain actual)")
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
