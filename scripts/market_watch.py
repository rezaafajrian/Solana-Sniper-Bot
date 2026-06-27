#!/usr/bin/env python3
"""
MARKET WATCH — "Ones to Watch" for the WHOLE market, via Birdeye. ISOLATED from the sniper.

WHY THIS EXISTS
  The sniper's built-in watchlist can only see tokens flowing through its pump.fun feed —
  i.e. fresh, PRE-BONDING bonding-curve coins, the single most rug-prone thing on Solana.
  That's why trash/rugs kept showing up there. This is a completely separate scanner that
  watches the ENTIRE Solana market through Birdeye's real market data (liquidity, 24h volume,
  holder count, market cap, price action) and surfaces only quality, liquid, established
  movers — the lifecycle AFTER bonding, where a pump is real and exitable.

ISOLATION (by design — never interferes with the sniper)
  • Separate process. Does NOT import, call, or share state with the Rust bot.
  • Writes ONLY its own files (market_watch.json, market_watch_state.json).
  • Uses its own Birdeye key + its own rate budget. Runs at whatever cadence you set.
  • Read-only toward everything else. It cannot affect a single sniper decision.

WHAT IT DOES
  1. Pulls the top tokens by 24h volume (Token List V3) + the trending list from Birdeye.
  2. Hard-filters for quality / anti-rug: min liquidity, min 24h volume, min holders, market
     cap band, liquidity-to-mcap sanity, and "not already dead" price action.
  3. Scores the survivors (volume surge + price momentum + liquidity health + holders +
     trending) into a 0..100 watch score.
  4. Writes the ranked list to market_watch.json and ALERTS new high-conviction names to
     Telegram ("📈 ONE TO WATCH — potential pump").

SETUP
  export BIRDEYE_API_KEY=...           # birdeye.so (Starter plan reaches Token List V3)
  export TELEGRAM_BOT_TOKEN=... TELEGRAM_CHAT_ID=...   # optional, for alerts
  python3 scripts/market_watch.py --once     # one scan
  python3 scripts/market_watch.py            # loop forever (every MARKET_INTERVAL_SECS)
  python3 scripts/market_watch.py --mock     # offline self-test (no network)

All thresholds are env-tunable (MARKET_* below). Defaults are deliberately STRICT so trash
never shows up — loosen them if the list is too quiet.
"""
import os, sys, csv, json, time, argparse, urllib.request, urllib.parse, urllib.error, datetime as dt

BIRDEYE = "https://public-api.birdeye.so"
OUT = "market_watch.json"
STATE = "market_watch_state.json"        # last-alerted map, so we don't spam the same token
HISTORY = "market_watch_history.json"    # per-token snapshots over time → growth/accel signals
STUDY = "runners_to_study.txt"           # tokens that ALREADY ran → feed the research/study layer
# never "watch" the base/stable assets
SKIP = {
    "So11111111111111111111111111111111111111112",  # wSOL
    "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v",  # USDC
    "Es9vMFrzaCERmJfrF4H2FYD4KCoNkY11McCe8BenwNYB",  # USDT
}


# ---------------------------------------------------------------------------
# config / helpers (same conventions as the other research-layer scripts)
# ---------------------------------------------------------------------------
def env(key, default=None):
    v = os.environ.get(key)
    if v is not None and v.strip() != "":
        return v.strip()
    try:
        for line in open(".env"):
            s = line.strip()
            if not s or s.startswith("#"):
                continue
            if s.startswith("export "):              # tolerate `export KEY=val`
                s = s[len("export "):].lstrip()
            if "=" not in s:
                continue
            k, val = s.split("=", 1)
            if k.strip() == key:                     # tolerate spaces: `KEY = val`
                val = val.strip().strip('"').strip("'")  # tolerate quotes
                if val:                              # skip empty/placeholder lines, keep looking
                    return val
    except OSError:
        pass
    return default


def envf(key, default):
    try:
        return float(env(key, default))
    except (TypeError, ValueError):
        return float(default)


# thresholds — STRICT by default so the list is quality-only (USD units)
MIN_LIQ_USD   = envf("MARKET_MIN_LIQ_USD", 20000)      # real, exitable liquidity
MIN_VOL_USD   = envf("MARKET_MIN_VOL24_USD", 50000)    # real 24h turnover
MIN_HOLDERS   = envf("MARKET_MIN_HOLDERS", 200)        # anti-rug: a crowd, not 5 wallets
MIN_MC_USD    = envf("MARKET_MIN_MC_USD", 50000)       # not dust
MAX_MC_USD    = envf("MARKET_MAX_MC_USD", 50_000_000)  # still has room to run
MIN_LIQ_MC    = envf("MARKET_MIN_LIQ_MC", 0.03)        # liquidity >= 3% of mcap (exit sanity)
MAX_DD_24H    = envf("MARKET_MAX_DD_24H", -60)         # skip if already down >60% in 24h (dying)
MAX_PUMP_24H  = envf("MARKET_MAX_PUMP_24H", 100)       # ABOVE this it ALREADY RAN → study, don't watch
HISTORY_HOURS = envf("MARKET_HISTORY_HOURS", 6)        # how long to keep per-token snapshots
ALERT_SCORE   = envf("MARKET_ALERT_SCORE", 70)         # Telegram alert threshold
SCAN_LIMIT    = int(envf("MARKET_SCAN_LIMIT", 50))     # tokens per list call (some plans cap at 50)
ENRICH_TOP    = int(envf("MARKET_ENRICH_TOP", 0))      # per-token overview calls (0 = rely on list; saves CU)
RATE_DELAY    = envf("MARKET_RATE_DELAY", 1.2)         # min seconds between Birdeye calls (free tier ~1 rps)
INTERVAL      = int(envf("MARKET_INTERVAL_SECS", 0))   # 0 = AUTO-pace from the CU budget (recommended)
ALERT_COOLDOWN = int(envf("MARKET_ALERT_COOLDOWN_SECS", 3600))  # per-token re-alert gap
CU_COOLDOWN   = int(envf("MARKET_CU_COOLDOWN_SECS", 3600))      # back off this long when CU quota is hit
PAGES         = int(envf("MARKET_PAGES", 1))           # pages (of SCAN_LIMIT) per sort dimension
# CU BUDGET — make a free key last a MONTH. The scanner spreads CU_BUDGET across BUDGET_DAYS
# and auto-paces its scan interval so it can't burn the quota early; it tracks usage (reset
# monthly) and stops before going over.
CU_BUDGET     = int(envf("MARKET_CU_BUDGET", 30000))   # free Standard ~30k CU/month
CU_PER_CALL   = envf("MARKET_CU_PER_CALL", 30)         # estimated CU per Birdeye call (tune to your plan)
BUDGET_DAYS   = int(envf("MARKET_BUDGET_DAYS", 30))    # spread the budget across this many days
RESERVE_FRAC  = envf("MARKET_BUDGET_RESERVE", 0.10)    # keep this fraction of budget as a safety margin
USE_TRENDING  = env("MARKET_USE_TRENDING", "false").lower() == "true"  # +1 call/scan; off by default to save CU
BUDGET_FILE   = "market_watch_budget.json"             # {month, cu_used} — persists across restarts
# COVERAGE vs COST. Each sort × page is a Birdeye call that costs compute units (CU). The free
# plan's monthly CU runs out fast, so the DEFAULT is frugal: 1 sort (volume), 1 page. On a paid
# plan, widen it back out for maximum recall by setting MARKET_SORTS to the full list:
#   volume_24h_usd,price_change_24h_percent,volume_24h_change_percent,recent_listing_time,liquidity
SORTS = [s.strip() for s in env("MARKET_SORTS", "volume_24h_usd").split(",") if s.strip()]


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


_last_call = [0.0]
_cu_exhausted = [False]   # set True when Birdeye reports the monthly compute-unit quota is gone
_calls = [0]              # actual HTTP calls made this process (for CU accounting)
def _throttle():
    """Global pacing so we stay under the Birdeye rate limit (free tier ~1 rps)."""
    wait = RATE_DELAY - (time.time() - _last_call[0])
    if wait > 0:
        time.sleep(wait)
    _last_call[0] = time.time()


# Sentinel telling fetch_token_list "the call ERRORED" vs "returned empty data" — so a 429
# never triggers the legacy fallback (which would just 429 again and double the storm).
ERRORED = object()


def _bget(path, key, params=None, retries=4):
    """GET a Birdeye endpoint with throttling + backoff. Returns parsed JSON, or ERRORED on
    a transport/rate error (vs None for a clean-but-empty body)."""
    url = f"{BIRDEYE}{path}"
    if params:
        url += "?" + urllib.parse.urlencode(params)
    h = {"X-API-KEY": key, "x-chain": "solana", "accept": "application/json", "User-Agent": "market-watch"}
    for attempt in range(retries):
        _throttle()
        _calls[0] += 1
        try:
            return json.load(urllib.request.urlopen(urllib.request.Request(url, headers=h), timeout=20))
        except urllib.error.HTTPError as e:
            if e.code == 429 or e.code >= 500:
                ra = e.headers.get("Retry-After") if e.headers else None
                back = float(ra) if (ra and str(ra).isdigit()) else RATE_DELAY * (2 ** attempt)
                back = min(back, 30.0)
                sys.stderr.write(f"  birdeye {path} {e.code} — backing off {back:.1f}s (try {attempt+1}/{retries})\n")
                time.sleep(back)
                continue
            body = ""
            try:
                body = e.read().decode("utf-8", "replace")[:300]
            except Exception:
                pass
            if "compute unit" in body.lower() or "usage limit" in body.lower():
                _cu_exhausted[0] = True   # quota gone — the loop will back off instead of hammering
                return ERRORED
            sys.stderr.write(f"  birdeye {path} failed: HTTP {e.code} {e.reason} — {body}\n")
            if params:
                sys.stderr.write(f"    (params: {urllib.parse.urlencode(params)})\n")
            return ERRORED
        except Exception as e:
            sys.stderr.write(f"  birdeye {path} failed: {e}\n")
            return ERRORED
    sys.stderr.write(f"  birdeye {path}: gave up after {retries} tries (rate limited)\n")
    return ERRORED


def g(row, *keys, default=0.0):
    """Field getter tolerant of Birdeye's camelCase/snake_case spelling differences across
    the V1/V3/overview endpoints (e.g. v24hUSD vs volume_24h_usd, mc vs market_cap)."""
    for k in keys:
        if isinstance(row, dict) and row.get(k) is not None:
            try:
                return float(row[k])
            except (TypeError, ValueError):
                return row[k]
    return default


# ---------------------------------------------------------------------------
# Birdeye fetchers
# ---------------------------------------------------------------------------
def _data(d):
    """Pull the token rows from a Birdeye response, ERRORED/None-safe."""
    if not isinstance(d, dict):
        return None
    data = d.get("data") or {}
    return data.get("items") or data.get("tokens")


def fetch_token_list(key, sort_by, limit, offset=0):
    """One page of the Token List V3, sorted by `sort_by`. Falls back to the legacy V1 list
    for the volume sort whenever V3 returns nothing OR errors (e.g. a 400 because the plan
    caps the limit or doesn't expose V3) — the V1 list is what most keys can actually reach."""
    st = "desc" if sort_by != "recent_listing_time" else "asc"
    d = _bget("/defi/v3/token/list", key,
              {"sort_by": sort_by, "sort_type": st, "offset": offset, "limit": min(limit, 50)})
    rows = _data(d) if d is not ERRORED else None
    if not rows and offset == 0 and sort_by == "volume_24h_usd":  # legacy V1 fallback (volume only)
        d2 = _bget("/defi/tokenlist", key,
                   {"sort_by": "v24hUSD", "sort_type": "desc", "offset": 0, "limit": min(limit, 50)})
        rows = _data(d2) if d2 is not ERRORED else []
    return rows or []


def gather_candidates(key):
    """MAXIMIZE recall: union the market across every sort dimension + pages + trending, so a
    surging degen that isn't yet top-volume still gets caught. Dedup by address."""
    by_addr = {}
    trank = {}
    if USE_TRENDING:
        for t in fetch_trending(key):
            a = t.get("address") or t.get("mint")
            if a and a not in SKIP:
                trank[a] = len(trank)
                by_addr.setdefault(a, {}).update(t)
    for sort_by in SORTS:
        for page in range(PAGES):
            rows = fetch_token_list(key, sort_by, SCAN_LIMIT, offset=page * SCAN_LIMIT)
            if not rows:
                break  # no more pages / unsupported sort -> move on
            for t in rows:
                a = t.get("address") or t.get("mint")
                if a and a not in SKIP and isinstance(t, dict):
                    by_addr.setdefault(a, {}).update(t)
    return by_addr, trank


def fetch_trending(key, limit=20):
    d = _bget("/defi/token_trending", key,
              {"sort_by": "rank", "sort_type": "asc", "offset": 0, "limit": min(limit, 20)})
    return (_data(d) or []) if d is not ERRORED else []


def fetch_overview(key, mint):
    d = _bget("/defi/token_overview", key, {"address": mint})
    return (d.get("data") or {}) if isinstance(d, dict) else {}


# ---------------------------------------------------------------------------
# per-token history → growth/acceleration (the real "about to run" signal)
# ---------------------------------------------------------------------------
def load_history():
    try:
        return json.load(open(HISTORY))
    except Exception:
        return {}


def update_history(hist, mint, snap, now):
    """Append this scan's snapshot for a token and keep the recent window. Returns the series."""
    arr = hist.get(mint, [])
    arr.append(snap)
    cutoff = now - HISTORY_HOURS * 3600
    arr = [s for s in arr if s.get("ts", 0) >= cutoff][-40:]
    hist[mint] = arr
    return arr


def growth(arr, key):
    """Fractional change PER HOUR of `key` across the snapshot history (None if <2 points).
    This is what tells us holders/volume are RISING — accumulation in progress — before price
    has moved. The whole point: catch the runner forming, not after it ran."""
    if not arr or len(arr) < 2:
        return None
    old, new = arr[0], arr[-1]
    span_h = max((new.get("ts", 0) - old.get("ts", 0)) / 3600.0, 1e-6)
    base = old.get(key, 0.0) or 0.0
    if base <= 0:
        return None
    return (new.get(key, 0.0) - base) / base / span_h


# ---------------------------------------------------------------------------
# quality filter + scoring
# ---------------------------------------------------------------------------
def passes_filters(t):
    """Hard anti-rug / quality floors. Returns (ok, reason_if_not)."""
    liq = g(t, "liquidity")
    vol = g(t, "v24hUSD", "volume_24h_usd", "v24h_usd")
    mc  = g(t, "mc", "market_cap", "marketCap")
    holders = g(t, "holder", "holders", "holder_count", default=-1)
    dd24 = g(t, "priceChange24hPercent", "price_change_24h_percent", default=0.0)
    if liq < MIN_LIQ_USD:                      return False, "thin liquidity"
    if vol < MIN_VOL_USD:                       return False, "low volume"
    if mc and (mc < MIN_MC_USD or mc > MAX_MC_USD): return False, "mcap out of band"
    if mc and liq / mc < MIN_LIQ_MC:            return False, "liquidity vs mcap (exit risk)"
    if holders >= 0 and holders < MIN_HOLDERS:  return False, "too few holders"
    if dd24 <= MAX_DD_24H:                       return False, "already dumping"
    return True, ""


def clamp01(x):
    return 0.0 if x < 0 else (1.0 if x > 1 else x)


def already_ran(t):
    """True if this token has ALREADY made its big move (24h change above the ceiling). These
    go to the STUDY pile (learn why they ran), NOT the watch list."""
    return g(t, "priceChange24hPercent", "price_change_24h_percent", default=0.0) > MAX_PUMP_24H


def score(t, arr, trending_rank=None):
    """EARLY-RUNNER score (0..100). The goal is to catch accumulation BEFORE the move, not to
    reward a pump that already happened. Leading signals: volume ACCELERATING and holders
    GROWING while price is still EARLY (not yet mooned), on a real/liquid/small-enough token."""
    liq = g(t, "liquidity")
    mc  = g(t, "mc", "market_cap", "marketCap") or 1.0
    v24 = g(t, "v24hUSD", "volume_24h_usd", default=0.0)
    v1  = g(t, "v1hUSD", "volume_1h_usd", default=0.0)
    pc24 = g(t, "priceChange24hPercent", "price_change_24h_percent", default=0.0)

    # 1) volume ACCELERATION: 1h rate vs the 24h average rate (the move building up)
    accel = (v1 / (v24 / 24.0)) if v24 > 0 else 0.0
    accel_term = clamp01((accel - 1.0) / 2.0)                  # 3x avg rate -> full
    # 2) EARLINESS: reward NOT-yet-pumped; the closer to the already-ran ceiling, the lower
    earliness_term = clamp01((MAX_PUMP_24H - max(pc24, 0.0)) / max(MAX_PUMP_24H, 1.0))
    # 3) turnover: real interest relative to size
    turnover_term = clamp01(v24 / max(mc, 1.0))
    # 4) liquidity health (exitability)
    liq_term = clamp01(liq / mc / 0.15)
    # 5) room to run: smaller mcap = more upside left
    room_term = clamp01(1.0 - mc / max(MAX_MC_USD, 1.0))
    base = (0.30 * accel_term + 0.25 * earliness_term + 0.20 * turnover_term
            + 0.15 * liq_term + 0.10 * room_term) * 100.0
    # GROWTH BONUS — the real leading indicator, available once we've watched the token across
    # scans: holders and volume RISING over time = accumulation in progress, before price moves.
    hg = growth(arr, "holders")
    vg = growth(arr, "vol24")
    if hg is not None:
        base += clamp01(hg / 0.20) * 15.0                     # +20%/hr holder growth -> +15
    if vg is not None:
        base += clamp01(vg / 0.50) * 10.0                     # +50%/hr volume growth -> +10
    if trending_rank is not None:
        base += max(0.0, 8.0 - trending_rank * 0.4)
    return round(min(base, 100.0), 1)


# ---------------------------------------------------------------------------
# scan
# ---------------------------------------------------------------------------
def scan(key):
    # MAXIMIZE: gather candidates from every angle (volume, gainers, volume-risers, new
    # listings, liquidity, trending) across pages — then filter hard so only quality survives.
    by_addr, trank = gather_candidates(key)
    print(f"  gathered {len(by_addr)} unique tokens across {len(SORTS)} sorts × {PAGES} pages + trending")

    # cheap filter first (on list fields), then enrich the top survivors for holders
    prelim = []
    for a, t in by_addr.items():
        liq = g(t, "liquidity"); vol = g(t, "v24hUSD", "volume_24h_usd")
        if liq >= MIN_LIQ_USD and vol >= MIN_VOL_USD:
            prelim.append((vol, a, t))
    prelim.sort(reverse=True)

    hist = load_history()
    now = int(time.time())
    watch, study = [], []
    for _, a, t in prelim[:ENRICH_TOP]:
        if g(t, "holder", "holders", "holder_count", default=-1) < 0:
            ov = fetch_overview(key, a)
            if ov:
                t.update(ov)
        ok, why = passes_filters(t)
        if not ok:
            continue
        # record this scan's snapshot so we can measure holder/volume GROWTH over time
        arr = update_history(hist, a, {
            "ts": now,
            "holders": g(t, "holder", "holders", "holder_count", default=0.0),
            "vol24": g(t, "v24hUSD", "volume_24h_usd", default=0.0),
            "mcap": g(t, "mc", "market_cap", "marketCap", default=0.0),
        }, now)
        row = {
            "mint": a,
            "symbol": t.get("symbol") or "",
            "name": t.get("name") or "",
            "score": score(t, arr, trank.get(a)),
            "liquidity_usd": round(g(t, "liquidity"), 0),
            "volume24h_usd": round(g(t, "v24hUSD", "volume_24h_usd"), 0),
            "mcap_usd": round(g(t, "mc", "market_cap", "marketCap"), 0),
            "holders": int(g(t, "holder", "holders", "holder_count", default=0)),
            "holder_growth_hr": round((growth(arr, "holders") or 0.0) * 100, 1),  # %/hr
            "price_change_1h": round(g(t, "priceChange1hPercent", "price_change_1h_percent", default=0.0), 1),
            "price_change_24h": round(g(t, "priceChange24hPercent", "price_change_24h_percent", default=0.0), 1),
            "trending_rank": trank.get(a, None),
        }
        # ALREADY RAN → study pile (learn the pattern). Still EARLY → the watch list.
        (study if already_ran(t) else watch).append(row)
    # keep history only for tokens we still track, then persist
    save_json(HISTORY, {k: v for k, v in hist.items() if k in {r["mint"] for r in watch + study}})
    watch.sort(key=lambda x: x["score"], reverse=True)
    study.sort(key=lambda x: x["price_change_24h"], reverse=True)
    return watch, study


def load_state():
    try:
        return json.load(open(STATE))
    except Exception:
        return {}


def save_json(path, obj):
    json.dump(obj, open(path + ".tmp", "w"), indent=1)
    os.replace(path + ".tmp", path)


# ---------------------------------------------------------------------------
# CU budget — make a free key last a month (auto-pace + monthly reset)
# ---------------------------------------------------------------------------
def _this_month():
    return dt.datetime.now().strftime("%Y-%m")


def load_budget():
    try:
        b = json.load(open(BUDGET_FILE))
    except Exception:
        b = {}
    if b.get("month") != _this_month():          # new billing month -> quota refills
        b = {"month": _this_month(), "cu_used": 0}
    return b


def calls_per_scan():
    return max(len(SORTS) * PAGES + (1 if USE_TRENDING else 0) + ENRICH_TOP, 1)


def seconds_until_reset():
    """Seconds until the start of next month (when the CU quota refills)."""
    now = dt.datetime.now()
    nxt = dt.datetime(now.year + 1, 1, 1) if now.month == 12 else dt.datetime(now.year, now.month + 1, 1)
    return max((nxt - now).total_seconds(), 3600)


def usable_budget():
    return CU_BUDGET * (1.0 - RESERVE_FRAC)


def auto_interval(cu_used=0):
    """Adaptive pacing: spread the REMAINING budget over the time left until the monthly
    reset, so the key lasts the whole month no matter when you (re)start it."""
    remaining = max(usable_budget() - cu_used, 0)
    cu_per_scan = calls_per_scan() * CU_PER_CALL
    scans_left = max(remaining / max(cu_per_scan, 1), 1)
    return max(int(seconds_until_reset() / scans_left), 60)   # never tighter than 60s


def append_study(study):
    """Record already-ran tokens (deduped) for the research/study layer to dissect later —
    'why did THIS one run?' is how the watchlist's pattern keeps improving."""
    if not study:
        return
    try:
        seen = set(l.strip() for l in open(STUDY)) if os.path.exists(STUDY) else set()
    except OSError:
        seen = set()
    new = [r["mint"] for r in study if r["mint"] not in seen]
    if new:
        with open(STUDY, "a") as f:
            for m in new:
                f.write(m + "\n")


def run_once(key):
    watch, study = scan(key)
    save_json(OUT, {"updated": int(time.time()), "count": len(watch),
                    "watchlist": watch, "studied": study[:20]})
    append_study(study)
    # alert NEW early runners (dedup with a cooldown)
    state = load_state()
    now = int(time.time())
    alerts = 0
    for r in watch:
        if r["score"] < ALERT_SCORE:
            continue
        last = state.get(r["mint"], 0)
        if now - last < ALERT_COOLDOWN:
            continue
        state[r["mint"]] = now
        alerts += 1
        msg = (f"🌱 EARLY RUNNER forming — potential pump (not yet run)\n"
               f"{r['symbol'] or r['mint'][:8]}  (score {r['score']})\n"
               f"liq ${r['liquidity_usd']:,.0f} · vol24h ${r['volume24h_usd']:,.0f} · "
               f"mc ${r['mcap_usd']:,.0f} · {r['holders']} holders (+{r['holder_growth_hr']:.0f}%/hr)\n"
               f"1h {r['price_change_1h']:+.1f}% · 24h {r['price_change_24h']:+.1f}% (still early)\n"
               f"https://birdeye.so/token/{r['mint']}?chain=solana\n"
               f"https://dexscreener.com/solana/{r['mint']}")
        print(f"  {dt.datetime.now():%H:%M:%S} EARLY {r['symbol'] or r['mint'][:8]} score {r['score']} (+{r['holder_growth_hr']:.0f}%/hr holders)")
        telegram(msg)
    state = {k: v for k, v in state.items() if now - v < ALERT_COOLDOWN * 6}
    save_json(STATE, state)
    print(f"  scanned → {len(watch)} EARLY on watch, {len(study)} already-ran → study, {alerts} new alert(s) → {OUT}")
    return watch


# ---------------------------------------------------------------------------
def mock():
    """Offline self-test: prove we WATCH the early accumulator, STUDY the already-ran, and
    cut the rug + the dying token — with no network."""
    print("  MOCK: early-runner detection on synthetic market data (no network)…")
    EARLY = {"address": "EarlyAccum1111111111111111111111111111111", "symbol": "EARLY", "liquidity": 80000,
             "v24hUSD": 900000, "v1hUSD": 180000, "mc": 600000, "holder": 900,
             "priceChange1hPercent": 6, "priceChange24hPercent": 18}   # flat-ish price, volume SURGING
    RAN   = {"address": "AlreadyRan2222222222222222222222222222222", "symbol": "RAN", "liquidity": 240000,
             "v24hUSD": 30000000, "v1hUSD": 500000, "mc": 1200000, "holder": 3300,
             "priceChange1hPercent": 9, "priceChange24hPercent": 335}  # already +335% → STUDY
    RUG   = {"address": "RugTrap33333333333333333333333333333333333", "symbol": "RUG", "liquidity": 3000,
             "v24hUSD": 40000, "v1hUSD": 9000, "mc": 900000, "holder": 18,
             "priceChange1hPercent": 140, "priceChange24hPercent": 95}
    DEAD  = {"address": "Dying444444444444444444444444444444444444", "symbol": "DEAD", "liquidity": 60000,
             "v24hUSD": 200000, "v1hUSD": 4000, "mc": 800000, "holder": 600,
             "priceChange1hPercent": -12, "priceChange24hPercent": -82}
    # a 2-point history showing holders climbing (+ that's the accumulation tell)
    now = int(time.time())
    arr_early = [{"ts": now - 3600, "holders": 700, "vol24": 500000},
                 {"ts": now, "holders": 900, "vol24": 900000}]
    for t, arr in [(EARLY, arr_early), (RAN, []), (RUG, []), (DEAD, [])]:
        ok, why = passes_filters(t)
        if not ok:
            print(f"   {t['symbol']:>5}  cut   REJECT ({why})")
        elif already_ran(t):
            print(f"   {t['symbol']:>5}  STUDY already ran (+{t['priceChange24hPercent']:.0f}% 24h) → research pile")
        else:
            print(f"   {t['symbol']:>5}  WATCH early runner, score {score(t, arr)}")
    assert passes_filters(EARLY)[0] and not already_ran(EARLY), "early accumulator must be WATCHED"
    assert passes_filters(RAN)[0] and already_ran(RAN), "already-ran must go to STUDY, not watch"
    assert not passes_filters(RUG)[0], "rug must be cut"
    assert not passes_filters(DEAD)[0], "dying token must be cut"
    # the holder-growth bonus must lift the early runner above a no-history version of itself
    assert score(EARLY, arr_early) > score(EARLY, []), "rising holders must raise the score"
    print("  ✅ WATCH the early accumulator · STUDY the already-ran · cut the rug & the dead.")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--once", action="store_true", help="one scan then exit")
    ap.add_argument("--mock", action="store_true", help="offline self-test (no network)")
    args = ap.parse_args()

    if args.mock:
        mock()
        return

    key = env("BIRDEYE_API_KEY")
    if not key:
        print("  ❌ BIRDEYE_API_KEY not set. Get one at birdeye.so and add it to .env, then re-run.")
        sys.exit(1)

    # Preflight: exercise the REAL list path (V3 with V1 fallback) so the verdict matches
    # what the scan will actually get.
    pf = fetch_token_list(key, "volume_24h_usd", 5)
    if pf:
        print(f"  ✅ Birdeye reachable — got {len(pf)} tokens from the list endpoint.")
    elif _cu_exhausted[0]:
        print("  ⛔ Birdeye COMPUTE-UNIT quota is exhausted for this billing period.")
        print("     This is a plan limit, not a bug. Options:")
        print("       • wait for the monthly CU reset, or upgrade at birdeye.so (Starter+),")
        print("       • keep it frugal: MARKET_SORTS=volume_24h_usd, MARKET_PAGES=1,")
        print("         MARKET_ENRICH_TOP=0, and a long MARKET_INTERVAL_SECS (e.g. 1800).")
    else:
        print("  ⚠️  Birdeye returned NO tokens. See the HTTP error above for the exact reason")
        print("     (400 = bad param/limit, 401/403 = key/plan, 429 = rate limit).")

    if args.once:
        run_once(key)
        return

    cps = calls_per_scan()
    b0 = load_budget()
    iv0 = INTERVAL if INTERVAL > 0 else auto_interval(b0["cu_used"])
    mode = "fixed" if INTERVAL > 0 else "AUTO"
    print(f"  market-watch — budget {CU_BUDGET} CU/mo · ~{cps} call(s)/scan (~{cps*CU_PER_CALL:.0f} CU) · "
          f"{mode}-paced ~1 scan / {iv0//60} min so the key lasts to the monthly reset. Ctrl-C to stop.")
    while True:
        _cu_exhausted[0] = False
        b = load_budget()
        cost = cps * CU_PER_CALL
        # HARD cap: if a scan would push us over the usable budget, pause until the reset.
        if b["cu_used"] + cost > usable_budget():
            print(f"  ⛔ CU budget for {b['month']} spent ({b['cu_used']:.0f}/{usable_budget():.0f}). "
                  f"Pausing until the monthly reset — re-checking hourly.")
            time.sleep(CU_COOLDOWN)
            continue
        start = _calls[0]
        try:
            run_once(key)
        except Exception as e:
            sys.stderr.write(f"  scan error: {e}\n")
        used = (_calls[0] - start) * CU_PER_CALL
        b["cu_used"] += used
        save_json(BUDGET_FILE, b)
        if _cu_exhausted[0]:
            # Birdeye says quota is gone — mark spent + back off so we don't hammer it.
            b["cu_used"] = max(b["cu_used"], usable_budget())
            save_json(BUDGET_FILE, b)
            print(f"  ⛔ Birdeye reports CU exhausted — pausing {CU_COOLDOWN}s.")
            time.sleep(CU_COOLDOWN)
            continue
        iv = INTERVAL if INTERVAL > 0 else auto_interval(b["cu_used"])
        print(f"  CU {b['cu_used']:.0f}/{usable_budget():.0f} used this month · next scan in {iv//60} min")
        time.sleep(iv)


if __name__ == "__main__":
    main()
