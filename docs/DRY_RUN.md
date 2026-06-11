# Dry-Run Guide (momentum mode)

Run the momentum bot in **dry run** (paper trading — no funds, no real wallet
signing) against a live feed, collect `momentum_trades.csv`, and analyze it.

> The managed cloud environment blocks outbound traffic to data providers, so the
> dry run must be done **on your own machine** (or in a cloud environment whose
> network policy allows your RPC/gRPC hosts). The bot is verified to boot through
> every stage and stops only at the live gRPC connection.

## 1. What you need

You need a normal Solana **RPC** url (Helius RPC is fine) plus a **data feed**.
There are two feed options — pick whichever matches what you have:

### Option A — Websocket feed (`MOMENTUM_FEED=ws`) — no gRPC needed
Runs on a **standard RPC websocket** via `blockSubscribe`. Works with the
Chainstack / Helius `wss://...` endpoint you already have (no Business plan).
Set `RPC_WSS=wss://...`. `blockSubscribe` must be enabled on the node
(Chainstack dedicated nodes and Helius support it). This is the easiest path.

### Option B — Yellowstone gRPC feed (`MOMENTUM_FEED=grpc`) — fastest
Lower latency, but needs a gRPC endpoint:
- **Helius LaserStream**: `https://laserstream-mainnet-ewr.helius-rpc.com`,
  x-token = your Helius API key. Needs a **Business plan** for mainnet.
- **Shyft gRPC**: `https://grpc.<region>.shyft.to` + a gRPC token.
- **Chainstack / Triton dedicated node** with the Yellowstone gRPC add-on (the
  plain `wss://...` URL is NOT gRPC — that's Option A).

## 2. Build

```bash
git checkout claude/code-review-weaknesses-c2c5ny
PROTOC=$(which protoc) cargo build --release   # needs protobuf-compiler installed
```

## 3. Create `.env` (fill the three credentials)

```env
MONITORING_MODE=momentum
MOMENTUM_DRY_RUN=true
MOMENTUM_SIM_COST_FRACTION=0.03
MOMENTUM_POSITION_SIZE_SOL=0.02
MOMENTUM_MAX_POSITIONS=5
MOMENTUM_TRADE_LOG=momentum_trades.csv
MOMENTUM_DAILY_LOSS_LIMIT_SOL=0.3
MOMENTUM_MAX_CONSECUTIVE_LOSSES=6
MOMENTUM_SMART_MONEY=true
MOMENTUM_LEADER_DUMP_EXIT=true
GMGN_ENABLED=false

# ---- feed: Option A (websocket, easiest) ----
MOMENTUM_FEED=ws
RPC_WSS=YOUR_WEBSOCKET_URL          # e.g. your Chainstack/Helius wss endpoint
RPC_HTTP=YOUR_SOLANA_RPC_URL
# unused with MOMENTUM_FEED=ws, but the loader still requires them to be present:
YELLOWSTONE_GRPC_HTTP=unused
YELLOWSTONE_GRPC_TOKEN=unused

# ---- OR feed: Option B (gRPC) — set MOMENTUM_FEED=grpc and fill these instead ----
# MOMENTUM_FEED=grpc
# YELLOWSTONE_GRPC_HTTP=YOUR_YELLOWSTONE_GRPC_ENDPOINT
# YELLOWSTONE_GRPC_TOKEN=YOUR_GRPC_XTOKEN

# ---- throwaway wallet: dry run never signs; DO NOT fund ----
PRIVATE_KEY=GENERATE_A_BURNER_OR_USE_ANY_VALID_BASE58_KEYPAIR

# ---- presets required by the config loader (unused in dry run) ----
TOKEN_AMOUNT=0.02
SLIPPAGE=3000
TRANSACTION_LANDING_SERVICE=0
COUNTER_LIMIT=10
COPY_SELLING_LIMIT=1.5
FOCUS_DROP_THRESHOLD_PCT=0.15
FOCUS_TRIGGER_SOL=1.0
SELLING_UNIT_PRICE=4000000
SELLING_UNIT_LIMIT=2000000
SELLING_TIME=600
ZERO_SLOT_URL=http://ny1.0slot.trade/?api-key=DRY_RUN_UNUSED
ZERO_SLOT_HEALTH=https://ny1.0slot.trade/health
ZERO_SLOT_TIP_VALUE=0.00015
TAKE_PROFIT=8.0
STOP_LOSS=-2
MAX_HOLD_TIME=3600
MAX_HOLD_TIME_SECS=3600
MIN_PROFIT_TIME_SECS=30
MIN_LIQUIDITY=4
MIN_ABSOLUTE_LIQUIDITY=1
MAX_ACCEPTABLE_DROP=50
RETRACEMENT_THRESHOLD=15
DYNAMIC_RETRACEMENT_PERCENTAGE=15
TRAILING_STOP_ACTIVATION_PERCENTAGE=20.0
TRAILING_STOP_TRAIL_PERCENTAGE=10.0
PROFIT_TAKING_TARGET_PERCENTAGE=1.0
PROFIT_TAKING_SCALE_OUT_PERCENTAGES=0.5,0.3,0.2
VOLUME_ANALYSIS_LOOKBACK_PERIOD=30
VOLUME_ANALYSIS_SPIKE_THRESHOLD=2
VOLUME_ANALYSIS_DROP_THRESHOLD=15
RISK_CHECK_INTERVAL_MINUTES=10
RISK_MINIMUM_TARGET_BALANCE=1000
```

> Note: the config loader **hangs** if any of these are missing (inherited
> behavior), so keep all of them. Values are parse-safe — avoid spaces / `<>` in
> values or `dotenv` will silently drop the rest of the file.

## 4. Run, then analyze

```bash
./target/release/solana-vntr-sniper        # let it run for a few hundred trades
# (Ctrl-C when done)

python3 scripts/analyze_momentum.py momentum_trades.csv
```

You should see the `MOMENTUM CONFIG` banner, then `🚀 Momentum sniper live`, and
`🟢 ENTRY ...` lines as it paper-trades. The analyzer prints realized PnL,
win/loss, score-vs-outcome, exit reasons, and the drawdown/discipline section.

## 5. (Optional) send me the result

`momentum_trades.csv` contains no secrets — paste it (or its analyzer output)
back and I'll read it with you and suggest what to tune.
