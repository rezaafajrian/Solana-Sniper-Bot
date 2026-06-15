#!/usr/bin/env bash
# Toggle momentum pacing without editing .env by hand.
#   ./scripts/momentum_speed.sh fast      # many trades, reacts fast, looser filters
#   ./scripts/momentum_speed.sh normal    # selective, higher-conviction (default)
#   ./scripts/momentum_speed.sh turbo      # maximum activity (noisy; dashboard demo)
#
# It SETS each key in .env (removing any duplicate lines first), so values
# actually take effect — appending duplicates does NOT work (first one wins).

set -e
MODE="${1:-fast}"
ENV_FILE=".env"
[ -f "$ENV_FILE" ] || { echo "No .env here. Run from the repo root after dry_run_setup.sh."; exit 1; }

case "$MODE" in
  normal)
    PAIRS=(
      "MOMENTUM_ENTRY_SCORE=65" "MOMENTUM_COLLAPSE_SCORE=35"
      "MOMENTUM_SHORT_WINDOW_SECS=30" "MOMENTUM_MEDIUM_WINDOW_SECS=120"
      "MOMENTUM_MAX_POSITIONS=5" "MOMENTUM_MIN_BUY_VOLUME_SOL=2.0"
      "MOMENTUM_TARGET_UNIQUE_BUYERS=10" "MOMENTUM_MIN_BUYER_DIVERSITY=0.35"
      "MOMENTUM_KOL_REQUIRE=false" )
    ;;
  fast)
    PAIRS=(
      "MOMENTUM_ENTRY_SCORE=42" "MOMENTUM_COLLAPSE_SCORE=25"
      "MOMENTUM_SHORT_WINDOW_SECS=10" "MOMENTUM_MEDIUM_WINDOW_SECS=40"
      "MOMENTUM_MAX_POSITIONS=10" "MOMENTUM_MIN_BUY_VOLUME_SOL=0.5"
      "MOMENTUM_TARGET_UNIQUE_BUYERS=4" "MOMENTUM_MIN_BUYER_DIVERSITY=0.20"
      "MOMENTUM_KOL_REQUIRE=false" )
    ;;
  turbo)
    PAIRS=(
      "MOMENTUM_ENTRY_SCORE=30" "MOMENTUM_COLLAPSE_SCORE=18"
      "MOMENTUM_SHORT_WINDOW_SECS=6" "MOMENTUM_MEDIUM_WINDOW_SECS=24"
      "MOMENTUM_MAX_POSITIONS=20" "MOMENTUM_MIN_BUY_VOLUME_SOL=0.2"
      "MOMENTUM_TARGET_UNIQUE_BUYERS=3" "MOMENTUM_MIN_BUYER_DIVERSITY=0.12"
      "MOMENTUM_KOL_REQUIRE=false" )
    ;;
  *) echo "usage: $0 [fast|normal|turbo]"; exit 1 ;;
esac

tmp="$(mktemp)"
cp "$ENV_FILE" "$tmp"
for kv in "${PAIRS[@]}"; do
  key="${kv%%=*}"
  grep -v "^${key}=" "$tmp" > "${tmp}.2" && mv "${tmp}.2" "$tmp"
  echo "$kv" >> "$tmp"
done
mv "$tmp" "$ENV_FILE"

echo "✅ Set momentum pacing to: $MODE"
echo "   Applied:"
printf "     %s\n" "${PAIRS[@]}"
echo "   Restart the bot:  ./target/release/solana-vntr-sniper"
