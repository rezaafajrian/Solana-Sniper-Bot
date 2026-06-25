#!/usr/bin/env python3
"""
MELT-style self-learning engine for the momentum bot.

Reads the decision log (every token evaluation) and the labeled outcomes the bot
records 2h after each detection, then answers: which features predict winners vs
rugs, which filters are rejecting profitable tokens, which accepted trades turn
into losers — and recommends concrete scoring/filter changes.

Inputs (produced by the bot when MOMENTUM_DECISION_LOG is set):
  <base>.csv            one row per evaluated token (features + BUY/REJECT)
  <base>_outcomes.csv   one row per token, labeled 2h later (returns + RUG/2X/...)

Usage:
  python3 scripts/learn.py                       # defaults to momentum_decisions
  python3 scripts/learn.py momentum_decisions    # explicit base path
  python3 scripts/learn.py --period weekly       # daily (default) or weekly buckets
"""

import csv, os, sys, math, argparse, datetime as dt, json, urllib.request
from collections import defaultdict


class Tee:
    """Write everything printed to both the console and a report file."""
    def __init__(self, path):
        self.f = open(path, "w")
        self.stdout = sys.stdout
    def write(self, s):
        self.stdout.write(s)
        self.f.write(s)
    def flush(self):
        self.stdout.flush()
        self.f.flush()


def _env(key):
    """Read a key from process env, falling back to a local .env file."""
    v = os.environ.get(key)
    if v:
        return v
    try:
        for line in open(".env"):
            line = line.strip()
            if line.startswith(f"{key}=") and not line.startswith("#"):
                return line.split("=", 1)[1].strip()
    except OSError:
        pass
    return None


def telegram_send(text):
    """Best-effort push of the synthesis to Telegram (no-op if creds absent)."""
    token, chat = _env("TELEGRAM_BOT_TOKEN"), _env("TELEGRAM_CHAT_ID")
    if not token or not chat:
        return False
    try:
        data = json.dumps({"chat_id": chat, "text": text[:3900], "disable_web_page_preview": True}).encode()
        req = urllib.request.Request(f"https://api.telegram.org/bot{token}/sendMessage",
                                     data=data, headers={"content-type": "application/json"})
        urllib.request.urlopen(req, timeout=10)
        return True
    except Exception:
        return False

NUM_FEATURES = [
    "creator_buy", "creator_score", "wallet_score", "insider_score",
    "smart_wallet_count", "holder_concentration", "marketcap", "volume",
    "liquidity", "overall_score", "confidence",
]
WIN_LABELS = {"2X", "5X", "10X", "20X+", "WIN"}
LOSS_LABELS = {"LOSS", "RUG"}


def f(row, k):
    try:
        return float(row.get(k, "") or "nan")
    except (ValueError, TypeError):
        return float("nan")


def load(base):
    dec_path = f"{base}.csv" if not base.endswith(".csv") else base
    base_noext = dec_path[:-4]
    out_path = f"{base_noext}_outcomes.csv"
    decisions = {}
    if os.path.exists(dec_path):
        with open(dec_path, newline="") as fh:
            for r in csv.DictReader(fh):
                ca = r.get("ca")
                if not ca:
                    continue
                # BUY rows win over earlier REJECT rows for the same CA
                if ca not in decisions or r.get("decision") == "BUY":
                    decisions[ca] = r
    outcomes = {}
    if os.path.exists(out_path):
        with open(out_path, newline="") as fh:
            for r in csv.DictReader(fh):
                ca = r.get("ca")
                if ca:
                    outcomes[ca] = r
    return decisions, outcomes, dec_path, out_path


def joined(decisions, outcomes):
    """Tokens that have both an evaluation and a labeled outcome."""
    rows = []
    for ca, o in outcomes.items():
        d = decisions.get(ca, {})
        rows.append({**d, **{f"o_{k}": v for k, v in o.items()}, "ca": ca})
    return rows


def label_of(row):
    return row.get("o_final_label", "") or ""


def is_winner(row):
    return label_of(row) in WIN_LABELS or f(row, "o_final_return") >= 0.15


def is_loser(row):
    return label_of(row) in LOSS_LABELS or f(row, "o_final_return") <= -0.30


def mean(xs):
    xs = [x for x in xs if not math.isnan(x)]
    return sum(xs) / len(xs) if xs else float("nan")


def pct(n, d):
    return 100.0 * n / d if d else 0.0


def hr():
    print("=" * 72)


def section(t):
    print(f"\n{t}")
    print("-" * len(t))


# ---------------------------------------------------------------------------
# Exit analysis (realized trades) — the half where the edge actually lives.
# ---------------------------------------------------------------------------
def categorize(reason):
    r = (reason or "").lower()
    if "insider" in r or "leader" in r: return "insider/leader-dump exit"
    if "slow-rug" in r: return "slow-rug exit"
    if "hard stop" in r: return "hard stop"
    if "trailing" in r: return "trailing stop (let it run)"
    if "stagnation" in r: return "stagnation stop (dead token)"
    if "max hold" in r or "zombie" in r: return "max-hold reap"
    if "collapse" in r: return "momentum collapse"
    if "scale-out" in r: return "scale-out (profit)"
    if "migration" in r: return "migration exit"
    return "other"


def load_trades(path):
    if not path or not os.path.exists(path):
        return []
    with open(path, newline="") as fh:
        return list(csv.DictReader(fh))


def realized_rows(rows):
    """Yield (mint, realized_sol, row) once per sell, preferring on-chain actuals."""
    actual_sigs = {r.get("signature") for r in rows if r.get("event") == "SELL_ACTUAL"}
    for r in rows:
        ev = r.get("event")
        if ev == "SELL_ACTUAL":
            yield r.get("mint"), f(r, "est_realized_pnl_sol"), r
        elif ev in ("SELL_PARTIAL", "SELL_FULL"):
            sig = r.get("signature")
            if sig in actual_sigs and sig not in ("", "DRY_RUN"):
                continue
            yield r.get("mint"), f(r, "est_realized_pnl_sol"), r


# ---------------------------------------------------------------------------
# Statistics: significance + out-of-sample splitting (anti-overfitting).
# ---------------------------------------------------------------------------
def wilson_lower(w, n, z=1.96):
    """Conservative lower bound on a proportion (a 100% on n=3 reads ~40%, not 100%)."""
    if n == 0:
        return 0.0
    p = w / n
    d = 1 + z * z / n
    centre = p + z * z / (2 * n)
    margin = z * math.sqrt((p * (1 - p) + z * z / (4 * n)) / n)
    return max(0.0, (centre - margin) / d)


def z_prop(w1, n1, w2, n2):
    """Two-proportion z-score (absolute) for a win-rate difference. >1.96 ~ p<0.05."""
    if n1 < 1 or n2 < 1:
        return 0.0
    p1, p2 = w1 / n1, w2 / n2
    p = (w1 + w2) / (n1 + n2)
    se = math.sqrt(p * (1 - p) * (1 / n1 + 1 / n2))
    return abs(p1 - p2) / se if se > 0 else 0.0


def sig_stars(z):
    return "***" if z >= 2.58 else "**" if z >= 1.96 else "*" if z >= 1.64 else ""


def row_time(r):
    """Sortable timestamp for a joined row (decision ISO, else detect_ts)."""
    ts = r.get("timestamp") or ""
    if ts:
        return ts
    d = f(r, "o_detect_ts")
    return "" if math.isnan(d) else f"{int(d):012d}"


def time_split(rows, test_frac):
    """Oldest (1-test_frac) = train, most-recent test_frac = test (out-of-sample)."""
    ordered = sorted(rows, key=row_time)
    cut = int(len(ordered) * (1 - test_frac))
    return ordered[:cut], ordered[cut:]


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("base", nargs="?", default="momentum_decisions")
    ap.add_argument("--period", choices=["daily", "weekly"], default="daily")
    ap.add_argument("--trades", default="momentum_trades.csv",
                    help="realized-trades log, for exit analysis (the edge lives here)")
    ap.add_argument("--rep", default="momentum_wallet_rep.csv",
                    help="wallet reputation file, for counterparty intelligence")
    ap.add_argument("--test-frac", type=float, default=0.30,
                    help="fraction of most-recent data held out for out-of-sample validation")
    ap.add_argument("--emit-config", action="store_true",
                    help="print validated recommendations as MOMENTUM_* env overrides")
    ap.add_argument("--report-dir", default=None,
                    help="also save the full report to <dir>/learn_<date>.txt")
    ap.add_argument("--telegram", action="store_true",
                    help="push the executive synthesis to Telegram (uses .env creds)")
    args = ap.parse_args()
    config_lines = []  # populated by validated recommendations for --emit-config
    synth = {"strengths": [], "weaknesses": [], "risks": [], "hidden": [],
             "missed": [], "hypotheses": []}  # the daily improvement report

    if args.report_dir:
        os.makedirs(args.report_dir, exist_ok=True)
        rpath = os.path.join(args.report_dir, f"learn_{dt.date.today().isoformat()}.txt")
        sys.stdout = Tee(rpath)

    decisions, outcomes, dec_path, out_path = load(args.base)
    if not decisions:
        sys.stderr.write(f"No decision log at '{dec_path}'. Run the bot with "
                         "MOMENTUM_DECISION_LOG set to generate it.\n")
        sys.exit(1)

    hr()
    print("  MOMENTUM SELF-LEARNING REPORT")
    print(f"  decisions: {dec_path} ({len(decisions)} tokens)")
    print(f"  outcomes : {out_path} ({len(outcomes)} labeled)")
    hr()

    rows = joined(decisions, outcomes)
    if not rows:
        print("\nNo labeled outcomes yet — the bot labels each token ~2h after"
              "\ndetection. Keep it running, then re-run this report.")
        # still show the decision mix
        dec_counts = defaultdict(int)
        for d in decisions.values():
            dec_counts[d.get("decision", "?")] += 1
        section("Decisions so far")
        for k, v in sorted(dec_counts.items()):
            print(f"  {k:8} {v}")
        return

    winners = [r for r in rows if is_winner(r)]
    losers = [r for r in rows if is_loser(r)]
    bought = [r for r in rows if r.get("decision") == "BUY"]
    rejected = [r for r in rows if r.get("decision") == "REJECT"]

    # Aggregates the executive synthesis reads (always defined; sections fill them).
    cat_pnl, caps = {}, []
    realized_total = payoff = runner_capture = float("nan")
    exit_winrate = float("nan")
    max_dd = 0.0
    clusters, rug_creators, dumper_clusters = {}, [], 0

    # ---- 5. Headline ----
    section("1. HEADLINE")
    print(f"  labeled tokens     : {len(rows)}")
    print(f"  winners / losers   : {len(winners)} / {len(losers)}  "
          f"(win rate {pct(len(winners), len(winners)+len(losers)):.1f}%)")
    lab = defaultdict(int)
    for r in rows:
        lab[label_of(r) or "?"] += 1
    print("  outcome labels     : " + ", ".join(f"{k}={v}" for k, v in
          sorted(lab.items(), key=lambda x: -x[1])))
    print(f"  avg max-return     : {mean([f(r,'o_max_return') for r in rows]):+.2f}x")
    print(f"  avg final-return   : {mean([f(r,'o_final_return') for r in rows]):+.2f}")

    # ---- 3. Pattern discovery: feature means, winners vs losers ----
    section("2. FEATURE SIGNAL (winners vs losers — does it separate?)")
    print(f"  {'feature':22} {'winners':>10} {'losers':>10} {'gap':>10}")
    feats_ranked = []
    for k in NUM_FEATURES:
        wm = mean([f(r, k) for r in winners])
        lm = mean([f(r, k) for r in losers])
        if math.isnan(wm) or math.isnan(lm):
            continue
        scale = max(abs(wm), abs(lm), 1e-9)
        gap = (wm - lm) / scale
        feats_ranked.append((abs(gap), k, wm, lm, gap))
    feats_ranked.sort(reverse=True)
    for _, k, wm, lm, gap in feats_ranked:
        flag = "  <<" if abs(gap) > 0.25 else ""
        print(f"  {k:22} {wm:>10.3f} {lm:>10.3f} {gap:>+9.0%}{flag}")
    print("  (gap = how differently the feature reads for winners vs losers;")
    print("   '<<' marks the ones actually worth using as signals)")

    # ---- 4. Feature discovery: win-rate by quantile bucket ----
    section("3. FEATURE DISCOVERY (win-rate across each feature's range)")
    for k in NUM_FEATURES:
        vals = sorted(f(r, k) for r in rows if not math.isnan(f(r, k)))
        if len(vals) < 12:
            continue
        lo_cut = vals[len(vals)//3]
        hi_cut = vals[2*len(vals)//3]
        buckets = {"low": [], "mid": [], "high": []}
        for r in rows:
            v = f(r, k)
            if math.isnan(v):
                continue
            b = "low" if v <= lo_cut else ("high" if v >= hi_cut else "mid")
            buckets[b].append(r)
        def wr(rs):
            w = sum(1 for r in rs if is_winner(r)); l = sum(1 for r in rs if is_loser(r))
            return pct(w, w+l), w+l
        (lw, ln), (mw, mn), (hw, hn) = wr(buckets["low"]), wr(buckets["mid"]), wr(buckets["high"])
        spread = abs(hw - lw)
        if spread >= 12 and ln >= 5 and hn >= 5:  # only show features that actually discriminate
            print(f"  {k:22} low {lw:4.0f}% (n={ln:<4}) | high {hw:4.0f}% (n={hn:<4}) | spread {spread:.0f}pts")

    # ---- which filters reject profitable opportunities ----
    section("4. REJECTED WINNERS (filters throwing away profit)")
    by_reason = defaultdict(lambda: {"n": 0, "win": 0, "big": 0})
    for r in rejected:
        reason = (r.get("rejection_reason") or "?").split(":")[0].strip() or "?"
        by_reason[reason]["n"] += 1
        if is_winner(r):
            by_reason[reason]["win"] += 1
        if f(r, "o_max_return") >= 1.0:
            by_reason[reason]["big"] += 1
    if by_reason:
        print(f"  {'rejection reason':32} {'rejected':>9} {'->won':>7} {'->2x+':>7}")
        for reason, d in sorted(by_reason.items(), key=lambda x: -x[1]["big"]):
            print(f"  {reason[:32]:32} {d['n']:>9} {pct(d['win'],d['n']):>6.0f}% {pct(d['big'],d['n']):>6.0f}%")
    else:
        print("  no rejected tokens with outcomes yet")

    # ---- accepted losers ----
    section("5. ACCEPTED LOSERS (buys that went bad)")
    bl = [r for r in bought if is_loser(r)]
    print(f"  bought: {len(bought)} | losers: {len(bl)} ({pct(len(bl),len(bought)):.0f}%) | "
          f"rugs: {sum(1 for r in bought if label_of(r)=='RUG')}")
    if bl:
        print("  shared traits of bought losers (avg):")
        for k in ["insider_score", "holder_concentration", "overall_score", "wallet_score"]:
            print(f"    {k:22} {mean([f(r,k) for r in bl]):.3f}   (winners-bought: "
                  f"{mean([f(r,k) for r in bought if is_winner(r)]):.3f})")

    # ---- period report ----
    section(f"6. {args.period.upper()} BREAKDOWN")
    def bucket_key(r):
        ts = r.get("timestamp", "")[:10]
        if args.period == "weekly" and len(ts) == 10:
            try:
                d = dt.date.fromisoformat(ts)
                return f"{d.isocalendar()[0]}-W{d.isocalendar()[1]:02d}"
            except ValueError:
                return ts
        return ts
    per = defaultdict(list)
    for r in rows:
        per[bucket_key(r)].append(r)
    print(f"  {'period':12} {'tokens':>7} {'winrate':>8} {'avg max':>9}")
    for key in sorted(per):
        rs = per[key]
        w = sum(1 for r in rs if is_winner(r)); l = sum(1 for r in rs if is_loser(r))
        print(f"  {key:12} {len(rs):>7} {pct(w,w+l):>7.0f}% {mean([f(r,'o_max_return') for r in rs]):>+8.2f}x")

    # ---- Per-signal performance: which signal type actually earns the most ----
    section("6b. PER-SIGNAL PERFORMANCE (which edge earns the most per token)")
    by_signal = defaultdict(list)
    for r in rows:
        by_signal[(r.get("signal") or "?")].append(r)
    if len(by_signal) > 1 or "?" not in by_signal:
        print(f"  {'signal':14} {'tokens':>7} {'winrate':>8} {'EV/token':>9} {'moonshot%':>10}")
        sig_rows = []
        for sig, rs in by_signal.items():
            w = sum(1 for r in rs if is_winner(r)); l = sum(1 for r in rs if is_loser(r))
            ev = mean([f(r, "o_final_return") for r in rs if not math.isnan(f(r, "o_final_return"))])
            moon = sum(1 for r in rs if label_of(r) in {"5X","10X","20X+"} or f(r,"o_max_return") >= 4.0)
            sig_rows.append((ev, sig, len(rs), pct(w, w+l), pct(moon, len(rs))))
        for ev, sig, n, wr, mp in sorted(sig_rows, reverse=True):
            print(f"  {sig:14} {n:>7} {wr:>7.0f}% {ev:>+8.3f} {mp:>9.1f}%")
        print("  (EV = avg final return per evaluated token. The signal with the highest")
        print("   EV is your real edge — lean into it; cut or down-weight the negative ones.)")
    else:
        print("  no signal-type data yet (older log without the 'signal' column, or no")
        print("  evaluations recorded). It populates on the next run.")

    # ---- EV-optimal thresholds: the settings that MAXIMIZE expected value ----
    section("7. EV-OPTIMAL THRESHOLDS — OUT-OF-SAMPLE VALIDATED (no overfitting)")
    fr = lambda r: f(r, "o_final_return")
    ev_all = mean([fr(r) for r in rows if not math.isnan(fr(r))])
    train, test = time_split(rows, args.test_frac)
    ev_train = mean([fr(r) for r in train if not math.isnan(fr(r))])
    ev_test = mean([fr(r) for r in test if not math.isnan(fr(r))])
    print(f"  baseline EV (all): {ev_all:+.3f} over {len(rows)} | train {len(train)} ({ev_train:+.3f}) "
          f"/ test {len(test)} ({ev_test:+.3f})")
    ev_recs = []  # only OOS-validated recommendations land here
    if len(train) < 20 or len(test) < 10:
        print("  not enough data to split train/test yet — gather more labeled outcomes.")
        print("  (until then, threshold mining would just overfit; nothing reported.)")
    else:
        print(f"  {'feature':20} {'cutoff':>8} {'EV train':>9} {'EV test':>9} {'test lift':>9}  OOS")
        for k in NUM_FEATURES:
            tv = sorted(f(r, k) for r in train if not math.isnan(f(r, k)))
            if len(tv) < 15:
                continue
            # Fit the best cutoff ON TRAIN ONLY.
            best = None  # (ev_train, cutoff, kept_train)
            for q in range(1, 10):
                cut = tv[int(len(tv) * q / 10)]
                kept = [r for r in train if not math.isnan(f(r, k)) and f(r, k) >= cut]
                if len(kept) < max(8, len(train) // 10):
                    continue
                ev = mean([fr(r) for r in kept if not math.isnan(fr(r))])
                if best is None or ev > best[0]:
                    best = (ev, cut, len(kept))
            if not best:
                continue
            # Validate that SAME cutoff on the held-out TEST window.
            kept_test = [r for r in test if not math.isnan(f(r, k)) and f(r, k) >= best[1]]
            if len(kept_test) < 5:
                continue
            ev_test_cut = mean([fr(r) for r in kept_test if not math.isnan(fr(r))])
            train_lift = best[0] - ev_train
            test_lift = ev_test_cut - ev_test
            holds = train_lift > 0.05 and test_lift > 0.0  # must work out-of-sample
            if train_lift > 0.05:
                flag = "✓ holds" if holds else "✗ overfit"
                print(f"  {k:20} {best[1]:>8.2f} {best[0]:>+8.3f} {ev_test_cut:>+8.3f} {test_lift:>+8.3f}  {flag}")
                if holds:
                    ev_recs.append((test_lift, k, best[1], ev_test_cut))
    if not ev_recs:
        print("  → no threshold survives out-of-sample yet. The honest read: the entry edge")
        print("    is weak/in-the-exits (see section 10), not in a single feature cutoff.")

    # ---- Moonshot attribution: which signals catch the 5x+? ----
    section("8. MOONSHOT ATTRIBUTION (what precedes the 5x+ winners)")
    moon = [r for r in rows if label_of(r) in {"5X", "10X", "20X+"} or f(r, "o_max_return") >= 4.0]
    moon_cas = {r.get("ca") for r in moon}
    nonmoon = [r for r in rows if r.get("ca") not in moon_cas]
    print(f"  moonshots (>=5x peak): {len(moon)} of {len(rows)}  (base rate {pct(len(moon),len(rows)):.1f}%)")
    if len(moon) >= 3:
        print(f"  {'feature':22} {'moonshots':>10} {'others':>10} {'gap':>8}")
        moon_ranked = []
        for k in NUM_FEATURES:
            mm, om = mean([f(r, k) for r in moon]), mean([f(r, k) for r in nonmoon])
            if math.isnan(mm) or math.isnan(om):
                continue
            scale = max(abs(mm), abs(om), 1e-9)
            moon_ranked.append((abs((mm-om)/scale), k, mm, om))
        moon_ranked.sort(reverse=True)
        for g, k, mm, om in moon_ranked[:6]:
            flag = "  <<" if g > 0.3 else ""
            print(f"  {k:22} {mm:>10.3f} {om:>10.3f} {g:>+7.0%}{flag}")
        print("  ('<<' = this feature is notably elevated before the big winners)")
    else:
        print("  not enough 5x+ tokens yet to fingerprint them — keep accumulating.")

    # ---- 10. EXIT ANALYSIS (realized trades — where the edge actually lives) ----
    trade_rows = load_trades(args.trades)
    exit_recs = []
    if trade_rows:
        section("10. EXIT ANALYSIS (realized PnL by exit — the half that makes money)")
        cat_pnl = defaultdict(lambda: {"n": 0, "sol": 0.0})
        per_token = defaultdict(float)
        best_exit_pct = {}  # mint -> best pnl_pct seen on a sell (capture analysis)
        total = 0.0
        for mint, pnl, row in realized_rows(trade_rows):
            c = categorize(row.get("reason"))
            cat_pnl[c]["n"] += 1
            cat_pnl[c]["sol"] += pnl
            per_token[mint] += pnl
            total += pnl
            pp = f(row, "pnl_pct")
            if not math.isnan(pp):
                best_exit_pct[mint] = max(best_exit_pct.get(mint, -1e9), pp)
        print(f"  total realized: {total:+.4f} SOL over {len(per_token)} tokens "
              f"({sum(c['n'] for c in cat_pnl.values())} sells)")
        print(f"  {'exit reason':30} {'sells':>6} {'realized SOL':>13} {'avg/sell':>9}")
        for c, d in sorted(cat_pnl.items(), key=lambda x: -x[1]["sol"]):
            avg = d["sol"] / d["n"] if d["n"] else 0.0
            print(f"  {c:30} {d['n']:>6} {d['sol']:>+13.4f} {avg:>+9.4f}")
        realized_total = total
        # max drawdown of the realized equity curve (risk factor).
        cum = peak = 0.0
        for _m, pnl, _r in realized_rows(trade_rows):
            cum += pnl
            peak = max(peak, cum)
            max_dd = min(max_dd, cum - peak)
        # win/loss asymmetry from whole-token realized PnL
        wins = [p for p in per_token.values() if p > 0]
        losses = [p for p in per_token.values() if p < 0]
        if per_token:
            exit_winrate = pct(len(wins), len(per_token))
            payoff = abs(mean(wins) / mean(losses)) if (wins and losses and mean(losses)) else float("nan")
            print(f"  win rate {exit_winrate:.1f}% | avg win {mean(wins) if wins else 0:+.4f} "
                  f"| avg loss {mean(losses) if losses else 0:+.4f} | payoff {payoff:.2f}x | "
                  f"max drawdown {max_dd:+.4f} SOL")
        # "Left on the table": of real runners, how much of the peak did we capture?
        caps = []
        for mint, ex_pp in best_exit_pct.items():
            o = outcomes.get(mint)
            if not o:
                continue
            avail = f({f"o_{k}": v for k, v in o.items()}, "o_max_return")  # peak multiple-1
            if not math.isnan(avail) and avail >= 0.5:  # a real runner (>=1.5x peak)
                caps.append(max(0.0, min(1.5, (ex_pp / 100.0) / avail)))
        if caps:
            runner_capture = mean(caps) * 100
            print(f"  runner capture: you exited at avg {runner_capture:.0f}% of the peak move "
                  f"(over {len(caps)} runners). Low % = selling winners too early.")
        # exit-tuning hint
        bleed = [(c, d["sol"]) for c, d in cat_pnl.items() if d["sol"] < 0]
        for c, sol in sorted(bleed, key=lambda x: x[1])[:2]:
            exit_recs.append(f"Exit '{c}' is net-negative ({sol:+.3f} SOL) — review its trigger/threshold.")

    # ---- 11. COUNTERPARTY INTELLIGENCE (from the wallet reputation file) ----
    if os.path.exists(args.rep):
        section("11. COUNTERPARTY INTELLIGENCE (wallets & coordinated clusters)")
        wr = list(csv.DictReader(open(args.rep, newline="")))
        def wi(r, k):
            try: return int(float(r.get(k, "0") or "0"))
            except ValueError: return 0
        scored = [r for r in wr if wi(r, "samples") >= 5]
        clusters = defaultdict(list)
        for r in wr:
            cid = wi(r, "cluster_id")
            if cid > 0:
                clusters[cid].append(r)
        rug_creators = sorted([r for r in wr if wi(r, "rugs_created") > 0],
                              key=lambda r: -wi(r, "rugs_created"))
        print(f"  wallets tracked: {len(wr)} ({len(scored)} with >=5 trades) | "
              f"coordinated clusters: {len(clusters)} | rug-creators: {len(rug_creators)}")
        if clusters:
            print(f"  {'cluster':>8} {'wallets':>8} {'avgROI':>8} {'rugsMade':>9}  verdict")
            crows = []
            for cid, members in clusters.items():
                n = len(members)
                samp = sum(wi(m, "samples") for m in members)
                roi = sum(f(m, "roi_sum") for m in members if not math.isnan(f(m, "roi_sum")))
                avg_roi = roi / samp if samp else 0.0
                made = sum(wi(m, "rugs_created") for m in members)
                verdict = "DUMPER" if (made > 0 or avg_roi <= -0.2) else ("ok" if avg_roi > 0.1 else "watch")
                if verdict == "DUMPER":
                    dumper_clusters += 1
                crows.append((n, cid, avg_roi, made, verdict))
            for n, cid, avg_roi, made, verdict in sorted(crows, reverse=True)[:10]:
                print(f"  #{cid:<7} {n:>8} {avg_roi*100:>7.0f}% {made:>9}  {verdict}")
        if rug_creators:
            print("  top rug-creators (blacklist their launches):")
            for r in rug_creators[:5]:
                print(f"    {r.get('wallet','')[:16]:16} created {wi(r,'rugs_created')} rug(s), "
                      f"{wi(r,'samples')} trades")

    # ---- 12. EDGE DECAY (is each signal's edge fading as others copy it?) ----
    section("12. EDGE DECAY (per-signal EV: earlier half vs recent half)")
    half = max(1, len(sorted(rows, key=row_time)) // 2)
    ordr = sorted(rows, key=row_time)
    early, recent = ordr[:half], ordr[half:]
    sig_set = {(r.get("signal") or "?") for r in rows}
    if len(recent) >= 10:
        print(f"  {'signal':14} {'early EV':>9} {'recent EV':>10} {'trend':>8}")
        for sig in sorted(sig_set):
            es = [fr(r) for r in early if (r.get('signal') or '?') == sig and not math.isnan(fr(r))]
            rs = [fr(r) for r in recent if (r.get('signal') or '?') == sig and not math.isnan(fr(r))]
            if len(es) < 4 or len(rs) < 4:
                continue
            ee, re = mean(es), mean(rs)
            trend = "↓ decay" if re < ee - 0.05 else ("↑ rising" if re > ee + 0.05 else "→ stable")
            print(f"  {sig:14} {ee:>+8.3f} {re:>+9.3f} {trend:>8}")
            if re < ee - 0.10 and re < 0:
                exit_recs.append(f"Signal '{sig}' edge is decaying (EV {ee:+.3f}→{re:+.3f}) and now negative — it's being copied out; down-weight it.")
    else:
        print("  not enough recent data to measure decay yet.")

    # ---- 13. SIGNAL PRECISION (did our pre-pump alerts actually fire BEFORE pumps?) ----
    section("13. SIGNAL PRECISION (are the early-warning alerts right?)")
    sig_path = dec_path[:-4] + "_signals.csv"
    if os.path.exists(sig_path):
        row_by_ca = {r["ca"]: r for r in rows}
        # per named signal: outcomes of every token where it fired
        per_sig = defaultdict(lambda: {"n": 0, "pumped": 0, "won": 0})
        lvl = defaultdict(lambda: {"n": 0, "pumped": 0})
        total_alerts = 0
        for a in csv.DictReader(open(sig_path, newline="")):
            ca = a.get("ca")
            total_alerts += 1
            jr = row_by_ca.get(ca)
            if not jr:
                continue  # not labeled yet
            pumped = f(jr, "o_max_return") >= 1.0  # >= 2x peak after the alert
            won = is_winner(jr)
            lvl[a.get("level", "?")]["n"] += 1
            if pumped:
                lvl[a.get("level", "?")]["pumped"] += 1
            for s in (a.get("signals", "") or "").split("|"):
                if not s:
                    continue
                per_sig[s]["n"] += 1
                if pumped:
                    per_sig[s]["pumped"] += 1
                if won:
                    per_sig[s]["won"] += 1
        labeled = sum(d["n"] for d in lvl.values())
        print(f"  {total_alerts} alerts fired, {labeled} now labeled.")
        if labeled:
            for L in ("MAJOR", "EARLY"):
                d = lvl.get(L)
                if d and d["n"]:
                    print(f"  {L:6} alerts: {d['n']:>4} | {pct(d['pumped'],d['n']):.0f}% hit 2x+ peak")
            print(f"\n  {'signal':20} {'fired':>6} {'→2x%':>6} {'→win%':>7}  precision (lower-bound)")
            for s, d in sorted(per_sig.items(), key=lambda x: -x[1]["n"]):
                if d["n"] < 5:
                    continue
                lb = wilson_lower(d["pumped"], d["n"]) * 100  # conservative
                star = "  ⭐ KEEP" if lb >= 50 else ("  ✗ weak" if lb < 20 else "")
                print(f"  {s:20} {d['n']:>6} {pct(d['pumped'],d['n']):>5.0f}% {pct(d['won'],d['n']):>6.0f}% "
                      f"  {lb:>5.0f}%{star}")
            print("  (precision = conservative % of alerted tokens that pumped. Keep the high")
            print("   ones, cut the weak ones — this is the alert system grading itself.)")
            # surface to the synthesis: signals proven to work / proven dead
            for s, d in per_sig.items():
                if d["n"] >= 10:
                    lb = wilson_lower(d["pumped"], d["n"]) * 100
                    if lb >= 50:
                        synth["strengths"].append(f"Alert '{s}' is reliable: {lb:.0f}% of its fires pumped (n={d['n']}).")
                    elif lb < 20:
                        synth["weaknesses"].append(f"Alert '{s}' is noise: only {lb:.0f}% pumped (n={d['n']}) — cut/raise its threshold.")
        else:
            print("  alerts fired but none labeled yet (wait for the 2h outcome horizon).")
    else:
        print("  no signal log yet — fires accumulate once the bot runs with alerts on.")

    # ---- 5. Recommendations ----
    section("9. RECOMMENDATIONS (data-driven, verify before trusting)")
    recs = []
    # Per-signal edge: lean into the best, cut the worst.
    sig_ev = []
    for sig, rs in by_signal.items():
        if sig == "?" or len(rs) < 8:
            continue
        ev = mean([f(r, "o_final_return") for r in rs if not math.isnan(f(r, "o_final_return"))])
        if not math.isnan(ev):
            sig_ev.append((ev, sig, len(rs)))
    sig_ev.sort(reverse=True)
    if sig_ev:
        bev, bsig, bn = sig_ev[0]
        recs.append(f"Best signal by EV: '{bsig}' ({bev:+.3f}/token over {bn}). Lean into it — "
                    "raise its boost/size or require it.")
        wev, wsig, wn = sig_ev[-1]
        if wev < 0 and wsig != bsig:
            recs.append(f"Worst signal: '{wsig}' is EV-negative ({wev:+.3f} over {wn}). "
                        "Down-weight or stop entering on it alone.")
    # OOS-validated threshold recommendations (survived the held-out window).
    ev_recs.sort(reverse=True)
    for lift, k, cut, ev in ev_recs[:3]:
        recs.append(f"Gating on {k} >= {cut:.2f} held up OUT-OF-SAMPLE (+{lift:.3f}/token on the "
                    f"held-out window). This one isn't overfit — A/B it.")
    # Exit + decay findings surfaced from sections 10/12.
    recs.extend(exit_recs)
    # rug predictors
    rugs = [r for r in rows if label_of(r) == "RUG"]
    nonrugs = [r for r in rows if label_of(r) != "RUG"]
    for k, env in [("insider_score", "MOMENTUM_INSIDER_DISTRIB_MAX_SOLD"),
                   ("holder_concentration", "MOMENTUM_MAX_TOP_HOLDER_SHARE")]:
        rm, nm = mean([f(r, k) for r in rugs]), mean([f(r, k) for r in nonrugs])
        if not math.isnan(rm) and not math.isnan(nm) and rm > nm * 1.3 and len(rugs) >= 8:
            recs.append(f"Rugs have much higher {k} ({rm:.2f} vs {nm:.2f}). "
                        f"Consider tightening {env} toward ~{(rm+nm)/2:.2f}.")
    # rejected-winner leakage
    for reason, d in by_reason.items():
        if d["n"] >= 10 and pct(d["big"], d["n"]) >= 25:
            recs.append(f"Filter '{reason}' rejected {d['n']} tokens, {pct(d['big'],d['n']):.0f}% of which "
                        f"hit 2x+. It's leaking winners — loosen it and A/B.")
    # score predictiveness
    ws = mean([f(r, "overall_score") for r in winners])
    ls = mean([f(r, "overall_score") for r in losers])
    if not math.isnan(ws) and not math.isnan(ls):
        if abs(ws - ls) < 3:
            recs.append(f"overall_score barely separates winners ({ws:.1f}) from losers ({ls:.1f}) "
                        "— the entry score is near-noise; lean on exits + the strongest features above.")
        elif ws > ls + 5:
            recs.append(f"overall_score is predictive (winners {ws:.1f} vs losers {ls:.1f}) "
                        "— raising MOMENTUM_ENTRY_SCORE should lift win rate.")
    # best discriminating feature
    if feats_ranked and feats_ranked[0][0] > 0.3:
        _, k, wm, lm, gap = feats_ranked[0]
        recs.append(f"Strongest signal is '{k}' (winners {wm:.2f} vs losers {lm:.2f}). "
                    "Worth adding/raising its weight in the entry score.")
    if not recs:
        recs.append("No statistically clear edge yet — gather more labeled outcomes "
                    "(let it run longer) before changing filters.")
    for i, r in enumerate(recs, 1):
        print(f"  {i}. {r}")

    # ---- machine-readable config from OOS-validated findings ----
    for lift, k, cut, ev in ev_recs[:3]:
        env = {
            "overall_score": "MOMENTUM_ENTRY_SCORE",
            "smart_wallet_count": "MOMENTUM_CONVERGENCE_MIN",
            "liquidity": "MOMENTUM_STRUCT_MIN_LIQ_SOL",
        }.get(k)
        if env:
            config_lines.append(f"{env}={cut:.2f}   # OOS-validated EV lift +{lift:.3f}/token on {k}")
    if args.emit_config:
        section("CONFIG OVERRIDES (only out-of-sample-validated knobs)")
        if config_lines:
            print("  # paste into .env, then A/B one at a time:")
            for line in config_lines:
                print(f"  {line}")
        else:
            print("  # nothing validated out-of-sample yet — no config changes recommended.")

    # ===================================================================
    #  EXECUTIVE SYNTHESIS — the daily improvement report
    # ===================================================================
    # STRENGTHS — what's working.
    if sig_ev:
        bev, bsig, bn = sig_ev[0]
        if bev > 0:
            synth["strengths"].append(f"Best edge: '{bsig}' signal, EV {bev:+.3f}/token over {bn}.")
    best_exit = max(cat_pnl.items(), key=lambda x: x[1]["sol"], default=None)
    if best_exit and best_exit[1]["sol"] > 0:
        synth["strengths"].append(f"Top exit: {best_exit[0]} earned {best_exit[1]['sol']:+.3f} SOL.")
    if not math.isnan(payoff) and payoff >= 2.0:
        synth["strengths"].append(f"Healthy asymmetry: payoff ratio {payoff:.1f}x (wins pay >> losses cost).")
    for lift, k, cut, ev in ev_recs[:2]:
        synth["strengths"].append(f"OOS-validated gate: {k} >= {cut:.2f} (+{lift:.3f}/token, held out-of-sample).")

    # WEAKNESSES — what's failing.
    if sig_ev:
        wev, wsig, wn = sig_ev[-1]
        if wev < 0:
            synth["weaknesses"].append(f"Losing edge: '{wsig}' is EV-negative ({wev:+.3f} over {wn}).")
    for c, d in cat_pnl.items():
        if d["sol"] < -0.01:
            synth["weaknesses"].append(f"Bleeding exit: {c} net {d['sol']:+.3f} SOL.")
    if not math.isnan(runner_capture) and runner_capture < 40:
        synth["weaknesses"].append(f"Selling winners early: capturing only {runner_capture:.0f}% of runner peaks "
                                   "— loosen the trailing stop / push scale-out targets higher.")
    if not math.isnan(ws) and not math.isnan(ls) and abs(ws - ls) < 3:
        synth["weaknesses"].append(f"Entry score near-noise (winners {ws:.0f} vs losers {ls:.0f}) — edge is in exits/structure.")

    # RISK FACTORS.
    if max_dd < -0.001:
        synth["risks"].append(f"Max realized drawdown {max_dd:+.3f} SOL — size so this is survivable.")
    rug_rate = pct(len(rugs), len(rows))
    if rug_rate > 0:
        synth["risks"].append(f"Rug rate {rug_rate:.0f}% of labeled tokens — anti-rug stack is load-bearing.")
    if dumper_clusters > 0:
        synth["risks"].append(f"{dumper_clusters} dumper/bundler cluster(s) active — the cluster veto is earning its keep.")
    if rug_creators:
        synth["risks"].append(f"{len(rug_creators)} known rug-creator wallet(s) seen — keep the creator blacklist on.")

    # HIDDEN PATTERNS — what precedes the big winners / discriminates.
    if len(moon) >= 3 and feats_ranked:
        top = [k for _, k, _, _, _ in feats_ranked[:3]]
        synth["hidden"].append(f"Moonshots ({len(moon)}) skew on: {', '.join(top)} — candidate predictive features.")
    if feats_ranked and feats_ranked[0][0] > 0.25:
        _, k, wm, lm, _ = feats_ranked[0]
        synth["hidden"].append(f"'{k}' separates winners ({wm:.2f}) from losers ({lm:.2f}) — weight it more.")

    # MISSED OPPORTUNITIES.
    for reason, d in sorted(by_reason.items(), key=lambda x: -x[1]["big"]):
        if d["n"] >= 8 and pct(d["big"], d["n"]) >= 25:
            synth["missed"].append(f"Filter '{reason}' tossed {d['n']} tokens, {pct(d['big'],d['n']):.0f}% hit 2x+ — it leaks winners.")
            break
    if not math.isnan(runner_capture) and runner_capture < 60:
        synth["missed"].append(f"~{100-runner_capture:.0f}% of runner upside left on the table — exit timing is the biggest lever.")

    # HYPOTHESES TO TEST — the continuous research engine.
    decay_up = []  # signals trending up are worth requiring/boosting
    for sig in sorted({(r.get("signal") or "?") for r in rows}):
        es = [f(r, "o_final_return") for r in early if (r.get('signal') or '?') == sig and not math.isnan(f(r, "o_final_return"))]
        rs = [f(r, "o_final_return") for r in recent if (r.get('signal') or '?') == sig and not math.isnan(f(r, "o_final_return"))]
        if len(es) >= 4 and len(rs) >= 4 and mean(rs) > mean(es) + 0.1 and mean(rs) > 0:
            decay_up.append(sig)
    for sig in decay_up:
        synth["hypotheses"].append(f"'{sig}' EV is rising — hypothesis: requiring/boosting it lifts win rate. A/B it.")
    for lift, k, cut, ev in ev_recs[:2]:
        synth["hypotheses"].append(f"Gate {k} >= {cut:.2f} (OOS-validated) — apply and measure win-rate delta.")
    if not math.isnan(runner_capture) and runner_capture < 50:
        synth["hypotheses"].append("Widen trailing-stop giveback (e.g. 0.35→0.45) — hypothesis: captures more of each runner.")
    # near-miss features worth turning into new filters
    for _, k, wm, lm, gap in feats_ranked[:5]:
        if 0.15 <= abs(gap) <= 0.25:
            synth["hypotheses"].append(f"'{k}' weakly discriminates (gap {gap:+.0%}) — test it as a soft score input / combo filter.")
    if dumper_clusters > 0:
        synth["hypotheses"].append("Test the inverse of the cluster veto: BOOST size when a CLEAN proven cluster is accumulating.")
    if not synth["hypotheses"]:
        synth["hypotheses"].append("Not enough signal yet — keep accumulating labeled outcomes, then re-run.")

    section("★ EXECUTIVE SYNTHESIS — what to improve next")
    titles = [("strengths", "✅ STRENGTHS (working)"), ("weaknesses", "❌ WEAKNESSES (failing)"),
              ("risks", "⚠️  RISK FACTORS"), ("hidden", "🔍 HIDDEN PATTERNS"),
              ("missed", "💸 MISSED OPPORTUNITIES"), ("hypotheses", "🧪 HYPOTHESES TO TEST NEXT")]
    synth_lines = [f"📊 Daily report {dt.date.today().isoformat()} — {len(rows)} labeled tokens, "
                   f"{pct(len(winners), len(winners)+len(losers)):.0f}% win rate"]
    for key, title in titles:
        items = synth[key] or ["(nothing flagged this period)"]
        print(f"\n  {title}")
        for it in items:
            print(f"    • {it}")
        # compact lines for the Telegram push (skip empty buckets)
        if synth[key]:
            synth_lines.append(f"\n{title}")
            synth_lines.extend(f"• {it}" for it in synth[key][:4])

    print()
    hr()
    print("  Method: thresholds are fit on older data and validated on a held-out recent")
    print("  window (section 7) — only edges that survive OOS are recommended. Exit edge")
    print("  (section 10) is from REAL realized PnL. Still: A/B one change at a time.")
    hr()

    if args.report_dir and isinstance(sys.stdout, Tee):
        sys.stdout.flush()
        print(f"\n  report saved → {os.path.join(args.report_dir, f'learn_{dt.date.today().isoformat()}.txt')}")
    if args.telegram:
        ok = telegram_send("\n".join(synth_lines))
        print(f"  telegram: {'sent ✅' if ok else 'not configured / failed'}")


if __name__ == "__main__":
    main()
