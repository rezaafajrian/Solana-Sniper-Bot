# Environment Network Setup (so the dry run can run in the cloud)

The default cloud environment uses **Trusted** network access, which only allows
package registries + GitHub — so it blocks Helius and Chainstack. To let the bot
reach your data providers, switch the environment to **Custom** and allow those
hosts. You do this once in the Claude Code web UI.

## Step 0 (recommended): rotate your Helius API key
It was pasted in chat earlier. Rotate it in the Helius dashboard, and use the new
key below. (Skip if you don't care.)

## Step 1: open the environment for editing
1. Go to **claude.ai/code**.
2. Click the **cloud icon** (it appears where you start a cloud session / pick an
   environment for this repo). There is no separate "Environments" page.
3. Open this repo's environment **for editing**.

## Step 2: set Network access = Custom
1. In the edit dialog, find the **Network access** selector.
2. Choose **Custom**. An **Allowed domains** field appears.
3. Enter these, one per line:

   ```
   mainnet.helius-rpc.com
   *.helius-rpc.com
   *.core.chainstack.com
   ```
   - `mainnet.helius-rpc.com` = your Helius RPC
   - `*.core.chainstack.com` = your Chainstack websocket host
   - `*.helius-rpc.com` also covers Helius LaserStream gRPC if you later use the
     gRPC feed instead of websocket.

4. **Check** the box **"Also include default list of common package managers."**
   This keeps crates.io / apt / ubuntu.com available so the project can still
   build (Rust deps + protobuf-compiler). Required.
5. **Save.**

> Alternative (simpler, broader): choose **Full** instead of Custom to allow any
> domain. Easiest, but less locked-down. Custom is the safer choice.

## Step 3: start a FRESH session
Network changes apply to **new** sessions in that environment — the currently
running container stays locked down. So:
1. Start a **new cloud session** on this repo in the reconfigured environment.
2. Select the branch **`claude/code-review-weaknesses-c2c5ny`**.

## Step 4: tell the new session to run it
Paste this to the new session (it starts fresh, without this conversation's
memory — the branch + `docs/DRY_RUN.md` have everything it needs):

> Read docs/DRY_RUN.md. Run the momentum dry run using the websocket feed.
> RPC_HTTP = https://mainnet.helius-rpc.com/?api-key=YOUR_KEY
> RPC_WSS  = wss://solana-mainnet.core.chainstack.com/YOUR_PATH
> Build release, run it in the background ~30–45 min, then analyze
> momentum_trades.csv with scripts/analyze_momentum.py and show me the result.

The assistant should first verify access with a quick `curl` to each host
(expect HTTP 200, not 403), then proceed.

## Notes
- Dry run never signs a transaction — no funds, no risk. The wallet is a throwaway.
- If Chainstack `blockSubscribe` is not enabled on your plan, the websocket feed
  will error on connect; switch to a Yellowstone gRPC endpoint (`MOMENTUM_FEED=grpc`)
  or another provider — see `docs/DRY_RUN.md`.
- This setup only grants network access; it does not change any trading behavior.
