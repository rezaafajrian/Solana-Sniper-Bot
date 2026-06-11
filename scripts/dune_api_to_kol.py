#!/usr/bin/env python3
"""
Pull KOL wallets straight from the Dune API (or a saved JSON file) into
kol_wallets.txt — no manual CSV downloads.

Setup:
  1. Get a Dune API key: dune.com -> Settings -> API.
  2. Find the QUERY ID behind each dashboard table: click the viz title to open
     its query; the URL is dune.com/queries/<QUERY_ID>/...  Use that number.

Usage:
  export DUNE_API_KEY=xxxxx
  python3 scripts/dune_api_to_kol.py 1234567 7654321 > kol_wallets.txt
  python3 scripts/dune_api_to_kol.py 1234567 --weight-col pnl --label alpha > kol_wallets.txt

  # offline / testing: pass a saved results JSON instead of a query id
  python3 scripts/dune_api_to_kol.py results.json > kol_wallets.txt

Options:
  --api-key KEY     Dune API key (else env DUNE_API_KEY)
  --label NAME      label all wallets from this source
  --weight-col COL  numeric column (e.g. pnl) -> per-wallet weight (normalized 1..3)
  --min-weight X    drop rows whose weight-col value is below X
  --limit N         max rows to request per query (default 1000)
"""

import json
import os
import sys
import urllib.request

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
from dune_to_kol import looks_like_addr, extract_addr, WALLET_NAMES  # noqa: E402

WEIGHT_MAX = 3.0


def fetch_rows(source, api_key, limit):
    """Return a list of row dicts from a Dune query id (via API) or a JSON file."""
    if source.endswith(".json") or os.path.exists(source):
        with open(source) as f:
            data = json.load(f)
    else:
        url = f"https://api.dune.com/api/v1/query/{source}/results?limit={limit}"
        req = urllib.request.Request(url, headers={"X-Dune-API-Key": api_key or ""})
        with urllib.request.urlopen(req, timeout=30) as resp:
            data = json.load(resp)
    # Dune shape: {"result": {"rows": [ {...}, ... ]}}
    return data.get("result", {}).get("rows", data.get("rows", []))


def pick_keys(rows, weight_col):
    if not rows:
        return None, None
    keys = list(rows[0].keys())
    wallet_key = None
    # by name first
    for k in keys:
        if any(n in k.lower() for n in WALLET_NAMES):
            if any(looks_like_addr(str(r.get(k, ""))) for r in rows[:20]):
                wallet_key = k
                break
    # by content
    if wallet_key is None:
        best, best_hits = None, 0
        for k in keys:
            hits = sum(1 for r in rows[:50] if looks_like_addr(str(r.get(k, ""))))
            if hits > best_hits:
                best, best_hits = k, hits
        wallet_key = best
    weight_key = None
    if weight_col:
        for k in keys:
            if weight_col in k.lower():
                weight_key = k
                break
    return wallet_key, weight_key


def main():
    args = sys.argv[1:]
    api_key = os.environ.get("DUNE_API_KEY")
    label_override = None
    weight_col = None
    min_weight = None
    limit = 1000
    sources = []
    i = 0
    while i < len(args):
        a = args[i]
        if a == "--api-key" and i + 1 < len(args):
            api_key = args[i + 1]; i += 2
        elif a == "--label" and i + 1 < len(args):
            label_override = args[i + 1]; i += 2
        elif a == "--weight-col" and i + 1 < len(args):
            weight_col = args[i + 1].lower(); i += 2
        elif a == "--min-weight" and i + 1 < len(args):
            min_weight = float(args[i + 1]); i += 2
        elif a == "--limit" and i + 1 < len(args):
            limit = int(args[i + 1]); i += 2
        else:
            sources.append(a); i += 1

    if not sources:
        print("usage: dune_api_to_kol.py <query_id|results.json> [...] "
              "[--api-key KEY] [--label NAME] [--weight-col COL] [--min-weight X] [--limit N]",
              file=sys.stderr)
        sys.exit(1)

    seen = {}  # wallet -> (raw_weight, label)
    for src in sources:
        try:
            rows = fetch_rows(src, api_key, limit)
        except Exception as e:  # noqa: BLE001
            print(f"# error fetching {src}: {e}", file=sys.stderr)
            continue
        if not rows:
            print(f"# no rows from {src}", file=sys.stderr)
            continue
        wallet_key, weight_key = pick_keys(rows, weight_col)
        if not wallet_key:
            print(f"# no wallet column found in {src}", file=sys.stderr)
            continue
        label = label_override or (src if not src.isdigit() else f"dune{src}")
        for r in rows:
            w = extract_addr(r.get(wallet_key, ""))
            if not w:
                continue
            weight = 1.0
            if weight_key is not None:
                try:
                    weight = float(str(r.get(weight_key, "1")).replace(",", "").replace("$", ""))
                except (ValueError, TypeError):
                    weight = 1.0
            if min_weight is not None and weight < min_weight:
                continue
            if w not in seen or weight > seen[w][0]:
                seen[w] = (weight, label)

    if weight_col is not None and seen:
        vals = [w for (w, _) in seen.values()]
        lo, hi = min(vals), max(vals)
        span = hi - lo
        for k, (w, lab) in list(seen.items()):
            norm = 1.0 if span <= 0 else 1.0 + (w - lo) / span * (WEIGHT_MAX - 1.0)
            seen[k] = (round(norm, 2), lab)

    print(f"# {len(seen)} KOL wallets — generated by dune_api_to_kol.py")
    print("# format: wallet[,weight][,label]")
    for w, (weight, label) in sorted(seen.items(), key=lambda kv: kv[1][0], reverse=True):
        print(f"{w},{weight:g},{label}")


if __name__ == "__main__":
    main()
