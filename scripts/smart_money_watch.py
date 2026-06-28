#!/usr/bin/env python3
"""
SMART-MONEY WATCH — follow proven wallets in REAL TIME, market-wide.

The bot only scores tokens flowing through its own pump.fun feed. This adds the highest-
conviction intelligence-gathering mechanism in memecoins: watch a curated set of PROVEN
wallets (the bot's own ELITE/STRONG, clean, profitable wallets) and get pinged the instant
ANY of them buys ANYTHING, anywhere — "a wallet that's been early to winners just aped into
X." Following proven money beats almost any chart signal.

Built on Helius webhooks (free tier): register the wallet list, and Helius POSTs every
transaction those wallets make to a receiver you run; the receiver detects a BUY and alerts
Telegram. Isolated from the sniper bot — it's a separate process and only READS the wallet
reputation file.

Setup:
  export HELIUS_API_KEY=...        (free at helius.dev)
  export SMART_MONEY_WEBHOOK_URL=https://<your-vps>:8899   (public URL the receiver listens on)
  python3 scripts/smart_money_watch.py --sync     # pick top wallets + register the webhook
  python3 scripts/smart_money_watch.py --serve    # run the receiver (alerts on buys)
  python3 scripts/smart_money_watch.py --mock      # offline self-test (no network)
"""
import os, sys, csv, json, math, time, argparse, urllib.request, datetime as dt
from http.server import BaseHTTPRequestHandler, HTTPServer

REP = "momentum_wallet_rep.csv"
WALLETS_FILE = "smart_money_wallets.json"  # the curated set the receiver matches against
WSOL = {"So11111111111111111111111111111111111111112"}
CONF_K = 30.0


def env(key):
    v = os.environ.get(key)
    if v:
        return v
    try:
        for line in open(".env"):
            s = line.strip()
            if s.startswith(f"{key}=") and not s.startswith("#"):
                return s.split("=", 1)[1].strip()
    except OSError:
        pass
    return None


def telegram(text):
    tok, chat = env("TELEGRAM_BOT_TOKEN"), env("TELEGRAM_CHAT_ID")
    if not tok or not chat:
        return
    try:
        data = json.dumps({"chat_id": chat, "text": text[:3900], "disable_web_page_preview": True}).encode()
        req = urllib.request.Request(f"https://api.telegram.org/bot{tok}/sendMessage", data=data,
                                     headers={"content-type": "application/json"})
        urllib.request.urlopen(req, timeout=10)
    except Exception:
        pass


def pick_smart_wallets(rep=REP, top_n=100, min_samples=10):
    """Select the bot's PROVEN, clean, profitable wallets to follow."""
    if not os.path.exists(rep):
        return []
    def i(r, k):
        try: return int(float(r.get(k, "0") or "0"))
        except ValueError: return 0
    def fl(r, k):
        try: return float(r.get(k, "0") or "0")
        except ValueError: return 0.0
    out = []
    for r in csv.DictReader(open(rep, newline="")):
        n = i(r, "samples")
        if n < min_samples or i(r, "rugs_created") > 0:      # must be proven and never a rug-creator
            continue
        wins = i(r, "wins")
        avg_roi = fl(r, "roi_sum") / n if n else 0
        if avg_roi <= 0:                                      # must be net-profitable
            continue
        wr = wins / n
        conf = n / (n + CONF_K)
        score = (conf * wr + (1 - conf) * 0.5) * 100          # confidence-shrunk win rate
        out.append((score, r.get("wallet", ""), round(avg_roi, 3), n))
    out.sort(reverse=True)
    return [{"wallet": w, "score": round(s, 1), "avg_roi": roi, "trades": n}
            for s, w, roi, n in out[:top_n] if w]


def helius_sync(wallets, key, webhook_url):
    addrs = [w["wallet"] for w in wallets]
    base = f"https://api.helius.xyz/v0/webhooks?api-key={key}"
    body = json.dumps({"webhookURL": webhook_url, "transactionTypes": ["ANY"],
                       "accountAddresses": addrs, "webhookType": "enhanced"}).encode()
    # find an existing webhook to UPDATE, else CREATE
    try:
        existing = json.load(urllib.request.urlopen(urllib.request.Request(base), timeout=15))
    except Exception:
        existing = []
    wid = None
    for h in existing or []:
        if h.get("webhookURL") == webhook_url:
            wid = h.get("webhookID")
            break
    try:
        if wid:
            req = urllib.request.Request(f"https://api.helius.xyz/v0/webhooks/{wid}?api-key={key}",
                                         data=body, headers={"content-type": "application/json"}, method="PUT")
        else:
            req = urllib.request.Request(base, data=body, headers={"content-type": "application/json"}, method="POST")
        urllib.request.urlopen(req, timeout=20)
        return True
    except Exception as e:
        sys.stderr.write(f"  helius sync failed: {e}\n")
        return False


def helius_recent_buys(wallet, key, since_sig):
    """POLL a wallet's recent parsed transactions (Helius Enhanced Tx API) and return the
    (mints_bought, newest_signature). Works WITHOUT a public URL — the laptop pulls, nothing
    has to POST in — so this is the dry-run-friendly path."""
    url = f"https://api.helius.xyz/v0/addresses/{wallet}/transactions?api-key={key}&limit=15"
    try:
        txs = json.load(urllib.request.urlopen(urllib.request.Request(url), timeout=15))
    except Exception:
        return [], since_sig
    if not isinstance(txs, list):
        return [], since_sig
    buys, newest = [], (txs[0].get("signature") if txs else since_sig)
    for tx in txs:
        if since_sig and tx.get("signature") == since_sig:
            break  # reached the last batch we already processed
        for tt in tx.get("tokenTransfers") or []:
            if tt.get("toUserAccount") == wallet and tt.get("mint") and tt.get("mint") not in WSOL:
                buys.append(tt.get("mint"))
    return buys, newest


def poll_loop(wallets, key, interval, top, rate):
    """Dry-run-friendly smart-money watch: poll the top proven wallets for new buys and alert.
    No public URL needed. Modest wallet set + pacing to respect the Helius free tier."""
    sel = wallets[:top]
    wmeta = {w["wallet"]: w for w in sel}
    seen = {}
    try:
        seen = json.load(open("smart_money_seen.json"))
    except Exception:
        pass
    print(f"  polling {len(sel)} proven wallets every {interval}s (dry-run mode, no public URL). Ctrl-C to stop.")
    while True:
        for w in sel:
            buys, newsig = helius_recent_buys(w["wallet"], key, seen.get(w["wallet"]))
            seen[w["wallet"]] = newsig
            for mint in dict.fromkeys(buys):   # dedup, keep order
                m = wmeta.get(w["wallet"], {})
                msg = (f"🐋 SMART MONEY BUY\nwallet {w['wallet'][:10]}… (score {m.get('score','?')}, "
                       f"avg ROI {m.get('avg_roi','?')}, {m.get('trades','?')} trades)\nbought {mint}\n"
                       f"https://dexscreener.com/solana/{mint}")
                print(f"  {dt.datetime.now():%H:%M:%S} 🐋 {w['wallet'][:8]} -> {mint[:12]}")
                telegram(msg)
            time.sleep(rate)
        try:
            json.dump(seen, open("smart_money_seen.json.tmp", "w")); os.replace("smart_money_seen.json.tmp", "smart_money_seen.json")
        except OSError:
            pass
        time.sleep(interval)


def detect_buys(txns, tracked):
    """From a Helius enhanced-tx payload, yield (wallet, mint) where a tracked wallet BOUGHT
    a (non-SOL) token. Pragmatic: tracked wallet is the recipient of a token transfer."""
    hits = []
    for tx in txns if isinstance(txns, list) else []:
        for tt in tx.get("tokenTransfers") or []:
            to = tt.get("toUserAccount")
            mint = tt.get("mint")
            if to in tracked and mint and mint not in WSOL:
                hits.append((to, mint))
    return hits


def make_handler(tracked, wmeta):
    class H(BaseHTTPRequestHandler):
        def log_message(self, *a):  # quiet
            pass
        def do_POST(self):
            n = int(self.headers.get("content-length", 0) or 0)
            try:
                payload = json.loads(self.rfile.read(n) or b"[]")
            except Exception:
                payload = []
            self.send_response(200); self.end_headers(); self.wfile.write(b"ok")
            seen = set()
            for wallet, mint in detect_buys(payload, tracked):
                if (wallet, mint) in seen:
                    continue
                seen.add((wallet, mint))
                m = wmeta.get(wallet, {})
                msg = (f"🐋 SMART MONEY BUY\nwallet {wallet[:10]}… (score {m.get('score','?')}, "
                       f"avg ROI {m.get('avg_roi','?')}, {m.get('trades','?')} trades)\nbought {mint}\n"
                       f"https://dexscreener.com/solana/{mint}")
                print(f"  {dt.datetime.now():%H:%M:%S} {msg.splitlines()[0]} {wallet[:8]} -> {mint[:12]}")
                telegram(msg)
    return H


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--sync", action="store_true", help="pick top wallets + register the Helius webhook")
    ap.add_argument("--serve", action="store_true", help="run the webhook receiver (needs a PUBLIC url)")
    ap.add_argument("--poll", action="store_true", help="POLL wallets for buys — works on a laptop, no public url (dry-run friendly)")
    ap.add_argument("--mock", action="store_true", help="offline self-test")
    ap.add_argument("--top", type=int, default=100)
    ap.add_argument("--poll-top", type=int, default=12, help="how many top wallets to poll (keep modest for the Helius free tier)")
    ap.add_argument("--poll-interval", type=int, default=120, help="seconds between full polling rounds")
    ap.add_argument("--poll-rate", type=float, default=1.5, help="seconds between per-wallet calls")
    ap.add_argument("--port", type=int, default=8899)
    args = ap.parse_args()

    if args.mock:
        wallets = [{"wallet": "Wsmart111111111111111111111111111111111111", "score": 88, "avg_roi": 0.6, "trades": 30}]
        tracked = {w["wallet"] for w in wallets}
        wmeta = {w["wallet"]: w for w in wallets}
        payload = [{"tokenTransfers": [{"toUserAccount": "Wsmart111111111111111111111111111111111111",
                    "mint": "8Jx8AAHj86wbQgUTjGuj6GTTL5Ps3cqxKRTvpaJApump", "tokenAmount": 1000}]}]
        print("  MOCK: simulating a Helius webhook for a tracked smart-money buy…")
        for w, mint in detect_buys(payload, tracked):
            print(f"  ✓ detected: {w[:10]}… bought {mint}  → would alert Telegram")
        return

    wallets = pick_smart_wallets(top_n=args.top)
    if not wallets:
        print("  No proven wallets yet (need >=10 trades, +ROI, no rugs created). "
              "Let the bot learn first, then --sync.")
        return
    json.dump(wallets, open(WALLETS_FILE + ".tmp", "w"), indent=1)
    os.replace(WALLETS_FILE + ".tmp", WALLETS_FILE)
    print(f"  {len(wallets)} proven wallets selected → {WALLETS_FILE}")

    if args.sync:
        key = env("HELIUS_API_KEY"); url = env("SMART_MONEY_WEBHOOK_URL")
        if not key or not url:
            print("  set HELIUS_API_KEY and SMART_MONEY_WEBHOOK_URL to register the webhook.")
            return
        ok = helius_sync(wallets, key, url)
        print(f"  Helius webhook {'registered/updated ✅' if ok else 'FAILED'} for {len(wallets)} wallets → {url}")

    if args.poll:
        key = env("HELIUS_API_KEY")
        if not key:
            print("  set HELIUS_API_KEY (helius.dev, free) to poll. No public URL needed in poll mode.")
            return
        try:
            poll_loop(wallets, key, args.poll_interval, args.poll_top, args.poll_rate)
        except KeyboardInterrupt:
            pass

    if args.serve:
        tracked = {w["wallet"] for w in wallets}
        wmeta = {w["wallet"]: w for w in wallets}
        srv = HTTPServer(("0.0.0.0", args.port), make_handler(tracked, wmeta))
        print(f"  receiver listening on :{args.port}, watching {len(tracked)} wallets. Ctrl-C to stop.")
        try:
            srv.serve_forever()
        except KeyboardInterrupt:
            pass


if __name__ == "__main__":
    main()
