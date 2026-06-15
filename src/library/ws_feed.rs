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
use crate::processor::transaction_parser::{decode_pumpfun_event, TradeInfoFromToken};

/// One parsed pump.fun trade plus the transaction's fee-payer (trader) address.
pub type FeedItem = (TradeInfoFromToken, String);

fn subscribe_msg() -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": 1,
        "method": "blockSubscribe",
        "params": [
            { "mentionsAccountOrProgram": PUMP_FUN_PROGRAM },
            {
                "commitment": "confirmed",
                "encoding": "json",
                "transactionDetails": "full",
                "maxSupportedTransactionVersion": 0,
                "showRewards": false
            }
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
                logger.log("📡 Websocket feed connected (blockSubscribe → pump.fun)".green().to_string());

                let mut msg_count: u64 = 0;
                let mut event_count: u64 = 0;
                let mut logged_first = false;
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
                                event_count += forward_block(&v, &tx).await as u64;
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

/// Pull pump.fun trades out of a blockNotification value and forward them.
/// Returns the number of events forwarded.
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
                if !matches!(bytes.len(), 266 | 170 | 138) {
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
