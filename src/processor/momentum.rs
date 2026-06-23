/*!
# Momentum Sniper Module

A pump.fun trading engine that buys **strength, not age**. It ignores how old a
token is, whether it is still on the bonding curve, or how close it is to
migration. The only thing that matters is whether a token is *currently* showing
statistically significant momentum and a high probability of continuation.

## Philosophy

Every pump.fun trade on the chain is streamed in real time and aggregated into a
rolling, per-token picture of:

- accelerating buy pressure (buy volume rate now vs. the recent baseline)
- increasing raw volume
- increasing unique buyers / breadth of participation
- positive market-cap velocity
- healthy holder distribution (no single wallet dominating the buys)
- low evidence of coordinated dumping (sells overwhelming buys)

Each token gets a continuation-probability **score in 0..100**. When a token's
score clears the entry threshold and we have a free position slot, the bot buys.

## Exit policy (asymmetric returns)

- Hard stop loss at `MOMENTUM_HARD_STOP_PCT` (default -35%).
- Scale out: sell 20% of the original position at each of +100/+200/+300/+400%,
  keeping a final 20% **runner**.
- Cut failed momentum quickly: if the score collapses below
  `MOMENTUM_COLLAPSE_SCORE` or sells significantly exceed buys, exit the
  remaining position regardless of which rungs were hit.
- The runner is only released when momentum deteriorates — never sold purely
  because a profit target was reached.

The goal is **not** win rate. It is long-term expectancy: many small, quickly-cut
losses, and a few large winners ridden as long as continuation holds.

## Configuration (env)

```env
MONITORING_MODE=momentum            # activate this engine instead of copy-trading
MOMENTUM_POSITION_SIZE_SOL=0.2      # SOL per trade
MOMENTUM_MAX_POSITIONS=5            # max concurrent positions
MOMENTUM_ENTRY_SCORE=65             # 0..100 score required to buy
MOMENTUM_COLLAPSE_SCORE=35          # below this, cut the position
MOMENTUM_HARD_STOP_PCT=-35          # hard stop loss
MOMENTUM_SHORT_WINDOW_SECS=30       # "now" window
MOMENTUM_MEDIUM_WINDOW_SECS=120     # baseline window
MOMENTUM_MIN_BUY_VOLUME_SOL=2.0     # buy volume (short window) that scores full marks
MOMENTUM_TARGET_UNIQUE_BUYERS=10    # unique buyers (short window) that scores full marks
MOMENTUM_TARGET_MCAP_GROWTH=0.30    # short-window mcap growth that scores full marks
MOMENTUM_MAX_WALLET_CONCENTRATION=0.50  # max single-wallet share of buy volume before penalty
MOMENTUM_SCALE_OUT_TARGETS=100,200,300,400  # PnL % rungs, 20% each
MOMENTUM_SLIPPAGE_BPS=1000          # slippage for momentum buys/sells (basis points)
```
*/

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use colored::Colorize;
use dashmap::DashMap;
use futures_util::{SinkExt, StreamExt};
use lazy_static::lazy_static;
use tokio::time;

use yellowstone_grpc_client::{ClientTlsConfig, GeyserGrpcClient};
use yellowstone_grpc_proto::geyser::{
    subscribe_update::UpdateOneof, CommitmentLevel, SubscribeRequest, SubscribeRequestFilterTransactions,
    SubscribeRequestPing, SubscribeUpdateTransaction,
};

use solana_sdk::signer::Signer;

use crate::common::config::{AppState, SwapConfig};
use crate::common::logger::Logger;
use crate::library::gmgn::{GmgnClient, GmgnConfig, SecurityVerdict};
use once_cell::sync::OnceCell;
use crate::dex::pump_fun::{Pump, PUMP_FUN_PROGRAM, TOKEN_TOTAL_SUPPLY};
use crate::processor::sniper_bot::{SniperConfig, BOUGHT_TOKEN_LIST};
use crate::processor::swap::{SwapDirection, SwapInType};
use crate::processor::transaction_parser::{parse_transaction_data, DexType, TradeInfoFromToken};

const WSOL_MINT: &str = "So11111111111111111111111111111111111111112";
const TOKEN_DECIMALS: f64 = 1_000_000.0; // pump.fun tokens use 6 decimals
const LAMPORTS_PER_SOL: f64 = 1_000_000_000.0;

/// The ONLY programs a legitimate pump.fun buy/sell transaction may invoke. Any signed
/// instruction touching a program outside this set is treated as tampering/attack and
/// the trade is refused BEFORE signing. This is defense-in-depth on top of the committed
/// lockfile: even if a dependency were compromised or a data path poisoned, the bot will
/// never sign a transaction that calls an arbitrary (wallet-draining) program. The set is
/// derived from the actual swap path: pump.fun swap, ATA create, SPL-token close, plus the
/// compute-budget + system (tip) instructions the landing layer appends.
const ALLOWED_TX_PROGRAMS: &[&str] = &[
    PUMP_FUN_PROGRAM,                               // pump.fun bonding-curve swap
    "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA",  // SPL Token (ATA close)
    "TokenzQdBNbLqP5VEhdkAS6EPFLC1PHnBqCXEpPxuEb",  // SPL Token-2022
    "ATokenGPvbdGVxr1b2hvZbsiqW5xWH25efTNsLJA8knL", // Associated Token Account (ATA create)
    "11111111111111111111111111111111",             // System program (tip transfer)
    "ComputeBudget111111111111111111111111111111",  // Compute budget (priority fee)
];

/// Anti-tamper guard: reject a transaction before signing if any instruction calls a
/// program outside `ALLOWED_TX_PROGRAMS`, or if no pump.fun swap instruction is present.
/// Returns the offending program on failure so it's logged. Pure in-memory (zero latency).
fn guard_swap_instructions(instructions: &[solana_sdk::instruction::Instruction]) -> Result<(), String> {
    if instructions.is_empty() {
        return Err("no instructions to send".to_string());
    }
    let mut saw_pump = false;
    for ix in instructions {
        let prog = ix.program_id.to_string();
        if !ALLOWED_TX_PROGRAMS.contains(&prog.as_str()) {
            return Err(format!("unexpected program {} — refusing to sign (possible tampering)", prog));
        }
        if prog == PUMP_FUN_PROGRAM {
            saw_pump = true;
        }
    }
    if !saw_pump {
        return Err("no pump.fun swap instruction present — refusing to sign".to_string());
    }
    Ok(())
}

/// Honeypot pre-check (read-only, cached). Reads the SPL mint and rejects tokens whose
/// authorities are still live: a set FREEZE authority lets the creator freeze your token
/// account so you can never sell (the on-curve honeypot vector); a set MINT authority lets
/// them inflate supply and dilute/rug. Legit pump.fun tokens renounce BOTH at creation, so
/// this passes them and only catches anomalies. Returns Some(reason) ONLY on a positive
/// unsafe determination — RPC/parse failures return None (fail-open: don't kill the edge on
/// a flaky read; the pump.fun bonding curve is structurally sellable anyway).
async fn authority_honeypot_reason(
    rpc: &Arc<anchor_client::solana_client::nonblocking::rpc_client::RpcClient>,
    mint: &str,
) -> Option<String> {
    use solana_program_pack::Pack;
    use std::str::FromStr;
    if let Some(v) = MINT_AUTHORITY_CHECKED.get(mint) {
        // Only safe verdicts are cached; nothing to reject on a cache hit.
        let _ = v;
        return None;
    }
    let pk = solana_sdk::pubkey::Pubkey::from_str(mint).ok()?;
    let acct = match rpc.get_account(&pk).await {
        Ok(a) => a,
        Err(_) => return None, // fail-open on RPC error
    };
    let m = match spl_token::state::Mint::unpack(&acct.data) {
        Ok(m) => m,
        Err(_) => return None, // not a standard SPL mint we can parse → don't block
    };
    if m.freeze_authority.is_some() {
        return Some("freeze authority active — token can be frozen (unsellable honeypot)".to_string());
    }
    if m.mint_authority.is_some() {
        return Some("mint authority active — supply can be inflated (dilution/rug)".to_string());
    }
    MINT_AUTHORITY_CHECKED.insert(mint.to_string(), true);
    None
}

/// Opt-in pre-send simulation. Builds and simulates the (signed) transaction against the
/// RPC before broadcasting; if the simulation REVERTS, the trade is aborted. On a buy this
/// catches a doomed entry before spending a real tx + fees; on a sell it catches a
/// honeypot / sell-path revert before firing a real, failing sell. Only a positive revert
/// blocks — RPC transport errors fail open so a flaky node doesn't freeze all trading.
/// Adds an RPC round-trip, so it's off by default (enable for cautious live runs).
async fn simulate_before_send(
    rpc: &Arc<anchor_client::solana_client::nonblocking::rpc_client::RpcClient>,
    keypair: &anchor_client::solana_sdk::signature::Keypair,
    instructions: &[solana_sdk::instruction::Instruction],
    blockhash: solana_sdk::hash::Hash,
    leg: &str,
) -> Result<(), String> {
    let tx = anchor_client::solana_sdk::transaction::Transaction::new_signed_with_payer(
        instructions,
        Some(&keypair.pubkey()),
        &vec![keypair],
        blockhash,
    );
    match rpc.simulate_transaction(&tx).await {
        Ok(resp) => {
            if let Some(err) = resp.value.err {
                let tail = resp.value.logs.unwrap_or_default()
                    .into_iter().rev().take(3).collect::<Vec<_>>().join(" | ");
                return Err(format!("{} simulation reverted: {:?} [{}]", leg, err, tail));
            }
            Ok(())
        }
        Err(_) => Ok(()), // RPC transport error — fail open (only block on a positive revert)
    }
}

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct MomentumConfig {
    pub position_size_sol: f64,
    /// Position size as a FRACTION of current equity (bankroll mode only). When > 0,
    /// each buy is `equity * this` instead of the fixed `position_size_sol` — so the
    /// bet scales up as the account grows and shrinks as it draws down. e.g. 0.05 = 5%.
    pub position_size_pct: f64,
    /// Liquidity caps so a position can never be so large you become the token's exit
    /// liquidity. entry_size <= mcap*max_mcap_fraction AND curve_liq*max_liq_fraction.
    pub max_mcap_fraction: f64,
    pub max_liq_fraction: f64,
    /// Skip a token if its liquidity caps the size below this (too illiquid to bother).
    pub min_position_sol: f64,
    pub max_positions: usize,
    pub entry_score: f64,
    /// Minimum BASE momentum score (before KOL/alpha/GMGN boosts) required to enter.
    /// Stops boost-only entries on weak tokens — you need real momentum AND a signal,
    /// not just a smart-wallet tag on a dead chart. 0 disables.
    pub min_base_score: f64,
    pub collapse_score: f64,
    pub hard_stop_pct: f64,
    /// Stagnation time-stop: if a position never reaches `stagnation_min_pnl`% within
    /// `stagnation_secs`, cut it — dead tokens tie up a slot and bleed into a hard stop.
    /// 0 secs disables.
    pub stagnation_secs: u64,
    pub stagnation_min_pnl: f64,
    /// Hard max hold time (seconds): force-close any position older than this — kills
    /// "zombies" (tokens that stopped trading, incl. positions recovered from a prior
    /// run with no live price feed) that would otherwise hold a slot forever. 0 = off.
    pub max_hold_secs: u64,
    /// Fast reaper: force-close a position whose token hasn't traded in this many
    /// seconds (a dead token) — clears zombies in minutes, not hours. 0 = off.
    pub stale_exit_secs: u64,
    pub short_window_secs: u64,
    pub medium_window_secs: u64,
    pub min_buy_volume_sol: f64,
    pub target_unique_buyers: f64,
    pub target_mcap_growth: f64,
    pub max_wallet_concentration: f64,
    /// Minimum ratio of unique buyers to buy transactions. Below this, the volume
    /// looks manufactured (few wallets making many buys) and the score is cut.
    pub min_buyer_diversity: f64,
    /// Max share of buy volume from wallets that ALSO sold in the window
    /// (round-tripping / wash trading). Above this, the score is cut.
    pub max_wash_fraction: f64,
    /// Max share of short-window buy VOLUME allowed from the token creator's own wallet
    /// before the momentum score is discounted — kills "fake pump" from creator/bundler
    /// self-buying so it can't clear the entry gate. Above this, score is cut toward 0.
    pub max_creator_buy_frac: f64,
    // ---- Anti-dump: stream-based concentration veto (no API, works on fresh tokens) ----
    /// Reject entry if the top-N traders hold more than this share of the net
    /// trader-held float (a free proxy for holder concentration). 0 disables.
    pub max_top_holder_share: f64,
    /// How many top traders to sum for the concentration check.
    pub top_holder_n: usize,
    /// Reject entry if the token creator holds more than this share of the net
    /// trader float (a proxy for bundled/insider supply). 0 disables.
    pub max_creator_share: f64,
    /// Minimum distinct holders before the concentration veto applies — below this
    /// the token is too young to judge concentration (top-N would be ~100%).
    pub concentration_min_traders: usize,
    // ---- Anti-dump: insider-distribution entry veto (MELT-inspired) ----
    /// Reject entry if the top early buyers have already SOLD back more than this
    /// fraction of what they bought — insiders are distributing, you'd be exit
    /// liquidity. Age-independent (a sold/bought ratio). 0 disables.
    pub insider_distrib_max_sold: f64,
    /// How many top early buyers (by buy volume) to check for distribution.
    pub insider_distrib_top_n: usize,
    /// Auto-mute a curated KOL once its tracked PnL goes net-negative over enough
    /// trades — so a losing KOL stops boosting entries without manual list edits.
    pub kol_automute: bool,
    pub kol_automute_min: u32,
    pub scale_out_targets: Vec<f64>,
    /// Fraction of the ORIGINAL position to sell at each corresponding rung.
    /// Aligned 1:1 with `scale_out_targets`. The runner (held until collapse/
    /// migration) is whatever's left: 1.0 - sum(fractions).
    pub scale_out_fractions: Vec<f64>,
    pub slippage_bps: u64,
    /// Allow buying a token again (after the exit cooldown) if it re-pumps.
    pub allow_reentry: bool,
    /// Seconds to wait after an exit before the same token may be re-entered.
    pub reentry_cooldown_secs: u64,
    /// Dry run: simulate fills instead of sending transactions (no money at risk).
    pub dry_run: bool,
    /// Simulated round-trip cost as a fraction of fill value (fees + tip + slippage),
    /// applied to each simulated buy and sell so paper PnL reflects reality.
    pub sim_cost_fraction: f64,

    // ---- Edge: smart-money memory ----
    /// Build per-wallet reputation from observed outcomes and bias entries toward
    /// tokens that proven wallets are buying.
    pub smart_money_enabled: bool,
    /// Minimum buy size (SOL) for a wallet's participation to be tracked/scored.
    pub smart_money_min_sol: f64,
    /// Seconds after a tracked buy at which we grade the wallet on the outcome.
    pub smart_money_horizon_secs: u64,
    /// Reputation samples a wallet needs before it can influence scoring.
    pub smart_money_min_samples: u32,
    /// Max additive score points smart-money confirmation can add (0..100 scale).
    pub smart_money_boost_max: f64,
    /// Sum of buyer reputations that earns the full boost.
    pub smart_money_boost_scale: f64,
    /// Reputation half-life (seconds): old performance decays so stale wallets
    /// fade and recent results dominate. Guards against ossified reputations.
    pub smart_money_halflife_secs: u64,
    /// File to persist wallet reputation across restarts (compounding edge).
    pub wallet_rep_file: String,

    // ---- Edge: alpha-follow (act on the learned smart-money memory) ----
    /// Follow PROVEN wallets: when a single wallet whose learned reputation clears
    /// the bar buys a token, treat it as a high-conviction entry signal. This is how
    /// the bot acts on its own memory — and it compounds, since more wallets cross
    /// the bar as reputation accumulates across runs.
    pub alpha_follow_enabled: bool,
    /// Minimum learned reputation (EMA of forward returns) for a wallet to be "alpha".
    pub alpha_rep_min: f64,
    /// Minimum recency-weighted wallet score (0..100) for a wallet to be "proven"
    /// and followable. 60 = WATCHLIST tier and up. Recent performance dominates it.
    pub alpha_min_wallet_score: f64,
    /// Minimum reputation samples before a wallet can be followed as alpha.
    pub alpha_min_samples: u32,
    /// Score points added when an alpha wallet is buying (0..100 scale).
    pub alpha_boost: f64,
    /// Seconds an alpha buy keeps a token "hot" for entry.
    pub alpha_window_secs: u64,
    /// Position-size multiple applied when following a proven wallet (KOL or alpha).
    pub alpha_size_mult: f64,
    /// Convergence: distinct proven wallets net-accumulating the same token at once.
    /// At/above this count it's treated as a high-conviction "convergence" entry.
    pub convergence_min: usize,
    /// Score boost when convergence fires (stronger than a single alpha wallet).
    pub convergence_boost: f64,
    /// Extra position-size multiple when convergence fires (on top of alpha_size_mult).
    pub convergence_size_mult: f64,

    // ---- "Ones to Watch": high-conviction composite watchlist ----
    /// Composite watch score = market-structure sub-score + smart-money convergence.
    /// Tokens clearing `watch_score_min` are flagged & tracked. In beta (autobuy off)
    /// they only alert; once learn.py validates them, autobuy buys them at watch_size.
    pub watch_enabled: bool,
    pub watch_score_min: f64,
    pub watch_autobuy: bool,
    pub watch_size_sol: f64,
    /// Weight on the market-structure half of the composite (rest goes to convergence).
    pub watch_ms_weight: f64,

    // ---- Learned avoidance: stop repeating mistakes (creator + pattern) ----
    /// Skip a token whose CREATOR has lost money for the bot before (decaying memory).
    pub creator_avoid: bool,
    /// Minimum closed trades on a creator before its record is trusted.
    pub creator_min_trades: u32,
    /// Veto a creator whose decaying realized PnL/token is at or below this (SOL).
    pub creator_avoid_pnl: f64,
    /// Penalize entries whose feature pattern (signal type, conviction, mcap, score
    /// band) has been losing — the bot learns which kinds of tokens burn it.
    pub pattern_avoid: bool,
    /// Min closed samples in a pattern band before it can penalize.
    pub pattern_min_samples: u32,
    /// Max entry-score penalty applied when a token matches losing patterns.
    pub pattern_penalty_max: f64,
    /// File persisting the learned creator + pattern memory across runs.
    pub avoidance_file: String,

    // ---- Edge: insider / leader-dump exit ----
    /// Exit immediately when the creator or top early buyers start distributing.
    pub leader_dump_exit_enabled: bool,
    /// How many top early buyers (by volume) to track per position, plus the creator.
    pub leader_track_top_n: usize,
    /// If tracked wallets sell at least this many SOL in the short window, exit.
    pub leader_dump_sol: f64,
    /// Fraction of the held position to sell on an insider-dump signal (1.0 = full
    /// exit, the protective default; lower keeps a runner that rides via the trail).
    pub leader_dump_fraction: f64,
    /// SLOW-RUG exit: total SOL the tracked (creator + early) wallets may cumulatively
    /// sell over the WHOLE hold before we exit — catches steady distribution that never
    /// trips the acute `leader_dump_sol` threshold in any single window. 0 disables.
    pub slow_rug_sol: f64,

    // ---- Edge: trailing stop (let winners run) ----
    /// Once a position's peak PnL clears `trail_activate_pct`, ride it and exit only
    /// when it gives back `trail_giveback_frac` of that peak — instead of dumping the
    /// whole bag on the first momentum wobble. Captures continued pumps.
    pub trail_enabled: bool,
    /// Peak PnL% a position must reach before the trailing stop arms.
    pub trail_activate_pct: f64,
    /// Fraction of the peak gain given back (from the peak) that triggers the exit.
    pub trail_giveback_frac: f64,

    // ---- Edge: conviction-based sizing ----
    /// Scale position size up with the entry score (higher conviction = bigger size).
    pub conviction_sizing: bool,
    /// Maximum size multiple at very high scores.
    pub conviction_max_mult: f64,

    // ---- Risk controls ----
    /// Hard cap on total SOL deployed across all open positions. New entries are
    /// blocked (or trimmed) so concurrent + conviction sizing can't overspend.
    pub max_deployed_sol: f64,
    /// Starting capital for a REAL bankroll simulation. When > 0, the bot models a
    /// finite account: equity = start_capital + realized PnL, you can only deploy what
    /// you have, the account compounds on profit, and a margin call halts new entries
    /// when equity can no longer fund a position. 0 = legacy fixed max_deployed cap.
    pub start_capital_sol: f64,
    /// Margin call when equity drops below this fraction of starting capital
    /// (bankroll mode). Default 0.10 = "lost 90%, you're ruined". Works for both
    /// fixed and percent-of-equity sizing.
    pub bankruptcy_floor_frac: f64,
    /// Estimated entry-leg cost (fees + tip + slippage) as a fraction of size,
    /// folded into the cost basis so realized PnL isn't optimistic about the buy.
    pub buy_cost_fraction: f64,
    /// SOL to keep in reserve for fees/tips so a live buy never drains the wallet
    /// below what it needs to pay for the eventual sell. Buys are skipped if the
    /// wallet balance is below entry_size + this reserve.
    pub fee_reserve_sol: f64,
    /// SECURITY TRIPWIRE: refuse to start LIVE if the wallet holds more than this many
    /// SOL — a guard against ever accidentally pointing the auto-signing bot at your
    /// main wallet. A hot wallet should hold only what you'd accept losing. 0 = disabled.
    pub max_wallet_sol: f64,
    /// Failed sell attempts before a position is quarantined (auto-sell halted, alerted)
    /// — stops the bot spinning forever on an illiquid/honeypot token. 0 = never quarantine.
    pub max_sell_retries: u32,
    /// Anti-tamper guard: refuse to sign any transaction that invokes a program outside
    /// the canonical pump.fun set (defense-in-depth against a poisoned build/feed). On by
    /// default; only disable if a future legitimate program addition trips a false positive.
    pub guard_programs: bool,
    /// Honeypot pre-check: read each token's SPL mint before buying and reject it if the
    /// freeze authority (can freeze your account → unsellable) or mint authority (can
    /// inflate supply) is still live. On by default; fails open on RPC errors.
    pub authority_check: bool,
    /// Opt-in pre-send simulation: simulate each buy/sell tx before broadcasting and abort
    /// if it would revert (catches honeypots / sell-path reverts / doomed builds). Adds an
    /// RPC round-trip per trade, so it's OFF by default — enable for cautious live runs.
    pub presend_simulate: bool,
    /// Minimum number of *distinct* reputable wallets required before smart-money
    /// boost applies — guards against a single farmed wallet baiting the bot.
    pub smart_money_min_distinct: usize,

    // ---- GMGN integration ----
    /// Veto buys using GMGN's /v1/token/security (honeypot / rug_ratio).
    pub gmgn_security_veto: bool,
    /// Score points added when a mint is on GMGN's smart-money / trenches watchlist.
    pub gmgn_boost: f64,
    /// Seconds between GMGN smartmoney/trenches polls.
    pub gmgn_poll_secs: u64,
    /// GMGN trenches filter preset (safe | smart-money | strict).
    pub gmgn_trenches_preset: String,
    /// How long a GMGN watchlist entry stays hot (seconds).
    pub gmgn_watchlist_ttl_secs: u64,
    /// Max smartmoney trades to pull per poll.
    pub gmgn_smartmoney_limit: u64,

    // ---- KOL tracking (your private edge) ----
    /// Treat buys from a curated KOL wallet list as a leading entry signal.
    pub kol_enabled: bool,
    /// File of KOL wallets, one per line: `wallet[,weight][,label]`.
    pub kol_file: String,
    /// Score points added to a token a KOL just bought (per unit weight).
    pub kol_boost: f64,
    /// Seconds a KOL buy keeps a mint "hot".
    pub kol_window_secs: u64,
    /// If true, ONLY enter tokens a KOL bought (momentum/LP/anti-fake just confirm).
    pub kol_require: bool,
    /// Hot-reload the KOL file every N seconds (0 = load once). Lets a cron'd
    /// Dune API fetch refresh the list live without restarting the bot.
    pub kol_reload_secs: u64,
    /// Path for the live dashboard status snapshot JSON ("" disables).
    pub status_file: String,
    /// File to persist open positions so a crash/restart never loses exit
    /// management of money already in the market ("" disables persistence).
    pub positions_file: String,
    /// Decision/learning log: record EVERY token evaluation (buy or reject) plus its
    /// post-detection outcome, so the strategy can be studied and improved offline.
    /// Empty base path disables it.
    pub decision_log_file: String,
    /// How long to track a token's post-detection price before finalizing its outcome
    /// label (seconds). Longer captures bigger/slower pumps for the "why did it pump"
    /// research; shorter labels faster. Default 2h.
    pub outcome_horizon_secs: u64,
    /// Transaction landing route: "zeroslot" | "jito" | "multi" (jito+rpc broadcast).
    pub landing: String,
    /// Exit a held token when its real SOL reserves reach this (bonding curve is
    /// about to complete/migrate to Raydium/PumpSwap, after which our bonding-curve
    /// pricing no longer applies). pump.fun graduates around ~85 SOL. 0 disables.
    pub migration_exit_sol: f64,

    // ---- Discipline: session circuit breaker + go-live gate ----
    /// Halt NEW entries once session realized PnL drops to -this many SOL.
    /// Open positions are still managed/exited normally. 0 disables.
    pub daily_loss_limit_sol: f64,
    /// Halt NEW entries after this many consecutive losing full exits. 0 disables.
    pub max_consecutive_losses: u32,
    /// Explicit acknowledgement required to trade live (when dry_run=false).
    pub live_confirmed: bool,
    /// Auto-reset the circuit breaker (loss limit + streak) at each day boundary.
    pub daily_reset: bool,
    /// Hour offset from UTC defining when the trading "day" rolls over.
    pub daily_reset_utc_offset_hours: i64,
}

fn env_f64(key: &str, default: f64) -> f64 {
    std::env::var(key).ok().and_then(|v| v.parse::<f64>().ok()).unwrap_or(default)
}
fn env_u64(key: &str, default: u64) -> u64 {
    std::env::var(key).ok().and_then(|v| v.parse::<u64>().ok()).unwrap_or(default)
}
fn env_usize(key: &str, default: usize) -> usize {
    std::env::var(key).ok().and_then(|v| v.parse::<usize>().ok()).unwrap_or(default)
}

impl MomentumConfig {
    pub fn from_env() -> Self {
        let scale_out_targets = std::env::var("MOMENTUM_SCALE_OUT_TARGETS")
            .unwrap_or_else(|_| "100,200,300,400".to_string())
            .split(',')
            .filter_map(|s| s.trim().parse::<f64>().ok())
            .collect::<Vec<f64>>();
        let scale_out_targets = if scale_out_targets.is_empty() {
            vec![100.0, 200.0, 300.0, 400.0]
        } else {
            scale_out_targets
        };

        // Per-rung sell fractions (of the ORIGINAL position), aligned 1:1 with the
        // targets. Default each rung to 0.20. Missing rungs default to 0.20, extras
        // are dropped, and cumulative is clamped to <= 1.0 so the runner (what's left)
        // is never negative. Sell less per rung to keep a fatter runner for moonshots.
        let scale_out_fractions = {
            let mut f = std::env::var("MOMENTUM_SCALE_OUT_FRACTIONS")
                .unwrap_or_default()
                .split(',')
                .filter_map(|s| s.trim().parse::<f64>().ok())
                .collect::<Vec<f64>>();
            f.resize(scale_out_targets.len(), 0.20);
            let mut cum: f64 = 0.0;
            for x in f.iter_mut() {
                *x = x.clamp(0.0, (1.0 - cum).max(0.0));
                cum += *x;
            }
            f
        };

        Self {
            position_size_sol: env_f64("MOMENTUM_POSITION_SIZE_SOL", 0.2),
            position_size_pct: env_f64("MOMENTUM_POSITION_SIZE_PCT", 0.0),
            max_mcap_fraction: env_f64("MOMENTUM_MAX_MCAP_FRACTION", 0.02),
            max_liq_fraction: env_f64("MOMENTUM_MAX_LIQ_FRACTION", 0.10),
            min_position_sol: env_f64("MOMENTUM_MIN_POSITION_SOL", 0.01),
            max_positions: env_usize("MOMENTUM_MAX_POSITIONS", 5),
            entry_score: env_f64("MOMENTUM_ENTRY_SCORE", 65.0),
            min_base_score: env_f64("MOMENTUM_MIN_BASE_SCORE", 0.0),
            collapse_score: env_f64("MOMENTUM_COLLAPSE_SCORE", 35.0),
            hard_stop_pct: env_f64("MOMENTUM_HARD_STOP_PCT", -35.0),
            stagnation_secs: env_u64("MOMENTUM_STAGNATION_SECS", 0),
            stagnation_min_pnl: env_f64("MOMENTUM_STAGNATION_MIN_PNL", 20.0),
            max_hold_secs: env_u64("MOMENTUM_MAX_HOLD_SECS", 10800),
            stale_exit_secs: env_u64("MOMENTUM_STALE_EXIT_SECS", 180),
            short_window_secs: env_u64("MOMENTUM_SHORT_WINDOW_SECS", 30),
            medium_window_secs: env_u64("MOMENTUM_MEDIUM_WINDOW_SECS", 120),
            min_buy_volume_sol: env_f64("MOMENTUM_MIN_BUY_VOLUME_SOL", 2.0),
            target_unique_buyers: env_f64("MOMENTUM_TARGET_UNIQUE_BUYERS", 10.0),
            target_mcap_growth: env_f64("MOMENTUM_TARGET_MCAP_GROWTH", 0.30),
            max_wallet_concentration: env_f64("MOMENTUM_MAX_WALLET_CONCENTRATION", 0.50),
            min_buyer_diversity: env_f64("MOMENTUM_MIN_BUYER_DIVERSITY", 0.35),
            max_wash_fraction: env_f64("MOMENTUM_MAX_WASH_FRACTION", 0.40),
            max_creator_buy_frac: env_f64("MOMENTUM_MAX_CREATOR_BUY_FRAC", 0.15),
            max_top_holder_share: env_f64("MOMENTUM_MAX_TOP_HOLDER_SHARE", 0.0),
            top_holder_n: env_usize("MOMENTUM_TOP_HOLDER_N", 10),
            max_creator_share: env_f64("MOMENTUM_MAX_CREATOR_SHARE", 0.0),
            concentration_min_traders: env_usize("MOMENTUM_CONCENTRATION_MIN_TRADERS", 25),
            insider_distrib_max_sold: env_f64("MOMENTUM_INSIDER_DISTRIB_MAX_SOLD", 0.5),
            insider_distrib_top_n: env_usize("MOMENTUM_INSIDER_DISTRIB_TOP_N", 5),
            kol_automute: std::env::var("MOMENTUM_KOL_AUTOMUTE").map(|v| v.to_lowercase() != "false").unwrap_or(true),
            kol_automute_min: env_u64("MOMENTUM_KOL_AUTOMUTE_MIN", 4) as u32,
            scale_out_targets,
            scale_out_fractions,
            slippage_bps: env_u64("MOMENTUM_SLIPPAGE_BPS", 1000),
            allow_reentry: std::env::var("MOMENTUM_ALLOW_REENTRY")
                .map(|v| v.to_lowercase() != "false")
                .unwrap_or(true),
            reentry_cooldown_secs: env_u64("MOMENTUM_REENTRY_COOLDOWN_SECS", 300),
            dry_run: std::env::var("MOMENTUM_DRY_RUN")
                .map(|v| v.to_lowercase() == "true")
                .unwrap_or(false),
            sim_cost_fraction: env_f64("MOMENTUM_SIM_COST_FRACTION", 0.03),

            smart_money_enabled: std::env::var("MOMENTUM_SMART_MONEY")
                .map(|v| v.to_lowercase() != "false")
                .unwrap_or(true),
            smart_money_min_sol: env_f64("MOMENTUM_SMART_MONEY_MIN_SOL", 0.5),
            smart_money_horizon_secs: env_u64("MOMENTUM_SMART_MONEY_HORIZON_SECS", 120),
            smart_money_min_samples: env_u64("MOMENTUM_SMART_MONEY_MIN_SAMPLES", 3) as u32,
            smart_money_boost_max: env_f64("MOMENTUM_SMART_MONEY_BOOST_MAX", 15.0),
            smart_money_boost_scale: env_f64("MOMENTUM_SMART_MONEY_BOOST_SCALE", 1.0),
            smart_money_halflife_secs: env_u64("MOMENTUM_SMART_MONEY_HALFLIFE_SECS", 604_800),
            wallet_rep_file: std::env::var("MOMENTUM_REP_FILE")
                .unwrap_or_else(|_| "momentum_wallet_rep.csv".to_string()),

            alpha_follow_enabled: std::env::var("MOMENTUM_ALPHA_FOLLOW")
                .map(|v| v.to_lowercase() != "false")
                .unwrap_or(true),
            alpha_rep_min: env_f64("MOMENTUM_ALPHA_REP_MIN", 0.5),
            alpha_min_wallet_score: env_f64("MOMENTUM_ALPHA_MIN_WALLET_SCORE", 60.0),
            alpha_min_samples: env_u64("MOMENTUM_ALPHA_MIN_SAMPLES", 5) as u32,
            alpha_boost: env_f64("MOMENTUM_ALPHA_BOOST", 35.0),
            alpha_window_secs: env_u64("MOMENTUM_ALPHA_WINDOW_SECS", 60),
            alpha_size_mult: env_f64("MOMENTUM_ALPHA_SIZE_MULT", 1.5),
            convergence_min: env_usize("MOMENTUM_CONVERGENCE_MIN", 2),
            convergence_boost: env_f64("MOMENTUM_CONVERGENCE_BOOST", 55.0),
            convergence_size_mult: env_f64("MOMENTUM_CONVERGENCE_SIZE_MULT", 1.5),
            watch_enabled: std::env::var("MOMENTUM_WATCH_ENABLED").map(|v| v.to_lowercase() != "false").unwrap_or(true),
            watch_score_min: env_f64("MOMENTUM_WATCH_SCORE", 70.0),
            watch_autobuy: std::env::var("MOMENTUM_WATCH_AUTOBUY").map(|v| v.to_lowercase() == "true").unwrap_or(false),
            watch_size_sol: env_f64("MOMENTUM_WATCH_SIZE_SOL", 0.5),
            watch_ms_weight: env_f64("MOMENTUM_WATCH_MS_WEIGHT", 0.5).clamp(0.0, 1.0),
            creator_avoid: std::env::var("MOMENTUM_CREATOR_AVOID").map(|v| v.to_lowercase() != "false").unwrap_or(true),
            creator_min_trades: env_u64("MOMENTUM_CREATOR_MIN_TRADES", 2) as u32,
            creator_avoid_pnl: env_f64("MOMENTUM_CREATOR_AVOID_PNL", -0.02),
            pattern_avoid: std::env::var("MOMENTUM_PATTERN_AVOID").map(|v| v.to_lowercase() != "false").unwrap_or(true),
            pattern_min_samples: env_u64("MOMENTUM_PATTERN_MIN_SAMPLES", 20) as u32,
            pattern_penalty_max: env_f64("MOMENTUM_PATTERN_PENALTY_MAX", 25.0),
            avoidance_file: std::env::var("MOMENTUM_AVOIDANCE_FILE").unwrap_or_else(|_| "momentum_avoidance.csv".to_string()),

            leader_dump_exit_enabled: std::env::var("MOMENTUM_LEADER_DUMP_EXIT")
                .map(|v| v.to_lowercase() != "false")
                .unwrap_or(true),
            leader_track_top_n: env_usize("MOMENTUM_LEADER_TRACK_TOP_N", 5),
            leader_dump_sol: env_f64("MOMENTUM_LEADER_DUMP_SOL", 1.0),
            leader_dump_fraction: env_f64("MOMENTUM_LEADER_DUMP_FRACTION", 1.0).clamp(0.0, 1.0),
            slow_rug_sol: env_f64("MOMENTUM_SLOW_RUG_SOL", 0.0),

            trail_enabled: std::env::var("MOMENTUM_TRAIL_ENABLED")
                .map(|v| v.to_lowercase() != "false")
                .unwrap_or(true),
            trail_activate_pct: env_f64("MOMENTUM_TRAIL_ACTIVATE_PCT", 50.0),
            trail_giveback_frac: env_f64("MOMENTUM_TRAIL_GIVEBACK_FRAC", 0.35).clamp(0.05, 0.95),

            conviction_sizing: std::env::var("MOMENTUM_CONVICTION_SIZING")
                .map(|v| v.to_lowercase() == "true")
                .unwrap_or(false),
            conviction_max_mult: env_f64("MOMENTUM_CONVICTION_MAX_MULT", 2.0),

            max_deployed_sol: env_f64("MOMENTUM_MAX_DEPLOYED_SOL", 1.0),
            start_capital_sol: env_f64("MOMENTUM_START_CAPITAL_SOL", 0.0),
            bankruptcy_floor_frac: env_f64("MOMENTUM_BANKRUPTCY_FLOOR_FRAC", 0.10),
            buy_cost_fraction: env_f64("MOMENTUM_BUY_COST_FRACTION", 0.015),
            fee_reserve_sol: env_f64("MOMENTUM_FEE_RESERVE_SOL", 0.02),
            max_wallet_sol: env_f64("MOMENTUM_MAX_WALLET_SOL", 0.0),
            max_sell_retries: env_u64("MOMENTUM_MAX_SELL_RETRIES", 8) as u32,
            guard_programs: std::env::var("MOMENTUM_GUARD_PROGRAMS").map(|v| v.to_lowercase() != "false").unwrap_or(true),
            authority_check: std::env::var("MOMENTUM_AUTHORITY_CHECK").map(|v| v.to_lowercase() != "false").unwrap_or(true),
            presend_simulate: std::env::var("MOMENTUM_PRESEND_SIMULATE").map(|v| v.to_lowercase() == "true").unwrap_or(false),
            smart_money_min_distinct: env_usize("MOMENTUM_SMART_MONEY_MIN_DISTINCT", 2),

            gmgn_security_veto: std::env::var("GMGN_SECURITY_VETO").map(|v| v.to_lowercase() != "false").unwrap_or(true),
            gmgn_boost: env_f64("MOMENTUM_GMGN_BOOST", 12.0),
            gmgn_poll_secs: env_u64("MOMENTUM_GMGN_POLL_SECS", 15),
            gmgn_trenches_preset: std::env::var("MOMENTUM_GMGN_TRENCHES_PRESET").unwrap_or_else(|_| "smart-money".to_string()),
            gmgn_watchlist_ttl_secs: env_u64("MOMENTUM_GMGN_WATCHLIST_TTL_SECS", 300),
            gmgn_smartmoney_limit: env_u64("MOMENTUM_GMGN_SMARTMONEY_LIMIT", 100),

            kol_enabled: std::env::var("MOMENTUM_KOL_ENABLED").map(|v| v.to_lowercase() == "true").unwrap_or(false),
            kol_file: std::env::var("MOMENTUM_KOL_FILE").unwrap_or_else(|_| "kol_wallets.txt".to_string()),
            kol_boost: env_f64("MOMENTUM_KOL_BOOST", 40.0),
            kol_window_secs: env_u64("MOMENTUM_KOL_WINDOW_SECS", 60),
            kol_require: std::env::var("MOMENTUM_KOL_REQUIRE").map(|v| v.to_lowercase() == "true").unwrap_or(false),
            kol_reload_secs: env_u64("MOMENTUM_KOL_RELOAD_SECS", 0),
            status_file: std::env::var("MOMENTUM_STATUS_FILE").unwrap_or_else(|_| "momentum_status.json".to_string()),
            positions_file: std::env::var("MOMENTUM_POSITIONS_FILE").unwrap_or_else(|_| "momentum_positions.json".to_string()),
            decision_log_file: std::env::var("MOMENTUM_DECISION_LOG").unwrap_or_else(|_| "momentum_decisions".to_string()),
            outcome_horizon_secs: env_u64("MOMENTUM_OUTCOME_HORIZON_SECS", 7200),
            landing: std::env::var("MOMENTUM_LANDING").unwrap_or_else(|_| "zeroslot".to_string()).to_lowercase(),
            migration_exit_sol: env_f64("MOMENTUM_MIGRATION_EXIT_SOL", 82.0),

            daily_loss_limit_sol: env_f64("MOMENTUM_DAILY_LOSS_LIMIT_SOL", 0.3),
            max_consecutive_losses: env_u64("MOMENTUM_MAX_CONSECUTIVE_LOSSES", 6) as u32,
            live_confirmed: std::env::var("MOMENTUM_LIVE_CONFIRM").map(|v| v.to_lowercase() == "true").unwrap_or(false),
            daily_reset: std::env::var("MOMENTUM_DAILY_RESET").map(|v| v.to_lowercase() != "false").unwrap_or(true),
            daily_reset_utc_offset_hours: std::env::var("MOMENTUM_DAILY_RESET_UTC_OFFSET_HOURS").ok().and_then(|v| v.parse().ok()).unwrap_or(0),
        }
    }

    pub fn log(&self, logger: &Logger) {
        logger.log("------- MOMENTUM CONFIG -------".cyan().bold().to_string());
        if self.dry_run {
            logger.log(format!(
                "🧪 DRY RUN — no transactions sent. Simulated round-trip cost {:.1}% per fill.",
                self.sim_cost_fraction * 100.0,
            ).yellow().bold().to_string());
        } else {
            logger.log("💰 LIVE — real transactions, real money.".red().bold().to_string());
        }
        logger.log(format!("Buy strength not age | position {} SOL x {} slots", self.position_size_sol, self.max_positions));
        logger.log(format!(
            "Liquidity cap: size <= {:.0}% of mcap AND {:.0}% of curve liquidity (never be the exit liquidity) | min size {:.3} SOL",
            self.max_mcap_fraction * 100.0, self.max_liq_fraction * 100.0, self.min_position_sol,
        ));
        logger.log(format!("Entry score >= {} | collapse < {} | hard stop {}%", self.entry_score, self.collapse_score, self.hard_stop_pct));
        logger.log(format!(
            "Entry filters: base-momentum floor {} | stagnation stop {}",
            if self.min_base_score > 0.0 { format!(">= {:.0} (boosts can't bypass)", self.min_base_score) } else { "off".to_string() },
            if self.stagnation_secs > 0 { format!("cut if peak < {:.0}% after {}s", self.stagnation_min_pnl, self.stagnation_secs) } else { "off".to_string() },
        ));
        logger.log(format!(
            "Max-hold reaper: {} (force-closes zombie / recovered-dead positions so they can't hold a slot forever)",
            if self.max_hold_secs > 0 { format!("{}s", self.max_hold_secs) } else { "off".to_string() },
        ));
        logger.log(format!(
            "Anti-dump concentration: top-{} traders {} | creator share {}",
            self.top_holder_n,
            if self.max_top_holder_share > 0.0 { format!("<= {:.0}% of float", self.max_top_holder_share * 100.0) } else { "off".to_string() },
            if self.max_creator_share > 0.0 { format!("<= {:.0}% of float", self.max_creator_share * 100.0) } else { "off".to_string() },
        ));
        logger.log(format!(
            "Insider-distribution veto: {} (MELT-inspired)",
            if self.insider_distrib_max_sold > 0.0 { format!("skip if top {} early buyers sold > {:.0}% of buys", self.insider_distrib_top_n, self.insider_distrib_max_sold * 100.0) } else { "off".to_string() },
        ));
        logger.log(format!("Windows: short {}s / baseline {}s", self.short_window_secs, self.medium_window_secs));
        logger.log(format!(
            "Targets: min buy vol {} SOL, {} unique buyers, {:.0}% mcap growth, max wallet concentration {:.0}%",
            self.min_buy_volume_sol, self.target_unique_buyers, self.target_mcap_growth * 100.0, self.max_wallet_concentration * 100.0,
        ));
        logger.log(format!(
            "Anti-fake: min buyer diversity {:.2}, max wash fraction {:.2}, max creator buy-share {:.2}",
            self.min_buyer_diversity, self.max_wash_fraction, self.max_creator_buy_frac,
        ));
        let runner = (1.0 - self.scale_out_fractions.iter().sum::<f64>()).max(0.0);
        let rungs: Vec<String> = self.scale_out_targets.iter().zip(self.scale_out_fractions.iter())
            .map(|(t, f)| format!("+{:.0}%→sell {:.0}%", t, f * 100.0)).collect();
        logger.log(format!("Scale-out: {} | runner {:.0}% (held to collapse/migration)", rungs.join(", "), runner * 100.0));
        logger.log(format!(
            "Trailing stop: {} | arms at +{:.0}% peak, exits on {:.0}% giveback from peak | insider-dump sells {:.0}% of position",
            if self.trail_enabled { "ON (lets winners run)" } else { "off" },
            self.trail_activate_pct, self.trail_giveback_frac * 100.0, self.leader_dump_fraction * 100.0,
        ));
        logger.log(format!(
            "Edge: smart-money {} (boost <= {:.0} pts, >= {} distinct) | leader-dump exit {} (>= {} SOL) | conviction sizing {} (<= {:.1}x)",
            if self.smart_money_enabled { "on" } else { "off" }, self.smart_money_boost_max, self.smart_money_min_distinct,
            if self.leader_dump_exit_enabled { "on" } else { "off" }, self.leader_dump_sol,
            if self.conviction_sizing { "on" } else { "off" }, self.conviction_max_mult,
        ));
        logger.log(format!(
            "🧠 Alpha-follow: {} | rep >= {:.2} over >= {} samples | accumulation in {}s window | single +{:.0} (xz{:.2})",
            if self.alpha_follow_enabled { "ON (follows learned proven wallets)" } else { "off" },
            self.alpha_rep_min, self.alpha_min_samples, self.alpha_window_secs, self.alpha_boost, self.alpha_size_mult,
        ));
        logger.log(format!(
            "🤝 Convergence: {} proven wallets accumulating together -> boost +{:.0}, extra size x{:.2} (the most reliable signal)",
            self.convergence_min, self.convergence_boost, self.convergence_size_mult,
        ));
        logger.log(format!(
            "🏅 Wallet score: recency-weighted win rate (50%/30%/20% recent50/recent200/lifetime), confidence-adjusted. Proven >= {:.0} (WATCHLIST+). Tiers: 90 ELITE / 75 STRONG / 60 WATCHLIST / 40 WEAK",
            self.alpha_min_wallet_score,
        ));
        logger.log(format!(
            "🧠 Learned avoidance: creator-blacklist {} (<= {:+.3} SOL/tok over {}+) | pattern-penalty {} (<= -{:.0} score, {}+ samples) — the bot stops repeating mistakes",
            if self.creator_avoid { "on" } else { "off" }, self.creator_avoid_pnl, self.creator_min_trades,
            if self.pattern_avoid { "on" } else { "off" }, self.pattern_penalty_max, self.pattern_min_samples,
        ));
        if self.watch_enabled {
            logger.log(format!(
                "👁  Ones to Watch: composite >= {:.0} (structure {:.0}% + convergence {:.0}%) -> {}",
                self.watch_score_min, self.watch_ms_weight * 100.0, (1.0 - self.watch_ms_weight) * 100.0,
                if self.watch_autobuy { format!("AUTOBUY @ {:.2} SOL", self.watch_size_sol) } else { "ALERT-ONLY (beta — validate with learn.py first)".to_string() },
            ).magenta().bold().to_string());
        }
        if self.start_capital_sol > 0.0 {
            let sizing = if self.position_size_pct > 0.0 {
                format!("{:.1}% of equity/position (scales with account)", self.position_size_pct * 100.0)
            } else {
                format!("{:.3} SOL/position (fixed)", self.position_size_sol)
            };
            logger.log(format!(
                "💰 BANKROLL MODE: start {:.3} SOL | {} | compounds on profit | MARGIN CALL at {:.0}% of start ({:.3} SOL)",
                self.start_capital_sol, sizing, self.bankruptcy_floor_frac * 100.0, self.start_capital_sol * self.bankruptcy_floor_frac,
            ).green().bold().to_string());
        } else {
            logger.log(format!(
                "Risk: max deployed {:.3} SOL | entry-cost basis +{:.1}% | landing: {}",
                self.max_deployed_sol, self.buy_cost_fraction * 100.0, self.landing,
            ));
        }
        logger.log(format!(
            "Discipline: breaker at -{:.3} SOL daily loss or {} consecutive losses | {}{}",
            self.daily_loss_limit_sol, self.max_consecutive_losses,
            if self.daily_reset { format!("auto-reset daily (UTC{:+})", self.daily_reset_utc_offset_hours) } else { "manual reset".to_string() },
            if self.dry_run { "" } else if self.live_confirmed { " | LIVE confirmed" } else { " | LIVE NOT confirmed" },
        ));
        if self.kol_enabled {
            logger.log(format!(
                "⭐ KOL edge: ON | file {} | boost +{:.0}/weight | window {}s | {}",
                self.kol_file, self.kol_boost, self.kol_window_secs,
                if self.kol_require { "PURE-KOL (only KOL buys)" } else { "boost mode" },
            ).cyan().bold().to_string());
        }
        logger.log("------------------------------".cyan().bold().to_string());
    }
}

// ---------------------------------------------------------------------------
// Per-token rolling state
// ---------------------------------------------------------------------------

#[derive(Clone)]
struct TradeTick {
    ts: u64,
    is_buy: bool,
    sol: f64,
    trader: String,
    mcap: f64,
}

struct TokenMomentum {
    ticks: VecDeque<TradeTick>,
    last_mcap: f64,
    last_trade_info: TradeInfoFromToken,
    last_score: f64,
}

/// A live momentum signal computed from the rolling window.
#[derive(Clone, Default)]
pub struct MomentumSignal {
    pub score: f64,
    pub buy_volume_short: f64,
    pub sell_volume_short: f64,
    pub unique_buyers_short: usize,
    pub mcap_velocity: f64,
    pub current_mcap: f64,
    /// Additive points contributed by smart-money confirmation (already in `score`).
    pub smart_money_boost: f64,
    /// 0..1 authenticity multiplier (1 = organic; <1 = manufactured/wash momentum).
    pub genuine_factor: f64,
}

/// An open position managed by the momentum exit policy.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct MomentumPosition {
    entry_mcap: f64,
    /// SOL actually sent on the buy (the swap amount_in).
    entry_size_sol: f64,
    /// True cost basis in SOL = entry_size + estimated entry cost (fees/tip/slippage),
    /// so realized PnL isn't optimistic about the buy leg.
    cost_basis_sol: f64,
    /// Fraction of the original position still held (1.0 -> 0.2 runner).
    remaining_fraction: f64,
    /// Which scale-out rungs have already been taken.
    rungs_hit: Vec<bool>,
    peak_pnl: f64,
    selling: bool,
    /// Creator + top early buyers; if these distribute, exit immediately.
    tracked_wallets: HashSet<String>,
    /// Unix seconds when the position was opened (for the dashboard age).
    entry_ts: u64,
    /// KOL label that triggered this entry, if any (for the dashboard).
    kol_label: String,
    /// Consecutive failed sell attempts (resets on a successful partial sell).
    #[serde(default)]
    sell_attempts: u32,
    /// Set once sells have failed too many times (likely illiquid/honeypot): auto-sell
    /// is halted so the bot stops spinning, and the position is surfaced for manual exit.
    #[serde(default)]
    quarantined: bool,
    /// Cumulative realized PnL (SOL) booked across this position's sells (scale-outs +
    /// final). Used to classify the whole position as a win/loss at full exit.
    #[serde(default)]
    realized_so_far: f64,
    /// Entry fingerprint for learned avoidance (attributed at exit).
    #[serde(default)]
    creator: String,
    #[serde(default)]
    signal_type: String,
    #[serde(default)]
    entry_conv: u32,
    #[serde(default)]
    entry_base_score: f64,
    /// SLOW-RUG detector: cumulative SOL sold by this token's tracked (creator + early)
    /// wallets across the WHOLE holding period — catches steady distribution that each
    /// stays under the acute leader-dump threshold but bleeds the position over time.
    #[serde(default)]
    insider_sold_cum: f64,
    /// Latest tick timestamp already counted into `insider_sold_cum` (avoids double-count).
    #[serde(default)]
    insider_seen_ts: u64,
}

lazy_static! {
    static ref TOKEN_STATE: DashMap<String, TokenMomentum> = DashMap::new();
    static ref POSITIONS: DashMap<String, MomentumPosition> = DashMap::new();
    /// Path positions are persisted to (set at startup). Empty = persistence off.
    static ref POSITIONS_PATH: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());
    static ref RECENTLY_EXITED: DashMap<String, u64> = DashMap::new();
    static ref IN_FLIGHT_BUYS: AtomicUsize = AtomicUsize::new(0);
    static ref MOMENTUM_RUNNING: AtomicBool = AtomicBool::new(true);
    /// Landing telemetry: transactions submitted vs confirmed on-chain (via the
    /// reconciliation lookup). Confirmed is a lower bound if get_transaction is flaky.
    static ref TX_SENT: AtomicU64 = AtomicU64::new(0);
    static ref TX_LANDED: AtomicU64 = AtomicU64::new(0);
    /// Latched once the simulated bankroll is blown (margin call). New entries stop
    /// for the rest of the session; open positions still exit. Reset at startup.
    static ref MARGIN_CALLED: AtomicBool = AtomicBool::new(false);
    /// Session-wide closed-position win/loss counts (honest win rate over the whole
    /// run, not the recent feed window). A position is a win if its TOTAL realized
    /// PnL across all sells is >= 0.
    static ref SESSION_WINS: AtomicU64 = AtomicU64::new(0);
    static ref SESSION_LOSSES: AtomicU64 = AtomicU64::new(0);
    /// Decision/learning log: mints already logged (one decision row per token),
    /// and per-token post-detection outcome tracking.
    static ref DECISION_LOGGED: DashMap<String, ()> = DashMap::new();
    static ref OUTCOMES: DashMap<String, OutcomeTrack> = DashMap::new();
    /// Honeypot authority pre-check cache: mint -> true (checked & safe). Avoids
    /// re-reading the same mint account on every evaluation.
    static ref MINT_AUTHORITY_CHECKED: DashMap<String, bool> = DashMap::new();
    /// "Ones to Watch": mint -> (watch_score, market_structure, convergence, ts).
    static ref WATCHLIST: DashMap<String, (f64, f64, f64, u64)> = DashMap::new();
    /// Learned avoidance — the bot's memory of what burns it, updated every exit:
    /// creator -> (decaying realized PnL/token, closed trades).
    static ref CREATOR_REP: DashMap<String, (f64, u32)> = DashMap::new();
    /// feature-pattern band -> (decaying realized PnL/token, closed samples).
    static ref PATTERN_EV: DashMap<String, (f64, u32)> = DashMap::new();
    static ref DECISION_LOG_PATH: std::sync::Mutex<String> = std::sync::Mutex::new(String::new());
    static ref DECISION_LOG_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    /// Serializes appends to the trade-log CSV.
    static ref TRADE_LOG_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    /// Running tallies for the live PnL summary: (realized_pnl_sol, buys, sells).
    static ref PNL_TALLY: std::sync::Mutex<(f64, u64, u64)> = std::sync::Mutex::new((0.0, 0, 0));
    /// Recent trade events for the live dashboard feed: (ts, event, mint, reason, pnl_sol).
    static ref RECENT_EVENTS: std::sync::Mutex<VecDeque<(u64, String, String, String, f64)>> =
        std::sync::Mutex::new(VecDeque::with_capacity(64));
    /// Per-KOL realized PnL leaderboard: label -> (realized_sol, wins, losses).
    static ref KOL_PNL: DashMap<String, (f64, u32, u32)> = DashMap::new();
    /// Per-wallet reputation learned from observed outcomes (the smart-money edge).
    static ref WALLET_REP: DashMap<String, WalletRep> = DashMap::new();
    /// Pending outcome evaluations for tracked buys, ordered by due time.
    static ref ATTR_QUEUE: std::sync::Mutex<VecDeque<PendingAttr>> = std::sync::Mutex::new(VecDeque::new());
    /// Mints GMGN smart-money / trenches flag as hot: mint -> expiry unix secs.
    static ref GMGN_WATCHLIST: DashMap<String, u64> = DashMap::new();
    /// Your curated KOL wallets: wallet -> (weight, label).
    static ref KOL_WALLETS: DashMap<String, (f64, String)> = DashMap::new();
    /// Mints a KOL just bought: mint -> (expiry_unix, weight, label).
    static ref KOL_HOT: DashMap<String, (u64, f64, String)> = DashMap::new();
}

/// Load the private KOL wallet list. Lines: `wallet[,weight][,label]` (# = comment).
fn load_kol_wallets(path: &str) -> usize {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return 0,
    };
    // Build fresh so a reload reflects additions AND removals.
    KOL_WALLETS.clear();
    for line in content.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let mut parts = line.split(',');
        let wallet = parts.next().unwrap_or("").trim().to_string();
        if wallet.is_empty() {
            continue;
        }
        let weight = parts.next().and_then(|w| w.trim().parse::<f64>().ok()).unwrap_or(1.0);
        let label = parts.next().map(|l| l.trim().to_string()).unwrap_or_else(|| short_addr(&wallet));
        KOL_WALLETS.insert(wallet, (weight, label));
    }
    KOL_WALLETS.len()
}

fn short_addr(a: &str) -> String {
    if a.len() > 8 { format!("{}…{}", &a[..4], &a[a.len() - 4..]) } else { a.to_string() }
}

/// If `mint` is currently KOL-hot (unexpired), return (weight, label).
fn kol_hot(mint: &str, now: u64) -> Option<(f64, String)> {
    KOL_HOT.get(mint).and_then(|e| {
        let (expiry, weight, label) = e.value();
        if *expiry > now { Some((*weight, label.clone())) } else { None }
    })
}

/// Optional GMGN client, initialised once at startup if enabled.
static GMGN: OnceCell<Arc<GmgnClient>> = OnceCell::new();

/// Circuit breaker: once tripped, NEW entries are blocked (open positions still
/// exit normally). Discipline against bleeding out on a bad session/day.
static TRADING_HALTED: AtomicBool = AtomicBool::new(false);
static CONSECUTIVE_LOSSES: AtomicUsize = AtomicUsize::new(0);
/// Current trading-day index; rolling it over resets the breaker.
static DAY_INDEX: AtomicU64 = AtomicU64::new(0);

lazy_static! {
    /// Cumulative realized PnL captured at the start of the current day, so the
    /// breaker measures the *day's* loss rather than all-time.
    static ref DAY_START_REALIZED: std::sync::Mutex<f64> = std::sync::Mutex::new(0.0);
}

fn trading_halted() -> bool {
    TRADING_HALTED.load(Ordering::SeqCst)
}

/// The trading-day number for `now`, offset so the day rolls over at the
/// configured local hour rather than midnight UTC.
fn current_day(cfg: &MomentumConfig) -> u64 {
    let shifted = now_secs() as i64 + cfg.daily_reset_utc_offset_hours * 3600;
    (shifted.max(0) / 86_400) as u64
}

/// Daily realized PnL = cumulative realized minus the day's starting baseline.
fn daily_realized() -> f64 {
    let cum = PNL_TALLY.lock().map(|g| g.0).unwrap_or(0.0);
    let base = DAY_START_REALIZED.lock().map(|g| *g).unwrap_or(0.0);
    cum - base
}

/// Roll the breaker into a new day when the date changes: rebase the daily PnL,
/// clear the loss streak, and auto-resume if it was halted. No-op within a day.
fn maybe_roll_day(cfg: &MomentumConfig, logger: &Logger) {
    if !cfg.daily_reset {
        return;
    }
    let day = current_day(cfg);
    let prev = DAY_INDEX.load(Ordering::SeqCst);
    if day == prev {
        return;
    }
    DAY_INDEX.store(day, Ordering::SeqCst);
    if let Ok(mut base) = DAY_START_REALIZED.lock() {
        *base = PNL_TALLY.lock().map(|g| g.0).unwrap_or(*base);
    }
    CONSECUTIVE_LOSSES.store(0, Ordering::SeqCst);
    let was_halted = TRADING_HALTED.swap(false, Ordering::SeqCst);
    logger.log(format!(
        "🔄 New trading day — circuit breaker reset{}.",
        if was_halted { " (auto-resumed from halt)" } else { "" },
    ).cyan().bold().to_string());
}

/// Accumulate realized PnL for the KOL that triggered a position. `is_full`
/// marks the closing exit, where we tally the win/loss for the whole trade.
fn record_kol_pnl(label: &str, realized_sol: f64, is_full: bool, trade_realized: f64) {
    if label.is_empty() {
        return;
    }
    let mut e = KOL_PNL.entry(label.to_string()).or_insert((0.0, 0, 0));
    e.0 += realized_sol;
    if is_full {
        if trade_realized >= 0.0 { e.1 += 1; } else { e.2 += 1; }
    }
}

/// A curated KOL is "muted" once it's net-negative over enough closed trades — so a
/// losing KOL (e.g. one the early tiny samples flattered) stops boosting entries.
fn kol_is_muted(label: &str, cfg: &MomentumConfig) -> bool {
    if !cfg.kol_automute {
        return false;
    }
    KOL_PNL.get(label).map(|e| {
        let (realized, wins, losses) = *e.value();
        (wins + losses) >= cfg.kol_automute_min && realized < 0.0
    }).unwrap_or(false)
}

/// Record the outcome of a closed position and evaluate the circuit breaker.
fn record_full_exit(realized_sol: f64, cfg: &MomentumConfig, logger: &Logger) {
    maybe_roll_day(cfg, logger);

    if realized_sol < 0.0 {
        CONSECUTIVE_LOSSES.fetch_add(1, Ordering::SeqCst);
    } else {
        CONSECUTIVE_LOSSES.store(0, Ordering::SeqCst);
    }

    if trading_halted() {
        return;
    }

    let day_realized = daily_realized();
    let losses = CONSECUTIVE_LOSSES.load(Ordering::SeqCst) as u32;

    let trip = if cfg.daily_loss_limit_sol > 0.0 && day_realized <= -cfg.daily_loss_limit_sol {
        Some(format!("day loss {:.4} SOL hit limit -{:.4}", day_realized, cfg.daily_loss_limit_sol))
    } else if cfg.max_consecutive_losses > 0 && losses >= cfg.max_consecutive_losses {
        Some(format!("{} consecutive losing trades", losses))
    } else {
        None
    };

    if let Some(reason) = trip {
        TRADING_HALTED.store(true, Ordering::SeqCst);
        let resume = if cfg.daily_reset { "resets automatically at the next day boundary" } else { "restart the bot to resume" };
        logger.log(format!(
            "🛑🛑 CIRCUIT BREAKER TRIPPED — {}. New entries halted; open positions will still exit ({}).",
            reason, resume,
        ).red().bold().to_string());
    }
}

fn gmgn() -> Option<&'static Arc<GmgnClient>> {
    GMGN.get()
}

fn on_gmgn_watchlist(mint: &str, now: u64) -> bool {
    GMGN_WATCHLIST.get(mint).map(|e| *e > now).unwrap_or(false)
}

/// Background task: poll GMGN smart-money trades and pre-filtered trenches, and
/// keep the watchlist of "hot" mints fresh. Best-effort; failures are ignored.
async fn run_gmgn_pollers(cfg: Arc<MomentumConfig>, logger: Logger) {
    let client = match gmgn() {
        Some(c) => c.clone(),
        None => return,
    };
    let mut interval = time::interval(Duration::from_secs(cfg.gmgn_poll_secs.max(5)));
    while MOMENTUM_RUNNING.load(Ordering::SeqCst) {
        interval.tick().await;
        let now = now_secs();
        let expiry = now + cfg.gmgn_watchlist_ttl_secs;

        let mut fresh: Vec<String> = Vec::new();
        for mint in client.smartmoney_buys(cfg.gmgn_smartmoney_limit).await {
            if !GMGN_WATCHLIST.contains_key(&mint) {
                fresh.push(mint.clone());
            }
            GMGN_WATCHLIST.insert(mint, expiry);
        }
        for mint in client.trenches_pumpfun(&cfg.gmgn_trenches_preset, 80).await {
            if !GMGN_WATCHLIST.contains_key(&mint) {
                fresh.push(mint.clone());
            }
            GMGN_WATCHLIST.insert(mint, expiry);
        }

        // Drop expired entries.
        GMGN_WATCHLIST.retain(|_, exp| *exp > now);

        // Edge/latency hardening: prewarm the security cache for newly-hot mints so
        // the pre-buy veto is a cache hit instead of a blocking call on the hot path.
        if cfg.gmgn_security_veto {
            for mint in fresh.iter().take(40) {
                let _ = client.security_verdict(mint).await;
            }
        }

        if !fresh.is_empty() {
            logger.log(format!("🛰️  GMGN watchlist refreshed: {} hot mints (+{} new, security prewarmed)", GMGN_WATCHLIST.len(), fresh.len()).cyan().to_string());
        }
    }
}

/// Reputation for a wallet. Beyond the legacy forward-return EMA (`score`), it
/// keeps a rolling window of recent win/loss outcomes so the wallet score can be
/// recency-weighted and confidence-adjusted — recent performance dominates, and a
/// formerly-good wallet loses standing fast when it starts losing.
#[derive(Clone, Default)]
struct WalletRep {
    score: f64,
    samples: u32,
    /// Graded buys that were winners (token up after they bought) — lifetime.
    wins: u32,
    /// Rolling window of the last (up to) 200 outcomes, newest at the back.
    recent: VecDeque<bool>,
    /// When the reputation was last updated (for time decay).
    last_update: u64,
    /// Last token graded, to dampen reputation farmed by buying one token repeatedly.
    last_mint: String,
}

const WALLET_RECENT_CAP: usize = 200;
const WALLET_CONF_K: f64 = 30.0; // sample-size smoothing: confidence = n/(n+K)

impl WalletRep {
    /// Record one graded outcome (win = token rose after the buy).
    fn push_outcome(&mut self, win: bool) {
        self.samples += 1;
        if win { self.wins += 1; }
        if self.recent.len() >= WALLET_RECENT_CAP { self.recent.pop_front(); }
        self.recent.push_back(win);
    }
    /// Lifetime win rate (0..1).
    fn lifetime_winrate(&self) -> f64 {
        if self.samples == 0 { 0.0 } else { self.wins as f64 / self.samples as f64 }
    }
    /// Win rate over the most recent `n` outcomes (0..1); falls back to lifetime if
    /// the rolling window is empty (e.g. an old rep file with no recent data).
    fn recent_winrate(&self, n: usize) -> f64 {
        if self.recent.is_empty() { return self.lifetime_winrate(); }
        let mut w = 0usize; let mut c = 0usize;
        for &b in self.recent.iter().rev().take(n) { c += 1; if b { w += 1; } }
        if c == 0 { self.lifetime_winrate() } else { w as f64 / c as f64 }
    }
    /// Confidence from sample size (0..1): n/(n+K). 10 trades -> 0.25, 300 -> 0.91.
    fn confidence(&self) -> f64 {
        let n = self.samples as f64;
        n / (n + WALLET_CONF_K)
    }
    /// Final recency-weighted, confidence-adjusted score (0..100):
    ///   raw = 0.50*recent50 + 0.30*recent200 + 0.20*lifetime
    /// then shrink toward 0.50 (neutral) by sample-size confidence so a 90% over 10
    /// trades scores LESS than a 65% over 300.
    fn final_score(&self) -> f64 {
        let raw = 0.50 * self.recent_winrate(50)
                + 0.30 * self.recent_winrate(200)
                + 0.20 * self.lifetime_winrate();
        let c = self.confidence();
        (c * raw + (1.0 - c) * 0.5) * 100.0
    }
    /// Tier per the classification bands. < min_samples = "IGNORE".
    fn tier(&self, min_samples: u32) -> &'static str {
        if self.samples < min_samples.max(5) { return "IGNORE"; }
        let s = self.final_score();
        if s >= 90.0 { "ELITE" }
        else if s >= 75.0 { "STRONG" }
        else if s >= 60.0 { "WATCHLIST" }
        else if s >= 40.0 { "WEAK" }
        else { "AVOID" }
    }
    fn low_confidence(&self) -> bool { self.samples < 20 }
    fn win_rate(&self) -> f64 { self.lifetime_winrate() }
}

/// A wallet is "proven" (followable by alpha/convergence) when it has enough graded
/// trades and its recency-weighted score clears the bar — recent performance first.
fn wallet_is_proven(r: &WalletRep, cfg: &MomentumConfig) -> bool {
    r.samples >= cfg.alpha_min_samples && r.final_score() >= cfg.alpha_min_wallet_score
}

/// A buy awaiting outcome grading at `eval_at`.
struct PendingAttr {
    wallet: String,
    mint: String,
    entry_mcap: f64,
    eval_at: u64,
}

const ATTR_QUEUE_MAX: usize = 100_000;

/// Sum the positive reputations of the given buyers into bonus score points.
///
/// Poisoning guards: a single wallet's contribution is capped (so one farmed
/// high-rep wallet can't max the boost), and the boost only applies once at least
/// `smart_money_min_distinct` reputable wallets are buying together.
fn smart_money_boost<'a>(buyers: impl Iterator<Item = &'a str>, cfg: &MomentumConfig) -> f64 {
    if !cfg.smart_money_enabled {
        return 0.0;
    }
    // No single wallet may contribute more than its even share of the cap.
    let per_wallet_cap = cfg.smart_money_boost_scale.max(1e-9) / cfg.smart_money_min_distinct.max(1) as f64;
    let mut sum = 0.0;
    let mut distinct = 0usize;
    for b in buyers {
        if let Some(r) = WALLET_REP.get(b) {
            if r.samples >= cfg.smart_money_min_samples && r.score > 0.0 {
                sum += r.score.min(per_wallet_cap);
                distinct += 1;
            }
        }
    }
    if distinct < cfg.smart_money_min_distinct {
        return 0.0;
    }
    (cfg.smart_money_boost_max * clamp01(sum / cfg.smart_money_boost_scale.max(1e-9))).min(cfg.smart_money_boost_max)
}

/// Smart-money accumulation + convergence on a token.
///
/// A wallet counts if it is PROVEN (learned reputation >= `alpha_rep_min` over
/// >= `alpha_min_samples` grades) AND is net-ACCUMULATING in the window — i.e. its
/// buys minus sells exceed `smart_money_min_sol` (so a wallet that bought then
/// dumped doesn't count; it must actually be holding what it bought). Returns
/// `(count, summed_reputation, best_wallet)`:
/// - count == 1 → a single proven wallet accumulating (the alpha-follow signal)
/// - count >= convergence_min → CONVERGENCE: several proven wallets piling into the
///   same token at once — the most reliable smart-money signal there is.
fn smart_convergence(mint: &str, now: u64, cfg: &MomentumConfig) -> Option<(usize, f64, String)> {
    if !cfg.alpha_follow_enabled {
        return None;
    }
    let cut = now.saturating_sub(cfg.alpha_window_secs);
    let state = TOKEN_STATE.get(mint)?;
    // Net SOL (buys - sells) per wallet within the window.
    let mut net: HashMap<&str, f64> = HashMap::new();
    for t in state.ticks.iter() {
        if t.ts < cut || t.trader.is_empty() {
            continue;
        }
        let e = net.entry(t.trader.as_str()).or_insert(0.0);
        if t.is_buy { *e += t.sol; } else { *e -= t.sol; }
    }
    let mut count = 0usize;
    let mut summed_rep = 0.0;
    let mut best: Option<(String, f64)> = None;
    for (w, n) in net.iter() {
        if *n < cfg.smart_money_min_sol {
            continue; // not net-accumulating enough
        }
        if let Some(r) = WALLET_REP.get(*w) {
            if wallet_is_proven(&r, cfg) {
                count += 1; // (ref derefs to WalletRep)
                summed_rep += r.score;
                if best.as_ref().map(|(_, s)| r.score > *s).unwrap_or(true) {
                    best = Some(((*w).to_string(), r.score));
                }
            }
        }
    }
    best.map(|(w, _)| (count, summed_rep, w))
}

/// "Ones to Watch" composite score (0..100) from two independent signals:
///   1) Market structure — turnover (vol/mc), the validated momentum/structure
///      composite, and bonding-curve liquidity health. (Multi-day metrics like
///      5d MC change / token-tier from established-token products don't apply to
///      fresh launches, so this is the bot-window equivalent of "market structure".)
///   2) Smart-money convergence — how many PROVEN wallets are accumulating at once.
/// Returns (total, market_structure, convergence_subscore).
fn watch_score(signal: &MomentumSignal, mint: &str, conv_count: usize, cfg: &MomentumConfig) -> (f64, f64, f64) {
    // --- market structure (0..100) ---
    let vol_mc = if signal.current_mcap > 0.0 { signal.buy_volume_short / signal.current_mcap } else { 0.0 };
    let vol_mc_norm = (vol_mc / 0.5).clamp(0.0, 1.0); // ~0.5 turnover = strong
    let liq = TOKEN_STATE.get(mint).map(|s| s.last_trade_info.liquidity).unwrap_or(0.0);
    let liq_norm = (liq / 50.0).clamp(0.0, 1.0);      // ~50 SOL in curve = healthy
    let struct_norm = (signal.score / 100.0).clamp(0.0, 1.0);
    let ms = (struct_norm * 0.6 + vol_mc_norm * 0.25 + liq_norm * 0.15) * 100.0;
    // --- smart-money convergence (0..100) ---
    let conv_full = (cfg.convergence_min + 2).max(1) as f64;
    let conv = (conv_count as f64 / conv_full).clamp(0.0, 1.0) * 100.0;
    let total = cfg.watch_ms_weight * ms + (1.0 - cfg.watch_ms_weight) * conv;
    (total, ms, conv)
}

/// Discrete feature bands describing a token at entry — the "kind" of token it is.
/// The bot learns the realized PnL of each band so it can avoid losing patterns.
fn token_bands(signal_type: &str, conv: u32, mcap: f64, score: f64) -> Vec<String> {
    let smart = if conv == 0 { "0" } else if conv < 3 { "lo" } else { "hi" };
    let mc = if mcap < 30.0 { "lo" } else if mcap < 60.0 { "mid" } else { "hi" };
    let sc = if score < 50.0 { "lo" } else if score < 70.0 { "mid" } else { "hi" };
    vec![
        format!("sig:{}", signal_type),
        format!("smart:{}", smart),
        format!("mcap:{}", mc),
        format!("score:{}", sc),
    ]
}

const AVOID_EMA_ALPHA: f64 = 0.2; // recent outcomes dominate -> the memory keeps adapting

/// Record a closed position's realized PnL against its creator and feature bands,
/// so future entries can avoid creators/patterns that keep losing.
fn learn_avoidance(creator: &str, bands: &[String], realized: f64) {
    if !creator.is_empty() {
        let mut e = CREATOR_REP.entry(creator.to_string()).or_insert((0.0, 0));
        e.0 = (1.0 - AVOID_EMA_ALPHA) * e.0 + AVOID_EMA_ALPHA * realized;
        e.1 += 1;
    }
    for b in bands {
        let mut e = PATTERN_EV.entry(b.clone()).or_insert((0.0, 0));
        e.0 = (1.0 - AVOID_EMA_ALPHA) * e.0 + AVOID_EMA_ALPHA * realized;
        e.1 += 1;
    }
}

/// Should this creator be avoided? (enough trades + decaying PnL at/below the bar.)
fn creator_is_bad(creator: &str, cfg: &MomentumConfig) -> Option<(f64, u32)> {
    if !cfg.creator_avoid || creator.is_empty() {
        return None;
    }
    CREATOR_REP.get(creator).and_then(|e| {
        let (pnl, n) = *e.value();
        if n >= cfg.creator_min_trades && pnl <= cfg.creator_avoid_pnl {
            Some((pnl, n))
        } else {
            None
        }
    })
}

/// Entry-score penalty (0..pattern_penalty_max) from how badly the candidate's
/// feature bands have performed. Only bands with enough samples count. Returns
/// (penalty, worst_band_label) so the decision can be explained.
fn pattern_penalty(bands: &[String], cfg: &MomentumConfig) -> (f64, String) {
    if !cfg.pattern_avoid {
        return (0.0, String::new());
    }
    let mut sum = 0.0;
    let mut cnt = 0;
    let mut worst = (0.0f64, String::new());
    for b in bands {
        if let Some(e) = PATTERN_EV.get(b) {
            let (pnl, n) = *e.value();
            if n >= cfg.pattern_min_samples {
                sum += pnl;
                cnt += 1;
                if pnl < worst.0 {
                    worst = (pnl, b.clone());
                }
            }
        }
    }
    if cnt == 0 {
        return (0.0, String::new());
    }
    let avg = sum / cnt as f64;
    if avg >= 0.0 {
        return (0.0, String::new());
    }
    // Map a losing average (toward -0.05 SOL/token) to a penalty up to the max.
    let penalty = (cfg.pattern_penalty_max * (-avg / 0.05).clamp(0.0, 1.0)).min(cfg.pattern_penalty_max);
    (penalty, worst.1)
}

/// Queue a buy for later outcome grading (smart-money learning).
fn enqueue_attribution(wallet: String, mint: String, entry_mcap: f64, eval_at: u64) {
    if let Ok(mut q) = ATTR_QUEUE.lock() {
        if q.len() >= ATTR_QUEUE_MAX {
            q.pop_front();
        }
        q.push_back(PendingAttr { wallet, mint, entry_mcap, eval_at });
    }
}

/// Background task: grade due buys on the token's forward return and update the
/// buyer's reputation. A token that died (no longer tracked / zero mcap) grades
/// as a loss, which is exactly the signal we want against rug-prone wallets.
async fn run_attribution(cfg: Arc<MomentumConfig>) {
    let mut interval = time::interval(Duration::from_secs(5));
    let mut since_save: u64 = 0;
    while MOMENTUM_RUNNING.load(Ordering::SeqCst) {
        interval.tick().await;
        let now = now_secs();

        let mut due: Vec<PendingAttr> = Vec::new();
        if let Ok(mut q) = ATTR_QUEUE.lock() {
            while q.front().map(|f| f.eval_at <= now).unwrap_or(false) {
                if let Some(item) = q.pop_front() {
                    due.push(item);
                }
            }
        }

        for a in due {
            let cur = TOKEN_STATE.get(&a.mint).map(|s| s.last_mcap).unwrap_or(0.0);
            let ret = if a.entry_mcap > 0.0 && cur > 0.0 {
                ((cur - a.entry_mcap) / a.entry_mcap).clamp(-1.0, 3.0)
            } else {
                -1.0 // token went cold / untracked: treat as a loss
            };
            let mut r = WALLET_REP.entry(a.wallet).or_default();
            // Time-decay old reputation toward 0 so stale wallets fade.
            if r.last_update > 0 {
                let elapsed = now.saturating_sub(r.last_update) as f64;
                let decay = 0.5f64.powf(elapsed / cfg.smart_money_halflife_secs.max(1) as f64);
                r.score *= decay;
            }
            // Dampen reputation farmed by repeatedly buying the same token.
            let alpha = if r.last_mint == a.mint { 0.02 } else { 0.1 };
            r.score = (1.0 - alpha) * r.score + alpha * ret;
            r.push_outcome(ret > 0.0); // updates samples, wins, and the recent window
            r.last_update = now;
            r.last_mint = a.mint.clone();
        }

        // Persist reputation roughly every 60s so the edge compounds across runs.
        since_save += 5;
        if since_save >= 60 {
            since_save = 0;
            save_wallet_rep(&cfg.wallet_rep_file);
            save_avoidance(&cfg.avoidance_file);
        }
    }
}

/// Persist the learned creator + pattern avoidance memory (one file, kind-tagged).
fn save_avoidance(path: &str) {
    use std::io::Write;
    if path.is_empty() { return; }
    let tmp = format!("{}.tmp", path);
    let mut file = match std::fs::File::create(&tmp) { Ok(f) => f, Err(_) => return };
    let _ = writeln!(file, "kind,key,pnl,count");
    for e in CREATOR_REP.iter() {
        let (pnl, n) = *e.value();
        let _ = writeln!(file, "creator,{},{:.6},{}", e.key(), pnl, n);
    }
    for e in PATTERN_EV.iter() {
        let (pnl, n) = *e.value();
        let _ = writeln!(file, "pattern,{},{:.6},{}", e.key(), pnl, n);
    }
    let _ = std::fs::rename(&tmp, path);
}

fn load_avoidance(path: &str) {
    let content = match std::fs::read_to_string(path) { Ok(c) => c, Err(_) => return };
    for line in content.lines().skip(1) {
        let mut it = line.split(',');
        if let (Some(kind), Some(key), Some(p), Some(c)) = (it.next(), it.next(), it.next(), it.next()) {
            if let (Ok(pnl), Ok(n)) = (p.parse::<f64>(), c.parse::<u32>()) {
                match kind {
                    "creator" => { CREATOR_REP.insert(key.to_string(), (pnl, n)); }
                    "pattern" => { PATTERN_EV.insert(key.to_string(), (pnl, n)); }
                    _ => {}
                }
            }
        }
    }
}

fn load_wallet_rep(path: &str) {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return,
    };
    for line in content.lines().skip(1) {
        let mut it = line.split(',');
        if let (Some(w), Some(s), Some(n)) = (it.next(), it.next(), it.next()) {
            if let (Ok(score), Ok(samples)) = (s.parse::<f64>(), n.parse::<u32>()) {
                let last_update = it.next().and_then(|v| v.parse::<u64>().ok()).unwrap_or(0);
                let last_mint = it.next().unwrap_or("").to_string();
                // `wins` then the recent-outcomes window appended last (backward-compat).
                let wins = it.next().and_then(|v| v.parse::<u32>().ok()).unwrap_or(0);
                let recent: VecDeque<bool> = it.next().unwrap_or("").chars()
                    .filter(|c| *c == '0' || *c == '1').map(|c| c == '1').collect();
                WALLET_REP.insert(w.to_string(), WalletRep { score, samples, wins, recent, last_update, last_mint });
            }
        }
    }
}

fn save_wallet_rep(path: &str) {
    use std::io::Write;
    let tmp = format!("{}.tmp", path);
    let file = std::fs::File::create(&tmp);
    let mut file = match file {
        Ok(f) => f,
        Err(_) => return,
    };
    let _ = writeln!(file, "wallet,score,samples,last_update,last_mint,wins,recent");
    for e in WALLET_REP.iter() {
        let r = e.value();
        let recent: String = r.recent.iter().map(|&b| if b { '1' } else { '0' }).collect();
        let _ = writeln!(file, "{},{:.6},{},{},{},{},{}", e.key(), r.score, r.samples, r.last_update, r.last_mint, r.wins, recent);
    }
    let _ = std::fs::rename(&tmp, path);
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// Write a live status snapshot for the web dashboard (best-effort).
fn write_status_snapshot(cfg: &MomentumConfig) {
    if cfg.status_file.is_empty() {
        return;
    }
    let now = now_secs();
    let (realized, buys, sells) = PNL_TALLY.lock().map(|g| *g).unwrap_or((0.0, 0, 0));

    // Open positions with live PnL.
    let mut positions: Vec<serde_json::Value> = Vec::new();
    for e in POSITIONS.iter() {
        let p = e.value();
        let cur = TOKEN_STATE.get(e.key()).map(|s| s.last_mcap).unwrap_or(p.entry_mcap);
        let pnl_pct = if p.entry_mcap > 0.0 { (cur - p.entry_mcap) / p.entry_mcap * 100.0 } else { 0.0 };
        let rungs = p.rungs_hit.iter().filter(|&&h| h).count();
        positions.push(serde_json::json!({
            "mint": e.key(),
            "entry_mcap": p.entry_mcap,
            "current_mcap": cur,
            "pnl_pct": pnl_pct,
            "peak_pnl": p.peak_pnl,
            "size_sol": p.entry_size_sol,
            "remaining_pct": p.remaining_fraction * 100.0,
            "rungs_hit": rungs,
            "age_secs": now.saturating_sub(p.entry_ts),
            "kol": p.kol_label,
            "selling": p.selling,
            "quarantined": p.quarantined,
        }));
    }
    positions.sort_by(|a, b| b["pnl_pct"].as_f64().unwrap_or(0.0).partial_cmp(&a["pnl_pct"].as_f64().unwrap_or(0.0)).unwrap_or(std::cmp::Ordering::Equal));

    // Recent event feed (newest first).
    let mut feed: Vec<serde_json::Value> = Vec::new();
    if let Ok(q) = RECENT_EVENTS.lock() {
        for (ts, event, mint, reason, pnl) in q.iter().rev() {
            feed.push(serde_json::json!({ "ts": ts, "event": event, "mint": mint, "reason": reason, "pnl_sol": pnl }));
        }
    }

    // "Ones to Watch" — prune entries older than 10 min, then take the top by score.
    WATCHLIST.retain(|_, v| now.saturating_sub(v.3) < 600);
    let mut watch: Vec<(String, f64, f64, f64, u64)> = WATCHLIST.iter()
        .map(|e| (e.key().clone(), e.value().0, e.value().1, e.value().2, e.value().3)).collect();
    watch.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    watch.truncate(15);
    let watchlist: Vec<serde_json::Value> = watch.into_iter()
        .map(|(m, s, ms, cv, ts)| serde_json::json!({ "mint": m, "score": s, "structure": ms, "convergence": cv, "age_secs": now.saturating_sub(ts) }))
        .collect();

    // Per-KOL leaderboard (best realized PnL first) — keep the green, hunt alts.
    let mut kol_board: Vec<serde_json::Value> = KOL_PNL.iter().map(|e| {
        let (realized, wins, losses) = *e.value();
        serde_json::json!({ "kol": e.key(), "realized_sol": realized, "wins": wins, "losses": losses })
    }).collect();
    kol_board.sort_by(|a, b| b["realized_sol"].as_f64().unwrap_or(0.0).partial_cmp(&a["realized_sol"].as_f64().unwrap_or(0.0)).unwrap_or(std::cmp::Ordering::Equal));

    // The bot's self-discovered "scout list": wallets it learned are proven, plus
    // the strongest few — this is the compounding memory made visible.
    let mut proven = 0usize;
    let mut top_alpha: Vec<(String, f64, u32, f64, String)> = Vec::new();
    for e in WALLET_REP.iter() {
        let r = e.value();
        if wallet_is_proven(&r, cfg) {
            proven += 1;
            top_alpha.push((e.key().clone(), r.final_score(), r.samples, r.recent_winrate(50), r.tier(cfg.alpha_min_samples).to_string()));
        }
    }
    top_alpha.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    top_alpha.truncate(15);
    let top_alpha: Vec<serde_json::Value> = top_alpha.into_iter()
        .map(|(w, s, n, wr, tier)| serde_json::json!({ "wallet": w, "score": s, "samples": n, "win_rate": wr, "tier": tier }))
        .collect();

    let snap = serde_json::json!({
        "updated": now,
        "mode": if cfg.dry_run { "DRY RUN" } else { "LIVE" },
        "halted": trading_halted(),
        "consecutive_losses": CONSECUTIVE_LOSSES.load(Ordering::SeqCst),
        "pnl": {
            "session_realized": realized,
            "today_realized": daily_realized(),
            "buys": buys,
            "sells": sells,
            "wins": SESSION_WINS.load(Ordering::SeqCst),
            "losses": SESSION_LOSSES.load(Ordering::SeqCst),
        },
        "capital": {
            "deployed_sol": deployed_sol(),
            "max_deployed_sol": cfg.max_deployed_sol,
            "start_capital_sol": cfg.start_capital_sol,
            "equity_sol": if cfg.start_capital_sol > 0.0 { equity(cfg) } else { 0.0 },
            "available_sol": if cfg.start_capital_sol > 0.0 { available_capital(cfg) } else { 0.0 },
            "bankroll_mode": cfg.start_capital_sol > 0.0,
            "margin_called": MARGIN_CALLED.load(Ordering::SeqCst),
        },
        "counts": {
            "open_positions": POSITIONS.len(),
            "max_positions": cfg.max_positions,
            "tokens_tracked": TOKEN_STATE.len(),
            "kol_wallets": KOL_WALLETS.len(),
            "kol_hot": KOL_HOT.len(),
            "gmgn_watchlist": GMGN_WATCHLIST.len(),
            "wallet_rep": WALLET_REP.len(),
            "alpha_proven": proven,
            "tx_sent": TX_SENT.load(Ordering::Relaxed),
            "tx_landed": TX_LANDED.load(Ordering::Relaxed),
        },
        "config": {
            "position_size_sol": cfg.position_size_sol,
            "entry_score": cfg.entry_score,
            "hard_stop_pct": cfg.hard_stop_pct,
            "kol_enabled": cfg.kol_enabled,
            "kol_require": cfg.kol_require,
            "daily_loss_limit_sol": cfg.daily_loss_limit_sol,
            "alpha_follow": cfg.alpha_follow_enabled,
            "alpha_rep_min": cfg.alpha_rep_min,
            "alpha_min_samples": cfg.alpha_min_samples,
            "watch_enabled": cfg.watch_enabled,
            "watch_autobuy": cfg.watch_autobuy,
            "watch_score_min": cfg.watch_score_min,
        },
        "positions": positions,
        "feed": feed,
        "kol_leaderboard": kol_board,
        "top_alpha": top_alpha,
        "watchlist": watchlist,
    });

    let tmp = format!("{}.tmp", cfg.status_file);
    if std::fs::write(&tmp, snap.to_string()).is_ok() {
        let _ = std::fs::rename(&tmp, &cfg.status_file);
    }
}

fn trade_log_path() -> String {
    std::env::var("MOMENTUM_TRADE_LOG").unwrap_or_else(|_| "momentum_trades.csv".to_string())
}

/// One recorded trade event (a buy or a scale-out / exit sell).
struct TradeLogEvent<'a> {
    event: &'a str,
    mint: &'a str,
    reason: &'a str,
    score: f64,
    entry_mcap: f64,
    current_mcap: f64,
    pnl_pct: f64,
    fraction_of_original: f64,
    /// Estimated SOL proceeds (sells) or cost (negative, buys).
    est_sol: f64,
    /// Estimated realized PnL in SOL for this event.
    est_realized_pnl_sol: f64,
    signature: &'a str,
}

/// Append a trade event to the CSV log (best-effort: never panics, never blocks trading).
fn log_trade_event(ev: &TradeLogEvent) {
    use std::io::Write;

    // Update running tallies for the live summary. SELL_ACTUAL rows are
    // reconciliation entries; their tally adjustment is applied separately so we
    // don't double-count against the estimate already recorded at sell time.
    if let Ok(mut tally) = PNL_TALLY.lock() {
        match ev.event {
            "BUY" => tally.1 += 1,
            "SELL_PARTIAL" | "SELL_FULL" => {
                tally.0 += ev.est_realized_pnl_sol;
                tally.2 += 1;
            }
            _ => {}
        }
    }

    // Feed the live dashboard (keep the last 50 events).
    if ev.event != "SELL_ACTUAL" {
        if let Ok(mut q) = RECENT_EVENTS.lock() {
            if q.len() >= 50 {
                q.pop_front();
            }
            q.push_back((now_secs(), ev.event.to_string(), ev.mint.to_string(), ev.reason.to_string(), ev.est_realized_pnl_sol));
        }
    }

    let path = trade_log_path();
    let _guard = match TRADE_LOG_LOCK.lock() {
        Ok(g) => g,
        Err(p) => p.into_inner(),
    };
    let need_header = !std::path::Path::new(&path).exists();
    let file = std::fs::OpenOptions::new().create(true).append(true).open(&path);
    let mut file = match file {
        Ok(f) => f,
        Err(_) => return,
    };
    if need_header {
        let _ = writeln!(
            file,
            "timestamp_unix,iso_time,event,mint,reason,score,entry_mcap_sol,current_mcap_sol,pnl_pct,fraction_of_original,est_sol,est_realized_pnl_sol,signature"
        );
    }
    let iso = chrono::Utc::now().to_rfc3339();
    // CSV-escape the human-written reason field.
    let reason = ev.reason.replace('"', "'").replace(',', ";");
    let _ = writeln!(
        file,
        "{},{},{},{},{},{:.2},{:.6},{:.6},{:.2},{:.3},{:.6},{:.6},{}",
        now_secs(), iso, ev.event, ev.mint, reason, ev.score,
        ev.entry_mcap, ev.current_mcap, ev.pnl_pct, ev.fraction_of_original,
        ev.est_sol, ev.est_realized_pnl_sol, ev.signature,
    );
}

/// Market cap in SOL implied by the bonding-curve virtual reserves.
fn mcap_from_reserves(vsol: u64, vtok: u64) -> Option<f64> {
    if vsol == 0 || vtok == 0 {
        return None;
    }
    let price_sol_per_token = (vsol as f64 / LAMPORTS_PER_SOL) / (vtok as f64 / TOKEN_DECIMALS);
    let supply = TOKEN_TOTAL_SUPPLY as f64 / TOKEN_DECIMALS;
    Some(price_sol_per_token * supply)
}

/// Fetch the actual net SOL delta of the wallet (fee payer, account index 0) for
/// a confirmed transaction, in SOL. This is the true realized cash flow for a
/// sell: proceeds minus network fees and the priority/zeroslot tip. Retries a
/// few times because the tx may not be queryable immediately after landing.
async fn fetch_actual_sol_delta(app_state: &AppState, signature: &str) -> Option<f64> {
    use anchor_client::solana_client::rpc_config::RpcTransactionConfig;
    use anchor_client::solana_sdk::commitment_config::CommitmentConfig;
    use solana_transaction_status::UiTransactionEncoding;
    use std::str::FromStr;

    let sig = anchor_client::solana_sdk::signature::Signature::from_str(signature).ok()?;
    let cfg = RpcTransactionConfig {
        encoding: Some(UiTransactionEncoding::Json),
        commitment: Some(CommitmentConfig::confirmed()),
        max_supported_transaction_version: Some(0),
    };

    for _ in 0..5 {
        if let Ok(tx) = app_state
            .rpc_nonblocking_client
            .get_transaction_with_config(&sig, cfg.clone())
            .await
        {
            if let Some(meta) = tx.transaction.meta {
                if !meta.pre_balances.is_empty() && !meta.post_balances.is_empty() {
                    let delta = meta.post_balances[0] as i128 - meta.pre_balances[0] as i128;
                    return Some(delta as f64 / LAMPORTS_PER_SOL);
                }
            }
            return None;
        }
        time::sleep(Duration::from_secs(3)).await;
    }
    None
}

// ---------------------------------------------------------------------------
// Scoring
// ---------------------------------------------------------------------------

fn clamp01(x: f64) -> f64 {
    x.max(0.0).min(1.0)
}

/// Compute the continuation-probability score (0..100) from the rolling window.
fn score_token(state: &TokenMomentum, cfg: &MomentumConfig, now: u64) -> MomentumSignal {
    let short_cutoff = now.saturating_sub(cfg.short_window_secs);
    let med_cutoff = now.saturating_sub(cfg.medium_window_secs);

    let mut buy_vol_s = 0.0;
    let mut sell_vol_s = 0.0;
    let mut buy_vol_m = 0.0;
    let mut buy_count_s = 0u32;
    let mut unique_buyers: HashSet<&str> = HashSet::new();
    let mut sellers_s: HashSet<&str> = HashSet::new();
    let mut per_wallet_buy: HashMap<&str, f64> = HashMap::new();
    let mut first_mcap_s: Option<f64> = None;
    let mut last_mcap_s: Option<f64> = None;

    for t in state.ticks.iter() {
        if t.ts < med_cutoff {
            continue;
        }
        if t.ts >= short_cutoff {
            if t.is_buy {
                buy_vol_s += t.sol;
                buy_count_s += 1;
                unique_buyers.insert(t.trader.as_str());
                *per_wallet_buy.entry(t.trader.as_str()).or_insert(0.0) += t.sol;
                if first_mcap_s.is_none() && t.mcap > 0.0 {
                    first_mcap_s = Some(t.mcap);
                }
                if t.mcap > 0.0 {
                    last_mcap_s = Some(t.mcap);
                }
            } else {
                sell_vol_s += t.sol;
                sellers_s.insert(t.trader.as_str());
            }
        }
        // medium baseline (buy volume rate)
        if t.is_buy {
            buy_vol_m += t.sol;
        }
    }

    // 1. Buy pressure: share of short-window volume that is buys.
    let total_vol_s = buy_vol_s + sell_vol_s;
    let buy_pressure = if total_vol_s > 0.0 {
        clamp01((buy_vol_s / total_vol_s - 0.5) * 2.0)
    } else {
        0.0
    };

    // 2. Buy acceleration: short-window buy rate vs. medium-window baseline rate.
    let rate_s = buy_vol_s / cfg.short_window_secs.max(1) as f64;
    let rate_m = buy_vol_m / cfg.medium_window_secs.max(1) as f64;
    let accel = if rate_m > 1e-9 { rate_s / rate_m } else if rate_s > 0.0 { 2.0 } else { 0.0 };
    let buy_accel = clamp01(accel - 1.0); // 2x baseline -> full marks

    // 3. Raw buy volume level.
    let volume_level = clamp01(buy_vol_s / cfg.min_buy_volume_sol.max(1e-9));

    // 4. Breadth of participation.
    let breadth = clamp01(unique_buyers.len() as f64 / cfg.target_unique_buyers.max(1e-9));

    // 5. Market-cap velocity over the short window.
    let mcap_velocity = match (first_mcap_s, last_mcap_s) {
        (Some(f), Some(l)) if f > 0.0 => (l - f) / f,
        _ => 0.0,
    };
    let mcap_score = clamp01(mcap_velocity / cfg.target_mcap_growth.max(1e-9));

    // 6. Holder distribution: penalize a single wallet dominating the buys.
    let max_wallet_frac = if buy_vol_s > 0.0 {
        per_wallet_buy.values().cloned().fold(0.0_f64, f64::max) / buy_vol_s
    } else {
        0.0
    };
    let distribution = if max_wallet_frac <= cfg.max_wallet_concentration {
        1.0
    } else {
        clamp01(1.0 - (max_wallet_frac - cfg.max_wallet_concentration) / (1.0 - cfg.max_wallet_concentration))
    };

    // Weighted blend.
    let weighted = 0.20 * buy_pressure
        + 0.20 * buy_accel
        + 0.15 * volume_level
        + 0.15 * breadth
        + 0.20 * mcap_score
        + 0.10 * distribution;

    // Coordinated-dump veto: if sells significantly exceed buys, suppress the score.
    let dump_factor = if sell_vol_s > buy_vol_s * 1.5 {
        0.2
    } else if sell_vol_s > buy_vol_s {
        0.6
    } else {
        1.0
    };

    // Manufactured-momentum veto (edge hardening): real demand comes from many
    // distinct wallets buying once; fake pumps come from a few wallets cycling.
    // (a) Buyer diversity: unique buyers / buy transactions. Low => few wallets
    //     spamming buys to fake volume and "unique buyer" breadth.
    let buyer_diversity = if buy_count_s > 0 { unique_buyers.len() as f64 / buy_count_s as f64 } else { 1.0 };
    let diversity_factor = if buyer_diversity >= cfg.min_buyer_diversity {
        1.0
    } else {
        clamp01(buyer_diversity / cfg.min_buyer_diversity.max(1e-9))
    };
    // (b) Wash trading: share of buy volume from wallets that also sold in-window
    //     (round-tripping to inflate volume without real accumulation).
    let wash_vol: f64 = per_wallet_buy.iter().filter(|(w, _)| sellers_s.contains(**w)).map(|(_, v)| *v).sum();
    let wash_frac = if buy_vol_s > 0.0 { wash_vol / buy_vol_s } else { 0.0 };
    let wash_factor = if wash_frac <= cfg.max_wash_fraction {
        1.0
    } else {
        clamp01(1.0 - (wash_frac - cfg.max_wash_fraction) / (1.0 - cfg.max_wash_fraction).max(1e-9))
    };
    // (c) Creator self-buying: share of short-window buy VOLUME from the token creator's
    //     own wallet. A "pump" that's largely the creator buying their own token (the
    //     bundler fake-momentum trap) is discounted here so manufactured volume can't
    //     clear the entry gate — the genuine demand it's faking simply isn't there.
    let creator_buy_vol = state.last_trade_info.coin_creator.as_deref()
        .and_then(|c| per_wallet_buy.get(c).copied())
        .unwrap_or(0.0);
    let creator_frac = if buy_vol_s > 0.0 { creator_buy_vol / buy_vol_s } else { 0.0 };
    let creator_factor = if creator_frac <= cfg.max_creator_buy_frac {
        1.0
    } else {
        clamp01(1.0 - (creator_frac - cfg.max_creator_buy_frac) / (1.0 - cfg.max_creator_buy_frac).max(1e-9))
    };
    let genuine_factor = diversity_factor * wash_factor * creator_factor;

    let base_score = (weighted * 100.0) * dump_factor * genuine_factor;

    // Smart-money edge: add points when proven-good wallets are among the buyers.
    // Scaled by dump + genuineness so it can't rescue a dumped or wash-traded token.
    let boost = smart_money_boost(per_wallet_buy.keys().copied(), cfg) * dump_factor * genuine_factor;
    let score = base_score + boost;

    MomentumSignal {
        score,
        buy_volume_short: buy_vol_s,
        sell_volume_short: sell_vol_s,
        unique_buyers_short: unique_buyers.len(),
        mcap_velocity,
        current_mcap: last_mcap_s.or(Some(state.last_mcap)).unwrap_or(0.0),
        smart_money_boost: boost,
        genuine_factor,
    }
}

// ---------------------------------------------------------------------------
// Stream ingestion -> entry
// ---------------------------------------------------------------------------

fn extract_signer(txn: &SubscribeUpdateTransaction) -> Option<String> {
    let message = txn.transaction.as_ref()?.transaction.as_ref()?.message.as_ref()?;
    message
        .account_keys
        .first()
        .map(|k| bs58::encode(k).into_string())
}

/// Record one parsed trade into the rolling window and return the fresh signal.
fn ingest_trade(parsed: &TradeInfoFromToken, trader: String, now: u64, cfg: &MomentumConfig) -> MomentumSignal {
    let mint = parsed.mint.clone();
    let mut entry = TOKEN_STATE.entry(mint).or_insert_with(|| TokenMomentum {
        ticks: VecDeque::with_capacity(256),
        last_mcap: 0.0,
        last_trade_info: parsed.clone(),
        last_score: 0.0,
    });

    let mcap = mcap_from_reserves(parsed.virtual_sol_reserves, parsed.virtual_token_reserves)
        .unwrap_or(entry.last_mcap);
    if mcap > 0.0 {
        entry.last_mcap = mcap;
    }
    entry.last_trade_info = parsed.clone();

    let tick_mcap = entry.last_mcap;
    let tick_sol = parsed.sol_change.abs();

    // Outcome tracking: update the post-detection price stats for any evaluated token.
    update_outcome(&parsed.mint, tick_mcap, now);

    // Smart-money learning: queue meaningful buys for later outcome grading.
    if cfg.smart_money_enabled
        && parsed.is_buy
        && tick_sol >= cfg.smart_money_min_sol
        && !trader.is_empty()
        && tick_mcap > 0.0
    {
        enqueue_attribution(trader.clone(), parsed.mint.clone(), tick_mcap, now + cfg.smart_money_horizon_secs);
    }

    // KOL edge: if a tracked KOL just bought this token, mark it hot. Keep the
    // highest-weight KOL seen within the window as the trigger.
    if cfg.kol_enabled && parsed.is_buy && !trader.is_empty() {
        if let Some(kw) = KOL_WALLETS.get(&trader) {
            let (weight, label) = kw.value().clone();
            let expiry = now + cfg.kol_window_secs;
            let keep = KOL_HOT.get(&parsed.mint).map(|e| weight >= e.value().1).unwrap_or(true);
            if keep {
                KOL_HOT.insert(parsed.mint.clone(), (expiry, weight, label));
            } else if let Some(mut e) = KOL_HOT.get_mut(&parsed.mint) {
                e.0 = expiry; // refresh window even if a lower-weight KOL re-buys
            }
        }
    }

    entry.ticks.push_back(TradeTick {
        ts: now,
        is_buy: parsed.is_buy,
        sol: tick_sol,
        trader,
        mcap: tick_mcap,
    });

    // Prune ticks older than the baseline window.
    let cutoff = now.saturating_sub(cfg.medium_window_secs);
    while let Some(front) = entry.ticks.front() {
        if front.ts < cutoff {
            entry.ticks.pop_front();
        } else {
            break;
        }
    }

    let signal = score_token(&entry, cfg, now);
    entry.last_score = signal.score;
    signal
}

fn position_slots_available(cfg: &MomentumConfig) -> bool {
    // POSITIONS is authoritative in both live and dry-run modes.
    let held = POSITIONS.len();
    let in_flight = IN_FLIGHT_BUYS.load(Ordering::SeqCst);
    held + in_flight < cfg.max_positions
}

/// SOL currently exposed across open positions (committed size x fraction still held).
fn deployed_sol() -> f64 {
    POSITIONS.iter().map(|p| p.entry_size_sol * p.remaining_fraction).sum()
}

/// Realized PnL so far (the bankroll's running profit/loss).
fn realized_pnl() -> f64 {
    PNL_TALLY.lock().map(|g| g.0).unwrap_or(0.0)
}

/// Account equity in bankroll mode = starting capital + realized PnL. This is the
/// money the account has actually made/lost (open positions count once they close).
fn equity(cfg: &MomentumConfig) -> f64 {
    cfg.start_capital_sol + realized_pnl()
}

/// SOL free to deploy right now in bankroll mode = equity minus what's already in
/// open positions.
fn available_capital(cfg: &MomentumConfig) -> f64 {
    equity(cfg) - deployed_sol()
}

/// Base position size before conviction/alpha multipliers. In bankroll mode with a
/// percent set, it's a fraction of current equity (scales with the account); else
/// the fixed position_size_sol.
fn base_position_size(cfg: &MomentumConfig) -> f64 {
    if cfg.start_capital_sol > 0.0 && cfg.position_size_pct > 0.0 {
        (equity(cfg) * cfg.position_size_pct).max(0.0)
    } else {
        cfg.position_size_sol
    }
}

fn is_on_cooldown(mint: &str, now: u64, cooldown_secs: u64) -> bool {
    if let Some(ts) = RECENTLY_EXITED.get(mint) {
        // Re-allow after the cooldown; momentum can return, but avoid instant churn.
        return now.saturating_sub(*ts) < cooldown_secs;
    }
    false
}

/// Stream-based anti-dump concentration check (no API). Approximates each wallet's
/// holdings as net SOL bought (buys - sells, floored at 0) from the token's tick
/// history, then flags tokens where the top-N traders or the creator control too
/// large a share of that net trader-held float. A free, instant proxy for
/// "top-10 holders / bundled supply" that works on the freshest tokens GMGN can't
/// see yet. Returns a rejection reason, or None to allow.
fn concentration_veto(parsed: &TradeInfoFromToken, mint: &str, cfg: &MomentumConfig) -> Option<String> {
    if cfg.max_top_holder_share <= 0.0 && cfg.max_creator_share <= 0.0 {
        return None;
    }
    let state = TOKEN_STATE.get(mint)?;
    let mut net: HashMap<&str, f64> = HashMap::new();
    for t in state.ticks.iter() {
        if t.trader.is_empty() {
            continue;
        }
        let e = net.entry(t.trader.as_str()).or_insert(0.0);
        if t.is_buy { *e += t.sol; } else { *e -= t.sol; }
    }
    // Each wallet's "holdings" = positive net SOL in; total float = sum of those.
    let mut stakes: Vec<(&str, f64)> = net.into_iter().map(|(w, v)| (w, v.max(0.0))).collect();
    let total: f64 = stakes.iter().map(|(_, v)| *v).sum();
    if total <= 0.0 {
        return None;
    }
    // Concentration is only meaningful once there's a crowd to measure it over. On a
    // brand-new token only a few wallets have traded, so the top-N "naturally" hold
    // ~100% of the float — vetoing on that would reject almost everything. Skip the
    // check until at least `concentration_min_traders` distinct holders exist.
    let holders = stakes.iter().filter(|(_, v)| *v > 0.0).count();
    if holders < cfg.concentration_min_traders {
        return None;
    }

    if cfg.max_top_holder_share > 0.0 {
        stakes.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        let topn: f64 = stakes.iter().take(cfg.top_holder_n.max(1)).map(|(_, v)| *v).sum();
        let share = topn / total;
        if share > cfg.max_top_holder_share {
            return Some(format!("top-{} traders hold {:.0}% of float > {:.0}%", cfg.top_holder_n, share * 100.0, cfg.max_top_holder_share * 100.0));
        }
    }
    if cfg.max_creator_share > 0.0 {
        if let Some(creator) = &parsed.coin_creator {
            if !creator.is_empty() {
                let cstake = stakes.iter().find(|(w, _)| *w == creator.as_str()).map(|(_, v)| *v).unwrap_or(0.0);
                let share = cstake / total;
                if share > cfg.max_creator_share {
                    return Some(format!("creator holds {:.0}% of float > {:.0}%", share * 100.0, cfg.max_creator_share * 100.0));
                }
            }
        }
    }
    None
}

/// MELT-inspired insider-distribution veto. Among the top early buyers (by total
/// buy volume on this token), what fraction of what they bought have they already
/// sold back? A high ratio means the biggest early money is distributing — you'd be
/// buying their exit liquidity. Age-independent (it's their own sold/bought ratio,
/// not a share of supply), so it doesn't false-trigger on young tokens the way a
/// concentration % does. Returns a rejection reason, or None to allow.
fn insider_distribution_veto(mint: &str, cfg: &MomentumConfig) -> Option<String> {
    if cfg.insider_distrib_max_sold <= 0.0 {
        return None;
    }
    let state = TOKEN_STATE.get(mint)?;
    // Per-wallet bought and sold SOL over the token's tracked history.
    let mut bought: HashMap<&str, f64> = HashMap::new();
    let mut sold: HashMap<&str, f64> = HashMap::new();
    for t in state.ticks.iter() {
        if t.trader.is_empty() {
            continue;
        }
        if t.is_buy {
            *bought.entry(t.trader.as_str()).or_insert(0.0) += t.sol;
        } else {
            *sold.entry(t.trader.as_str()).or_insert(0.0) += t.sol;
        }
    }
    if bought.is_empty() {
        return None;
    }
    // Rank buyers by how much they bought; check the top N.
    let mut ranked: Vec<(&str, f64)> = bought.iter().map(|(w, v)| (*w, *v)).collect();
    ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let mut top_bought = 0.0;
    let mut top_sold = 0.0;
    for (w, b) in ranked.into_iter().take(cfg.insider_distrib_top_n.max(1)) {
        top_bought += b;
        top_sold += sold.get(w).copied().unwrap_or(0.0);
    }
    if top_bought <= 0.0 {
        return None;
    }
    let sold_ratio = top_sold / top_bought;
    if sold_ratio > cfg.insider_distrib_max_sold {
        return Some(format!(
            "top {} early buyers already sold {:.0}% of what they bought (> {:.0}%) — distributing",
            cfg.insider_distrib_top_n, sold_ratio * 100.0, cfg.insider_distrib_max_sold * 100.0,
        ));
    }
    None
}

// ===========================================================================
// Decision logging + outcome tracking (the learning system)
// ===========================================================================

/// Post-detection price tracking for one evaluated token, so we can label its
/// outcome (rug/loss/2x/...) and study which features predicted it.
#[derive(Clone, serde::Serialize, serde::Deserialize)]
struct OutcomeTrack {
    detect_ts: u64,
    detect_mcap: f64,
    peak_mcap: f64,
    trough_mcap: f64,
    last_mcap: f64,
    decision: String,
    overall_score: f64,
    // forward-return marks (mcap ratio - 1) captured the first time each elapses
    m15: Option<f64>,
    m30: Option<f64>,
    m60: Option<f64>,
    m120: Option<f64>,
    finalized: bool,
}

/// Concentration (top-N net float share) and insider sold-ratio as plain numbers
/// for the log — same math as the vetoes, but always returns a value.
fn concentration_metrics(parsed: &TradeInfoFromToken, mint: &str, cfg: &MomentumConfig) -> (f64, f64, f64, u32) {
    let state = match TOKEN_STATE.get(mint) {
        Some(s) => s,
        None => return (0.0, 0.0, 0.0, 0),
    };
    let mut net: HashMap<&str, f64> = HashMap::new();
    let mut bought: HashMap<&str, f64> = HashMap::new();
    let mut sold: HashMap<&str, f64> = HashMap::new();
    let mut smart = 0u32;
    let mut counted_smart: HashSet<&str> = HashSet::new();
    for t in state.ticks.iter() {
        if t.trader.is_empty() { continue; }
        let e = net.entry(t.trader.as_str()).or_insert(0.0);
        if t.is_buy {
            *e += t.sol;
            *bought.entry(t.trader.as_str()).or_insert(0.0) += t.sol;
            if !counted_smart.contains(t.trader.as_str()) {
                if let Some(r) = WALLET_REP.get(t.trader.as_str()) {
                    if wallet_is_proven(&r, cfg) {
                        smart += 1; counted_smart.insert(t.trader.as_str());
                    }
                }
            }
        } else {
            *e -= t.sol;
            *sold.entry(t.trader.as_str()).or_insert(0.0) += t.sol;
        }
    }
    // top-N holder concentration (net float share)
    let mut stakes: Vec<f64> = net.values().map(|v| v.max(0.0)).collect();
    let total: f64 = stakes.iter().sum();
    stakes.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
    let topn: f64 = stakes.iter().take(cfg.top_holder_n.max(1)).sum();
    let conc = if total > 0.0 { topn / total } else { 0.0 };
    // insider sold-ratio over top-N early buyers
    let mut ranked: Vec<(&str, f64)> = bought.iter().map(|(w, v)| (*w, *v)).collect();
    ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    let (mut tb, mut ts) = (0.0, 0.0);
    for (w, b) in ranked.into_iter().take(cfg.insider_distrib_top_n.max(1)) {
        tb += b; ts += sold.get(w).copied().unwrap_or(0.0);
    }
    let insider = if tb > 0.0 { ts / tb } else { 0.0 };
    // creator buy amount
    let creator_buy = parsed.coin_creator.as_ref()
        .and_then(|c| bought.get(c.as_str()).copied()).unwrap_or(0.0);
    (conc, insider, creator_buy, smart)
}

/// Record one token evaluation (BUY or REJECT) to the CSV + JSONL decision log and
/// register it for post-detection outcome tracking. One row per mint (first
/// decision wins). Best-effort; never blocks trading.
fn record_decision(parsed: &TradeInfoFromToken, signal: &MomentumSignal, cfg: &MomentumConfig, effective_score: f64, signal_type: &str, decision: &str, reason: &str) {
    let base = { DECISION_LOG_PATH.lock().map(|p| p.clone()).unwrap_or_default() };
    if base.is_empty() {
        return;
    }
    let mint = parsed.mint.clone();
    let is_buy = decision == "BUY";
    // Rejects log once per token (first reason wins); a BUY always logs and wins.
    if is_buy {
        DECISION_LOGGED.insert(mint.clone(), ());
    } else if DECISION_LOGGED.insert(mint.clone(), ()).is_some() {
        return;
    }
    let now = now_secs();
    let creator = parsed.coin_creator.clone().unwrap_or_default();
    let (conc, insider, creator_buy, smart_cnt) = concentration_metrics(parsed, &mint, cfg);
    let creator_score = if creator.is_empty() { 0.0 } else { WALLET_REP.get(&creator).map(|r| r.score).unwrap_or(0.0) };
    let liquidity = TOKEN_STATE.get(&mint).map(|s| s.last_trade_info.liquidity).unwrap_or(0.0);
    let confidence = if cfg.entry_score > 0.0 { effective_score / cfg.entry_score } else { 0.0 };
    let mcap = signal.current_mcap;

    // Register outcome tracking from the detection point (upgrade decision to BUY if
    // a token previously logged as REJECT is now bought).
    OUTCOMES.entry(mint.clone())
        .and_modify(|o| { if is_buy { o.decision = "BUY".to_string(); } })
        .or_insert_with(|| OutcomeTrack {
            detect_ts: now, detect_mcap: mcap, peak_mcap: mcap, trough_mcap: mcap, last_mcap: mcap,
            decision: decision.to_string(), overall_score: effective_score,
            m15: None, m30: None, m60: None, m120: None, finalized: false,
        });

    use std::io::Write;
    let _guard = DECISION_LOG_LOCK.lock().map(|g| g).unwrap_or_else(|p| p.into_inner());
    let safe = |s: &str| s.replace('"', "'").replace(',', ";");
    let iso = chrono::Utc::now().to_rfc3339();

    // CSV
    let csv_path = format!("{}.csv", base);
    let need_header = !std::path::Path::new(&csv_path).exists();
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&csv_path) {
        if need_header {
            let _ = writeln!(f, "timestamp,ca,creator,creator_buy,creator_score,wallet_score,insider_score,smart_wallet_count,holder_concentration,marketcap,volume,liquidity,overall_score,signal,decision,rejection_reason,confidence");
        }
        let _ = writeln!(f, "{},{},{},{:.4},{:.3},{:.2},{:.3},{},{:.3},{:.3},{:.3},{:.3},{:.2},{},{},{},{:.3}",
            iso, mint, creator, creator_buy, creator_score, signal.smart_money_boost, insider, smart_cnt,
            conc, mcap, signal.buy_volume_short, liquidity, effective_score, signal_type, decision,
            safe(if decision == "BUY" { "" } else { reason }), confidence);
    }
    // JSONL
    let jsonl_path = format!("{}.jsonl", base);
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&jsonl_path) {
        let obj = serde_json::json!({
            "timestamp": iso, "ca": mint, "creator": creator, "creator_buy": creator_buy,
            "creator_score": creator_score, "wallet_score": signal.smart_money_boost,
            "insider_score": insider, "smart_wallet_count": smart_cnt, "holder_concentration": conc,
            "marketcap": mcap, "volume": signal.buy_volume_short, "liquidity": liquidity,
            "overall_score": effective_score, "signal": signal_type, "decision": decision,
            "rejection_reason": if decision == "BUY" { "" } else { reason }, "confidence": confidence,
        });
        let _ = writeln!(f, "{}", obj);
    }
}

/// Update a tracked token's post-detection price stats on each new tick.
fn update_outcome(mint: &str, mcap: f64, now: u64) {
    if mcap <= 0.0 {
        return;
    }
    if let Some(mut o) = OUTCOMES.get_mut(mint) {
        if o.finalized || o.detect_mcap <= 0.0 {
            return;
        }
        o.last_mcap = mcap;
        if mcap > o.peak_mcap { o.peak_mcap = mcap; }
        if mcap < o.trough_mcap { o.trough_mcap = mcap; }
        let elapsed = now.saturating_sub(o.detect_ts);
        let ret = mcap / o.detect_mcap - 1.0;
        if o.m15.is_none() && elapsed >= 900 { o.m15 = Some(ret); }
        if o.m30.is_none() && elapsed >= 1800 { o.m30 = Some(ret); }
        if o.m60.is_none() && elapsed >= 3600 { o.m60 = Some(ret); }
        if o.m120.is_none() && elapsed >= 7200 { o.m120 = Some(ret); }
    }
}

/// Label an outcome from its peak return and final return.
fn outcome_label(max_ret: f64, final_ret: f64) -> &'static str {
    if final_ret <= -0.9 { "RUG" }
    else if max_ret >= 20.0 { "20X+" }
    else if max_ret >= 10.0 { "10X" }
    else if max_ret >= 5.0 { "5X" }
    else if max_ret >= 1.0 { "2X" }
    else if final_ret <= -0.3 { "LOSS" }
    else if final_ret >= 0.15 { "WIN" }
    else { "BREAKEVEN" }
}

/// Path for the pending (not-yet-finalized) outcome map: derived from the decision
/// log base so it travels with the rest of the learning artifacts.
fn outcomes_pending_path(cfg: &MomentumConfig) -> String {
    if cfg.decision_log_file.is_empty() { String::new() } else { format!("{}_pending.json", cfg.decision_log_file) }
}

/// Persist the in-memory outcome map so a restart doesn't reset every token's 2h
/// labeling clock. Without this, frequent restarts mean outcomes are NEVER labeled
/// and the self-learning loop starves. Atomic temp-file write, best-effort.
fn save_outcomes(path: &str) {
    if path.is_empty() {
        return;
    }
    let map: HashMap<String, OutcomeTrack> = OUTCOMES
        .iter()
        .filter(|e| !e.value().finalized)
        .map(|e| (e.key().clone(), e.value().clone()))
        .collect();
    if let Ok(json) = serde_json::to_string(&map) {
        let tmp = format!("{}.tmp", path);
        if std::fs::write(&tmp, json).is_ok() {
            let _ = std::fs::rename(&tmp, path);
        }
    }
}

/// Reload the pending outcome map saved by a previous run so the 2h labeling clock
/// survives restarts. Returns the number of tracked tokens restored.
fn load_outcomes(path: &str) -> usize {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return 0,
    };
    let map: HashMap<String, OutcomeTrack> = match serde_json::from_str(&content) {
        Ok(m) => m,
        Err(_) => return 0,
    };
    let mut n = 0;
    for (mint, o) in map {
        if !o.finalized {
            OUTCOMES.insert(mint, o);
            n += 1;
        }
    }
    n
}

/// Background task: finalize outcomes older than 2h, append a labeled row to the
/// outcomes CSV, and drop them from memory.
async fn run_outcome_tracker(cfg: Arc<MomentumConfig>) {
    use std::io::Write;
    let base = cfg.decision_log_file.clone();
    if base.is_empty() {
        return;
    }
    let path = format!("{}_outcomes.csv", base);
    let pending_path = format!("{}_pending.json", base);
    let mut interval = time::interval(Duration::from_secs(30));
    while MOMENTUM_RUNNING.load(Ordering::SeqCst) {
        interval.tick().await;
        let now = now_secs();
        let due: Vec<String> = OUTCOMES.iter()
            .filter(|e| !e.finalized && now.saturating_sub(e.detect_ts) >= cfg.outcome_horizon_secs)
            .map(|e| e.key().clone()).collect();
        for mint in due {
            if let Some(mut o) = OUTCOMES.get_mut(&mint) {
                o.finalized = true;
                let dm = o.detect_mcap;
                if dm <= 0.0 { continue; }
                let max_ret = o.peak_mcap / dm - 1.0;
                let dd = o.trough_mcap / dm - 1.0;
                let final_ret = o.last_mcap / dm - 1.0;
                let label = outcome_label(max_ret, final_ret);
                let need_header = !std::path::Path::new(&path).exists();
                if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(&path) {
                    if need_header {
                        let _ = writeln!(f, "ca,detect_ts,detect_mcap,ret_15m,ret_30m,ret_1h,ret_2h,max_return,max_drawdown,final_return,final_label,decision,overall_score");
                    }
                    let g = |v: Option<f64>| v.map(|x| format!("{:.4}", x)).unwrap_or_default();
                    let _ = writeln!(f, "{},{},{:.3},{},{},{},{},{:.4},{:.4},{:.4},{},{},{:.2}",
                        mint, o.detect_ts, dm, g(o.m15), g(o.m30), g(o.m60), g(o.m120),
                        max_ret, dd, final_ret, label, o.decision, o.overall_score);
                }
            }
            OUTCOMES.remove(&mint);
        }
        // Bound memory: if the map grows huge, drop the oldest unfinalized beyond 3h.
        if OUTCOMES.len() > 50_000 {
            let stale: Vec<String> = OUTCOMES.iter()
                .filter(|e| now.saturating_sub(e.detect_ts) >= cfg.outcome_horizon_secs + 3600)
                .map(|e| e.key().clone()).collect();
            for m in stale { OUTCOMES.remove(&m); }
        }
        // Persist the pending map every tick so a restart resumes the 2h clock
        // instead of wiping it (the bug that kept labeled outcomes at zero).
        save_outcomes(&pending_path);
    }
    // Flush once more on shutdown so the in-flight clock survives a clean Ctrl-C.
    save_outcomes(&pending_path);
}

/// Creator + top early buyers (by short-window volume) — the wallets whose
/// selling is the strongest early warning of a rug/distribution.
fn build_tracked_wallets(parsed: &TradeInfoFromToken, mint: &str, now: u64, cfg: &MomentumConfig) -> HashSet<String> {
    let mut tracked = HashSet::new();
    if !cfg.leader_dump_exit_enabled {
        return tracked;
    }
    if let Some(creator) = &parsed.coin_creator {
        if !creator.is_empty() {
            tracked.insert(creator.clone());
        }
    }
    if let Some(state) = TOKEN_STATE.get(mint) {
        let cut = now.saturating_sub(cfg.short_window_secs);
        let mut vol: HashMap<String, f64> = HashMap::new();
        for t in state.ticks.iter() {
            if t.is_buy && t.ts >= cut && !t.trader.is_empty() {
                *vol.entry(t.trader.clone()).or_insert(0.0) += t.sol;
            }
        }
        let mut ranked: Vec<(String, f64)> = vol.into_iter().collect();
        ranked.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        for (w, _) in ranked.into_iter().take(cfg.leader_track_top_n) {
            tracked.insert(w);
        }
    }
    tracked
}

/// SOL sold by tracked wallets within the short window — the insider-dump signal.
fn tracked_wallet_sell_volume(mint: &str, tracked: &HashSet<String>, now: u64, cfg: &MomentumConfig) -> f64 {
    if tracked.is_empty() {
        return 0.0;
    }
    let cut = now.saturating_sub(cfg.short_window_secs);
    TOKEN_STATE
        .get(mint)
        .map(|s| {
            s.ticks
                .iter()
                .filter(|t| !t.is_buy && t.ts >= cut && tracked.contains(&t.trader))
                .map(|t| t.sol)
                .sum()
        })
        .unwrap_or(0.0)
}

/// SOL sold by tracked wallets with tick.ts > `since_ts` (for cumulative SLOW-RUG
/// tracking across the whole hold). Returns (new sells since `since_ts`, latest tick ts
/// seen) so the caller can advance its watermark and never double-count.
fn tracked_wallet_sell_since(mint: &str, tracked: &HashSet<String>, since_ts: u64) -> (f64, u64) {
    if tracked.is_empty() {
        return (0.0, since_ts);
    }
    TOKEN_STATE
        .get(mint)
        .map(|s| {
            let mut vol = 0.0;
            let mut max_ts = since_ts;
            for t in s.ticks.iter() {
                if !t.is_buy && t.ts > since_ts && tracked.contains(&t.trader) {
                    vol += t.sol;
                }
                if t.ts > max_ts {
                    max_ts = t.ts;
                }
            }
            (vol, max_ts)
        })
        .unwrap_or((0.0, since_ts))
}

async fn try_enter(parsed: TradeInfoFromToken, signal: MomentumSignal, cfg: Arc<MomentumConfig>, sniper: Arc<SniperConfig>, logger: Logger) {
    // Discipline: if the circuit breaker has tripped, take no new entries.
    if trading_halted() {
        return;
    }
    // Bankroll mode: once the account is blown, the session is over for new entries.
    if MARGIN_CALLED.load(Ordering::SeqCst) {
        return;
    }

    let mint = parsed.mint.clone();
    let now = now_secs();

    // KOL edge: a tracked KOL buying this token is the leading signal.
    let kol = if cfg.kol_enabled { kol_hot(&mint, now) } else { None };
    // Auto-mute losing KOLs: a curated KOL that's net-negative over enough trades is
    // dropped (treated as no-KOL) so it stops boosting — no manual list editing needed.
    let kol = match kol {
        Some((_, ref label)) if kol_is_muted(label, &cfg) => None,
        other => other,
    };
    // Pure-KOL mode: only ever enter what a KOL bought (momentum/LP/anti-fake confirm).
    if cfg.kol_require && kol.is_none() {
        return;
    }
    let kol_pts = kol.as_ref().map(|(w, _)| cfg.kol_boost * w).unwrap_or(0.0);

    // Alpha-follow edge: a wallet the bot LEARNED is reliable is buying this token.
    // Only when no curated KOL already fired (avoid stacking two big boosts). This is
    // the memory in action — and it strengthens every run as more wallets earn the bar.
    // Smart-money accumulation + convergence: how many proven wallets are net-buying
    // this token at once. Convergence (several at once) is the strongest, most
    // reliable signal — boosted and sized harder than a single proven wallet.
    let conv = if kol.is_none() { smart_convergence(&mint, now, &cfg) } else { None };
    let conv_count = conv.as_ref().map(|(c, _, _)| *c).unwrap_or(0);
    let is_convergence = conv.as_ref().map(|(c, _, _)| *c >= cfg.convergence_min).unwrap_or(false);
    let alpha = conv.as_ref().map(|(_, _, w)| (w.clone(), 0.0));
    let alpha_pts = if is_convergence {
        cfg.convergence_boost
    } else if conv.is_some() {
        cfg.alpha_boost
    } else {
        0.0
    };

    // GMGN smart-money / trenches confirmation adds score points for the gate.
    let gmgn_hot = gmgn().is_some() && on_gmgn_watchlist(&mint, now);
    let mut effective_score = signal.score + if gmgn_hot { cfg.gmgn_boost } else { 0.0 } + kol_pts + alpha_pts;

    // Which signal drove this evaluation (for per-signal learning/attribution).
    let signal_type = if is_convergence { "convergence" }
        else if conv.is_some() { "alpha" }
        else if kol.is_some() { "kol" }
        else if gmgn_hot { "gmgn" }
        else { "momentum" };

    // Learned avoidance — stop repeating mistakes:
    //  (a) hard-skip a CREATOR whose tokens have lost the bot money before;
    //  (b) penalize the score by how badly this token's feature PATTERN has performed.
    let creator = parsed.coin_creator.clone().unwrap_or_default();
    if let Some((pnl, n)) = creator_is_bad(&creator, &cfg) {
        logger.log(format!("🧠 Avoid {} — creator {} burned us before ({:+.3} SOL/tok over {})", mint, &creator[..creator.len().min(8)], pnl, n).yellow().to_string());
        record_decision(&parsed, &signal, &cfg, effective_score, signal_type, "REJECT", &format!("creator avoided ({:+.3} over {})", pnl, n));
        return;
    }
    let cand_bands = token_bands(signal_type, conv_count as u32, signal.current_mcap, signal.score);
    let (pat_penalty, worst_band) = pattern_penalty(&cand_bands, &cfg);
    if pat_penalty > 0.0 {
        effective_score -= pat_penalty;
        if pat_penalty >= 5.0 && !DECISION_LOGGED.contains_key(&mint) {
            logger.log(format!("🧠 {} -{:.0} score: pattern '{}' has been losing", mint, pat_penalty, worst_band).yellow().to_string());
        }
    }

    // "Ones to Watch": high-conviction composite (market structure + convergence).
    // In beta (autobuy off) this only flags + alerts + tracks the token for learn.py
    // to validate; with autobuy on it becomes a large-size entry that bypasses the
    // normal score gate (but still respects the safety vetoes + capital below).
    let (wscore, ms_sub, conv_sub) = if cfg.watch_enabled {
        watch_score(&signal, &mint, conv_count, &cfg)
    } else { (0.0, 0.0, 0.0) };
    let is_watch = cfg.watch_enabled && wscore >= cfg.watch_score_min && signal.current_mcap > 0.0;
    if is_watch {
        WATCHLIST.insert(mint.clone(), (wscore, ms_sub, conv_sub, now));
        if !DECISION_LOGGED.contains_key(&mint) {
            logger.log(format!(
                "👁  ONE TO WATCH {} | watch {:.0} (structure {:.0}, convergence {:.0}, {} whales) | mcap {:.1} SOL{}",
                mint, wscore, ms_sub, conv_sub, conv_count, signal.current_mcap,
                if cfg.watch_autobuy { " | AUTOBUY" } else { " | alert-only (beta)" },
            ).magenta().bold().to_string());
        }
        record_decision(&parsed, &signal, &cfg, wscore, "watch",
            if cfg.watch_autobuy { "WATCH" } else { "WATCH" },
            &format!("watch {:.0} (ms {:.0}, conv {:.0})", wscore, ms_sub, conv_sub));
    }
    // A watch token in autobuy mode is its own entry trigger — let it bypass the
    // normal momentum score/floor gates (it has its own, higher composite bar).
    let watch_buy = is_watch && cfg.watch_autobuy;
    let signal_type = if watch_buy { "watch" } else { signal_type };

    if effective_score < cfg.entry_score && !watch_buy {
        record_decision(&parsed, &signal, &cfg, effective_score, signal_type, "REJECT", "score below entry bar");
        return;
    }
    // Base-momentum floor: a boost (KOL/alpha/GMGN) can't drag in a token that has
    // no real momentum of its own. Require genuine strength AND the signal.
    if cfg.min_base_score > 0.0 && signal.score < cfg.min_base_score && !watch_buy {
        record_decision(&parsed, &signal, &cfg, effective_score, signal_type, "REJECT", "base momentum below floor");
        return;
    }
    if POSITIONS.contains_key(&mint) || BOUGHT_TOKEN_LIST.contains_key(&mint) {
        return;
    }
    if is_on_cooldown(&mint, now, cfg.reentry_cooldown_secs) {
        return;
    }
    if !position_slots_available(&cfg) {
        record_decision(&parsed, &signal, &cfg, effective_score, signal_type, "REJECT", "no position slot / capacity");
        return;
    }
    if signal.current_mcap <= 0.0 {
        return;
    }

    // Stream-based anti-dump concentration veto (free, instant, works on fresh
    // tokens). Checked before the GMGN call so we reject dump setups without an API hit.
    if let Some(reason) = concentration_veto(&parsed, &mint, &cfg) {
        logger.log(format!("🛑 Concentration veto {} — {}", mint, reason).yellow().to_string());
        record_decision(&parsed, &signal, &cfg, effective_score, signal_type, "REJECT", &format!("concentration: {}", reason));
        return;
    }
    // MELT-inspired: don't buy into a token whose biggest early buyers are already
    // distributing — that's the trash-coin / coordinated-dump pattern.
    if let Some(reason) = insider_distribution_veto(&mint, &cfg) {
        logger.log(format!("🛑 Insider-distribution veto {} — {}", mint, reason).yellow().to_string());
        record_decision(&parsed, &signal, &cfg, effective_score, signal_type, "REJECT", &format!("insider-distribution: {}", reason));
        return;
    }

    // GMGN security veto: skip honeypots / high rug_ratio before committing capital.
    if cfg.gmgn_security_veto {
        if let Some(client) = gmgn() {
            match client.security_verdict(&mint).await {
                SecurityVerdict::Reject(reason) => {
                    logger.log(format!("🛑 GMGN veto {} — {}", mint, reason).red().to_string());
                    record_decision(&parsed, &signal, &cfg, effective_score, signal_type, "REJECT", &format!("gmgn: {}", reason));
                    return;
                }
                SecurityVerdict::Unknown if client.veto_on_unknown() => {
                    logger.log(format!("🛑 GMGN veto {} — security unknown (fail-closed)", mint).yellow().to_string());
                    record_decision(&parsed, &signal, &cfg, effective_score, signal_type, "REJECT", "gmgn: security unknown");
                    return;
                }
                _ => {}
            }
        }
    }

    // Honeypot authority pre-check: reject tokens that can be frozen (unsellable) or
    // whose supply can still be minted, BEFORE committing capital. Cached per mint;
    // fails open on RPC error so a flaky read doesn't block every entry.
    if cfg.authority_check {
        if let Some(reason) = authority_honeypot_reason(&sniper.app_state.rpc_nonblocking_client, &mint).await {
            logger.log(format!("🛑 Authority veto {} — {}", mint, reason).red().to_string());
            record_decision(&parsed, &signal, &cfg, effective_score, signal_type, "REJECT", &format!("authority: {}", reason));
            return;
        }
    }

    // Allow re-entry on a fresh pump: clear the bot's permanent buy blacklist for
    // this mint (it was added on a prior buy). The exit cooldown above still
    // prevents instant churn.
    if cfg.allow_reentry {
        crate::processor::sniper_bot::clear_bought_blacklist(&mint);
    }

    // Base size: a fraction of equity in bankroll+percent mode (scales with the
    // account), else the fixed position size. Conviction then scales up from there.
    let base_size = base_position_size(&cfg);
    let mut entry_size = if cfg.conviction_sizing && cfg.entry_score > 0.0 {
        let mult = (signal.score / cfg.entry_score).clamp(1.0, cfg.conviction_max_mult);
        base_size * mult
    } else {
        base_size
    };
    // Following a proven wallet (curated KOL or learned alpha) is our highest-
    // conviction signal — size up. Capped so one trade can't dwarf the book.
    let following = kol.is_some() || alpha.is_some();
    if following && cfg.alpha_size_mult > 1.0 {
        // Convergence (several proven wallets) earns an extra size bump over a single one.
        let follow_mult = if is_convergence { cfg.alpha_size_mult * cfg.convergence_size_mult } else { cfg.alpha_size_mult };
        let cap = base_size * cfg.conviction_max_mult.max(follow_mult);
        entry_size = (entry_size * follow_mult).min(cap);
    }
    // "Ones to Watch" autobuy: these are the highest-conviction setups, so they get
    // the larger watch size (not the usual position size).
    if watch_buy {
        entry_size = entry_size.max(cfg.watch_size_sol);
    }

    // LIQUIDITY CAP: never take a position so large you become the token's exit
    // liquidity (e.g. 14 SOL into a 29 SOL mcap). Cap to a fraction of mcap AND of
    // bonding-curve liquidity, so size scales with the token — small on tiny tokens,
    // bigger as they grow — while wallet equity still scales the base size up.
    if cfg.max_mcap_fraction > 0.0 && signal.current_mcap > 0.0 {
        entry_size = entry_size.min(signal.current_mcap * cfg.max_mcap_fraction);
    }
    let curve_liq = parsed.liquidity;
    if cfg.max_liq_fraction > 0.0 && curve_liq > 0.0 {
        entry_size = entry_size.min(curve_liq * cfg.max_liq_fraction);
    }
    // Too illiquid to size meaningfully — skip rather than take a dust position that
    // also can't exit cleanly.
    if entry_size < cfg.min_position_sol {
        record_decision(&parsed, &signal, &cfg, effective_score, signal_type, "REJECT",
            &format!("too illiquid: capped size {:.4} < min {:.4} (mcap {:.1}, liq {:.1})", entry_size, cfg.min_position_sol, signal.current_mcap, curve_liq));
        return;
    }

    if cfg.start_capital_sol > 0.0 {
        // REAL bankroll: equity = start capital + realized PnL. You can only deploy
        // what you actually have, the account compounds on profit, and a margin call
        // ends the session when equity falls below the bankruptcy floor (ruined).
        let eq = equity(&cfg);
        let floor = (cfg.start_capital_sol * cfg.bankruptcy_floor_frac).max(0.0);
        if eq <= floor {
            if !MARGIN_CALLED.swap(true, Ordering::SeqCst) {
                logger.log(format!(
                    "💀 MARGIN CALL — equity {:.3} SOL <= floor {:.3} ({:.0}% of start). Account ruined after {:+.3} SOL realized. Halting new entries; open positions still exit.",
                    eq, floor, cfg.bankruptcy_floor_frac * 100.0, realized_pnl(),
                ).red().bold().to_string());
            }
            return;
        }
        if entry_size <= 0.0 {
            return;
        }
        let avail = available_capital(&cfg);
        if avail < entry_size {
            // Fully deployed for the current equity — wait for capital to free up.
            return;
        }
    } else {
        // Legacy fixed cap: never let total exposure exceed max_deployed_sol.
        let headroom = cfg.max_deployed_sol - deployed_sol();
        if headroom < cfg.position_size_sol.min(entry_size) * 0.5 {
            logger.log(format!(
                "⛔ Skipping {} — capital cap reached ({:.3}/{:.3} SOL deployed)",
                mint, deployed_sol(), cfg.max_deployed_sol,
            ).yellow().to_string());
            return;
        }
        if entry_size > headroom {
            entry_size = headroom;
        }
    }

    // True cost basis includes the estimated entry-leg cost so PnL isn't optimistic.
    let cost_basis_sol = entry_size * (1.0 + cfg.buy_cost_fraction);

    // Build the tracked-wallet set for the insider/leader-dump exit: the creator
    // plus the largest early buyers (by short-window volume).
    let tracked_wallets = build_tracked_wallets(&parsed, &mint, now, &cfg);

    // Reserve a slot before the async buy to prevent overshooting max positions.
    IN_FLIGHT_BUYS.fetch_add(1, Ordering::SeqCst);

    // Attribution label: the wallet(s) we're following. Curated KOLs keep their
    // label; convergence gets a "conv:N" label; a single learned wallet gets "α:<w>".
    let conv_count = conv.as_ref().map(|(c, _, _)| *c).unwrap_or(0);
    let lead_label: Option<String> = if let Some((_, l)) = &kol {
        Some(l.clone())
    } else if is_convergence {
        Some(format!("conv:{}", conv_count))
    } else {
        alpha.as_ref().map(|(w, _)| format!("α:{}", &w[..w.len().min(8)]))
    };

    let lead_tag = if let Some((_, l)) = &kol {
        format!(", KOL:{} +{:.0}", l, kol_pts)
    } else if is_convergence {
        format!(", CONVERGENCE x{} +{:.0}", conv_count, alpha_pts)
    } else if let Some((w, _)) = &alpha {
        format!(", ALPHA:{} +{:.0}", &w[..w.len().min(8)], alpha_pts)
    } else {
        String::new()
    };
    logger.log(format!(
        "🟢 ENTRY {} | score {:.1} (smart +{:.1}{}{}) | genuine {:.0}% | size {:.3} SOL | buyvol {:.2} | {} buyers | mcap {:.1} SOL",
        mint, signal.score, signal.smart_money_boost,
        if gmgn_hot { format!(", GMGN +{:.1}", cfg.gmgn_boost) } else { String::new() },
        lead_tag,
        signal.genuine_factor * 100.0,
        entry_size, signal.buy_volume_short, signal.unique_buyers_short, signal.current_mcap,
    ).green().bold().to_string());

    // Reason carries the lead label so the analyzer can attribute PnL per wallet.
    let buy_reason = match &lead_label {
        Some(l) => format!("momentum entry [KOL:{}]", l),
        None => "momentum entry".to_string(),
    };

    // Live buy via the configured landing route (zeroslot | jito | multi), the
    // same fast path the sells use. Returns the tx signature for reconciliation.
    let result: Result<Option<String>, String> = if cfg.dry_run {
        Ok(None)
    } else {
        momentum_buy(&parsed, entry_size, Arc::new(sniper.app_state.clone()), &cfg, &logger)
            .await
            .map(Some)
    };

    IN_FLIGHT_BUYS.fetch_sub(1, Ordering::SeqCst);

    match result {
        Ok(buy_sig) => {
            POSITIONS.insert(mint.clone(), MomentumPosition {
                entry_mcap: signal.current_mcap,
                entry_size_sol: entry_size,
                cost_basis_sol,
                remaining_fraction: 1.0,
                rungs_hit: vec![false; cfg.scale_out_targets.len()],
                peak_pnl: 0.0,
                selling: false,
                tracked_wallets,
                entry_ts: now,
                kol_label: lead_label.clone().unwrap_or_default(),
                sell_attempts: 0,
                quarantined: false,
                realized_so_far: 0.0,
                creator: parsed.coin_creator.clone().unwrap_or_default(),
                signal_type: signal_type.to_string(),
                entry_conv: conv_count as u32,
                entry_base_score: signal.score,
                insider_sold_cum: 0.0,
                insider_seen_ts: now,
            });
            save_positions(); // crash-safety: a new open position is on disk immediately
            record_decision(&parsed, &signal, &cfg, effective_score, signal_type, "BUY", "");
            log_trade_event(&TradeLogEvent {
                event: "BUY",
                mint: &mint,
                reason: &buy_reason,
                score: signal.score,
                entry_mcap: signal.current_mcap,
                current_mcap: signal.current_mcap,
                pnl_pct: 0.0,
                fraction_of_original: 1.0,
                est_sol: -cost_basis_sol,
                est_realized_pnl_sol: 0.0,
                signature: "",
            });
            let tag = if cfg.dry_run { "📝 [DRY] Entered" } else { "✅ Bought" };
            logger.log(format!("{} {} at mcap {:.2} SOL", tag, mint, signal.current_mcap).green().to_string());

            // Exact buy reconciliation: read the true SOL spent from the buy tx and
            // correct the position's cost basis so realized PnL is exact on both legs.
            if let Some(sig) = buy_sig {
                spawn_buy_reconcile(Arc::new(sniper.app_state.clone()), mint.clone(), sig, cost_basis_sol, signal.current_mcap, signal.score);
            }
        }
        Err(e) => {
            logger.log(format!("❌ Buy failed for {}: {}", mint, e).red().to_string());
        }
    }
}

/// Read the true on-chain SOL spent on a buy and correct the position's cost
/// basis (the buy leg of exact PnL). Best-effort, in the background.
#[allow(clippy::too_many_arguments)]
fn spawn_buy_reconcile(app_state: Arc<AppState>, mint: String, signature: String, est_cost: f64, entry_mcap: f64, score: f64) {
    if signature.is_empty() {
        return;
    }
    TX_SENT.fetch_add(1, Ordering::Relaxed);
    tokio::spawn(async move {
        if let Some(delta) = fetch_actual_sol_delta(&app_state, &signature).await {
            TX_LANDED.fetch_add(1, Ordering::Relaxed);
            // A buy spends SOL -> wallet delta is negative; actual cost = -delta.
            let actual_cost = (-delta).max(0.0);
            if actual_cost <= 0.0 {
                return;
            }
            if let Some(mut p) = POSITIONS.get_mut(&mint) {
                p.cost_basis_sol = actual_cost;
            }
            log_trade_event(&TradeLogEvent {
                event: "BUY_ACTUAL",
                mint: &mint,
                reason: "on-chain buy cost",
                score,
                entry_mcap,
                current_mcap: entry_mcap,
                pnl_pct: 0.0,
                fraction_of_original: 1.0,
                est_sol: -actual_cost,
                est_realized_pnl_sol: est_cost - actual_cost, // estimate error (info only)
                signature: &signature,
            });
        }
    });
}

/// Buy `amount_sol` of a pump.fun token, landing via the configured route
/// (zeroslot | jito | multi). Mirrors momentum_sell so both legs share the same
/// fast-landing path. Returns the tx signature.
async fn momentum_buy(
    parsed: &TradeInfoFromToken,
    amount_sol: f64,
    app_state: Arc<AppState>,
    cfg: &MomentumConfig,
    logger: &Logger,
) -> Result<String, String> {
    // Pre-buy balance + fee-reserve guard: never send a buy the wallet can't afford,
    // and always keep a reserve so there's SOL left to pay for the eventual sell.
    // Best-effort: if the balance lookup fails we proceed (the tx would fail on-chain).
    if let Ok(pubkey) = app_state.wallet.try_pubkey() {
        if let Ok(lamports) = app_state.rpc_nonblocking_client.get_balance(&pubkey).await {
            let balance = lamports as f64 / LAMPORTS_PER_SOL;
            let needed = amount_sol + cfg.fee_reserve_sol;
            if balance < needed {
                return Err(format!(
                    "insufficient balance: have {:.4} SOL, need {:.4} (size {:.4} + reserve {:.4})",
                    balance, needed, amount_sol, cfg.fee_reserve_sol,
                ));
            }
        }
    }

    let buy_config = SwapConfig {
        swap_direction: SwapDirection::Buy,
        in_type: SwapInType::Qty,
        amount_in: amount_sol,
        slippage: cfg.slippage_bps,
    };
    let mut trade_info = parsed.clone();
    trade_info.dex_type = DexType::PumpFun;
    trade_info.is_buy = true;

    let pump = Pump::new(
        app_state.rpc_nonblocking_client.clone(),
        app_state.rpc_client.clone(),
        app_state.wallet.clone(),
    );
    let (keypair, instructions, _price) = pump
        .build_swap_from_parsed_data(&trade_info, buy_config)
        .await
        .map_err(|e| format!("build buy failed: {}", e))?;

    // Anti-tamper guard: never sign a buy that touches an unexpected program.
    if cfg.guard_programs {
        guard_swap_instructions(&instructions).map_err(|e| format!("buy blocked by program guard: {}", e))?;
    }

    let blockhash = crate::library::blockhash_processor::BlockhashProcessor::get_latest_blockhash()
        .await
        .ok_or_else(|| "no recent blockhash".to_string())?;

    // Opt-in pre-send simulation: abort a buy that would revert before spending fees.
    if cfg.presend_simulate {
        simulate_before_send(&app_state.rpc_nonblocking_client, &keypair, &instructions, blockhash, "buy")
            .await
            .map_err(|e| format!("buy blocked by pre-send simulation: {}", e))?;
    }

    let sigs = match cfg.landing.as_str() {
        "jito" => crate::block_engine::tx::new_signed_and_send_jito(
            blockhash, &keypair, instructions, logger,
        ).await,
        "multi" => crate::block_engine::tx::new_signed_and_send_multi(
            &app_state, blockhash, &keypair, instructions, logger,
        ).await,
        _ => crate::block_engine::tx::new_signed_and_send_zeroslot(
            app_state.zeroslot_rpc_client.clone(), blockhash, &keypair, instructions, logger,
        ).await,
    }
    .map_err(|e| format!("send buy failed: {}", e))?;

    Ok(sigs.first().cloned().unwrap_or_default())
}

// ---------------------------------------------------------------------------
// Exit policy
// ---------------------------------------------------------------------------

/// Sell a fraction (0..1) of the *current* token balance via PumpFun + zeroslot.
async fn momentum_sell(
    mint: &str,
    fraction: f64,
    app_state: Arc<AppState>,
    cfg: &MomentumConfig,
    reason: &str,
    logger: &Logger,
) -> Result<String, String> {
    let fraction = fraction.max(0.0).min(1.0);
    if fraction <= 0.0 {
        return Ok(String::new());
    }

    let trade_info = TOKEN_STATE
        .get(mint)
        .map(|s| s.last_trade_info.clone())
        .unwrap_or_else(|| TradeInfoFromToken {
            dex_type: DexType::PumpFun,
            slot: 0,
            signature: "momentum_sell".to_string(),
            pool_id: String::new(),
            mint: mint.to_string(),
            timestamp: now_secs(),
            is_buy: false,
            price: 0,
            is_reverse_when_pump_swap: false,
            coin_creator: None,
            sol_change: 0.0,
            token_change: 0.0,
            liquidity: 0.0,
            virtual_sol_reserves: 0,
            virtual_token_reserves: 0,
        });

    let sell_config = SwapConfig {
        swap_direction: SwapDirection::Sell,
        in_type: SwapInType::Pct,
        amount_in: fraction,
        slippage: cfg.slippage_bps,
    };

    let mut sell_trade_info = trade_info;
    sell_trade_info.dex_type = DexType::PumpFun;
    sell_trade_info.is_buy = false;

    let pump = Pump::new(
        app_state.rpc_nonblocking_client.clone(),
        app_state.rpc_client.clone(),
        app_state.wallet.clone(),
    );

    let (keypair, instructions, price) = pump
        .build_swap_from_parsed_data(&sell_trade_info, sell_config)
        .await
        .map_err(|e| format!("build sell failed: {}", e))?;

    // Anti-tamper guard: never sign a sell that touches an unexpected program.
    if cfg.guard_programs {
        guard_swap_instructions(&instructions).map_err(|e| format!("sell blocked by program guard: {}", e))?;
    }

    let blockhash = crate::library::blockhash_processor::BlockhashProcessor::get_latest_blockhash()
        .await
        .ok_or_else(|| "no recent blockhash".to_string())?;

    // Opt-in pre-send simulation: catch a honeypot / sell-path revert before firing a
    // real, failing sell (the existing retry/quarantine logic handles repeated failures).
    if cfg.presend_simulate {
        simulate_before_send(&app_state.rpc_nonblocking_client, &keypair, &instructions, blockhash, "sell")
            .await
            .map_err(|e| format!("sell blocked by pre-send simulation: {}", e))?;
    }

    let sigs = match cfg.landing.as_str() {
        "jito" => crate::block_engine::tx::new_signed_and_send_jito(
            blockhash, &keypair, instructions, logger,
        ).await,
        "multi" => crate::block_engine::tx::new_signed_and_send_multi(
            &app_state, blockhash, &keypair, instructions, logger,
        ).await,
        _ => crate::block_engine::tx::new_signed_and_send_zeroslot(
            app_state.zeroslot_rpc_client.clone(), blockhash, &keypair, instructions, logger,
        ).await,
    }
    .map_err(|e| format!("send sell failed: {}", e))?;

    let signature = sigs.first().cloned().unwrap_or_default();
    logger.log(format!(
        "🔴 SELL {:.0}% of {} ({}) at price {} | sig {}",
        fraction * 100.0, mint, reason, price, signature,
    ).red().bold().to_string());

    Ok(signature)
}

fn finalize_exit(mint: &str) {
    POSITIONS.remove(mint);
    BOUGHT_TOKEN_LIST.remove(mint);
    TOKEN_STATE.remove(mint);
    RECENTLY_EXITED.insert(mint.to_string(), now_secs());
    save_positions();
}

/// Persist all open positions atomically (tmp + rename) so a crash/restart never
/// loses exit management of money already in the market. Called on every change.
fn save_positions() {
    let path = match POSITIONS_PATH.lock() {
        Ok(p) => p.clone(),
        Err(_) => return,
    };
    if path.is_empty() {
        return;
    }
    let map: HashMap<String, MomentumPosition> = POSITIONS
        .iter()
        .map(|e| (e.key().clone(), e.value().clone()))
        .collect();
    if let Ok(json) = serde_json::to_string(&map) {
        let tmp = format!("{}.tmp", path);
        if std::fs::write(&tmp, json).is_ok() {
            let _ = std::fs::rename(&tmp, &path);
        }
    }
}

/// Restore open positions saved by a previous run so the exit monitor resumes
/// managing them. Returns the recovered mints. The transient `selling` lock is
/// cleared so each restored position is freshly re-evaluated.
fn load_positions(path: &str) -> Vec<String> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };
    let map: HashMap<String, MomentumPosition> = match serde_json::from_str(&content) {
        Ok(m) => m,
        Err(_) => return Vec::new(),
    };
    let mut recovered = Vec::new();
    for (mint, mut pos) in map {
        pos.selling = false;
        POSITIONS.insert(mint.clone(), pos);
        recovered.push(mint);
    }
    recovered
}

/// Evaluate one position against the live signal and act.
/// Force-close a position that's exceeded the max hold time (a zombie). Prices it at
/// the last-known mcap if available, else entry mcap (breakeven write-off). In live
/// mode it attempts a real sell; either way the slot is freed and PnL booked.
async fn force_close(mint: &str, app_state: &Arc<AppState>, cfg: &Arc<MomentumConfig>, logger: &Logger) {
    let snap = match POSITIONS.get(mint) {
        Some(p) => (p.entry_mcap, p.entry_size_sol, p.cost_basis_sol, p.remaining_fraction,
                    p.realized_so_far, p.kol_label.clone(), p.creator.clone(),
                    p.signal_type.clone(), p.entry_conv, p.entry_base_score),
        None => return,
    };
    let (entry_mcap, entry_size, cost_basis, frac, prior_realized, kol_label, creator, sig_type, conv, base_score) = snap;
    if let Some(mut p) = POSITIONS.get_mut(mint) { p.selling = true; }

    let cur_mcap = TOKEN_STATE.get(mint).map(|s| s.last_mcap).filter(|m| *m > 0.0).unwrap_or(entry_mcap);
    let ratio = if entry_mcap > 0.0 { cur_mcap / entry_mcap } else { 1.0 };
    let cost = frac * cost_basis;
    let proceeds = frac * entry_size * ratio * if cfg.dry_run { 1.0 - cfg.sim_cost_fraction } else { 1.0 };
    let realized = proceeds - cost;

    if !cfg.dry_run {
        // Best-effort real sell; if it fails, the position is still finalized below
        // (unsellable quarantine already handles tokens that truly can't be sold).
        let _ = momentum_sell(mint, 1.0, app_state.clone(), cfg, "max hold", logger).await;
    }

    log_trade_event(&TradeLogEvent {
        event: "SELL_FULL", mint, reason: "max hold / zombie reaped",
        score: base_score, entry_mcap, current_mcap: cur_mcap,
        pnl_pct: (ratio - 1.0) * 100.0, fraction_of_original: frac,
        est_sol: proceeds, est_realized_pnl_sol: realized, signature: if cfg.dry_run { "DRY_RUN" } else { "" },
    });
    let pos_total = prior_realized + realized;
    learn_avoidance(&creator, &token_bands(&sig_type, conv, entry_mcap, base_score), pos_total);
    record_kol_pnl(&kol_label, realized, true, realized);
    if pos_total >= 0.0 { SESSION_WINS.fetch_add(1, Ordering::SeqCst); } else { SESSION_LOSSES.fetch_add(1, Ordering::SeqCst); }
    finalize_exit(mint);
    record_full_exit(pos_total, cfg, logger);
    logger.log(format!("⏱  Force-closed {} — stale/dead position reaped | PnL {:+.4} SOL (priced at {:.1} mcap)", mint, realized, cur_mcap).yellow().to_string());
}

async fn evaluate_position(mint: String, app_state: Arc<AppState>, cfg: Arc<MomentumConfig>, logger: Logger) {
    let now = now_secs();

    // Max-hold reaper: force-close a position held past the hard limit — this catches
    // "zombies" (tokens that stopped trading, or positions recovered from a prior run
    // with no live price feed) that the normal exit logic can never price/close.
    if cfg.max_hold_secs > 0 {
        let stale = POSITIONS.get(&mint).map(|p| !p.selling && now.saturating_sub(p.entry_ts) >= cfg.max_hold_secs).unwrap_or(false);
        if stale {
            force_close(&mint, &app_state, &cfg, &logger).await;
            return;
        }
    }
    // Fast no-activity reaper: a token that's stopped trading is dead. If the position
    // is old enough AND its token hasn't traded in `stale_exit_secs`, force-close it —
    // catches zombies in minutes instead of waiting for the 3h max-hold.
    if cfg.stale_exit_secs > 0 {
        let pos_age = POSITIONS.get(&mint).map(|p| if p.selling { 0 } else { now.saturating_sub(p.entry_ts) }).unwrap_or(0);
        let last_tick_age = TOKEN_STATE.get(&mint)
            .and_then(|s| s.ticks.back().map(|t| now.saturating_sub(t.ts)))
            .unwrap_or(u64::MAX);
        if pos_age >= cfg.stale_exit_secs && last_tick_age >= cfg.stale_exit_secs {
            force_close(&mint, &app_state, &cfg, &logger).await;
            return;
        }
    }

    // Snapshot the live signal.
    let signal = match TOKEN_STATE.get(&mint) {
        Some(s) => score_token(&s, &cfg, now),
        None => return,
    };

    // KOL that triggered this position (for the per-KOL leaderboard).
    let pos_kol = POSITIONS.get(&mint).map(|p| p.kol_label.clone()).unwrap_or_default();

    // Real SOL reserves in the bonding curve (the `liquidity` field). When it nears
    // the graduation threshold the token is about to migrate, after which our
    // bonding-curve mcap is no longer valid — so we exit rather than hold blind.
    let curve_sol = TOKEN_STATE.get(&mint).map(|s| s.last_trade_info.liquidity).unwrap_or(0.0);

    // Insider/leader-dump signal (read-only) computed before locking the position.
    let leader_sell = {
        let tracked = POSITIONS.get(&mint).map(|p| p.tracked_wallets.clone());
        match tracked {
            Some(t) => tracked_wallet_sell_volume(&mint, &t, now, &cfg),
            None => return,
        }
    };

    // Read + update position bookkeeping without holding the lock across awaits.
    let decision = {
        let mut pos = match POSITIONS.get_mut(&mint) {
            Some(p) => p,
            None => return,
        };
        if pos.selling || pos.quarantined {
            return;
        }
        if signal.current_mcap <= 0.0 || pos.entry_mcap <= 0.0 {
            return;
        }

        let pnl = (signal.current_mcap - pos.entry_mcap) / pos.entry_mcap * 100.0;
        if pnl > pos.peak_pnl {
            pos.peak_pnl = pnl;
        }

        let entry_mcap = pos.entry_mcap;
        let entry_size_sol = pos.entry_size_sol;
        let cost_basis_sol = pos.cost_basis_sol;

        // Slow-rug accumulation: add tracked-wallet (creator + early buyer) sells seen
        // since we last looked, so a steady drip that never trips the acute leader-dump
        // threshold still accumulates over the whole hold.
        if cfg.slow_rug_sol > 0.0 {
            let tracked = pos.tracked_wallets.clone();
            let (new_sold, seen_ts) = tracked_wallet_sell_since(&mint, &tracked, pos.insider_seen_ts);
            pos.insider_sold_cum += new_sold;
            pos.insider_seen_ts = seen_ts;
        }
        let insider_cum = pos.insider_sold_cum;

        // 0a. Migration imminent -> exit before bonding-curve pricing goes invalid.
        if cfg.migration_exit_sol > 0.0 && curve_sol >= cfg.migration_exit_sol {
            pos.selling = true;
            Decision::full(pos.remaining_fraction, format!("migration imminent ({:.1} SOL in curve, pnl {:.1}%)", curve_sol, pnl), pnl, entry_mcap, signal.current_mcap, entry_size_sol, cost_basis_sol)
        }
        // 0b. Insider/leader distribution -> exit (full by default; configurable to a
        // partial so a runner can keep riding via the trailing stop).
        else if cfg.leader_dump_exit_enabled && leader_sell >= cfg.leader_dump_sol {
            pos.selling = true;
            let frac = cfg.leader_dump_fraction;
            if frac >= 1.0 {
                Decision::full(pos.remaining_fraction, format!("insider distribution ({:.2} SOL sold by tracked wallets, pnl {:.1}%)", leader_sell, pnl), pnl, entry_mcap, signal.current_mcap, entry_size_sol, cost_basis_sol)
            } else {
                Decision {
                    action: ExitAction::Partial,
                    frac_of_current: frac,
                    frac_of_original: frac * pos.remaining_fraction,
                    rung_index: None,
                    reason: format!("insider distribution partial (sell {:.0}%, {:.2} SOL dumped, pnl {:.1}%)", frac * 100.0, leader_sell, pnl),
                    pnl, entry_mcap, current_mcap: signal.current_mcap, entry_size_sol, cost_basis_sol,
                }
            }
        }
        // 0c. SLOW RUG: insiders bled out cumulatively over the whole hold without any
        // single dump big enough to trip 0b. Once total tracked-wallet selling crosses
        // the threshold, the smart money has left — exit before the grind-down finishes.
        else if cfg.slow_rug_sol > 0.0 && insider_cum >= cfg.slow_rug_sol {
            pos.selling = true;
            Decision::full(pos.remaining_fraction, format!("slow-rug distribution ({:.2} SOL cumulatively sold by insiders, pnl {:.1}%)", insider_cum, pnl), pnl, entry_mcap, signal.current_mcap, entry_size_sol, cost_basis_sol)
        }
        // 1. Hard stop.
        else if pnl <= cfg.hard_stop_pct {
            pos.selling = true;
            Decision::full(pos.remaining_fraction, format!("hard stop {:.1}%", pnl), pnl, entry_mcap, signal.current_mcap, entry_size_sol, cost_basis_sol)
        }
        // 1b. Stagnation time-stop: a position that never reached the min PnL within
        // the time window is a dud — cut it before it bleeds into a hard stop and free
        // the slot. Uses peak (not current) so a spiked-then-faded token is left to the
        // trailing stop instead.
        else if cfg.stagnation_secs > 0
            && now.saturating_sub(pos.entry_ts) >= cfg.stagnation_secs
            && pos.peak_pnl < cfg.stagnation_min_pnl
        {
            pos.selling = true;
            Decision::full(pos.remaining_fraction, format!("stagnation (peak {:.0}% after {}s)", pos.peak_pnl, now.saturating_sub(pos.entry_ts)), pnl, entry_mcap, signal.current_mcap, entry_size_sol, cost_basis_sol)
        }
        // 2. Trailing stop: once a position has run past the activation threshold,
        // ride it and exit only when it gives back a chunk of its PEAK gain. This is
        // what lets winners keep pumping instead of being dumped on the first wobble.
        else if cfg.trail_enabled
            && pos.peak_pnl >= cfg.trail_activate_pct
            && pnl <= pos.peak_pnl * (1.0 - cfg.trail_giveback_frac)
        {
            pos.selling = true;
            Decision::full(pos.remaining_fraction, format!("trailing stop (peak {:.0}%, gave back to {:.0}%)", pos.peak_pnl, pnl), pnl, entry_mcap, signal.current_mcap, entry_size_sol, cost_basis_sol)
        }
        // 3. Momentum collapse / dump -> cut remaining. SKIPPED for armed winners:
        // once the trailing stop is active, a brief sell spike during a pump no longer
        // dumps the bag — the trailing stop governs the exit instead.
        else if !(cfg.trail_enabled && pos.peak_pnl >= cfg.trail_activate_pct)
            && (signal.score < cfg.collapse_score || signal.sell_volume_short > signal.buy_volume_short * 1.5)
        {
            pos.selling = true;
            Decision::full(pos.remaining_fraction, format!("momentum collapse (score {:.1}, pnl {:.1}%)", signal.score, pnl), pnl, entry_mcap, signal.current_mcap, entry_size_sol, cost_basis_sol)
        }
        // 3. Scale-out ladder: take the configured fraction of the ORIGINAL at the
        // next uncleared rung (default 20% each). NOTE: the rung is NOT marked hit
        // here — that happens only after the sell confirms, so a failed sell never
        // silently consumes a rung.
        else {
            let mut chosen: Option<(usize, f64, f64)> = None;
            for (i, target) in cfg.scale_out_targets.iter().enumerate() {
                if !pos.rungs_hit[i] && pnl >= *target {
                    let frac_orig = cfg.scale_out_fractions.get(i).copied().unwrap_or(0.20);
                    if frac_orig <= 0.0 {
                        // A zero-size rung: mark it consumed-on-confirm with no sell
                        // would be wrong; just skip it and let the next rung apply.
                        continue;
                    }
                    // Sell frac_orig of the original = (frac_orig / remaining_fraction)
                    // of the CURRENT balance.
                    let frac_of_current = (frac_orig / pos.remaining_fraction).min(1.0);
                    chosen = Some((i, frac_of_current, frac_orig));
                    break;
                }
            }
            match chosen {
                Some((i, frac_of_current, frac_orig)) => {
                    pos.selling = true; // lock the position while the sell is in flight
                    let tgt = cfg.scale_out_targets[i];
                    Decision {
                        action: ExitAction::Partial,
                        frac_of_current,
                        frac_of_original: frac_orig,
                        rung_index: Some(i),
                        reason: format!("scale-out +{:.0}% (sell {:.0}%, pnl {:.1}%)", tgt, frac_orig * 100.0, pnl),
                        pnl,
                        entry_mcap,
                        current_mcap: signal.current_mcap,
                        entry_size_sol,
                        cost_basis_sol,
                    }
                }
                None => Decision::none(),
            }
        }
    };

    let (event, frac_of_current, is_full) = match decision.action {
        ExitAction::None => return,
        ExitAction::Partial => ("SELL_PARTIAL", decision.frac_of_current, false),
        ExitAction::Full => ("SELL_FULL", 1.0, true),
    };

    // Mark-to-curve proceeds (gross market value of the chunk) and realized PnL
    // against the true cost basis (which includes the buy-leg cost).
    let price_ratio = if decision.entry_mcap > 0.0 { decision.current_mcap / decision.entry_mcap } else { 1.0 };
    let est_proceeds = decision.frac_of_original * decision.entry_size_sol * price_ratio;
    let cost_basis = decision.frac_of_original * decision.cost_basis_sol;
    let est_realized_pnl = est_proceeds - cost_basis;

    // Apply the bookkeeping that should only happen once a sell actually succeeds:
    // mark the scale-out rung hit and reduce the remaining fraction, or finalize
    // a full exit. On a partial, also release the `selling` lock.
    // Returns the position's TOTAL realized PnL (all sells) when this is the closing
    // full exit, so the caller can classify the whole position as a win or loss.
    let commit_sell = |succeeded: bool, chunk_realized: f64| -> Option<f64> {
        if !succeeded {
            if let Some(mut p) = POSITIONS.get_mut(&mint) {
                p.selling = false;
                p.sell_attempts += 1;
                // Unsellable-token guard: after too many failed sells the token is
                // likely illiquid/honeypot. Quarantine it (halt auto-sell) and alert,
                // instead of spinning forever every evaluation tick.
                if cfg.max_sell_retries > 0 && p.sell_attempts >= cfg.max_sell_retries && !p.quarantined {
                    p.quarantined = true;
                    logger.log(format!(
                        "🚨 QUARANTINE {} — sell failed {} times (likely illiquid/honeypot). Auto-sell halted; check your wallet and exit manually.",
                        mint, p.sell_attempts,
                    ).red().bold().to_string());
                }
            }
            save_positions();
            return None;
        }
        if is_full {
            // Whole-position realized = prior scale-outs + this final chunk.
            let total = POSITIONS.get(&mint).map(|p| p.realized_so_far + chunk_realized).unwrap_or(chunk_realized);
            finalize_exit(&mint); // persists via save_positions()
            Some(total)
        } else {
            if let Some(mut p) = POSITIONS.get_mut(&mint) {
                if let Some(i) = decision.rung_index {
                    if i < p.rungs_hit.len() {
                        p.rungs_hit[i] = true;
                    }
                }
                p.remaining_fraction = (p.remaining_fraction - decision.frac_of_original).max(0.0);
                p.selling = false;
                p.sell_attempts = 0; // a successful sell clears the failure streak
                p.realized_so_far += chunk_realized; // accumulate for the win/loss tally
            }
            save_positions(); // persist the reduced position (rung hit + fraction)
            None
        }
    };

    // Count a closed position as a win/loss by its TOTAL realized PnL (honest, whole-
    // position win rate), and run the circuit breaker on that total.
    let finalize_win_loss = |pos_total: f64, cfg: &MomentumConfig, logger: &Logger| {
        if pos_total >= 0.0 {
            SESSION_WINS.fetch_add(1, Ordering::SeqCst);
        } else {
            SESSION_LOSSES.fetch_add(1, Ordering::SeqCst);
        }
        record_full_exit(pos_total, cfg, logger);
    };

    if cfg.dry_run {
        // Paper trade: assume the chunk fills at current mcap, minus a simulated
        // sell-side cost (fees + tip + slippage). Truth in dry-run; no reconciliation.
        let sim_proceeds = est_proceeds * (1.0 - cfg.sim_cost_fraction);
        let sim_realized = sim_proceeds - cost_basis;
        log_trade_event(&TradeLogEvent {
            event,
            mint: &mint,
            reason: &decision.reason,
            score: signal.score,
            entry_mcap: decision.entry_mcap,
            current_mcap: decision.current_mcap,
            pnl_pct: decision.pnl,
            fraction_of_original: decision.frac_of_original,
            est_sol: sim_proceeds,
            est_realized_pnl_sol: sim_realized,
            signature: "DRY_RUN",
        });
        logger.log(format!(
            "📝 [DRY] {} {:.0}% of {} ({}) | sim proceeds {:.4} SOL | sim PnL {:+.4} SOL",
            event, decision.frac_of_original * 100.0, mint, decision.reason, sim_proceeds, sim_realized,
        ).yellow().to_string());
        let fp = if is_full { POSITIONS.get(&mint).map(|p| (p.creator.clone(), token_bands(&p.signal_type, p.entry_conv, p.entry_mcap, p.entry_base_score))) } else { None };
        let pos_total = commit_sell(true, sim_realized);
        record_kol_pnl(&pos_kol, sim_realized, is_full, sim_realized);
        if is_full {
            let total = pos_total.unwrap_or(sim_realized);
            if let Some((c, b)) = &fp { learn_avoidance(c, b, total); }
            finalize_win_loss(total, &cfg, &logger);
        }
        return;
    }

    let recon_app = app_state.clone();
    match momentum_sell(&mint, frac_of_current, app_state, &cfg, &decision.reason, &logger).await {
        Ok(sig) => {
            log_trade_event(&TradeLogEvent {
                event,
                mint: &mint,
                reason: &decision.reason,
                score: signal.score,
                entry_mcap: decision.entry_mcap,
                current_mcap: decision.current_mcap,
                pnl_pct: decision.pnl,
                fraction_of_original: decision.frac_of_original,
                est_sol: est_proceeds,
                est_realized_pnl_sol: est_realized_pnl,
                signature: &sig,
            });
            let fp = if is_full { POSITIONS.get(&mint).map(|p| (p.creator.clone(), token_bands(&p.signal_type, p.entry_conv, p.entry_mcap, p.entry_base_score))) } else { None };
            let pos_total = commit_sell(true, est_realized_pnl);
            record_kol_pnl(&pos_kol, est_realized_pnl, is_full, est_realized_pnl);
            if is_full {
                let total = pos_total.unwrap_or(est_realized_pnl);
                if let Some((c, b)) = &fp { learn_avoidance(c, b, total); }
                finalize_win_loss(total, &cfg, &logger);
            }
            spawn_reconcile(recon_app, mint.clone(), sig, cost_basis,
                est_realized_pnl, signal.score, decision.entry_mcap, decision.current_mcap, decision.pnl, decision.frac_of_original, decision.reason.clone());
        }
        Err(e) => {
            logger.log(format!("Sell error {}: {}", mint, e).red().to_string());
            commit_sell(false, 0.0);
        }
    }
}

/// Fetch the real on-chain SOL proceeds for a sell in the background and append a
/// `SELL_ACTUAL` reconciliation row, nudging the running realized-PnL tally from
/// the mark-to-curve estimate toward the true figure.
#[allow(clippy::too_many_arguments)]
fn spawn_reconcile(
    app_state: Arc<AppState>,
    mint: String,
    signature: String,
    cost_basis: f64,
    est_realized: f64,
    score: f64,
    entry_mcap: f64,
    current_mcap: f64,
    pnl: f64,
    frac_of_original: f64,
    reason: String,
) {
    if signature.is_empty() {
        return;
    }
    TX_SENT.fetch_add(1, Ordering::Relaxed);
    tokio::spawn(async move {
        if let Some(proceeds) = fetch_actual_sol_delta(&app_state, &signature).await {
            TX_LANDED.fetch_add(1, Ordering::Relaxed);
            let actual_realized = proceeds - cost_basis;
            if let Ok(mut t) = PNL_TALLY.lock() {
                t.0 += actual_realized - est_realized;
            }
            log_trade_event(&TradeLogEvent {
                event: "SELL_ACTUAL",
                mint: &mint,
                reason: &reason,
                score,
                entry_mcap,
                current_mcap,
                pnl_pct: pnl,
                fraction_of_original: frac_of_original,
                est_sol: proceeds,
                est_realized_pnl_sol: actual_realized,
                signature: &signature,
            });
        }
    });
}

/// What the exit policy decided to do for a position this tick.
struct Decision {
    action: ExitAction,
    /// Fraction of the *current* balance to sell.
    frac_of_current: f64,
    /// Fraction of the *original* position this represents (for PnL math).
    frac_of_original: f64,
    /// For a partial scale-out, which rung this clears (applied only after a
    /// confirmed sell, so a failed sell doesn't consume the rung).
    rung_index: Option<usize>,
    reason: String,
    pnl: f64,
    entry_mcap: f64,
    current_mcap: f64,
    /// SOL value bought at entry (for mark-to-curve proceeds estimate).
    entry_size_sol: f64,
    /// Cost basis (SOL) of the whole original position, including entry costs.
    cost_basis_sol: f64,
}

impl Decision {
    fn none() -> Self {
        Decision {
            action: ExitAction::None,
            frac_of_current: 0.0,
            frac_of_original: 0.0,
            rung_index: None,
            reason: String::new(),
            pnl: 0.0,
            entry_mcap: 0.0,
            current_mcap: 0.0,
            entry_size_sol: 0.0,
            cost_basis_sol: 0.0,
        }
    }

    fn full(remaining_fraction: f64, reason: String, pnl: f64, entry_mcap: f64, current_mcap: f64, entry_size_sol: f64, cost_basis_sol: f64) -> Self {
        Decision {
            action: ExitAction::Full,
            frac_of_current: 1.0,
            frac_of_original: remaining_fraction,
            rung_index: None,
            reason,
            pnl,
            entry_mcap,
            current_mcap,
            entry_size_sol,
            cost_basis_sol,
        }
    }
}

enum ExitAction {
    None,
    Partial,
    Full,
}

/// Background loop that evaluates every open position on a fixed cadence.
async fn run_exit_monitor(app_state: Arc<AppState>, cfg: Arc<MomentumConfig>, logger: Logger) {
    let mut interval = time::interval(Duration::from_secs(3));
    let mut ticks: u64 = 0;
    while MOMENTUM_RUNNING.load(Ordering::SeqCst) {
        interval.tick().await;
        // Roll the trading day even when no trades happen, so a halted breaker
        // auto-resumes at the day boundary.
        maybe_roll_day(&cfg, &logger);
        let mints: Vec<String> = POSITIONS.iter().map(|e| e.key().clone()).collect();
        for mint in mints {
            evaluate_position(mint, app_state.clone(), cfg.clone(), logger.clone()).await;
        }

        // Refresh the live dashboard snapshot every tick (~3s).
        write_status_snapshot(&cfg);

        // Live PnL summary + stale-state cleanup roughly every 60s.
        ticks += 1;
        if ticks % 20 == 0 {
            let (realized, buys, sells) = PNL_TALLY.lock().map(|g| *g).unwrap_or((0.0, 0, 0));
            let status = if trading_halted() { " | 🛑 HALTED" } else { "" };
            logger.log(format!(
                "📊 PnL | session {:+.4} | today {:+.4} SOL | {} buys / {} sells | {} open | streak {} | tracking {}{}",
                realized, daily_realized(), buys, sells, POSITIONS.len(), CONSECUTIVE_LOSSES.load(Ordering::SeqCst), TOKEN_STATE.len(), status,
            ).cyan().bold().to_string());

            // Drop rolling state for tokens we don't hold and haven't seen trade recently,
            // so memory doesn't grow unbounded over a long session.
            let now = now_secs();
            let stale_after = cfg.medium_window_secs.saturating_mul(4).max(300);
            TOKEN_STATE.retain(|mint, state| {
                if POSITIONS.contains_key(mint) {
                    return true;
                }
                match state.ticks.back() {
                    Some(t) => now.saturating_sub(t.ts) < stale_after,
                    None => false,
                }
            });
            KOL_HOT.retain(|_, v| v.0 > now);
        }
    }
}

// ---------------------------------------------------------------------------
// Public entrypoint
// ---------------------------------------------------------------------------

async fn send_heartbeat_ping(
    subscribe_tx: &Arc<tokio::sync::Mutex<impl futures_util::Sink<SubscribeRequest, Error = impl std::fmt::Debug> + Unpin>>,
) -> Result<(), String> {
    let ping = SubscribeRequest {
        ping: Some(SubscribeRequestPing { id: 0 }),
        ..Default::default()
    };
    let mut tx = subscribe_tx.lock().await;
    tx.send(ping).await.map_err(|e| format!("ping failed: {:?}", e))
}

/// Shared startup for both feeds: go-live gate, circuit-breaker/day init,
/// reputation load, and the background tasks (exit monitor, attribution, GMGN).
/// Returns the app_state + sniper handles, or an error if the go-live gate blocks.
/// Startup security checks that run BEFORE any live trading. Cheap insurance against
/// the two scariest operational mistakes: (1) auto-signing on your main wallet, and
/// (2) a world-readable `.env` leaking the private key. Dry-run skips the balance
/// tripwire (no real funds at risk) but still warns on file permissions.
async fn security_preflight(cfg: &Arc<MomentumConfig>, app_state: &Arc<AppState>, logger: &Logger) -> Result<(), String> {
    // --- .env permission check: warn if the secrets file is group/world readable. ---
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        if let Ok(meta) = std::fs::metadata(".env") {
            let mode = meta.permissions().mode() & 0o077;
            if mode != 0 {
                logger.log(format!(
                    "🔓 SECURITY: .env is readable by group/other (mode {:o}). Run `chmod 600 .env` — it holds your private key.",
                    meta.permissions().mode() & 0o777,
                ).yellow().bold().to_string());
            }
        }
    }

    // --- Wallet-balance tripwire: never auto-sign on a wallet holding more than the
    //     configured ceiling. The whole point of a hot wallet is a small blast radius. ---
    if !cfg.dry_run && cfg.max_wallet_sol > 0.0 {
        if let Ok(pubkey) = app_state.wallet.try_pubkey() {
            match app_state.rpc_nonblocking_client.get_balance(&pubkey).await {
                Ok(lamports) => {
                    let bal = lamports as f64 / LAMPORTS_PER_SOL;
                    if bal > cfg.max_wallet_sol {
                        let msg = format!(
                            "Refusing to start LIVE: wallet holds {:.3} SOL, above the safety ceiling of {:.3} (MOMENTUM_MAX_WALLET_SOL). \
                             This looks like the wrong (main) wallet. Use a DEDICATED hot wallet with a small balance, or raise the ceiling if intentional.",
                            bal, cfg.max_wallet_sol,
                        );
                        logger.log(format!("⛔ {}", msg).red().bold().to_string());
                        return Err(msg);
                    }
                    logger.log(format!("🔐 Security preflight OK — hot wallet balance {:.3} SOL within ceiling {:.3}", bal, cfg.max_wallet_sol).green().to_string());
                }
                Err(e) => {
                    // Can't verify balance — fail closed in live mode rather than trade blind.
                    let msg = format!("Refusing to start LIVE: could not verify wallet balance for the safety tripwire ({}). Check RPC.", e);
                    logger.log(format!("⛔ {}", msg).red().bold().to_string());
                    return Err(msg);
                }
            }
        }
    }
    Ok(())
}

async fn momentum_startup(
    cfg: &Arc<MomentumConfig>,
    sniper: SniperConfig,
    logger: &Logger,
) -> Result<(Arc<AppState>, Arc<SniperConfig>), String> {
    cfg.log(logger);

    // Go-live gate: refuse to trade real money unless explicitly acknowledged.
    if !cfg.dry_run && !cfg.live_confirmed {
        let msg = "Refusing to start LIVE: set MOMENTUM_LIVE_CONFIRM=true to trade real money, \
                   or MOMENTUM_DRY_RUN=true to paper-trade. Validate in dry run + analyzer/A-B first.";
        logger.log(format!("⛔ {}", msg).red().bold().to_string());
        return Err(msg.to_string());
    }

    TRADING_HALTED.store(false, Ordering::SeqCst);
    MARGIN_CALLED.store(false, Ordering::SeqCst);
    SESSION_WINS.store(0, Ordering::SeqCst);
    SESSION_LOSSES.store(0, Ordering::SeqCst);
    WATCHLIST.clear();
    CONSECUTIVE_LOSSES.store(0, Ordering::SeqCst);
    DAY_INDEX.store(current_day(cfg), Ordering::SeqCst);
    if let Ok(mut base) = DAY_START_REALIZED.lock() {
        *base = 0.0;
    }
    MOMENTUM_RUNNING.store(true, Ordering::SeqCst);

    // Crash recovery: adopt any open positions a previous run left on disk so the
    // exit monitor resumes managing them. Without this, a crash/restart silently
    // abandons money already in the market (no stop-loss, no scale-out running).
    if let Ok(mut p) = POSITIONS_PATH.lock() {
        *p = cfg.positions_file.clone();
    }
    if !cfg.positions_file.is_empty() {
        let recovered = load_positions(&cfg.positions_file);
        if !recovered.is_empty() {
            logger.log(format!(
                "♻️  Recovered {} open position(s) from {} — resuming exit management: {}",
                recovered.len(), cfg.positions_file,
                recovered.iter().map(|m| m.chars().take(6).collect::<String>()).collect::<Vec<_>>().join(", "),
            ).yellow().bold().to_string());
            if !cfg.dry_run {
                logger.log("⚠️  LIVE recovery: verify these are still held on-chain. A position already sold/illiquid will fail its next sell harmlessly, but check your wallet.".yellow().to_string());
            }
        }
    }

    let app_state = Arc::new(sniper.app_state.clone());
    let sniper = Arc::new(sniper);

    // Security preflight: hot-wallet balance tripwire + .env permission check. Fails
    // closed in live mode so the bot can never auto-sign on the wrong (main) wallet.
    security_preflight(cfg, &app_state, logger).await?;

    if cfg.smart_money_enabled {
        load_wallet_rep(&cfg.wallet_rep_file);
        let proven = WALLET_REP
            .iter()
            .filter(|e| wallet_is_proven(e.value(), &cfg))
            .count();
        logger.log(format!(
            "🧠 Loaded reputation for {} wallets from {} — {} already proven (rep >= {:.2}, >= {} samples) and will be followed",
            WALLET_REP.len(), cfg.wallet_rep_file, proven, cfg.alpha_rep_min, cfg.alpha_min_samples,
        ).cyan().to_string());
    }

    // Learned avoidance memory (creators + patterns that have lost) — compounds across runs.
    if cfg.creator_avoid || cfg.pattern_avoid {
        load_avoidance(&cfg.avoidance_file);
        let bad = CREATOR_REP.iter().filter(|e| { let (p, n) = *e.value(); n >= cfg.creator_min_trades && p <= cfg.creator_avoid_pnl }).count();
        logger.log(format!(
            "🧠 Loaded avoidance memory: {} creators / {} patterns — {} creators currently blacklisted",
            CREATOR_REP.len(), PATTERN_EV.len(), bad,
        ).cyan().to_string());
    }

    if cfg.kol_enabled {
        let n = load_kol_wallets(&cfg.kol_file);
        if n == 0 {
            logger.log(format!("⚠️  KOL tracking on but no wallets loaded from {} — add wallets (one per line)", cfg.kol_file).yellow().to_string());
        } else {
            logger.log(format!("⭐ Loaded {} KOL wallets from {} | mode: {}", n, cfg.kol_file,
                if cfg.kol_require { "PURE-KOL (only enter KOL buys)" } else { "boost (KOL buys prioritized)" }).cyan().bold().to_string());
        }

        // Hot-reload the KOL list so a cron'd Dune API fetch refreshes it live.
        if cfg.kol_reload_secs > 0 {
            let file = cfg.kol_file.clone();
            let secs = cfg.kol_reload_secs;
            let logger2 = logger.clone();
            tokio::spawn(async move {
                let mut interval = time::interval(Duration::from_secs(secs.max(10)));
                interval.tick().await; // skip immediate tick (already loaded)
                while MOMENTUM_RUNNING.load(Ordering::SeqCst) {
                    interval.tick().await;
                    let n = load_kol_wallets(&file);
                    logger2.log(format!("⭐ KOL list reloaded: {} wallets", n).cyan().to_string());
                }
            });
        }
    }

    {
        let app_state = app_state.clone();
        let cfg = cfg.clone();
        let logger = logger.clone();
        tokio::spawn(async move { run_exit_monitor(app_state, cfg, logger).await });
    }

    if cfg.smart_money_enabled {
        let cfg = cfg.clone();
        tokio::spawn(async move { run_attribution(cfg).await });
    }

    // Decision/learning log + post-detection outcome tracking.
    if !cfg.decision_log_file.is_empty() {
        if let Ok(mut p) = DECISION_LOG_PATH.lock() {
            *p = cfg.decision_log_file.clone();
        }
        DECISION_LOGGED.clear();
        // Reload the pending outcome map from the previous run so the 2h labeling
        // clock survives restarts (without this, frequent restarts keep labeled
        // outcomes permanently at zero and the learning loop never gets data).
        OUTCOMES.clear();
        let restored = load_outcomes(&outcomes_pending_path(&cfg));
        logger.log(format!(
            "📒 Decision log ON: {}.csv / .jsonl (every evaluation) + {}_outcomes.csv (2h labeled outcomes) | {} pending outcomes restored",
            cfg.decision_log_file, cfg.decision_log_file, restored,
        ).cyan().to_string());
        let cfg = cfg.clone();
        tokio::spawn(async move { run_outcome_tracker(cfg).await });
    }

    {
        let gmgn_cfg = GmgnConfig::from_env();
        let client = GmgnClient::new(gmgn_cfg);
        if client.enabled() {
            let _ = GMGN.set(client);
            logger.log("🛰️  GMGN integration active (rug veto + smart-money/trenches watchlist)".cyan().bold().to_string());
            let cfg = cfg.clone();
            let logger2 = logger.clone();
            tokio::spawn(async move { run_gmgn_pollers(cfg, logger2).await });
        } else {
            logger.log("GMGN integration disabled (set GMGN_ENABLED=true and GMGN_API_KEY)".to_string());
        }
    }

    Ok((app_state, sniper))
}

/// Momentum sniper over a standard RPC **websocket** (blockSubscribe → pump.fun).
/// No Yellowstone gRPC needed — runs on a plain wss endpoint (Chainstack/Helius).
pub async fn start_momentum_ws(sniper: SniperConfig, ws_url: String) -> Result<(), String> {
    let logger = Logger::new("[MOMENTUM-WS] => ".green().bold().to_string());
    let cfg = Arc::new(MomentumConfig::from_env());
    let (_app_state, sniper) = momentum_startup(&cfg, sniper, &logger).await?;

    if ws_url.trim().is_empty() {
        return Err("RPC_WSS is empty — set it to your websocket endpoint for MOMENTUM_FEED=ws".to_string());
    }

    let (tx, mut rx) = tokio::sync::mpsc::channel::<crate::library::ws_feed::FeedItem>(10_000);
    {
        let logger = logger.clone();
        tokio::spawn(async move { crate::library::ws_feed::run(ws_url, tx, logger).await });
    }

    logger.log("🚀 Momentum sniper live (websocket feed) — buying strength.".green().bold().to_string());

    while MOMENTUM_RUNNING.load(Ordering::SeqCst) {
        match rx.recv().await {
            Some((parsed, trader)) => {
                if parsed.mint == WSOL_MINT || parsed.dex_type != DexType::PumpFun {
                    continue;
                }
                let now = now_secs();
                let signal = ingest_trade(&parsed, trader, now, &cfg);
                let cfg2 = cfg.clone();
                let sniper2 = sniper.clone();
                let logger2 = logger.clone();
                tokio::spawn(async move { try_enter(parsed, signal, cfg2, sniper2, logger2).await });
            }
            None => {
                logger.log("Websocket feed channel closed".yellow().to_string());
                break;
            }
        }
    }
    Ok(())
}

/// Start the momentum sniper: stream every pump.fun trade, score momentum, buy
/// strength, and manage exits with the scale-out + collapse policy.
pub async fn start_momentum_monitoring(sniper: SniperConfig) -> Result<(), String> {
    let logger = Logger::new("[MOMENTUM] => ".green().bold().to_string());
    let cfg = Arc::new(MomentumConfig::from_env());
    let (app_state, sniper) = momentum_startup(&cfg, sniper, &logger).await?;
    let _ = &app_state;

    // Connect to Yellowstone gRPC.
    let mut client = GeyserGrpcClient::build_from_shared(sniper.yellowstone_grpc_http.clone())
        .map_err(|e| format!("Failed to build client: {}", e))?
        .x_token::<String>(Some(sniper.yellowstone_grpc_token.clone()))
        .map_err(|e| format!("Failed to set x_token: {}", e))?
        .tls_config(ClientTlsConfig::new().with_native_roots())
        .map_err(|e| format!("Failed to set tls config: {}", e))?
        .connect()
        .await
        .map_err(|e| format!("Failed to connect: {}", e))?;

    let (subscribe_tx, mut stream) = client
        .subscribe()
        .await
        .map_err(|e| format!("Failed to subscribe: {}", e))?;
    let subscribe_tx = Arc::new(tokio::sync::Mutex::new(subscribe_tx));

    // Only pump.fun program transactions.
    let subscription_request = SubscribeRequest {
        transactions: maplit::hashmap! {
            "pumpfun".to_owned() => SubscribeRequestFilterTransactions {
                vote: Some(false),
                failed: Some(false),
                signature: None,
                account_include: vec![PUMP_FUN_PROGRAM.to_string()],
                account_exclude: vec![],
                account_required: Vec::<String>::new(),
            }
        },
        commitment: Some(CommitmentLevel::Processed as i32),
        ..Default::default()
    };

    subscribe_tx
        .lock()
        .await
        .send(subscription_request)
        .await
        .map_err(|e| format!("Failed to send subscribe request: {}", e))?;

    // Heartbeat.
    {
        let subscribe_tx = subscribe_tx.clone();
        tokio::spawn(async move {
            let mut interval = time::interval(Duration::from_secs(30));
            loop {
                interval.tick().await;
                if send_heartbeat_ping(&subscribe_tx).await.is_err() {
                    break;
                }
            }
        });
    }

    logger.log("🚀 Momentum sniper live — streaming pump.fun, buying strength.".green().bold().to_string());

    while MOMENTUM_RUNNING.load(Ordering::SeqCst) {
        match stream.next().await {
            Some(Ok(msg)) => {
                if let Some(UpdateOneof::Transaction(txn)) = &msg.update_oneof {
                    let inner_instructions = match &txn.transaction {
                        Some(txn_info) => match &txn_info.meta {
                            Some(meta) => meta.inner_instructions.clone(),
                            None => vec![],
                        },
                        None => vec![],
                    };
                    if inner_instructions.is_empty() {
                        continue;
                    }
                    let cpi_log_data = inner_instructions
                        .iter()
                        .flat_map(|inner| &inner.instructions)
                        .find(|ix| matches!(ix.data.len(), 368 | 266 | 270 | 146 | 170 | 138))
                        .map(|ix| ix.data.clone());

                    if let Some(data) = cpi_log_data {
                        let txn = txn.clone();
                        let cfg = cfg.clone();
                        let sniper = sniper.clone();
                        let logger = logger.clone();
                        tokio::spawn(async move {
                            if let Some(parsed) = parse_transaction_data(&txn, &data) {
                                if parsed.mint == WSOL_MINT || parsed.dex_type != DexType::PumpFun {
                                    return;
                                }
                                let trader = extract_signer(&txn).unwrap_or_default();
                                let now = now_secs();
                                let signal = ingest_trade(&parsed, trader, now, &cfg);
                                try_enter(parsed, signal, cfg, sniper, logger).await;
                            }
                        });
                    }
                }
            }
            Some(Err(e)) => {
                logger.log(format!("Stream error: {:?}", e).red().to_string());
                break;
            }
            None => {
                logger.log("Stream ended".yellow().to_string());
                break;
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::str::FromStr;

    // ---- helpers ----------------------------------------------------------
    fn ix(prog: &str) -> solana_sdk::instruction::Instruction {
        solana_sdk::instruction::Instruction {
            program_id: solana_sdk::pubkey::Pubkey::from_str(prog).unwrap(),
            accounts: vec![],
            data: vec![],
        }
    }
    const TOKEN_PROG: &str = "TokenkegQfeZyiNwAJbNbGKPFXCWuBvf9Ss623VQ5DA";
    const SYS_PROG: &str = "11111111111111111111111111111111";
    const EVIL_PROG: &str = "Evi1xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx";

    fn tick(ts: u64, is_buy: bool, sol: f64, trader: &str, mcap: f64) -> TradeTick {
        TradeTick { ts, is_buy, sol, trader: trader.to_string(), mcap }
    }
    fn trade_info(creator: Option<&str>) -> TradeInfoFromToken {
        TradeInfoFromToken {
            dex_type: DexType::PumpFun, slot: 0, signature: String::new(), pool_id: String::new(),
            mint: "M".into(), timestamp: 0, is_buy: true, price: 0, is_reverse_when_pump_swap: false,
            coin_creator: creator.map(|s| s.to_string()), sol_change: 0.0, token_change: 0.0,
            liquidity: 0.0, virtual_sol_reserves: 0, virtual_token_reserves: 0,
        }
    }
    fn test_cfg() -> MomentumConfig {
        let mut c = MomentumConfig::from_env();
        c.short_window_secs = 30;
        c.medium_window_secs = 120;
        c.min_buy_volume_sol = 1.0;
        c.target_unique_buyers = 10.0;
        c.target_mcap_growth = 0.5;
        c.max_wallet_concentration = 0.5;
        c.min_buyer_diversity = 0.5;
        c.max_wash_fraction = 0.4;
        c.max_creator_buy_frac = 0.15;
        c
    }
    fn state(ticks: Vec<TradeTick>, creator: Option<&str>) -> TokenMomentum {
        TokenMomentum { ticks: ticks.into_iter().collect(), last_mcap: 1100.0, last_trade_info: trade_info(creator), last_score: 0.0 }
    }

    // ---- anti-tamper program guard ---------------------------------------
    #[test]
    fn guard_accepts_canonical_pumpfun_tx() {
        let ixs = vec![ix(PUMP_FUN_PROGRAM), ix(TOKEN_PROG), ix(SYS_PROG)];
        assert!(guard_swap_instructions(&ixs).is_ok());
    }
    #[test]
    fn guard_rejects_unknown_program() {
        let ixs = vec![ix(PUMP_FUN_PROGRAM), ix(EVIL_PROG)];
        assert!(guard_swap_instructions(&ixs).is_err());
    }
    #[test]
    fn guard_rejects_missing_pumpfun_swap() {
        let ixs = vec![ix(TOKEN_PROG), ix(SYS_PROG)];
        assert!(guard_swap_instructions(&ixs).is_err());
    }
    #[test]
    fn guard_rejects_empty() {
        assert!(guard_swap_instructions(&[]).is_err());
    }

    // ---- outcome labelling -----------------------------------------------
    #[test]
    fn outcome_labels_map_correctly() {
        assert_eq!(outcome_label(0.0, -0.95), "RUG");
        assert_eq!(outcome_label(25.0, 5.0), "20X+");
        assert_eq!(outcome_label(12.0, 3.0), "10X");
        assert_eq!(outcome_label(6.0, 2.0), "5X");
        assert_eq!(outcome_label(1.5, 0.5), "2X");
        assert_eq!(outcome_label(0.1, -0.5), "LOSS");
        assert_eq!(outcome_label(0.3, 0.2), "WIN");
        assert_eq!(outcome_label(0.05, 0.0), "BREAKEVEN");
    }

    // ---- pure math --------------------------------------------------------
    #[test]
    fn clamp01_bounds() {
        assert_eq!(clamp01(-1.0), 0.0);
        assert_eq!(clamp01(0.5), 0.5);
        assert_eq!(clamp01(2.0), 1.0);
    }
    #[test]
    fn mcap_from_reserves_zero_is_none() {
        assert!(mcap_from_reserves(0, 0).is_none());
        assert!(mcap_from_reserves(30_000_000_000, 1_000_000_000_000_000).is_some());
    }

    // ---- core scoring: the anti-fake creator self-buy discount -----------
    #[test]
    fn creator_self_buy_pump_scores_zero() {
        let cfg = test_cfg();
        let now = 1000;
        // A "pump" that is entirely the creator buying its own token.
        let ticks: Vec<_> = (0..10).map(|i| tick(990, true, 1.0, "CREATOR", 1000.0 + i as f64 * 10.0)).collect();
        let sig = score_token(&state(ticks, Some("CREATOR")), &cfg, now);
        // genuine_factor collapses to 0 -> the fake pump cannot clear the gate.
        assert_eq!(sig.score, 0.0, "creator-only pump must score 0");
        assert_eq!(sig.genuine_factor, 0.0);
    }
    #[test]
    fn organic_pump_outscores_creator_pump() {
        let cfg = test_cfg();
        let now = 1000;
        let creator_ticks: Vec<_> = (0..10).map(|_| tick(990, true, 1.0, "CREATOR", 1100.0)).collect();
        let organic_ticks: Vec<_> = (0..10).map(|i| tick(990, true, 1.0, &format!("w{i}"), 1100.0)).collect();
        let creator_score = score_token(&state(creator_ticks, Some("CREATOR")), &cfg, now).score;
        let organic_score = score_token(&state(organic_ticks, Some("CREATOR")), &cfg, now).score;
        assert!(organic_score > creator_score, "organic {organic_score} should beat fake {creator_score}");
        assert!(organic_score > 0.0);
    }
    #[test]
    fn heavy_selling_suppresses_score() {
        let cfg = test_cfg();
        let now = 1000;
        let mut ticks: Vec<_> = (0..10).map(|i| tick(990, true, 1.0, &format!("w{i}"), 1100.0)).collect();
        let buys_only = score_token(&state(ticks.clone(), Some("C")), &cfg, now).score;
        // Add big sells (> 1.5x buys) -> dump_factor cuts the score hard.
        for i in 0..10 { ticks.push(tick(991, false, 3.0, &format!("s{i}"), 1100.0)); }
        let with_sells = score_token(&state(ticks, Some("C")), &cfg, now).score;
        assert!(with_sells < buys_only, "heavy selling must suppress: {with_sells} !< {buys_only}");
    }

    // ---- slow-rug cumulative accumulator (no double counting) ------------
    #[test]
    fn tracked_sell_since_watermark_no_double_count() {
        let mint = "test_mint_slowrug_unit";
        let mut tracked = HashSet::new();
        tracked.insert("insider".to_string());
        let ticks = vec![
            tick(100, false, 0.5, "insider", 1000.0),
            tick(110, false, 0.7, "insider", 1000.0),
            tick(120, false, 0.3, "other", 1000.0),   // not tracked
            tick(130, true, 2.0, "insider", 1000.0),   // a buy, not a sell
        ];
        TOKEN_STATE.insert(mint.to_string(), state(ticks, Some("creator")));
        // From the start: count both insider sells (0.5 + 0.7), watermark advances to 130.
        let (vol, seen) = tracked_wallet_sell_since(mint, &tracked, 0);
        assert!((vol - 1.2).abs() < 1e-9, "got {vol}");
        assert_eq!(seen, 130);
        // Re-running from the watermark counts nothing new (no double count).
        let (vol2, _) = tracked_wallet_sell_since(mint, &tracked, seen);
        assert_eq!(vol2, 0.0);
        TOKEN_STATE.remove(mint);
    }
}
