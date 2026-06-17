# Production Readiness Roadmap

Honest path from "working dry-run momentum bot" to "trades real money safely and
profitably, unattended." Ordered by what actually gates production — not by what's
easiest to build.

The two real gates are at the top. Everything below them is moot until they pass:
1. **The edge must be proven** (dry run + small live, multiple windows).
2. **The bot must not lose track of money** when it crashes or restarts.

Status legend: ✅ done · 🟡 partial · ❌ missing

---

## Phase 0 — Validate the edge (GATE: no real capital until this passes)
No feature matters if it isn't profitable on real fills. This is the bottleneck.

- 🟡 Dry-run paper trading with analyzer + A/B compare — **built**, needs to be *used*.
- ❌ **Two+ independent dry-run windows green** at the chosen config (score 57,
      alpha-follow, scale-out). One positive window can be luck.
- ❌ **Alpha-follow proven to fire and pay** — confirm `ALPHA:` entries actually
      happen and show positive PnL in the per-wallet leaderboard. If it never fires,
      lower the thresholds; if it fires red, the learned signal isn't real edge yet.
- ❌ **Tiny live test (0.02–0.05 SOL/trade)** to measure the gap between *simulated*
      fills and *real* fills: slippage beyond the sim %, failed-tx rate, landing
      latency. This number is the moment of truth — the dry-run +0.49 is a ceiling.
- ❌ Decision rule written down: "go live at size X only if live-small matches dry
      run within Y%." Without a pre-committed rule, you'll rationalize a bad edge.

## Phase 1 — Survival-critical hardening (GATE: no real capital until this passes)
These are the ways a *technically working* bot still loses your money.

- ❌ **Open-position persistence + crash recovery.** `POSITIONS` is in-memory only.
      If the bot crashes or you restart it with open positions, it **forgets them and
      stops managing exits** — you silently hold bags with no stop-loss running. Must:
      persist positions to disk on every change, and on startup reconcile against
      actual on-chain token balances (adopt orphans, drop dust) before trading.
- 🟡 **Secret management.** Private key sits in plaintext `.env`. At minimum: confirm
      `.env` is gitignored (it is), **rotate the keys that were exposed in chat**, use
      a dedicated low-balance hot wallet (never your main), and never log key material.
- ❌ **Pre-buy balance + fee-reserve check.** Verify the wallet has size + fee buffer
      before sending a buy, so live buys fail *gracefully* instead of half-landing.
- ❌ **Unsellable-token handling.** A token that passes the rug veto can still become
      illiquid/honeypot post-buy. Need a max-retry sell, then quarantine + alert rather
      than retrying forever or marking a phantom exit.
- ✅ Circuit breaker (daily loss limit, consecutive losses, daily reset) + go-live gate.

## Phase 2 — Execution quality (makes live match the backtest)
- 🟡 **Exact on-chain PnL reconciliation.** Buy-leg reconcile exists; extend to every
      sell leg + fees so the ledger is ground truth, not estimate.
- 🟡 Transaction landing (zeroslot / jito / multi) — **built**; needs live
      success-rate measurement and a fallback when the primary route is failing.
- ❌ Blockhash/expiry + retry policy hardened for live (stale blockhash, dropped tx,
      re-sign vs re-quote).
- ❌ Slippage model calibrated from live fills (feed real slippage back into the sim
      cost fraction so dry runs stop lying).

## Phase 3 — Reliability & operations (unattended uptime)
- 🟡 Feed reconnect with backoff — **built** for the ws feed; verify under long runs.
- ❌ Process supervision (systemd / docker restart=always) so a crash auto-restarts —
      only safe *after* Phase 1 position recovery exists.
- ❌ Remote alerting (Telegram/Discord/push) on: halted, repeated buy/sell failures,
      feed down, drawdown threshold. You need to know *while away*, not via the
      dashboard you have to be watching.
- ✅ Live dashboard (status snapshot + terminal UI).
- ❌ Structured logs + log rotation; a daily PnL/health summary.
- ❌ Operational runbook: how to kill it, how to flatten all positions manually, what
      each halt reason means, recovery steps.

## Phase 4 — Testing & confidence
- ❌ Unit tests on the money-touching logic: scoring, exit ladder ordering, scale-out
      fraction/runner math, cost-basis/PnL, circuit-breaker transitions.
- ❌ Backtest harness on recorded streams so config changes can be evaluated offline
      before risking a live window.
- 🟡 Anti-fake / wash + rug veto — **built**; needs validation against known bad tokens.

## Phase 5 — Scale & optimize (only once consistently profitable small)
- ❌ Latency: colocate near the RPC/validator; measure end-to-end signal→land time.
- ❌ Capital scaling rules: grow size as a function of realized edge + drawdown, not
      gut feel. Per-token max exposure.
- ❌ Continuous edge monitoring: alert when live win-rate / avg-PnL drifts from the
      validated baseline (the edge decays as more people copy it).

---

## The blunt summary
- **Features:** ~70% there. Engine, exits, learning, dashboard, landing, discipline
  are built.
- **Edge:** unproven on real fills. This is the gate, not more features.
- **Safety:** the in-memory position handling (Phase 1) is the single scariest gap —
  a crash with open positions loses money silently. Fix that before any live run.

Recommended order: finish **Phase 0** (validate) and the **position-recovery** item
in Phase 1 *in parallel* — they're the only two things standing between today and a
defensible first live run with tiny size.
