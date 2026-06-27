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
    if v is not None and v != "":
        return v
    try:
        for line in open(".env"):
            s = line.strip()
            if s.startswith(f"{key}=") and not s.startswith("#"):
                return s.split("=", 1)[1].strip()
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
SCAN_LIMIT    = int(envf("MARKET_SCAN_LIMIT", 100))    # tokens per list call (<=100)
ENRICH_TOP    = int(envf("MARKET_ENRICH_TOP", 25))     # how many survivors to enrich w/ holders
RATE_DELAY    = envf("MARKET_RATE_DELAY", 1.2)         # min seconds between Birdeye calls (free tier ~1 rps)
INTERVAL      = int(envf("MARKET_INTERVAL_SECS", 180)) # loop cadence
ALERT_COOLDOWN = int(envf("MARKET_ALERT_COOLDOWN_SECS", 3600))  # per-token re-alert gap
PAGES         = int(envf("MARKET_PAGES", 2))           # pages (of SCAN_LIMIT) per sort dimension
# MAXIMIZE COVERAGE — scan the market from EVERY angle so a degen play can't slip through:
# top volume, biggest gainers, fastest volume RISERS, freshest listings, deepest liquidity.
# Each is a different way a runner shows up; the union is the funnel, the hard floors are the
# filter. Tune via MARKET_SORTS (comma-separated Birdeye sort_by keys).
SORTS = [s.strip() for s in env("MARKET_SORTS",
         "volume_24h_usd,price_change_24h_percent,volume_24h_change_percent,recent_listing_time,liquidity"
         ).split(",") if s.strip()]


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
            sys.stderr.write(f"  birdeye {path} failed: HTTP {e.code} {e.reason}\n")
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
    """One page of the Token List V3, sorted by `sort_by` (legacy fallback on a CLEAN empty,
    never on a rate-limit error — that would just 429 again)."""
    st = "desc" if sort_by != "recent_listing_time" else "asc"
    d = _bget("/defi/v3/token/list", key,
              {"sort_by": sort_by, "sort_type": st, "offset": offset, "limit": min(limit, 100)})
    if d is ERRORED:
        return []
    rows = _data(d)
    if not rows and offset == 0 and sort_by == "volume_24h_usd":  # legacy fallback (volume only)
        d = _bget("/defi/tokenlist", key,
                  {"sort_by": "v24hUSD", "sort_type": "desc", "offset": 0, "limit": min(limit, 100)})
        rows = _data(d) if d is not ERRORED else []
    return rows or []


def gather_candidates(key):
    """MAXIMIZE recall: union the market across every sort dimension + pages + trending, so a
    surging degen that isn't yet top-volume still gets caught. Dedup by address."""
    by_addr = {}
    trank = {}
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

    # Preflight: one cheap call to verify the key + which plan/endpoints it can reach.
    pf = _bget("/defi/v3/token/list", key, {"sort_by": "volume_24h_usd", "sort_type": "desc", "offset": 0, "limit": 1})
    if pf is ERRORED:
        print("  ⚠️  Birdeye Token List V3 is unreachable for this key (rate-limited or not on your plan).")
        print("     The whole-market list needs the STARTER plan. On the free plan you'll see 429s.")
        print("     Options: upgrade at birdeye.so, or raise MARKET_RATE_DELAY (e.g. 2.0) if it's just throttling.")
    elif not _data(pf):
        print("  ⚠️  Birdeye returned an empty token list — check the key, or your plan's endpoint access.")
    else:
        print("  ✅ Birdeye key OK — Token List V3 reachable.")

    if args.once:
        run_once(key)
        return

    print(f"  market-watch loop every {INTERVAL}s — isolated scanner, writes {OUT}. Ctrl-C to stop.")
    while True:
        try:
            run_once(key)
        except Exception as e:
            sys.stderr.write(f"  scan error: {e}\n")
        time.sleep(INTERVAL)


if __name__ == "__main__":
    main()
