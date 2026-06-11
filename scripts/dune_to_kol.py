#!/usr/bin/env python3
"""
Convert a Dune CSV export (or any CSV with a wallet column) into kol_wallets.txt
for the momentum bot.

Get the CSV: open the Dune dashboard, hover the result table, click the "..."
(or download icon) -> "Download CSV" (needs a free Dune login). Then:

    python3 scripts/dune_to_kol.py downloaded.csv > kol_wallets.txt
    python3 scripts/dune_to_kol.py a.csv b.csv > kol_wallets.txt   # merge multiple

Options:
    --label NAME     label all wallets from this file (default: source filename)
    --weight-col COL use a numeric column (e.g. pnl) to set per-wallet weight
    --min-weight X   drop wallets whose weight-col value is below X

It auto-detects the wallet column (named wallet/address/trader/account/owner, or
the column whose values look like base58 Solana addresses), validates addresses,
and de-duplicates.
"""

import csv
import sys
import re

B58 = re.compile(r"^[1-9A-HJ-NP-Za-km-z]{32,44}$")
# Dune often wraps addresses in an HTML link, e.g.
#   <a href=https://solscan.io/account/8Twth...QLAtE target=_blank>8Twth...QLAtE</a>
# This pulls the first base58 address out of any such string.
B58_ANY = re.compile(r"[1-9A-HJ-NP-Za-km-z]{32,44}")
WALLET_NAMES = ("wallet", "address", "trader", "account", "owner", "signer", "user")


def extract_addr(s):
    """Return a clean base58 Solana address from a raw cell value, or "".
    Handles bare addresses and HTML-wrapped ones (solscan links etc.)."""
    if not s:
        return ""
    s = str(s).strip()
    if B58.match(s):
        return s
    m = B58_ANY.search(s)
    return m.group(0) if m else ""


def looks_like_addr(s):
    return bool(extract_addr(s))


def pick_wallet_col(header, rows):
    # 1) by name
    for i, h in enumerate(header):
        if any(n in h.lower() for n in WALLET_NAMES):
            # verify it actually holds addresses
            if any(looks_like_addr(r[i]) for r in rows[:20] if i < len(r)):
                return i
    # 2) by content: the column with the most base58-looking values
    best, best_hits = None, 0
    ncol = len(header)
    for i in range(ncol):
        hits = sum(1 for r in rows[:50] if i < len(r) and looks_like_addr(r[i]))
        if hits > best_hits:
            best, best_hits = i, hits
    return best


def main():
    args = sys.argv[1:]
    label_override = None
    weight_col = None
    min_weight = None
    files = []
    i = 0
    while i < len(args):
        a = args[i]
        if a == "--label" and i + 1 < len(args):
            label_override = args[i + 1]; i += 2
        elif a == "--weight-col" and i + 1 < len(args):
            weight_col = args[i + 1].lower(); i += 2
        elif a == "--min-weight" and i + 1 < len(args):
            min_weight = float(args[i + 1]); i += 2
        else:
            files.append(a); i += 1

    if not files:
        print("usage: dune_to_kol.py <export.csv> [more.csv ...] "
              "[--label NAME] [--weight-col COL] [--min-weight X]", file=sys.stderr)
        sys.exit(1)

    weight_max = 3.0  # rescale weight-col values into [1.0, weight_max]
    seen = {}  # wallet -> (raw_weight, label)
    for path in files:
        try:
            with open(path, newline="") as f:
                reader = list(csv.reader(f))
        except FileNotFoundError:
            print(f"# skip (not found): {path}", file=sys.stderr)
            continue
        if not reader:
            continue
        header = reader[0]
        rows = reader[1:]
        wcol = pick_wallet_col(header, rows)
        if wcol is None:
            print(f"# no wallet column found in {path}", file=sys.stderr)
            continue
        wcol_i = None
        if weight_col:
            for idx, h in enumerate(header):
                if weight_col in h.lower():
                    wcol_i = idx; break
        src_label = label_override or path.rsplit("/", 1)[-1].rsplit(".", 1)[0]
        for r in rows:
            if wcol >= len(r):
                continue
            w = extract_addr(r[wcol])
            if not w:
                continue
            weight = 1.0
            if wcol_i is not None and wcol_i < len(r):
                try:
                    weight = float(r[wcol_i].replace(",", "").replace("$", ""))
                except ValueError:
                    weight = 1.0
            if min_weight is not None and weight < min_weight:
                continue
            # keep the higher-weight entry on duplicates
            if w not in seen or weight > seen[w][0]:
                seen[w] = (weight, src_label)

    # Rescale weight-col values into [1.0, weight_max] so they're usable as
    # entry-boost multipliers (raw PnL dollars would otherwise be nonsensical).
    if weight_col is not None and seen:
        vals = [w for (w, _) in seen.values()]
        lo, hi = min(vals), max(vals)
        span = hi - lo
        for k, (w, label) in list(seen.items()):
            norm = 1.0 if span <= 0 else 1.0 + (w - lo) / span * (weight_max - 1.0)
            seen[k] = (round(norm, 2), label)

    print(f"# {len(seen)} KOL wallets — generated by dune_to_kol.py")
    print("# format: wallet[,weight][,label]")
    for w, (weight, label) in sorted(seen.items(), key=lambda kv: kv[1][0], reverse=True):
        if weight != 1.0:
            print(f"{w},{weight:.4g},{label}")
        else:
            print(f"{w},1,{label}")


if __name__ == "__main__":
    main()
