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

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------

#[derive(Clone)]
pub struct MomentumConfig {
    pub position_size_sol: f64,
    pub max_positions: usize,
    pub entry_score: f64,
    pub collapse_score: f64,
    pub hard_stop_pct: f64,
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
    pub scale_out_targets: Vec<f64>,
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

    // ---- Edge: insider / leader-dump exit ----
    /// Exit immediately when the creator or top early buyers start distributing.
    pub leader_dump_exit_enabled: bool,
    /// How many top early buyers (by volume) to track per position, plus the creator.
    pub leader_track_top_n: usize,
    /// If tracked wallets sell at least this many SOL in the short window, exit.
    pub leader_dump_sol: f64,

    // ---- Edge: conviction-based sizing ----
    /// Scale position size up with the entry score (higher conviction = bigger size).
    pub conviction_sizing: bool,
    /// Maximum size multiple at very high scores.
    pub conviction_max_mult: f64,

    // ---- Risk controls ----
    /// Hard cap on total SOL deployed across all open positions. New entries are
    /// blocked (or trimmed) so concurrent + conviction sizing can't overspend.
    pub max_deployed_sol: f64,
    /// Estimated entry-leg cost (fees + tip + slippage) as a fraction of size,
    /// folded into the cost basis so realized PnL isn't optimistic about the buy.
    pub buy_cost_fraction: f64,
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
    /// Transaction landing route: "zeroslot" | "jito" | "multi" (jito+rpc broadcast).
    pub landing: String,

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

        Self {
            position_size_sol: env_f64("MOMENTUM_POSITION_SIZE_SOL", 0.2),
            max_positions: env_usize("MOMENTUM_MAX_POSITIONS", 5),
            entry_score: env_f64("MOMENTUM_ENTRY_SCORE", 65.0),
            collapse_score: env_f64("MOMENTUM_COLLAPSE_SCORE", 35.0),
            hard_stop_pct: env_f64("MOMENTUM_HARD_STOP_PCT", -35.0),
            short_window_secs: env_u64("MOMENTUM_SHORT_WINDOW_SECS", 30),
            medium_window_secs: env_u64("MOMENTUM_MEDIUM_WINDOW_SECS", 120),
            min_buy_volume_sol: env_f64("MOMENTUM_MIN_BUY_VOLUME_SOL", 2.0),
            target_unique_buyers: env_f64("MOMENTUM_TARGET_UNIQUE_BUYERS", 10.0),
            target_mcap_growth: env_f64("MOMENTUM_TARGET_MCAP_GROWTH", 0.30),
            max_wallet_concentration: env_f64("MOMENTUM_MAX_WALLET_CONCENTRATION", 0.50),
            min_buyer_diversity: env_f64("MOMENTUM_MIN_BUYER_DIVERSITY", 0.35),
            max_wash_fraction: env_f64("MOMENTUM_MAX_WASH_FRACTION", 0.40),
            scale_out_targets: if scale_out_targets.is_empty() {
                vec![100.0, 200.0, 300.0, 400.0]
            } else {
                scale_out_targets
            },
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

            leader_dump_exit_enabled: std::env::var("MOMENTUM_LEADER_DUMP_EXIT")
                .map(|v| v.to_lowercase() != "false")
                .unwrap_or(true),
            leader_track_top_n: env_usize("MOMENTUM_LEADER_TRACK_TOP_N", 5),
            leader_dump_sol: env_f64("MOMENTUM_LEADER_DUMP_SOL", 1.0),

            conviction_sizing: std::env::var("MOMENTUM_CONVICTION_SIZING")
                .map(|v| v.to_lowercase() == "true")
                .unwrap_or(false),
            conviction_max_mult: env_f64("MOMENTUM_CONVICTION_MAX_MULT", 2.0),

            max_deployed_sol: env_f64("MOMENTUM_MAX_DEPLOYED_SOL", 1.0),
            buy_cost_fraction: env_f64("MOMENTUM_BUY_COST_FRACTION", 0.015),
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
            landing: std::env::var("MOMENTUM_LANDING").unwrap_or_else(|_| "zeroslot".to_string()).to_lowercase(),

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
        logger.log(format!("Entry score >= {} | collapse < {} | hard stop {}%", self.entry_score, self.collapse_score, self.hard_stop_pct));
        logger.log(format!("Windows: short {}s / baseline {}s", self.short_window_secs, self.medium_window_secs));
        logger.log(format!(
            "Targets: min buy vol {} SOL, {} unique buyers, {:.0}% mcap growth, max wallet concentration {:.0}%",
            self.min_buy_volume_sol, self.target_unique_buyers, self.target_mcap_growth * 100.0, self.max_wallet_concentration * 100.0,
        ));
        logger.log(format!(
            "Anti-fake: min buyer diversity {:.2}, max wash fraction {:.2}",
            self.min_buyer_diversity, self.max_wash_fraction,
        ));
        logger.log(format!("Scale-out rungs (20% each): {:?}% PnL, then 20% runner", self.scale_out_targets));
        logger.log(format!(
            "Edge: smart-money {} (boost <= {:.0} pts, >= {} distinct) | leader-dump exit {} (>= {} SOL) | conviction sizing {} (<= {:.1}x)",
            if self.smart_money_enabled { "on" } else { "off" }, self.smart_money_boost_max, self.smart_money_min_distinct,
            if self.leader_dump_exit_enabled { "on" } else { "off" }, self.leader_dump_sol,
            if self.conviction_sizing { "on" } else { "off" }, self.conviction_max_mult,
        ));
        logger.log(format!(
            "Risk: max deployed {:.3} SOL | entry-cost basis +{:.1}% | landing: {}",
            self.max_deployed_sol, self.buy_cost_fraction * 100.0, self.landing,
        ));
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
}

lazy_static! {
    static ref TOKEN_STATE: DashMap<String, TokenMomentum> = DashMap::new();
    static ref POSITIONS: DashMap<String, MomentumPosition> = DashMap::new();
    static ref RECENTLY_EXITED: DashMap<String, u64> = DashMap::new();
    static ref IN_FLIGHT_BUYS: AtomicUsize = AtomicUsize::new(0);
    static ref MOMENTUM_RUNNING: AtomicBool = AtomicBool::new(true);
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

/// Reputation for a wallet: an EMA of the clamped forward returns of tokens it
/// bought. Positive means its buys tend to precede pumps.
#[derive(Clone, Default)]
struct WalletRep {
    score: f64,
    samples: u32,
    /// When the reputation was last updated (for time decay).
    last_update: u64,
    /// Last token graded, to dampen reputation farmed by buying one token repeatedly.
    last_mint: String,
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
            r.samples += 1;
            r.last_update = now;
            r.last_mint = a.mint.clone();
        }

        // Persist reputation roughly every 60s so the edge compounds across runs.
        since_save += 5;
        if since_save >= 60 {
            since_save = 0;
            save_wallet_rep(&cfg.wallet_rep_file);
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
                WALLET_REP.insert(w.to_string(), WalletRep { score, samples, last_update, last_mint });
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
    let _ = writeln!(file, "wallet,score,samples,last_update,last_mint");
    for e in WALLET_REP.iter() {
        let r = e.value();
        let _ = writeln!(file, "{},{:.6},{},{},{}", e.key(), r.score, r.samples, r.last_update, r.last_mint);
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

    // Per-KOL leaderboard (best realized PnL first) — keep the green, hunt alts.
    let mut kol_board: Vec<serde_json::Value> = KOL_PNL.iter().map(|e| {
        let (realized, wins, losses) = *e.value();
        serde_json::json!({ "kol": e.key(), "realized_sol": realized, "wins": wins, "losses": losses })
    }).collect();
    kol_board.sort_by(|a, b| b["realized_sol"].as_f64().unwrap_or(0.0).partial_cmp(&a["realized_sol"].as_f64().unwrap_or(0.0)).unwrap_or(std::cmp::Ordering::Equal));

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
        },
        "capital": { "deployed_sol": deployed_sol(), "max_deployed_sol": cfg.max_deployed_sol },
        "counts": {
            "open_positions": POSITIONS.len(),
            "max_positions": cfg.max_positions,
            "tokens_tracked": TOKEN_STATE.len(),
            "kol_wallets": KOL_WALLETS.len(),
            "kol_hot": KOL_HOT.len(),
            "gmgn_watchlist": GMGN_WATCHLIST.len(),
        },
        "config": {
            "position_size_sol": cfg.position_size_sol,
            "entry_score": cfg.entry_score,
            "hard_stop_pct": cfg.hard_stop_pct,
            "kol_enabled": cfg.kol_enabled,
            "kol_require": cfg.kol_require,
            "daily_loss_limit_sol": cfg.daily_loss_limit_sol,
        },
        "positions": positions,
        "feed": feed,
        "kol_leaderboard": kol_board,
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
    let genuine_factor = diversity_factor * wash_factor;

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

fn is_on_cooldown(mint: &str, now: u64, cooldown_secs: u64) -> bool {
    if let Some(ts) = RECENTLY_EXITED.get(mint) {
        // Re-allow after the cooldown; momentum can return, but avoid instant churn.
        return now.saturating_sub(*ts) < cooldown_secs;
    }
    false
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

async fn try_enter(parsed: TradeInfoFromToken, signal: MomentumSignal, cfg: Arc<MomentumConfig>, sniper: Arc<SniperConfig>, logger: Logger) {
    // Discipline: if the circuit breaker has tripped, take no new entries.
    if trading_halted() {
        return;
    }

    let mint = parsed.mint.clone();
    let now = now_secs();

    // KOL edge: a tracked KOL buying this token is the leading signal.
    let kol = if cfg.kol_enabled { kol_hot(&mint, now) } else { None };
    // Pure-KOL mode: only ever enter what a KOL bought (momentum/LP/anti-fake confirm).
    if cfg.kol_require && kol.is_none() {
        return;
    }
    let kol_pts = kol.as_ref().map(|(w, _)| cfg.kol_boost * w).unwrap_or(0.0);

    // GMGN smart-money / trenches confirmation adds score points for the gate.
    let gmgn_hot = gmgn().is_some() && on_gmgn_watchlist(&mint, now);
    let effective_score = signal.score + if gmgn_hot { cfg.gmgn_boost } else { 0.0 } + kol_pts;

    if effective_score < cfg.entry_score {
        return;
    }
    if POSITIONS.contains_key(&mint) || BOUGHT_TOKEN_LIST.contains_key(&mint) {
        return;
    }
    if is_on_cooldown(&mint, now, cfg.reentry_cooldown_secs) {
        return;
    }
    if !position_slots_available(&cfg) {
        return;
    }
    if signal.current_mcap <= 0.0 {
        return;
    }

    // GMGN security veto: skip honeypots / high rug_ratio before committing capital.
    if cfg.gmgn_security_veto {
        if let Some(client) = gmgn() {
            match client.security_verdict(&mint).await {
                SecurityVerdict::Reject(reason) => {
                    logger.log(format!("🛑 GMGN veto {} — {}", mint, reason).red().to_string());
                    return;
                }
                SecurityVerdict::Unknown if client.veto_on_unknown() => {
                    logger.log(format!("🛑 GMGN veto {} — security unknown (fail-closed)", mint).yellow().to_string());
                    return;
                }
                _ => {}
            }
        }
    }

    // Allow re-entry on a fresh pump: clear the bot's permanent buy blacklist for
    // this mint (it was added on a prior buy). The exit cooldown above still
    // prevents instant churn.
    if cfg.allow_reentry {
        crate::processor::sniper_bot::clear_bought_blacklist(&mint);
    }

    // Conviction sizing: scale up with how far the score clears the entry bar.
    let mut entry_size = if cfg.conviction_sizing && cfg.entry_score > 0.0 {
        let mult = (signal.score / cfg.entry_score).clamp(1.0, cfg.conviction_max_mult);
        cfg.position_size_sol * mult
    } else {
        cfg.position_size_sol
    };

    // Global capital cap: never let total exposure exceed the budget. Trim the
    // last entry to the remaining headroom; skip if there isn't enough room.
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

    // True cost basis includes the estimated entry-leg cost so PnL isn't optimistic.
    let cost_basis_sol = entry_size * (1.0 + cfg.buy_cost_fraction);

    // Build the tracked-wallet set for the insider/leader-dump exit: the creator
    // plus the largest early buyers (by short-window volume).
    let tracked_wallets = build_tracked_wallets(&parsed, &mint, now, &cfg);

    // Reserve a slot before the async buy to prevent overshooting max positions.
    IN_FLIGHT_BUYS.fetch_add(1, Ordering::SeqCst);

    let kol_tag = kol.as_ref().map(|(_, l)| format!(", KOL:{} +{:.0}", l, kol_pts)).unwrap_or_default();
    logger.log(format!(
        "🟢 ENTRY {} | score {:.1} (smart +{:.1}{}{}) | genuine {:.0}% | size {:.3} SOL | buyvol {:.2} | {} buyers | mcap {:.1} SOL",
        mint, signal.score, signal.smart_money_boost,
        if gmgn_hot { format!(", GMGN +{:.1}", cfg.gmgn_boost) } else { String::new() },
        kol_tag,
        signal.genuine_factor * 100.0,
        entry_size, signal.buy_volume_short, signal.unique_buyers_short, signal.current_mcap,
    ).green().bold().to_string());

    // Reason carries the KOL label so the analyzer can attribute PnL per KOL.
    let buy_reason = match &kol {
        Some((_, l)) => format!("momentum entry [KOL:{}]", l),
        None => "momentum entry".to_string(),
    };

    let result: Result<(), String> = if cfg.dry_run {
        // Paper trade: assume the buy fills at the current market cap.
        Ok(())
    } else {
        // Live buy via the configured landing route (zeroslot | jito | multi),
        // the same fast path the sells use.
        momentum_buy(&parsed, entry_size, Arc::new(sniper.app_state.clone()), &cfg, &logger)
            .await
            .map(|_| ())
    };

    IN_FLIGHT_BUYS.fetch_sub(1, Ordering::SeqCst);

    match result {
        Ok(_) => {
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
                kol_label: kol.as_ref().map(|(_, l)| l.clone()).unwrap_or_default(),
            });
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
        }
        Err(e) => {
            logger.log(format!("❌ Buy failed for {}: {}", mint, e).red().to_string());
        }
    }
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

    let blockhash = crate::library::blockhash_processor::BlockhashProcessor::get_latest_blockhash()
        .await
        .ok_or_else(|| "no recent blockhash".to_string())?;

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

    let blockhash = crate::library::blockhash_processor::BlockhashProcessor::get_latest_blockhash()
        .await
        .ok_or_else(|| "no recent blockhash".to_string())?;

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
}

/// Evaluate one position against the live signal and act.
async fn evaluate_position(mint: String, app_state: Arc<AppState>, cfg: Arc<MomentumConfig>, logger: Logger) {
    let now = now_secs();

    // Snapshot the live signal.
    let signal = match TOKEN_STATE.get(&mint) {
        Some(s) => score_token(&s, &cfg, now),
        None => return,
    };

    // KOL that triggered this position (for the per-KOL leaderboard).
    let pos_kol = POSITIONS.get(&mint).map(|p| p.kol_label.clone()).unwrap_or_default();

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
        if pos.selling {
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

        // 0. Insider/leader distribution -> exit immediately, ahead of everything.
        if cfg.leader_dump_exit_enabled && leader_sell >= cfg.leader_dump_sol {
            pos.selling = true;
            Decision::full(pos.remaining_fraction, format!("insider distribution ({:.2} SOL sold by tracked wallets, pnl {:.1}%)", leader_sell, pnl), pnl, entry_mcap, signal.current_mcap, entry_size_sol, cost_basis_sol)
        }
        // 1. Hard stop.
        else if pnl <= cfg.hard_stop_pct {
            pos.selling = true;
            Decision::full(pos.remaining_fraction, format!("hard stop {:.1}%", pnl), pnl, entry_mcap, signal.current_mcap, entry_size_sol, cost_basis_sol)
        }
        // 2. Momentum collapse / dump -> cut remaining regardless of rung.
        else if signal.score < cfg.collapse_score || signal.sell_volume_short > signal.buy_volume_short * 1.5 {
            pos.selling = true;
            Decision::full(pos.remaining_fraction, format!("momentum collapse (score {:.1}, pnl {:.1}%)", signal.score, pnl), pnl, entry_mcap, signal.current_mcap, entry_size_sol, cost_basis_sol)
        }
        // 3. Scale-out ladder: take 20% of the original at the next uncleared rung.
        // NOTE: the rung is NOT marked hit here — that happens only after the sell
        // confirms, so a failed sell never silently consumes a rung.
        else {
            let mut chosen: Option<(usize, f64)> = None;
            for (i, target) in cfg.scale_out_targets.iter().enumerate() {
                if !pos.rungs_hit[i] && pnl >= *target {
                    // Sell 0.2 of the original = (0.2 / remaining_fraction) of the current balance.
                    let frac_of_current = (0.2 / pos.remaining_fraction).min(1.0);
                    chosen = Some((i, frac_of_current));
                    break;
                }
            }
            match chosen {
                Some((i, frac_of_current)) => {
                    pos.selling = true; // lock the position while the sell is in flight
                    let tgt = cfg.scale_out_targets[i];
                    Decision {
                        action: ExitAction::Partial,
                        frac_of_current,
                        frac_of_original: 0.2,
                        rung_index: Some(i),
                        reason: format!("scale-out +{:.0}% (pnl {:.1}%)", tgt, pnl),
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
    let commit_sell = |succeeded: bool| {
        if !succeeded {
            if let Some(mut p) = POSITIONS.get_mut(&mint) {
                p.selling = false;
            }
            return;
        }
        if is_full {
            finalize_exit(&mint);
        } else if let Some(mut p) = POSITIONS.get_mut(&mint) {
            if let Some(i) = decision.rung_index {
                if i < p.rungs_hit.len() {
                    p.rungs_hit[i] = true;
                }
            }
            p.remaining_fraction = (p.remaining_fraction - decision.frac_of_original).max(0.0);
            p.selling = false;
        }
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
        commit_sell(true);
        record_kol_pnl(&pos_kol, sim_realized, is_full, sim_realized);
        if is_full {
            record_full_exit(sim_realized, &cfg, &logger);
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
            commit_sell(true);
            record_kol_pnl(&pos_kol, est_realized_pnl, is_full, est_realized_pnl);
            if is_full {
                record_full_exit(est_realized_pnl, &cfg, &logger);
            }
            spawn_reconcile(recon_app, mint.clone(), sig, cost_basis,
                est_realized_pnl, signal.score, decision.entry_mcap, decision.current_mcap, decision.pnl, decision.frac_of_original, decision.reason.clone());
        }
        Err(e) => {
            logger.log(format!("Sell error {}: {}", mint, e).red().to_string());
            commit_sell(false);
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
    tokio::spawn(async move {
        if let Some(proceeds) = fetch_actual_sol_delta(&app_state, &signature).await {
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
    CONSECUTIVE_LOSSES.store(0, Ordering::SeqCst);
    DAY_INDEX.store(current_day(cfg), Ordering::SeqCst);
    if let Ok(mut base) = DAY_START_REALIZED.lock() {
        *base = 0.0;
    }
    MOMENTUM_RUNNING.store(true, Ordering::SeqCst);

    let app_state = Arc::new(sniper.app_state.clone());
    let sniper = Arc::new(sniper);

    if cfg.smart_money_enabled {
        load_wallet_rep(&cfg.wallet_rep_file);
        logger.log(format!("🧠 Loaded reputation for {} wallets from {}", WALLET_REP.len(), cfg.wallet_rep_file).cyan().to_string());
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
