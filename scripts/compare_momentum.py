#!/usr/bin/env python3
"""
A/B comparison of two momentum trade logs — e.g. edges ON vs edges OFF.

Run the bot twice in dry run over comparable windows, logging to two files
(set MOMENTUM_TRADE_LOG differently each run), then:

    python3 scripts/compare_momentum.py on.csv off.csv
    python3 scripts/compare_momentum.py on.csv off.csv --labels "edges on" "edges off"

It prints the headline metrics side by side and the delta, so you can see whether
the edge mechanisms actually moved expected value rather than guessing.
"""

import sys
import os

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from analyze_momentum import load, summarize  # noqa: E402


def load_summary(path):
    try:
        rows = load(path)
    except FileNotFoundError:
        print(f"No trade log at '{path}'.")
        sys.exit(1)
    if not rows:
        print(f"'{path}' is empty — no trades recorded yet.")
        sys.exit(1)
    return summarize(rows)


def main():
    args = [a for a in sys.argv[1:]]
    labels = ["A", "B"]
    if "--labels" in args:
        i = args.index("--labels")
        if len(args) >= i + 3:
            labels = [args[i + 1], args[i + 2]]
        args = args[:i] + args[i + 3:]

    if len(args) != 2:
        print("usage: compare_momentum.py <a.csv> <b.csv> [--labels 'A name' 'B name']")
        sys.exit(1)

    a, b = load_summary(args[0]), load_summary(args[1])
    la, lb = labels[0][:16], labels[1][:16]

    def row(name, va, vb, fmt="{:+.4f}", delta=True):
        sa, sb = fmt.format(va), fmt.format(vb)
        d = fmt.format(va - vb) if delta else ""
        print(f"   {name:<26} {sa:>14} {sb:>14}   {d:>14}")

    print("=" * 76)
    print(f"  MOMENTUM A/B COMPARISON")
    print(f"  A = {args[0]}   B = {args[1]}")
    print("=" * 76)
    print(f"   {'metric':<26} {la:>14} {lb:>14}   {'A - B':>14}")
    print("   " + "-" * 70)
    row("Total realized PnL (SOL)", a["total"], b["total"])
    row("Avg PnL / token (SOL)", a["avg_per_token"], b["avg_per_token"])
    row("Win rate (%)", a["win_rate"], b["win_rate"], fmt="{:.1f}")
    row("Tokens traded", a["tokens"], b["tokens"], fmt="{:d}", delta=False)
    row("Buys / sells (buys)", a["buys"], b["buys"], fmt="{:d}", delta=False)
    row("Winners sum (SOL)", a["winners_sum"], b["winners_sum"])
    row("Losers sum (SOL)", a["losers_sum"], b["losers_sum"])

    print("\n   By exit reason (realized PnL, SOL):")
    cats = sorted(set(a["cat_pnl"]) | set(b["cat_pnl"]))
    for c in cats:
        row(c, a["cat_pnl"].get(c, 0.0), b["cat_pnl"].get(c, 0.0))

    print("\n" + "=" * 76)
    diff = a["total"] - b["total"]
    if diff > 0:
        print(f"  '{labels[0]}' outperformed '{labels[1]}' by {diff:+.4f} SOL on this sample.")
        print("  Encouraging — but confirm over more trades and a fresh window before trusting it.")
    elif diff < 0:
        print(f"  '{labels[0]}' UNDERperformed '{labels[1]}' by {diff:+.4f} SOL on this sample.")
        print("  The change did not help here. Re-test, or reconsider the settings.")
    else:
        print("  Dead heat on this sample — need more data to distinguish them.")
    print("  Note: small samples and different time windows lie. Compare like with like.")
    print("=" * 76)


if __name__ == "__main__":
    main()
