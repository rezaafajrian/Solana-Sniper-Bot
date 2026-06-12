#!/usr/bin/env python3
"""
Pull labeled wallets (KOLs, smart money, etc.) from the Vybe Network API into
kol_wallets.txt.

Setup: get an API key at alpha.vybenetwork.com (or docs.vybenetwork.com).

Usage:
  python3 scripts/vybe_to_kol.py --api-key YOUR_VYBE_KEY --labels KOL --limit 500 >> kol_wallets.txt
  python3 scripts/vybe_to_kol.py --api-key KEY --labels KOL --labels "SMART_MONEY" > kol_wallets.txt
  # if the path differs, override it:
  python3 scripts/vybe_to_kol.py --api-key KEY --path /wallets/known-accounts --debug

Options:
  --api-key KEY   Vybe API key (else env VYBE_API_KEY)
  --base URL      base url (default https://api.vybenetwork.com)
  --path P        endpoint path (default /account/known-accounts)
  --labels L      label filter, repeatable (e.g. KOL). Omit for all.
  --limit N       max rows (default 1000)
  --weight W      weight to assign each wallet (default 1.5)
  --label NAME    source label written to the file (default vybe)
  --debug         print the raw first row to stderr to inspect fields
"""
import json, os, re, sys, urllib.parse, urllib.request

B58 = re.compile(r"^[1-9A-HJ-NP-Za-km-z]{32,44}$")
WALLET_KEYS = ("owneraddress", "address", "wallet", "pubkey", "account", "owner", "walletaddress")
TOKEN_KEYS = ("token", "mint", "contract", "pool")


def fetch(base, path, api_key, params):
    qs = urllib.parse.urlencode(params, doseq=True)
    url = f"{base.rstrip('/')}{path}?{qs}"
    req = urllib.request.Request(url, headers={"X-API-KEY": api_key or "", "accept": "application/json"})
    with urllib.request.urlopen(req, timeout=30) as r:
        return json.load(r)


def rows_of(data):
    if isinstance(data, list):
        return data
    if isinstance(data, dict):
        # common containers: accounts, data, result, wallets
        for key in ("accounts", "data", "result", "results", "wallets", "knownAccounts"):
            v = data.get(key)
            if isinstance(v, list):
                return v
        for v in data.values():
            if isinstance(v, list):
                return v
    return []


def extract_wallet(row):
    if not isinstance(row, dict):
        return None
    for k, v in row.items():
        kl = k.lower()
        if any(w in kl for w in WALLET_KEYS) and not any(t in kl for t in TOKEN_KEYS):
            s = str(v).strip()
            if B58.match(s):
                return s
    # fallback: any base58 string value
    for v in row.values():
        if isinstance(v, str) and B58.match(v.strip()):
            return v.strip()
    return None


def extract_name(row, default):
    for k in ("name", "entityName", "label", "friendlyName"):
        v = row.get(k)
        if isinstance(v, str) and v.strip():
            return re.sub(r"[,\s]+", "_", v.strip())[:24]
    return default


def main():
    a = sys.argv[1:]
    api_key = os.environ.get("VYBE_API_KEY")
    base = "https://api.vybenetwork.com"; path = "/account/known-accounts"
    labels = []; limit = 1000; weight = 1.5; src_label = "vybe"; debug = False
    i = 0
    while i < len(a):
        if a[i] == "--api-key": api_key = a[i+1]; i += 2
        elif a[i] == "--base": base = a[i+1]; i += 2
        elif a[i] == "--path": path = a[i+1]; i += 2
        elif a[i] == "--labels": labels.append(a[i+1]); i += 2
        elif a[i] == "--limit": limit = int(a[i+1]); i += 2
        elif a[i] == "--weight": weight = float(a[i+1]); i += 2
        elif a[i] == "--label": src_label = a[i+1]; i += 2
        elif a[i] == "--debug": debug = True; i += 1
        else: i += 1

    params = [("limit", str(limit))]
    for lb in labels:
        params.append(("labels", lb))
    try:
        data = fetch(base, path, api_key, params)
    except Exception as e:  # noqa: BLE001
        print(f"# error: {e}", file=sys.stderr)
        print("# try a different --path (e.g. /wallets/known-accounts) and --debug", file=sys.stderr)
        sys.exit(1)

    rows = rows_of(data)
    if debug:
        sample = json.dumps(rows[0]) if rows else json.dumps(data)[:600]
        print(f"# DEBUG first row:\n# {sample[:600]}", file=sys.stderr)

    seen = {}
    for r in rows:
        w = extract_wallet(r)
        if w and w not in seen:
            seen[w] = extract_name(r, src_label)

    print(f"# {len(seen)} Vybe wallets (labels={labels or 'all'})")
    for w, name in seen.items():
        print(f"{w},{weight:g},{name}")
    if not seen:
        print("# no wallets parsed — re-run with --debug and paste the output", file=sys.stderr)


if __name__ == "__main__":
    main()
