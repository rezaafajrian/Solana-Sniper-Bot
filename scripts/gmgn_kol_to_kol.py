#!/usr/bin/env python3
"""
Pull ACTIVE KOL / smart-money WALLETS from GMGN into kol_wallets.txt.
Unlike Dune (historical), GMGN's feed is live — these wallets are trading now.

Usage:
  python3 scripts/gmgn_kol_to_kol.py --api-key YOUR_GMGN_XAPIKEY --limit 200 >> kol_wallets.txt
  # or merge fresh:
  python3 scripts/gmgn_kol_to_kol.py --api-key KEY > kol_wallets.txt

Options:
  --api-key KEY   GMGN X-APIKEY (else env GMGN_API_KEY)
  --base URL      base url (default https://api.gmgn.ai)
  --chain C       sol | bsc | base (default sol)
  --limit N       rows per endpoint (default 200)
  --label NAME    label for these wallets (default gmgn)
  --debug         print the raw first row to stderr (to inspect fields)
"""
import json, os, re, sys, urllib.request

B58 = re.compile(r"^[1-9A-HJ-NP-Za-km-z]{32,44}$")
WALLET_KEYS = ("address", "wallet_address", "wallet", "maker", "maker_address",
               "user_address", "trader", "account", "owner", "signer", "user")
# token-ish keys to avoid mistaking a token mint for a wallet
TOKEN_KEYS = ("token", "mint", "contract", "pool", "pair", "ca")


def get(base, path, api_key, params):
    q = "&".join(f"{k}={v}" for k, v in params)
    url = f"{base.rstrip('/')}{path}?{q}"
    req = urllib.request.Request(url, headers={"X-APIKEY": api_key or "", "accept": "application/json"})
    try:
        with urllib.request.urlopen(req, timeout=30) as r:
            return json.load(r)
    except Exception as e:  # noqa: BLE001
        print(f"# error {path}: {e}", file=sys.stderr)
        return None


def rows_of(data):
    if data is None:
        return []
    d = data.get("data", data) if isinstance(data, dict) else data
    if isinstance(d, list):
        return d
    if isinstance(d, dict):
        for v in d.values():
            if isinstance(v, list):
                return v
    return []


def extract_wallet(row):
    if not isinstance(row, dict):
        return None
    # nested maker_info?
    nested = {}
    for k, v in row.items():
        if isinstance(v, dict):
            nested.update({f"{k}.{ik}": iv for ik, iv in v.items()})
    flat = {**row, **nested}
    # 1) named wallet keys (not token keys)
    for key, val in flat.items():
        kl = key.lower()
        if any(w in kl for w in WALLET_KEYS) and not any(t in kl for t in TOKEN_KEYS):
            s = str(val).strip()
            if B58.match(s):
                return s
    return None


def main():
    a = sys.argv[1:]
    api_key = os.environ.get("GMGN_API_KEY")
    base = "https://api.gmgn.ai"; chain = "sol"; limit = 200; label = "gmgn"; debug = False
    i = 0
    while i < len(a):
        if a[i] == "--api-key": api_key = a[i+1]; i += 2
        elif a[i] == "--base": base = a[i+1]; i += 2
        elif a[i] == "--chain": chain = a[i+1]; i += 2
        elif a[i] == "--limit": limit = int(a[i+1]); i += 2
        elif a[i] == "--label": label = a[i+1]; i += 2
        elif a[i] == "--debug": debug = True; i += 1
        else: i += 1

    seen = set()
    for path in ("/v1/user/smartmoney", "/v1/user/kol"):
        data = get(base, path, api_key, [("chain", chain), ("limit", str(limit))])
        rows = rows_of(data)
        if debug and rows:
            print(f"# DEBUG first row of {path}:\n# {json.dumps(rows[0])[:600]}", file=sys.stderr)
        for r in rows:
            w = extract_wallet(r)
            if w:
                seen.add(w)

    print(f"# {len(seen)} active GMGN KOL/smart-money wallets")
    for w in seen:
        print(f"{w},1.5,{label}")
    if not seen:
        print("# no wallets found — run again with --debug and paste the output", file=sys.stderr)


if __name__ == "__main__":
    main()
