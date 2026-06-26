/*!
# Websocket feed (blockSubscribe)

A no-gRPC data source for momentum mode. It connects to a standard Solana RPC
**websocket** (e.g. Chainstack, Helius) and uses `blockSubscribe` filtered to the
pump.fun program to stream full transactions, decoding the pump.fun trade events
with the same offsets the gRPC parser trusts (`decode_pumpfun_event`).

This lets the bot run on a plain wss endpoint — no Yellowstone gRPC, no Business
plan required.

## Notes / limitations
- `blockSubscribe` must be enabled on the node (Chainstack dedicated nodes and
  Helius support it; it is gated/"unstable" on some providers).
- It is somewhat higher-latency and heavier than gRPC — fine for dry-run
  validation and for setups without gRPC access.
- Reconnects with backoff on disconnect.
*/

use std::time::Duration;

use colored::Colorize;
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_tungstenite::{connect_async, tungstenite::protocol::Message};

use crate::common::logger::Logger;
use crate::dex::pump_fun::PUMP_FUN_PROGRAM;
use crate::processor::transaction_parser::{
    decode_pumpfun_event, TradeInfoFromToken, PUMPFUN_TRADE_DISCRIMINATOR,
};

/// One parsed pump.fun trade plus the transaction's fee-payer (trader) address.
pub type FeedItem = (TradeInfoFromToken, String);

fn subscribe_msg() -> String {
    // logsSubscribe (not blockSubscribe): every Solana RPC supports it (Helius/Chainstack/
    // QuickNode/public), whereas blockSubscribe is gated on most. The pump.fun trade event
    // rides in the tx logs as a "Program data:" line and carries the trader, so logs are enough.
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "logsSubscribe",
        "params": [
            { "mentions": [ PUMP_FUN_PROGRAM ] },
            { "commitment": "confirmed" }
        ]
    })
    .to_string()
}

/// Connect, subscribe, and forward decoded pump.fun trades to `tx` until the
/// channel closes. Reconnects with backoff on any error.
pub async fn run(ws_url: String, tx: mpsc::Sender<FeedItem>, logger: Logger) {
    let mut backoff = 1u64;
    loop {
        if tx.is_closed() {
            return;
        }
        match connect_async(&ws_url).await {
            Ok((mut stream, _)) => {
                backoff = 1;
                if stream.send(Message::Text(subscribe_msg())).await.is_err() {
                    logger.log("ws: failed to send blockSubscribe".red().to_string());
                    continue;
                }
                logger.log("📡 Websocket feed connected (logsSubscribe → pump.fun)".green().to_string());

                let mut msg_count: u64 = 0;
                let mut event_count: u64 = 0;
                let mut logged_first = false;
                let mut logged_shape = false;
                let mut last_report = std::time::Instant::now();

                while let Some(msg) = stream.next().await {
                    match msg {
                        Ok(Message::Text(text)) => {
                            msg_count += 1;
                            // Log the FIRST message verbatim — it's the subscribe ack
                            // (or an error), which tells us instantly if blockSubscribe
                            // was accepted.
                            if !logged_first {
                                logged_first = true;
                                let head: String = text.chars().take(400).collect();
                                logger.log(format!("ws first message: {}", head).cyan().to_string());
                            }
                            if let Ok(v) = serde_json::from_str::<Value>(&text) {
                                let _ = &logged_shape; // (block-shape debug no longer used with logs)
                                event_count += forward_logs(&v, &tx).await as u64;
                                if tx.is_closed() {
                                    return;
                                }
                            }
                            // Heartbeat every ~30s so we can see msgs-in vs events-out.
                            if last_report.elapsed().as_secs() >= 30 {
                                last_report = std::time::Instant::now();
                                logger.log(format!("ws: {} msgs received, {} pump.fun events parsed", msg_count, event_count).cyan().to_string());
                            }
                        }
                        Ok(Message::Ping(p)) => {
                            let _ = stream.send(Message::Pong(p)).await;
                        }
                        Ok(Message::Close(c)) => {
                            logger.log(format!("ws: server closed: {:?}", c).yellow().to_string());
                            break;
                        }
                        Err(e) => {
                            logger.log(format!("ws: stream error: {}", e).red().to_string());
                            break;
                        }
                        _ => {}
                    }
                }
                logger.log("ws: stream ended, reconnecting…".yellow().to_string());
            }
            Err(e) => {
                logger.log(format!("ws: connect failed: {} (retry in {}s)", e, backoff).red().to_string());
            }
        }
        tokio::time::sleep(Duration::from_secs(backoff)).await;
        backoff = (backoff * 2).min(30);
    }
}

/// One-shot structural dump of a block notification, so we can map the real field
/// layout (it varies by provider/encoding) and fix `forward_block`.
#[allow(dead_code)]
fn log_block_shape(v: &Value, logger: &Logger) {
    fn keys(v: Option<&Value>) -> String {
        match v {
            Some(Value::Object(m)) => m.keys().cloned().collect::<Vec<_>>().join(","),
            Some(Value::Array(a)) => format!("[array len {}]", a.len()),
            Some(other) => format!("[{}]", if other.is_string() { "string" } else { "value" }),
            None => "<none>".to_string(),
        }
    }
    let value = v.pointer("/params/result/value");
    logger.log(format!("shape: params.result.value keys = {}", keys(value)).cyan().to_string());
    let block = v.pointer("/params/result/value/block");
    logger.log(format!("shape: .block keys = {}", keys(block)).cyan().to_string());
    let txns = v.pointer("/params/result/value/block/transactions").and_then(|t| t.as_array());
    let arr = match txns {
        Some(a) => a,
        None => {
            let raw = serde_json::to_string(value.unwrap_or(v)).unwrap_or_default();
            logger.log(format!("shape: value head = {}", raw.chars().take(700).collect::<String>()).cyan().to_string());
            return;
        }
    };
    logger.log(format!("shape: block.transactions len = {}", arr.len()).cyan().to_string());

    // Find the first SUCCESSFUL tx with non-empty inner instructions and dump its
    // inner-instruction data fields + decoded byte lengths (that's where the
    // pump.fun trade event lives), plus any "Program data:" log lines.
    for (i, t) in arr.iter().enumerate() {
        let failed = t.pointer("/meta/err").map(|e| !e.is_null()).unwrap_or(false);
        let inner = t.pointer("/meta/innerInstructions").and_then(|x| x.as_array());
        let has_inner = inner.map(|a| !a.is_empty()).unwrap_or(false);
        if failed || !has_inner {
            continue;
        }
        logger.log(format!("shape: first success tx with inner = idx {}", i).cyan().to_string());
        if let Some(groups) = inner {
            for grp in groups.iter().take(2) {
                if let Some(ixs) = grp.get("instructions").and_then(|x| x.as_array()) {
                    for ix in ixs.iter().take(6) {
                        let d = ix.get("data").and_then(|x| x.as_str()).unwrap_or("");
                        let b58 = bs58::decode(d).into_vec().map(|b| b.len()).unwrap_or(0);
                        logger.log(format!("shape: inner ix data_head={} b58_len={}", d.chars().take(24).collect::<String>(), b58).cyan().to_string());
                    }
                }
            }
        }
        // Program data logs (alternative event source)
        if let Some(logs) = t.pointer("/meta/logMessages").and_then(|x| x.as_array()) {
            for l in logs.iter().filter_map(|x| x.as_str()).filter(|s| s.contains("Program data:")).take(2) {
                logger.log(format!("shape: {}", l.chars().take(80).collect::<String>()).cyan().to_string());
            }
        }
        return;
    }
    logger.log("shape: no successful tx with inner instructions in this block".yellow().to_string());
}

/// Pull pump.fun trades out of a logsNotification and forward them. The pump.fun trade
/// event is emitted as a `Program data: <base64>` log line whose bytes carry the trader
/// (`user`) at offset 65 — so we need neither the full block nor the account keys. Works on
/// any RPC that supports logsSubscribe (i.e. effectively all of them).
async fn forward_logs(v: &Value, tx: &mpsc::Sender<FeedItem>) -> usize {
    let value = match v.pointer("/params/result/value") {
        Some(x) => x,
        None => return 0,
    };
    if value.get("err").map(|e| !e.is_null()).unwrap_or(false) {
        return 0; // skip failed transactions
    }
    let logs = match value.get("logs").and_then(|l| l.as_array()) {
        Some(l) => l,
        None => return 0,
    };
    let mut forwarded = 0usize;
    for line in logs {
        let s = match line.as_str() { Some(s) => s, None => continue };
        let b64 = match s.strip_prefix("Program data: ") { Some(b) => b, None => continue };
        let raw = match base64::decode(b64) { Ok(b) => b, Err(_) => continue };

        // pump.fun's TradeEvent reaches us in two byte layouts depending on how it
        // was emitted, and `decode_pumpfun_event` expects the CPI inner-instruction
        // layout where the 8-byte discriminator sits at offset 8 (mint at 16):
        //
        //   • CPI / inner-instruction (`emit_cpi!`): [8-byte CPI marker][8-byte disc][fields…]
        //       → disc at 8..16  → feed straight to decode_pumpfun_event
        //   • "Program data:" log line (`emit!` via sol_log_data): [8-byte disc][fields…]
        //       → disc at 0..8  → SHORT by 8 bytes, so we left-pad 8 bytes to realign.
        //
        // A "Program data:" log nearly always carries the bare-disc layout (its base64
        // begins "vdt/007mYe…", which is the discriminator itself), but we detect the
        // layout from the bytes rather than assume, so either form decodes correctly.
        let buf: Vec<u8>;
        let bytes: &[u8] = if raw.get(0..8) == Some(&PUMPFUN_TRADE_DISCRIMINATOR[..]) {
            // bare-disc (log) layout — left-pad 8 bytes so disc lands at 8..16.
            buf = std::iter::repeat(0u8).take(8).chain(raw.iter().copied()).collect();
            &buf
        } else {
            // already CPI layout (disc at 8..16) — use as-is.
            &raw
        };
        if bytes.len() < 129 {
            continue;
        }
        if let Some(parsed) = decode_pumpfun_event(bytes) {
            // user/trader pubkey is at offset 65..97 in the aligned (CPI) layout.
            let trader = bs58::encode(&bytes[65..97]).into_string();
            if tx.send((parsed, trader)).await.is_err() {
                return forwarded;
            }
            forwarded += 1;
        }
    }
    forwarded
}

/// Pull pump.fun trades out of a blockNotification value and forward them.
/// Returns the number of events forwarded.
#[allow(dead_code)]
async fn forward_block(v: &Value, tx: &mpsc::Sender<FeedItem>) -> usize {
    // blockNotification: params.result.value.block.transactions[]
    let txns = match v
        .get("params")
        .and_then(|p| p.get("result"))
        .and_then(|r| r.get("value"))
        .and_then(|val| val.get("block"))
        .and_then(|b| b.get("transactions"))
        .and_then(|t| t.as_array())
    {
        Some(arr) => arr,
        None => return 0,
    };

    let mut forwarded = 0usize;
    for txn in txns {
        // Fee payer = first account key (the trader).
        let signer = txn
            .pointer("/transaction/message/accountKeys/0")
            .and_then(|k| k.as_str())
            .unwrap_or("")
            .to_string();

        // Scan inner instructions for a pump.fun event buffer.
        let inner = txn
            .pointer("/meta/innerInstructions")
            .and_then(|i| i.as_array());
        let inner = match inner {
            Some(i) => i,
            None => continue,
        };
        for grp in inner {
            let instrs = match grp.get("instructions").and_then(|i| i.as_array()) {
                Some(i) => i,
                None => continue,
            };
            for ix in instrs {
                let data_b58 = match ix.get("data").and_then(|d| d.as_str()) {
                    Some(d) => d,
                    None => continue,
                };
                let bytes = match bs58::decode(data_b58).into_vec() {
                    Ok(b) => b,
                    Err(_) => continue,
                };
                // decode_pumpfun_event identifies the event by its discriminator,
                // so we don't filter by length here (pump.fun's event grew over time).
                if bytes.len() < 129 {
                    continue;
                }
                if let Some(parsed) = decode_pumpfun_event(&bytes) {
                    if tx.send((parsed, signer.clone())).await.is_err() {
                        return forwarded;
                    }
                    forwarded += 1;
                }
            }
        }
    }
    forwarded
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a bare-discriminator ("Program data:" log) layout TradeEvent buffer:
    /// [disc(8)][mint(32)][sol_amount(8)][token_amount(8)][is_buy(1)][user(32)]
    /// [timestamp(8)][vsol(8)][vtok(8)][real_sol(8)] — i.e. the CPI layout minus the
    /// 8-byte CPI marker. forward_logs must left-pad this to decode it.
    fn build_log_layout_event(mint: &[u8; 32], user: &[u8; 32]) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&PUMPFUN_TRADE_DISCRIMINATOR); // 0..8
        b.extend_from_slice(mint); // 8..40
        b.extend_from_slice(&1_000_000_000u64.to_le_bytes()); // sol_amount 40..48 (1 SOL)
        b.extend_from_slice(&5_000_000u64.to_le_bytes()); // token_amount 48..56
        b.push(1u8); // is_buy 56
        b.extend_from_slice(user); // user 57..89
        b.extend_from_slice(&1_700_000_000u64.to_le_bytes()); // timestamp 89..97
        b.extend_from_slice(&30_000_000_000u64.to_le_bytes()); // vsol 97..105
        b.extend_from_slice(&1_000_000_000_000_000u64.to_le_bytes()); // vtok 105..113
        b.extend_from_slice(&10_000_000_000u64.to_le_bytes()); // real_sol 113..121
        b
    }

    #[tokio::test]
    async fn forward_logs_decodes_program_data_log_layout() {
        let mint = [7u8; 32];
        let user = [9u8; 32];
        let raw = build_log_layout_event(&mint, &user);
        // sanity: the base64 must begin with the known pump.fun trade prefix
        let b64 = base64::encode(&raw);
        assert!(b64.starts_with("vdt/007mYe"), "unexpected event prefix: {}", &b64[..12]);

        let notif = serde_json::json!({
            "jsonrpc": "2.0",
            "method": "logsNotification",
            "params": {
                "result": {
                    "value": {
                        "signature": "sig",
                        "err": null,
                        "logs": [
                            "Program 6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P invoke [1]",
                            format!("Program data: {}", b64),
                            "Program 6EF8rrecthR5Dkzon8Nwu78hRvfCKubJ14M5uBEwF6P success"
                        ]
                    }
                }
            }
        });

        let (tx, mut rx) = mpsc::channel::<FeedItem>(8);
        let n = forward_logs(&notif, &tx).await;
        assert_eq!(n, 1, "expected exactly one decoded pump.fun event");

        let (parsed, trader) = rx.recv().await.expect("event should be forwarded");
        assert_eq!(parsed.mint, bs58::encode(&mint).into_string());
        assert_eq!(trader, bs58::encode(&user).into_string());
        assert!(parsed.is_buy);
        assert_eq!(parsed.virtual_sol_reserves, 30_000_000_000);
        assert_eq!(parsed.virtual_token_reserves, 1_000_000_000_000_000);
    }

    #[tokio::test]
    async fn forward_logs_skips_failed_and_unrelated() {
        // failed tx → nothing
        let failed = serde_json::json!({
            "params": { "result": { "value": {
                "err": {"InstructionError": [0, "Custom"]},
                "logs": ["Program data: vdt/007mYe0000"]
            }}}
        });
        let (tx, _rx) = mpsc::channel::<FeedItem>(8);
        assert_eq!(forward_logs(&failed, &tx).await, 0);

        // unrelated logs → nothing
        let unrelated = serde_json::json!({
            "params": { "result": { "value": {
                "err": null,
                "logs": ["Program log: hello", "Program data: AAAA"]
            }}}
        });
        assert_eq!(forward_logs(&unrelated, &tx).await, 0);
    }
}
