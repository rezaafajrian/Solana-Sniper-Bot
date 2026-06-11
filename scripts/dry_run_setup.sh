#!/usr/bin/env bash
# One-shot dry-run setup for the momentum bot (paper trading; no funds, never signs).
#
# Usage:
#   ./scripts/dry_run_setup.sh "<RPC_HTTP_URL>" "<RPC_WSS_URL>"
#
# Example:
#   ./scripts/dry_run_setup.sh \
#     "https://mainnet.helius-rpc.com/?api-key=YOUR_KEY" \
#     "wss://solana-mainnet.core.chainstack.com/YOUR_PATH"
#
# It writes a ready .env (dry run, websocket feed) and prints the next commands.

set -e

RPC_HTTP="$1"
RPC_WSS="$2"

if [ -z "$RPC_HTTP" ] || [ -z "$RPC_WSS" ]; then
  echo "Usage: $0 \"<RPC_HTTP_URL>\" \"<RPC_WSS_URL>\""
  echo
  echo "  RPC_HTTP  = your Solana RPC https url (e.g. Helius)"
  echo "  RPC_WSS   = your Solana websocket url (e.g. Chainstack wss)"
  exit 1
fi

cat > .env <<EOF
# ===== momentum DRY RUN (paper trading, no funds, never signs) =====
MONITORING_MODE=momentum
MOMENTUM_DRY_RUN=true
MOMENTUM_FEED=ws
RPC_HTTP=${RPC_HTTP}
RPC_WSS=${RPC_WSS}

MOMENTUM_SIM_COST_FRACTION=0.03
MOMENTUM_POSITION_SIZE_SOL=0.02
MOMENTUM_MAX_POSITIONS=5
MOMENTUM_TRADE_LOG=momentum_trades.csv
MOMENTUM_DAILY_LOSS_LIMIT_SOL=0.3
MOMENTUM_MAX_CONSECUTIVE_LOSSES=6
MOMENTUM_SMART_MONEY=true
MOMENTUM_LEADER_DUMP_EXIT=true
GMGN_ENABLED=false

# gRPC vars unused with MOMENTUM_FEED=ws, but the loader requires them present:
YELLOWSTONE_GRPC_HTTP=unused
YELLOWSTONE_GRPC_TOKEN=unused

# Throwaway wallet — dry run NEVER signs. Do NOT fund this.
PRIVATE_KEY=Wr3PDGgVmjRK6VdzEM4NrunorHMZMLn93UuD3XZ7UfEPhyQFXm5wphGJHAWuwxguBo6GNouDsDmF6QfPLTtp95p

# Presets required by the config loader (unused in dry run):
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
EOF

echo "✅ Wrote .env (dry run, websocket feed)."
echo
echo "Next, run these two commands:"
echo
echo "  1) Build (first time only, ~2-5 min):"
echo "       PROTOC=\$(which protoc) cargo build --release"
echo
echo "  2) Run the bot (let it run ~30-45 min, then press Ctrl-C):"
echo "       ./target/release/solana-vntr-sniper"
echo
echo "  3) See the results:"
echo "       python3 scripts/analyze_momentum.py momentum_trades.csv"
echo
echo "Then paste momentum_trades.csv (or the analyzer output) back to me."
