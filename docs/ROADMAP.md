# Production Readiness Roadmap

## ⭐ North Star (the goal — remember this)
Turn a small stake into a large one — serious account growth. That ambition is the
point of the project. But the way you reach a big number is NOT one oversized bet
(e.g. 14 SOL into a 29 SOL mcap — that PnL is fantasy, you can't exit it). You reach
it by **compounding a PROVEN edge over many trades**:
- a real edge (validated live, not just simulated),
- percent-of-equity sizing so wins compound (size grows as the account grows),
- liquidity-aware caps so every position stays exitable (never be the exit liquidity),
- as equity grows, deploy across more positions and larger-mcap tokens that can
  absorb the size.
Big numbers = edge × compounding × time. The caps don't kill the dream — they make
it reachable instead of a sim illusion.

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

- ✅ **Open-position persistence + crash recovery.** Positions persist to disk on every
      change (entry/scale-out/exit) and are re-adopted on startup so the exit monitor
      resumes managing them. (Live on-chain balance reconciliation of recovered
      positions is still a refinement — see Phase 2.)
- 🟡 **Secret management.** Private key sits in plaintext `.env`. At minimum: confirm
      `.env` is gitignored (it is), **rotate the keys that were exposed in chat**, use
      a dedicated low-balance hot wallet (never your main), and never log key material.
- 🟡 **Dependency CVEs (`cargo audit`).** `Cargo.lock` is committed + audited. `quinn-proto`
      bumped to 0.11.15 (cleared the one remotely-triggerable advisory, RUSTSEC-2026-0185).
      The remaining 5 (ed25519-dalek 1.x oracle, curve25519-dalek 3.x timing, rustls-webpki
      0.101 ×3) are **baked into the solana 2.1 / anchor 0.31 tree** — unfixable without a
      major SDK upgrade. Consciously accepted in `.cargo/audit.toml` with rationale (low
      practical risk for a client-side signing bot). **Real fix = bump the Solana SDK**, a
      tracked Phase-5 task to do WITH full re-validation, not before the first live run.
- ✅ **Pre-buy balance + fee-reserve check.** momentum_buy verifies wallet balance >=
      size + MOMENTUM_FEE_RESERVE_SOL before sending; short balance skips the entry.
- ✅ **Anti-dump entry filters.** Base-momentum floor (boosts can't drag in dead charts),
      GMGN top-10-holder + bundle vetoes, and a free stream-based concentration veto,
      plus a stagnation time-stop that cuts dead positions before they bleed.
- ✅ **Unsellable-token handling.** After MOMENTUM_MAX_SELL_RETRIES failed sells a
      position is quarantined (auto-sell halted, alerted, surfaced on the dashboard)
      instead of spinning forever on an illiquid/honeypot token.
- ✅ Circuit breaker (daily loss limit, consecutive losses, daily reset) + go-live gate.

**Phase 1 is now functionally complete** — the remaining item is operational
(rotate exposed keys, use a dedicated hot wallet). Live on-chain reconciliation of
recovered positions stays in Phase 2 as a refinement.

## Phase 2 — Execution quality (makes live match the backtest)
- ✅ **Exact on-chain PnL reconciliation.** Both legs reconcile: spawn_buy_reconcile
      and spawn_reconcile read the true on-chain SOL delta and write BUY_ACTUAL /
      SELL_ACTUAL rows, correcting the live realized-PnL tally.
- ✅ **Slippage calibration from live fills.** The analyzer now matches each leg's
      estimate to its *_ACTUAL row and reports real buy/sell-leg slippage + the
      recommended MOMENTUM_SIM_COST_FRACTION so dry runs stop being optimistic. This
      is the "is the simulated PnL real?" answer — it just needs live fills to chew on.
- 🟡 Transaction landing (zeroslot / jito / multi) — **built**; success-rate
      telemetry added (tx_sent / tx_landed in the status snapshot). Still needs an
      automatic fallback when the primary route's land rate is poor.
- ❌ Blockhash/expiry + retry policy hardened for live (stale blockhash, dropped tx,
      re-sign vs re-quote).

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

## Planned deployment — gRPC + VPS, runs 24/7 ("the monster can't be stopped")
Decided direction for production hosting:
- **gRPC feed**: switch from the websocket feed to Yellowstone gRPC for lower latency.
  Already supported — set `MOMENTUM_FEED=grpc` + `YELLOWSTONE_GRPC_HTTP`/`_TOKEN`.
  (ws was the no-gRPC fallback; gRPC is the faster path once an endpoint is available.)
- **VPS, always-on**: run on a VPS so it isn't tied to the local machine's power. The
  bot is already VPS-ready — position persistence + crash recovery (Phase 1) means a
  restart re-adopts open positions, and the decision/outcome logs keep accumulating.

What an unattended 24/7 deployment makes NON-optional (do these before leaving it
running on real money):
- **Process supervision** (systemd/docker `restart=always`) — safe now that crash
  recovery exists.
- **Remote alerting** (Telegram/Discord) — if it halts, margin-calls, or the feed
  drops while you're away, you must be told. This is the top Phase-3 gap for 24/7.
- **The risk controls ARE the kill switch**: an always-on bot can't be babysat, so the
  margin call (bankroll floor), circuit breaker, and slippage calibration are what stop
  it quietly bleeding. Keep them ON in live; only disable for dry-run data gathering.

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
