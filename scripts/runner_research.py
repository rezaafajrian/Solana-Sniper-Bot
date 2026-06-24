#!/usr/bin/env python3
"""
RUNNER DNA — research & intelligence layer.

Continuously studies tokens that achieved massive gains (2x → 1000x), dissects them,
and learns the recurring patterns / signals / conditions that existed BEFORE their
explosive moves. It compares winners vs losers, finds what separates them, generates
hypotheses, and BACKTESTS them out-of-sample — then persists what holds up to a
knowledge base that sharpens over time.

This is NOT a trade-signal generator. It is a discovery/validation/refinement layer
whose outputs (validated filters + scoring factors) are PROPOSALS to A/B into the bot's
selection — never auto-applied. Discipline is enforced throughout: out-of-sample splits,
minimum samples, significance, conservative (Wilson lower-bound) precision, and explicit
false-positive reporting, so we chase the long-term >80%-precision target WITHOUT
overfitting to history.

Outputs:
  * Common characteristics of high-performing tokens
  * Early indicators that precede major expansions
  * Behavioral patterns of successful creators & wallets
  * Liquidity / volume / holder / distribution patterns
  * Features correlating with outsized returns AND with failure / rugs
  * New filters & scoring factors (validated OOS) to improve selection
  * A persistent knowledge base that gains/loses confidence as patterns re-validate

Usage:
  python3 scripts/runner_research.py                       # default momentum_decisions
  python3 scripts/runner_research.py --report-dir reports  # save a timestamped report
"""
import csv, os, sys, math, json, argparse, datetime as dt
from collections import defaultdict

# Detection-time features (from the decision log) we test as runner predictors.
FEATURES = [
    "creator_buy", "creator_score", "wallet_score", "insider_score",
    "smart_wallet_count", "holder_concentration", "marketcap", "volume",
    "liquidity", "overall_score", "confidence",
]
# Winner tiers by peak multiple (o_max_return = peak/detect - 1; so 9.0 == 10x).
TIERS = [("1000x", 999.0), ("100x", 99.0), ("50x", 49.0),
         ("10x", 9.0), ("5x", 4.0), ("2x", 1.0)]
RUG_LABELS = {"RUG"}
KB_FILE = "runner_patterns.json"  # the cumulative knowledge base


# --------------------------------------------------------------------------- io
def fval(row, k):
    try:
        return float(row.get(k, "") or "nan")
    except (ValueError, TypeError):
        return float("nan")


def load(base):
    dec_path = base if base.endswith(".csv") else f"{base}.csv"
    out_path = dec_path[:-4] + "_outcomes.csv"
    decisions = {}
    if os.path.exists(dec_path):
        for r in csv.DictReader(open(dec_path, newline="")):
            ca = r.get("ca")
            if ca and (ca not in decisions or r.get("decision") == "BUY"):
                decisions[ca] = r
    rows = []
    if os.path.exists(out_path):
        for o in csv.DictReader(open(out_path, newline="")):
            ca = o.get("ca")
            if not ca:
                continue
            d = decisions.get(ca, {})
            rows.append({**d, **{f"o_{k}": v for k, v in o.items()}, "ca": ca})
    return rows, dec_path, out_path


# --------------------------------------------------------------- stats helpers
def mean(xs):
    xs = [x for x in xs if not math.isnan(x)]
    return sum(xs) / len(xs) if xs else float("nan")


def median(xs):
    xs = sorted(x for x in xs if not math.isnan(x))
    return xs[len(xs) // 2] if xs else float("nan")


def pct(n, d):
    return 100.0 * n / d if d else 0.0


def wilson_lower(w, n, z=1.96):
    """Conservative lower bound on a proportion — anti-overfitting: a 100% on n=3
    reports ~40%, not 100%, so small-sample flukes don't masquerade as edges."""
    if n == 0:
        return 0.0
    p = w / n
    d = 1 + z * z / n
    centre = p + z * z / (2 * n)
    margin = z * math.sqrt((p * (1 - p) + z * z / (4 * n)) / n)
    return max(0.0, (centre - margin) / d)


def z_prop(w1, n1, w2, n2):
    if n1 < 1 or n2 < 1:
        return 0.0
    p1, p2 = w1 / n1, w2 / n2
    p = (w1 + w2) / (n1 + n2)
    se = math.sqrt(p * (1 - p) * (1 / n1 + 1 / n2)) if (0 < p < 1) else 0.0
    return abs(p1 - p2) / se if se > 0 else 0.0


def row_time(r):
    return r.get("timestamp") or (f"{int(fval(r,'o_detect_ts')):012d}"
                                  if not math.isnan(fval(r, "o_detect_ts")) else "")


def time_split(rows, test_frac):
    o = sorted(rows, key=row_time)
    cut = int(len(o) * (1 - test_frac))
    return o[:cut], o[cut:]


def maxret(r):
    return fval(r, "o_max_return")


def is_rug(r):
    return (r.get("o_final_label") or "") in RUG_LABELS or fval(r, "o_final_return") <= -0.9


def hr():
    print("=" * 78)


def section(t):
    print(f"\n{t}\n" + "-" * len(t))


# --------------------------------------------------------------------- report
def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("base", nargs="?", default="momentum_decisions")
    ap.add_argument("--rep", default="momentum_wallet_rep.csv")
    ap.add_argument("--runner-mult", type=float, default=9.0,
                    help="peak multiple-1 that counts as a 'runner' for the DNA study (default 9 = 10x)")
    ap.add_argument("--test-frac", type=float, default=0.30)
    ap.add_argument("--report-dir", default=None)
    args = ap.parse_args()

    rows, dec_path, out_path = load(args.base)
    if args.report_dir:
        os.makedirs(args.report_dir, exist_ok=True)
        import io
        class Tee(io.TextIOBase):
            def __init__(s):
                s.f = open(os.path.join(args.report_dir, f"runner_dna_{dt.date.today().isoformat()}.txt"), "w")
                s.o = sys.__stdout__
            def write(s, x):
                s.o.write(x); s.f.write(x); return len(x)
        sys.stdout = Tee()

    hr()
    print("  RUNNER DNA — research & intelligence layer (NOT a trade signal)")
    print(f"  data: {dec_path} + {out_path}  ({len(rows)} labeled tokens)")
    hr()
    if len(rows) < 30:
        print("\n  Not enough labeled tokens yet to study runner DNA (need ~30+).")
        print("  Keep the bot running so outcomes accumulate, then re-run.")
        return

    # ---- 1. Winner tiers ---------------------------------------------------
    section("1. WINNER TIERS (how often, and how big, do tokens run?)")
    print(f"  {'tier':8} {'count':>6} {'base rate':>10}")
    for name, thr in TIERS:
        n = sum(1 for r in rows if maxret(r) >= thr)
        print(f"  {name:8} {n:>6} {pct(n, len(rows)):>9.2f}%")
    rugs = [r for r in rows if is_rug(r)]
    print(f"  {'RUG':8} {len(rugs):>6} {pct(len(rugs), len(rows)):>9.2f}%")
    runner_thr = args.runner_mult
    runners = [r for r in rows if maxret(r) >= runner_thr]
    field = [r for r in rows if maxret(r) < runner_thr]
    print(f"\n  studying RUNNERS = peak >= {runner_thr+1:.0f}x  ({len(runners)} of {len(rows)})")
    if len(runners) < 5:
        print("  (few runners so far — findings are provisional; they sharpen as more appear.)")

    # ---- 2. Runner DNA: feature fingerprint vs the field -------------------
    section("2. RUNNER DNA (what's different about them, with significance)")
    print(f"  {'feature':20} {'runners':>10} {'field':>10} {'lift':>8}  sig")
    dna = []
    for k in FEATURES:
        rm, fm = mean([fval(r, k) for r in runners]), mean([fval(r, k) for r in field])
        if math.isnan(rm) or math.isnan(fm):
            continue
        scale = max(abs(rm), abs(fm), 1e-9)
        lift = (rm - fm) / scale
        # significance: split feature at field-median, compare runner-rate above/below
        med = median([fval(r, k) for r in rows])
        hi = [r for r in rows if fval(r, k) >= med]
        lo = [r for r in rows if fval(r, k) < med]
        hw = sum(1 for r in hi if maxret(r) >= runner_thr)
        lw = sum(1 for r in lo if maxret(r) >= runner_thr)
        z = z_prop(hw, len(hi), lw, len(lo))
        dna.append((abs(lift), k, rm, fm, lift, z))
    dna.sort(reverse=True)
    for _, k, rm, fm, lift, z in dna:
        stars = "***" if z >= 2.58 else "**" if z >= 1.96 else "*" if z >= 1.64 else ""
        print(f"  {k:20} {rm:>10.3f} {fm:>10.3f} {lift:>+7.0%}  {stars}")
    print("  (lift = how elevated the feature is in runners; *** = statistically real,")
    print("   not noise. These are the COMMON CHARACTERISTICS / early indicators.)")

    # ---- 3. Failure / rug fingerprint -------------------------------------
    section("3. FAILURE & RUG FINGERPRINT (what precedes the bad outcomes)")
    nonrug = [r for r in rows if not is_rug(r)]
    if len(rugs) >= 5:
        print(f"  {'feature':20} {'rugs':>10} {'non-rugs':>10} {'lift':>8}")
        rug_rank = []
        for k in FEATURES:
            rm, nm = mean([fval(r, k) for r in rugs]), mean([fval(r, k) for r in nonrug])
            if math.isnan(rm) or math.isnan(nm):
                continue
            scale = max(abs(rm), abs(nm), 1e-9)
            rug_rank.append((abs((rm - nm) / scale), k, rm, nm, (rm - nm) / scale))
        rug_rank.sort(reverse=True)
        for _, k, rm, nm, lift in rug_rank[:6]:
            flag = "  << rug tell" if abs(lift) > 0.3 else ""
            print(f"  {k:20} {rm:>10.3f} {nm:>10.3f} {lift:>+7.0%}{flag}")
    else:
        print(f"  only {len(rugs)} rugs labeled — need more to fingerprint failure reliably.")

    # ---- 4. Creator / wallet behavior behind runners ----------------------
    section("4. CREATOR & WALLET BEHAVIOR (who is behind the big winners)")
    print("  detection-time signals, runners vs field:")
    for k in ["creator_score", "wallet_score", "smart_wallet_count", "creator_buy"]:
        rm, fm = mean([fval(r, k) for r in runners]), mean([fval(r, k) for r in field])
        print(f"    {k:20} runners {rm:>7.3f} | field {fm:>7.3f}")
    if os.path.exists(args.rep):
        wr = list(csv.DictReader(open(args.rep, newline="")))
        def wi(r, k):
            try: return int(float(r.get(k, "0") or "0"))
            except ValueError: return 0
        elite = [r for r in wr if wi(r, "samples") >= 10 and fval(r, "roi_sum") > 0
                 and (fval(r, "roi_sum") / max(1, wi(r, "samples"))) > 0.2 and wi(r, "rugs_created") == 0]
        print(f"  wallet rep: {len(wr)} wallets | {len(elite)} look like genuine alpha "
              "(>=10 trades, +ROI, never created a rug).")
        print("  → these are the wallets whose buys are the strongest runner precursor; "
              "follow/weight them.")

    # ---- 5. Liquidity / volume / holder / distribution --------------------
    section("5. LIQUIDITY / VOLUME / HOLDER / DISTRIBUTION patterns")
    for k in ["liquidity", "volume", "holder_concentration", "marketcap"]:
        rvals = [fval(r, k) for r in runners]
        fvals = [fval(r, k) for r in field]
        print(f"  {k:20} runner median {median(rvals):>8.2f} | field median {median(fvals):>8.2f}")
    print("  (use the gaps as candidate band filters — e.g. a runner-typical liquidity floor.)")

    # ---- 6. Hypotheses: generate + BACKTEST out-of-sample -----------------
    section("6. HYPOTHESES — generated, then BACKTESTED out-of-sample (anti-overfit)")
    train, test = time_split(rows, args.test_frac)
    base_rate = pct(len(runners), len(rows))
    print(f"  baseline runner rate: {base_rate:.1f}%  |  train {len(train)} / test {len(test)}  "
          f"|  target precision: 80%")
    validated = []
    if len(train) >= 20 and len(test) >= 10:
        print(f"  {'filter':28} {'train prec':>10} {'test prec':>10} {'recall':>7}  verdict")
        # single-feature threshold hypotheses
        for k in FEATURES:
            tv = sorted(fval(r, k) for r in train if not math.isnan(fval(r, k)))
            if len(tv) < 15:
                continue
            best = None  # (train_prec_lb, cut, kept_n, kept_runners)
            for q in range(2, 10):
                cut = tv[int(len(tv) * q / 10)]
                kept = [r for r in train if not math.isnan(fval(r, k)) and fval(r, k) >= cut]
                if len(kept) < 8:
                    continue
                w = sum(1 for r in kept if maxret(r) >= runner_thr)
                prec_lb = wilson_lower(w, len(kept))  # conservative
                if best is None or prec_lb > best[0]:
                    best = (prec_lb, cut, len(kept), w)
            if not best or best[0] <= base_rate / 100:
                continue
            # validate the SAME cut on the held-out test window
            kt = [r for r in test if not math.isnan(fval(r, k)) and fval(r, k) >= best[1]]
            if len(kt) < 5:
                continue
            tw = sum(1 for r in kt if maxret(r) >= runner_thr)
            test_prec = wilson_lower(tw, len(kt))
            all_runners_test = sum(1 for r in test if maxret(r) >= runner_thr)
            recall = pct(tw, all_runners_test) if all_runners_test else 0.0
            holds = test_prec > base_rate / 100 and recall >= 10
            verdict = "✓ holds" if holds else "✗ overfit/weak"
            print(f"  {k+' >= '+format(best[1],'.2f'):28} {best[0]*100:>9.0f}% {test_prec*100:>9.0f}% "
                  f"{recall:>6.0f}%  {verdict}")
            if holds:
                validated.append({"feature": k, "cutoff": round(best[1], 4),
                                  "test_precision": round(test_prec, 3), "recall": round(recall, 1)})
        if not validated:
            print("  → no single-feature filter beats the base rate out-of-sample yet.")
            print("    Honest read: the runner edge isn't in one detection feature (or too few")
            print("    runners so far). Keep accumulating; combos + wallet signals come next.")
    else:
        print("  not enough data to split train/test — keep accumulating outcomes.")

    # ---- 7. Proposed filters / scoring factors ----------------------------
    section("7. PROPOSED FILTERS / SCORING FACTORS (validated only — A/B before trusting)")
    if validated:
        for v in sorted(validated, key=lambda x: -x["test_precision"]):
            print(f"  • {v['feature']} >= {v['cutoff']}: {v['test_precision']*100:.0f}% precision OOS, "
                  f"{v['recall']:.0f}% runner recall. Add as a scoring factor / soft filter.")
        best_p = max(v["test_precision"] for v in validated) * 100
        print(f"\n  best validated precision so far: {best_p:.0f}%  (target 80%)")
        if best_p < 80:
            print("  → not at the 80% target yet. Treat these as edges-in-progress: stack the")
            print("    validated ones, keep gathering data, and re-test — do NOT overfit to hit 80.")
    else:
        print("  none validated yet. The discipline is working: we report NOTHING until it")
        print("  survives the held-out window. Better an honest 'no edge yet' than an overfit one.")

    # ---- 8. Knowledge base: accumulate + refine over time -----------------
    section("8. KNOWLEDGE BASE (patterns refined across runs)")
    kb = {}
    if os.path.exists(KB_FILE):
        try: kb = json.load(open(KB_FILE))
        except Exception: kb = {}
    today = dt.date.today().isoformat()
    seen_keys = set()
    for v in validated:
        key = f"{v['feature']}>={v['cutoff']}"
        seen_keys.add(key)
        rec = kb.get(key, {"first_seen": today, "validations": 0, "history": []})
        rec["feature"] = v["feature"]      # explicit for the Rust bridge (don't parse the key)
        rec["cutoff"] = v["cutoff"]
        rec["validations"] += 1
        rec["last_seen"] = today
        rec["last_precision"] = v["test_precision"]
        rec["recall"] = v["recall"]
        rec["history"] = (rec.get("history", []) + [v["test_precision"]])[-30:]
        # confidence grows with repeated independent validations and precision
        rec["confidence"] = round(min(1.0, rec["validations"] / 5.0) * v["test_precision"], 3)
        kb[key] = rec
    # decay confidence of patterns that did NOT re-validate this run
    for key, rec in kb.items():
        if key not in seen_keys:
            rec["confidence"] = round(rec.get("confidence", 0) * 0.8, 3)
    try:
        json.dump(kb, open(KB_FILE, "w"), indent=2)
    except Exception:
        pass
    ranked = sorted(kb.items(), key=lambda x: -x[1].get("confidence", 0))
    if ranked:
        print(f"  {'pattern':30} {'conf':>5} {'validations':>12} {'last prec':>10}")
        for key, rec in ranked[:12]:
            print(f"  {key:30} {rec.get('confidence',0):>5.2f} {rec.get('validations',0):>12} "
                  f"{rec.get('last_precision',0)*100:>9.0f}%")
        print(f"\n  {len(kb)} patterns tracked. Confidence rises as a pattern re-validates across")
        print("  runs and fades when it stops — this is the layer that learns over time.")
    else:
        print("  empty — no pattern has survived validation yet. It fills as the edge emerges.")

    print()
    hr()
    print("  This is RESEARCH, not signals. Every proposed filter is a hypothesis validated")
    print("  out-of-sample with conservative precision — apply them to selection via A/B,")
    print("  one at a time. The 80% target is a destination, not a license to overfit.")
    hr()


if __name__ == "__main__":
    main()
