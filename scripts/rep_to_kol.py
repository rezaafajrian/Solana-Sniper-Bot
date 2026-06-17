#!/usr/bin/env python3
"""
Turn the bot's LEARNED wallet reputation into a proven-winner KOL list.

The momentum bot grades every wallet on the forward returns of the tokens it
buys and persists the result to momentum_wallet_rep.csv. Over many runs this
becomes a self-discovered "smart money" list — wallets whose buys repeatedly
precede pumps. This script exports the best of them to kol_wallets.txt so you
can seed a fresh bot, inspect what it learned, or pin the proven wallets as a
curated KOL list.

This is the edge loop: the bot watches everyone -> learns who wins -> you
promote the winners -> the bot follows them with conviction.

Usage:
  python3 scripts/rep_to_kol.py momentum_wallet_rep.csv > kol_wallets.txt
  python3 scripts/rep_to_kol.py momentum_wallet_rep.csv --min-rep 0.5 --min-samples 5 --top 100 > kol_wallets.txt

Options:
  --min-rep X       minimum reputation score to include (default 0.5)
  --min-samples N   minimum graded samples to include (default 5)
  --top N           keep only the top N by reputation (default: all that qualify)
  --label NAME      label for every exported wallet (default "learned")
  --weight-min X    weight floor for the weakest kept wallet (default 1.0)
  --weight-max X    weight ceiling for the strongest kept wallet (default 2.5)

Output format (one per line), ready for MOMENTUM_KOL_FILE:
  <wallet>,<weight>,<label>
"""

import argparse
import csv
import sys
from datetime import datetime, timezone


def main():
    ap = argparse.ArgumentParser(description="Export learned reputation to a KOL list")
    ap.add_argument("rep_csv", help="path to momentum_wallet_rep.csv")
    ap.add_argument("--min-rep", type=float, default=0.5)
    ap.add_argument("--min-samples", type=int, default=5)
    ap.add_argument("--top", type=int, default=0)
    ap.add_argument("--label", default="learned")
    ap.add_argument("--weight-min", type=float, default=1.0)
    ap.add_argument("--weight-max", type=float, default=2.5)
    args = ap.parse_args()

    rows = []
    try:
        with open(args.rep_csv, newline="") as f:
            reader = csv.DictReader(f)
            for r in reader:
                try:
                    score = float(r.get("score", "nan"))
                    samples = int(r.get("samples", "0"))
                except (ValueError, TypeError):
                    continue
                wallet = (r.get("wallet") or "").strip()
                if not wallet:
                    continue
                if samples >= args.min_samples and score >= args.min_rep:
                    rows.append((wallet, score, samples))
    except FileNotFoundError:
        sys.stderr.write(f"No reputation file at '{args.rep_csv}'. "
                         "Run the bot first so it can learn.\n")
        sys.exit(1)

    if not rows:
        sys.stderr.write(
            f"No wallets cleared the bar (rep >= {args.min_rep}, "
            f">= {args.min_samples} samples). Let the bot run longer, or lower the "
            "thresholds.\n")
        sys.exit(2)

    # Strongest first.
    rows.sort(key=lambda x: x[1], reverse=True)
    if args.top > 0:
        rows = rows[: args.top]

    # Map reputation -> weight in [weight_min, weight_max] so the bot's KOL boost
    # scales with how proven each wallet is.
    hi = rows[0][1]
    lo = rows[-1][1]
    span = max(hi - lo, 1e-9)
    wspan = args.weight_max - args.weight_min

    now = datetime.now(timezone.utc).strftime("%Y-%m-%d %H:%M UTC")
    print(f"# proven-winner wallets exported from {args.rep_csv} on {now}")
    print(f"# filter: rep >= {args.min_rep}, samples >= {args.min_samples}"
          + (f", top {args.top}" if args.top else "") )
    print(f"# {len(rows)} wallets | format: wallet,weight,label")
    for wallet, score, samples in rows:
        weight = args.weight_min + wspan * ((score - lo) / span)
        print(f"{wallet},{weight:.2f},{args.label}")

    sys.stderr.write(
        f"Exported {len(rows)} proven wallets "
        f"(rep {lo:.2f}..{hi:.2f}, >= {args.min_samples} samples). "
        "Set MOMENTUM_KOL_ENABLED=true and point MOMENTUM_KOL_FILE at the output.\n")


if __name__ == "__main__":
    main()
