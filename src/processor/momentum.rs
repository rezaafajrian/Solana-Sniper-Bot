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
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
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
use crate::dex::pump_fun::{Pump, PUMP_FUN_PROGRAM, TOKEN_TOTAL_SUPPLY};
use crate::processor::sniper_bot::{execute_buy, SniperConfig, BOUGHT_TOKEN_LIST};
use crate::processor::swap::{SwapDirection, SwapInType, SwapProtocol};
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
    pub scale_out_targets: Vec<f64>,
    pub slippage_bps: u64,
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
            scale_out_targets: if scale_out_targets.is_empty() {
                vec![100.0, 200.0, 300.0, 400.0]
            } else {
                scale_out_targets
            },
            slippage_bps: env_u64("MOMENTUM_SLIPPAGE_BPS", 1000),
        }
    }

    pub fn log(&self, logger: &Logger) {
        logger.log("------- MOMENTUM CONFIG -------".cyan().bold().to_string());
        logger.log(format!("Buy strength not age | position {} SOL x {} slots", self.position_size_sol, self.max_positions));
        logger.log(format!("Entry score >= {} | collapse < {} | hard stop {}%", self.entry_score, self.collapse_score, self.hard_stop_pct));
        logger.log(format!("Windows: short {}s / baseline {}s", self.short_window_secs, self.medium_window_secs));
        logger.log(format!(
            "Targets: min buy vol {} SOL, {} unique buyers, {:.0}% mcap growth, max wallet concentration {:.0}%",
            self.min_buy_volume_sol, self.target_unique_buyers, self.target_mcap_growth * 100.0, self.max_wallet_concentration * 100.0,
        ));
        logger.log(format!("Scale-out rungs (20% each): {:?}% PnL, then 20% runner", self.scale_out_targets));
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
}

/// An open position managed by the momentum exit policy.
struct MomentumPosition {
    entry_mcap: f64,
    /// Fraction of the original position still held (1.0 -> 0.2 runner).
    remaining_fraction: f64,
    /// Which scale-out rungs have already been taken.
    rungs_hit: Vec<bool>,
    peak_pnl: f64,
    selling: bool,
}

lazy_static! {
    static ref TOKEN_STATE: DashMap<String, TokenMomentum> = DashMap::new();
    static ref POSITIONS: DashMap<String, MomentumPosition> = DashMap::new();
    static ref RECENTLY_EXITED: DashMap<String, u64> = DashMap::new();
    static ref IN_FLIGHT_BUYS: AtomicUsize = AtomicUsize::new(0);
    static ref MOMENTUM_RUNNING: AtomicBool = AtomicBool::new(true);
}

fn now_secs() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
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
    let mut unique_buyers: HashSet<&str> = HashSet::new();
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

    let score = (weighted * 100.0) * dump_factor;

    MomentumSignal {
        score,
        buy_volume_short: buy_vol_s,
        sell_volume_short: sell_vol_s,
        unique_buyers_short: unique_buyers.len(),
        mcap_velocity,
        current_mcap: last_mcap_s.or(Some(state.last_mcap)).unwrap_or(0.0),
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
    entry.ticks.push_back(TradeTick {
        ts: now,
        is_buy: parsed.is_buy,
        sol: parsed.sol_change.abs(),
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
    let held = BOUGHT_TOKEN_LIST.len();
    let in_flight = IN_FLIGHT_BUYS.load(Ordering::SeqCst);
    held + in_flight < cfg.max_positions
}

fn is_on_cooldown(mint: &str, now: u64) -> bool {
    if let Some(ts) = RECENTLY_EXITED.get(mint) {
        // Re-allow after 5 minutes; momentum can return, but avoid instant churn.
        return now.saturating_sub(*ts) < 300;
    }
    false
}

async fn try_enter(parsed: TradeInfoFromToken, signal: MomentumSignal, cfg: Arc<MomentumConfig>, sniper: Arc<SniperConfig>, logger: Logger) {
    let mint = parsed.mint.clone();
    let now = now_secs();

    if signal.score < cfg.entry_score {
        return;
    }
    if POSITIONS.contains_key(&mint) || BOUGHT_TOKEN_LIST.contains_key(&mint) {
        return;
    }
    if is_on_cooldown(&mint, now) {
        return;
    }
    if !position_slots_available(&cfg) {
        return;
    }
    if signal.current_mcap <= 0.0 {
        return;
    }

    // Reserve a slot before the async buy to prevent overshooting max positions.
    IN_FLIGHT_BUYS.fetch_add(1, Ordering::SeqCst);

    logger.log(format!(
        "🟢 ENTRY {} | score {:.1} | buyvol {:.2} SOL | {} buyers | mcap-vel {:.0}% | mcap {:.1} SOL",
        mint, signal.score, signal.buy_volume_short, signal.unique_buyers_short, signal.mcap_velocity * 100.0, signal.current_mcap,
    ).green().bold().to_string());

    let mut buy_config = sniper.swap_config.clone();
    buy_config.swap_direction = SwapDirection::Buy;
    buy_config.in_type = SwapInType::Qty;
    buy_config.amount_in = cfg.position_size_sol;
    buy_config.slippage = cfg.slippage_bps;

    let app_state = Arc::new(sniper.app_state.clone());
    let mut buy_trade_info = parsed.clone();
    buy_trade_info.dex_type = DexType::PumpFun;

    let result = execute_buy(
        buy_trade_info,
        app_state,
        Arc::new(buy_config),
        SwapProtocol::PumpFun,
    ).await;

    IN_FLIGHT_BUYS.fetch_sub(1, Ordering::SeqCst);

    match result {
        Ok(_) => {
            POSITIONS.insert(mint.clone(), MomentumPosition {
                entry_mcap: signal.current_mcap,
                remaining_fraction: 1.0,
                rungs_hit: vec![false; cfg.scale_out_targets.len()],
                peak_pnl: 0.0,
                selling: false,
            });
            logger.log(format!("✅ Bought {} at mcap {:.2} SOL", mint, signal.current_mcap).green().to_string());
        }
        Err(e) => {
            logger.log(format!("❌ Buy failed for {}: {}", mint, e).red().to_string());
        }
    }
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
) -> Result<(), String> {
    let fraction = fraction.max(0.0).min(1.0);
    if fraction <= 0.0 {
        return Ok(());
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

    let sigs = crate::block_engine::tx::new_signed_and_send_zeroslot(
        app_state.zeroslot_rpc_client.clone(),
        blockhash,
        &keypair,
        instructions,
        logger,
    )
    .await
    .map_err(|e| format!("send sell failed: {}", e))?;

    logger.log(format!(
        "🔴 SELL {:.0}% of {} ({}) at price {} | sig {}",
        fraction * 100.0, mint, reason, price,
        sigs.first().cloned().unwrap_or_default(),
    ).red().bold().to_string());

    Ok(())
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

    // Read + update position bookkeeping without holding the lock across awaits.
    let (action, fraction, reason) = {
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

        // 1. Hard stop.
        if pnl <= cfg.hard_stop_pct {
            pos.selling = true;
            (ExitAction::Full, pos.remaining_fraction, format!("hard stop {:.1}%", pnl))
        }
        // 2. Momentum collapse / dump -> cut remaining regardless of rung.
        else if signal.score < cfg.collapse_score || signal.sell_volume_short > signal.buy_volume_short * 1.5 {
            pos.selling = true;
            (ExitAction::Full, pos.remaining_fraction, format!("momentum collapse (score {:.1}, pnl {:.1}%)", signal.score, pnl))
        }
        // 3. Scale-out ladder: take 20% of the original at the next uncleared rung.
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
                    pos.rungs_hit[i] = true;
                    pos.remaining_fraction = (pos.remaining_fraction - 0.2).max(0.0);
                    let tgt = cfg.scale_out_targets[i];
                    (ExitAction::Partial, frac_of_current, format!("scale-out +{:.0}% (pnl {:.1}%)", tgt, pnl))
                }
                None => (ExitAction::None, 0.0, String::new()),
            }
        }
    };

    match action {
        ExitAction::None => {}
        ExitAction::Partial => {
            if let Err(e) = momentum_sell(&mint, fraction, app_state, &cfg, &reason, &logger).await {
                logger.log(format!("Partial sell error {}: {}", mint, e).red().to_string());
                if let Some(mut p) = POSITIONS.get_mut(&mint) {
                    p.selling = false;
                }
            }
        }
        ExitAction::Full => {
            if let Err(e) = momentum_sell(&mint, 1.0, app_state, &cfg, &reason, &logger).await {
                logger.log(format!("Exit sell error {}: {}", mint, e).red().to_string());
                if let Some(mut p) = POSITIONS.get_mut(&mint) {
                    p.selling = false;
                }
            } else {
                finalize_exit(&mint);
            }
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
    while MOMENTUM_RUNNING.load(Ordering::SeqCst) {
        interval.tick().await;
        let mints: Vec<String> = POSITIONS.iter().map(|e| e.key().clone()).collect();
        for mint in mints {
            evaluate_position(mint, app_state.clone(), cfg.clone(), logger.clone()).await;
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

/// Start the momentum sniper: stream every pump.fun trade, score momentum, buy
/// strength, and manage exits with the scale-out + collapse policy.
pub async fn start_momentum_monitoring(sniper: SniperConfig) -> Result<(), String> {
    let logger = Logger::new("[MOMENTUM] => ".green().bold().to_string());
    let cfg = Arc::new(MomentumConfig::from_env());
    cfg.log(&logger);

    MOMENTUM_RUNNING.store(true, Ordering::SeqCst);

    let app_state = Arc::new(sniper.app_state.clone());
    let sniper = Arc::new(sniper);

    // Exit monitor in the background.
    {
        let app_state = app_state.clone();
        let cfg = cfg.clone();
        let logger = logger.clone();
        tokio::spawn(async move { run_exit_monitor(app_state, cfg, logger).await });
    }

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
