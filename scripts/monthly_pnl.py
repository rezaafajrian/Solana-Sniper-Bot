#!/usr/bin/env python3
"""Aggregate the momentum trade log into a per-day realized-PnL calendar.

Reads momentum_trades.csv (the bot's close log) and writes monthly_pnl.json,
which web/month.html renders as a GitHub-style calendar heatmap: one cell per
day, colored by that day's net realized PnL (SOL), with month total, green/red
day counts and the day-level win rate.

Days are bucketed in the bot's local timezone (DAILY_RESET_UTC_OFFSET_HOURS,
default +7 / WIB) so a "day" on the calendar matches the dashboard's daily reset.

Usage:
    python3 scripts/monthly_pnl.py                 # month of the latest trade
    python3 scripts/monthly_pnl.py --month 2026-06 # a specific month
    python3 scripts/monthly_pnl.py --watch 15      # re-aggregate every 15s
"""
import csv, json, os, sys, time, calendar
from datetime import datetime, timezone, timedelta

TRADE_LOG = os.environ.get("MOMENTUM_TRADE_LOG", "momentum_trades.csv")
OUT = os.environ.get("MONTHLY_PNL_FILE", "monthly_pnl.json")
TZ_OFFSET_H = float(os.environ.get("DAILY_RESET_UTC_OFFSET_HOURS", "7"))
TZ = timezone(timedelta(hours=TZ_OFFSET_H))


def parse_args(argv):
    month = None
    watch = 0
    i = 0
    while i < len(argv):
        if argv[i] == "--month" and i + 1 < len(argv):
            month = argv[i + 1]; i += 2
        elif argv[i] == "--watch" and i + 1 < len(argv):
            watch = float(argv[i + 1]); i += 2
        else:
            i += 1
    return month, watch


def load_rows(path):
    """Yield (local_datetime, event, realized_pnl_sol) for each SELL/close row."""
    if not os.path.exists(path):
        return
    with open(path, newline="") as f:
        r = csv.reader(f)
        header = next(r, None)
        # Resolve columns by name (robust to schema drift); fall back to fixed indices.
        idx = {name: i for i, name in enumerate(header or [])}
        ts_i = idx.get("timestamp_unix", 0)
        ev_i = idx.get("event", 2)
        pnl_i = idx.get("est_realized_pnl_sol", 11)
        for row in r:
            if len(row) <= max(ts_i, ev_i, pnl_i):
                continue
            try:
                ts = int(float(row[ts_i]))
                pnl = float(row[pnl_i])
            except (ValueError, IndexError):
                continue
            ev = row[ev_i].strip().upper()
            # A "close" = a sell (partial or full). Buys carry pnl 0 and aren't closes.
            if not ev.startswith("SELL"):
                continue
            yield datetime.fromtimestamp(ts, TZ), ev, pnl


def aggregate(path, month_key):
    days = {}          # day-of-month -> {"pnl_sol": x, "closes": n}
    latest = None
    rows = list(load_rows(path))
    for dt, ev, pnl in rows:
        if latest is None or dt > latest:
            latest = dt
        if dt.strftime("%Y-%m") != month_key:
            continue
        d = dt.day
        slot = days.setdefault(d, {"pnl_sol": 0.0, "closes": 0})
        slot["pnl_sol"] += pnl
        slot["closes"] += 1
    return days, latest


def build(month_key):
    days, latest = aggregate(TRADE_LOG, month_key)
    year, month = int(month_key[:4]), int(month_key[5:7])

    total = sum(v["pnl_sol"] for v in days.values())
    closes = sum(v["closes"] for v in days.values())
    green = sum(1 for v in days.values() if v["pnl_sol"] > 0)
    red = sum(1 for v in days.values() if v["pnl_sol"] < 0)
    decided = green + red
    win_rate = (green / decided * 100.0) if decided else 0.0
    max_abs = max((abs(v["pnl_sol"]) for v in days.values()), default=0.0)

    best = max(days.items(), key=lambda kv: kv[1]["pnl_sol"], default=None)
    worst = min(days.items(), key=lambda kv: kv[1]["pnl_sol"], default=None)

    day_list = [
        {"day": d, "pnl_sol": round(v["pnl_sol"], 6), "closes": v["closes"]}
        for d, v in sorted(days.items())
    ]

    return {
        "month_label": f"{calendar.month_name[month].upper()} {year}",
        "year": year,
        "month": month,
        "first_weekday": calendar.monthrange(year, month)[0],  # 0=Mon … 6=Sun
        "days_in_month": calendar.monthrange(year, month)[1],
        "total_sol": round(total, 6),
        "closes": closes,
        "green_days": green,
        "red_days": red,
        "win_rate_pct": round(win_rate, 2),
        "max_abs": round(max_abs, 6),
        "best_day": ({"day": best[0], "pnl_sol": round(best[1]["pnl_sol"], 6)} if best else None),
        "worst_day": ({"day": worst[0], "pnl_sol": round(worst[1]["pnl_sol"], 6)} if worst else None),
        "days": day_list,
        "tz": f"GMT{'+' if TZ_OFFSET_H >= 0 else ''}{TZ_OFFSET_H:g}",
        "tz_offset_hours": TZ_OFFSET_H,
        "updated_utc": datetime.now(timezone.utc).strftime("%Y-%m-%d %H:%M UTC"),
        "latest_trade_local": latest.strftime("%Y-%m-%d %H:%M") if latest else None,
    }


def resolve_month(explicit):
    if explicit:
        return explicit
    # Default to the month containing the most recent trade; else current month.
    _, latest = aggregate(TRADE_LOG, "0000-00")  # month filter never matches → just find latest
    if latest:
        return latest.strftime("%Y-%m")
    return datetime.now(TZ).strftime("%Y-%m")


def run_once(explicit_month):
    month_key = resolve_month(explicit_month)
    data = build(month_key)
    tmp = OUT + ".tmp"
    with open(tmp, "w") as f:
        json.dump(data, f, indent=2)
    os.replace(tmp, OUT)
    return data


def main():
    explicit_month, watch = parse_args(sys.argv[1:])
    if watch > 0:
        print(f"monthly_pnl: watching {TRADE_LOG} → {OUT} every {watch:g}s (Ctrl-C to stop)")
        while True:
            try:
                d = run_once(explicit_month)
                print(f"  {d['month_label']}: {d['total_sol']:+.4f} SOL · "
                      f"{d['green_days']}g/{d['red_days']}r · {d['win_rate_pct']:.1f}% · {d['closes']} closes")
            except Exception as e:  # keep the watcher alive on transient read races
                print(f"  (skip: {e})")
            time.sleep(watch)
    else:
        d = run_once(explicit_month)
        print(f"{d['month_label']}: {d['total_sol']:+.4f} SOL · "
              f"{d['green_days']}g/{d['red_days']}r · {d['win_rate_pct']:.1f}% win · "
              f"{d['closes']} closes → {OUT}")


if __name__ == "__main__":
    main()
