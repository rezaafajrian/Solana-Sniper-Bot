use std::sync::Arc;
use std::str::FromStr;
use anyhow::{Result, anyhow};
use colored::Colorize;
use anchor_client::solana_client::nonblocking::rpc_client::RpcClient;
use anchor_client::solana_sdk::{
    instruction::Instruction,
    signature::Keypair,
    system_instruction,
    transaction::Transaction,
};
use std::env;
use anchor_client::solana_sdk::pubkey::Pubkey;
use spl_token::ui_amount_to_amount;
use solana_sdk::signature::Signer;
use tokio::time::{Instant, sleep};
use tokio::sync::Mutex;
use once_cell::sync::Lazy;
use reqwest::{Client, ClientBuilder};
use base64;
use bs58;
use std::time::Duration;
use crate::{
    common::{
        logger::Logger,
        config::TransactionLandingMode,
    },
    library::{
        zeroslot::{self, ZeroSlotClient},
    },
};
use dotenv::dotenv;

// prioritization fee = UNIT_PRICE * UNIT_LIMIT
fn get_unit_price() -> u64 {
    env::var("UNIT_PRICE")
        .ok()
        .and_then(|v| u64::from_str(&v).ok())
        .unwrap_or(20000)
}

fn get_unit_limit() -> u32 {
    env::var("UNIT_LIMIT")
        .ok()
        .and_then(|v| u32::from_str(&v).ok())
        .unwrap_or(200_000)
}


pub async fn new_signed_and_send_zeroslot(
    zeroslot_rpc_client: Arc<crate::library::zeroslot::ZeroSlotClient>,
    recent_blockhash: solana_sdk::hash::Hash,
    keypair: &Keypair,
    mut instructions: Vec<Instruction>,
    logger: &Logger,
) -> Result<Vec<String>> {
    let tip_account = zeroslot::get_tip_account()?;
    let start_time = Instant::now();
    let mut txs: Vec<String> = vec![];
    
    // zeroslot tip, the upper limit is 0.1
    let tip = zeroslot::get_tip_value().await?;
    let tip_lamports = ui_amount_to_amount(tip, spl_token::native_mint::DECIMALS);

    let zeroslot_tip_instruction = 
        system_instruction::transfer(&keypair.pubkey(), &tip_account, tip_lamports);
        
        let unit_limit = get_unit_limit(); // TODO: update in mev boost
        let unit_price = get_unit_price(); // TODO: update in mev boost
        let modify_compute_units =
        solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_limit(unit_limit);
        let add_priority_fee =
        solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_price(unit_price);
        instructions.insert(1, modify_compute_units);
        instructions.insert(2, add_priority_fee);
        
        instructions.push(zeroslot_tip_instruction); // zeroslot is different with others.
    // send init tx
    let txn = Transaction::new_signed_with_payer(
        &instructions,
        Some(&keypair.pubkey()),
        &vec![keypair],
        recent_blockhash,
    );

    let tx_result = zeroslot_rpc_client.send_transaction(&txn).await;
    
    match tx_result {
        Ok(signature) => {
            txs.push(signature.to_string());
            logger.log(
                format!("[TXN-ELAPSED(ZEROSLOT)]: {:?}", start_time.elapsed())
                    .yellow()
                    .to_string(),
            );
        }
        Err(_) => {
            // Convert the error to a Send-compatible form
            return Err(anyhow::anyhow!("zeroslot send_transaction failed"));
        }
    };

    Ok(txs)
}


pub async fn new_signed_and_send_zeroslot_fast(
    compute_unit_limit: u32,
    compute_unit_price: u64,
    tip_lamports: u64,
    zeroslot_rpc_client: Arc<crate::library::zeroslot::ZeroSlotClient>,
    recent_blockhash: solana_sdk::hash::Hash,
    keypair: &Keypair,
    mut instructions: Vec<Instruction>,
    logger: &Logger,
) -> Result<Vec<String>> {
    let tip_account = zeroslot::get_tip_account()?;
    let start_time = Instant::now();
    let mut txs: Vec<String> = vec![];
    
    // zeroslot tip, the upper limit is 0.1
    let tip = zeroslot::get_tip_value().await?;
    let tip_lamports = ui_amount_to_amount(tip, spl_token::native_mint::DECIMALS);

    let zeroslot_tip_instruction = 
        system_instruction::transfer(&keypair.pubkey(), &tip_account, tip_lamports);
        
        let unit_limit = get_unit_limit(); // TODO: update in mev boost
        let unit_price = get_unit_price(); // TODO: update in mev boost
        let modify_compute_units =
        solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_limit(unit_limit);
        let add_priority_fee =
        solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_price(unit_price);
        instructions.insert(1, modify_compute_units);
        instructions.insert(2, add_priority_fee);
        
        instructions.push(zeroslot_tip_instruction); // zeroslot is different with others.
    // send init tx
    let txn = Transaction::new_signed_with_payer(
        &instructions,
        Some(&keypair.pubkey()),
        &vec![keypair],
        recent_blockhash,
    );

    let tx_result = zeroslot_rpc_client.send_transaction(&txn).await;
    
    match tx_result {
        Ok(signature) => {
            txs.push(signature.to_string());
            logger.log(
                format!("[TXN-ELAPSED(ZEROSLOT)]: {:?}", start_time.elapsed())
                    .yellow()
                    .to_string(),
            );
        }
        Err(_) => {
            // Convert the error to a Send-compatible form
            return Err(anyhow::anyhow!("zeroslot send_transaction failed"));
        }
    };

    Ok(txs)
}

/// Send transaction using normal RPC without any service or tips
pub async fn new_signed_and_send_normal(
    rpc_client: Arc<anchor_client::solana_client::nonblocking::rpc_client::RpcClient>,
    recent_blockhash: anchor_client::solana_sdk::hash::Hash,
    keypair: &Keypair,
    mut instructions: Vec<Instruction>,
    logger: &Logger,
) -> Result<Vec<String>> {
    let start_time = Instant::now();
    
    // Add compute budget instructions for priority fee
    // let unit_limit = 200000;
    // let unit_price = 20000;
    // let modify_compute_units =
    //     solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_limit(unit_limit);
    // let add_priority_fee =
    //     solana_sdk::compute_budget::ComputeBudgetInstruction::set_compute_unit_price(unit_price);
    // instructions.insert(0, modify_compute_units);
    // instructions.insert(1, add_priority_fee);
    
    // Create and send transaction
    let txn = Transaction::new_signed_with_payer(
        &instructions,
        Some(&keypair.pubkey()),
        &vec![keypair],
        recent_blockhash,
    );

    match rpc_client.send_transaction(&txn).await {
        Ok(signature) => {
            logger.log(
                format!("[TXN-ELAPSED(NORMAL)]: {:?}", start_time.elapsed())
                    .yellow()
                    .to_string(),
            );
            Ok(vec![signature.to_string()])
        }
        Err(e) => Err(anyhow!("Failed to send normal transaction: {}", e))
    }
}

/// Universal transaction landing function that routes to the appropriate service
pub async fn new_signed_and_send_with_landing_mode(
    transaction_landing_mode: TransactionLandingMode,
    app_state: &crate::common::config::AppState,
    recent_blockhash: anchor_client::solana_sdk::hash::Hash,
    keypair: &Keypair,
    instructions: Vec<Instruction>,
    logger: &Logger,
) -> Result<Vec<String>> {
    // Route to the appropriate service
    match transaction_landing_mode {
        TransactionLandingMode::Zeroslot => {
            logger.log("Using Zeroslot for transaction landing".green().to_string());
            new_signed_and_send_zeroslot(
                app_state.zeroslot_rpc_client.clone(),
                recent_blockhash,
                keypair,
                instructions,
                logger,
            ).await
        },
        TransactionLandingMode::Normal => {
            logger.log("Using Normal RPC for transaction landing".green().to_string());
            new_signed_and_send_normal(
                app_state.rpc_nonblocking_client.clone(),
                recent_blockhash,
                keypair,
                instructions,
                logger,
            ).await
        },
    }
}


// ---------------------------------------------------------------------------
// Jito bundle landing + safe multi-route broadcast
// ---------------------------------------------------------------------------

/// Public Jito tip accounts (rotate to spread load).
pub const JITO_TIP_ACCOUNTS: [&str; 8] = [
    "96gYZGLnJYVFmbjzopPSU6QiEV5fGqZNyN9nmNhvrZU5",
    "HFqU5x63VTqvQss8hp11i4wVV8bD44PvwucfZ2bU7gRe",
    "Cw8CFyM9FkoMi7K7Crf6HNQqf4uEMzpKw6QNghXLvLkY",
    "ADaUMid9yfUytqMBgopwjb2DTLSokTSzL1zt6iGPaS49",
    "DfXygSm4jCyNCybVYYK6DwvWqjKee8pbDmJGcLWNDXjh",
    "ADuUkR4vqLUMWXxW9gh6D6L8pMSawimctcNZ5pGwDcEt",
    "DttWaMuVvTiduZRnguLF7jNxTgiMBZ1hyAumKUiL2KRL",
    "3AVi9Tg9Uo68tJfuvoKvqKNWKkC5wPdSSdeBnizKZ6jT",
];

fn jito_block_engine_url() -> String {
    env::var("JITO_BLOCK_ENGINE")
        .unwrap_or_else(|_| "https://mainnet.block-engine.jito.wtf/api/v1/bundles".to_string())
}

fn jito_tip_lamports() -> u64 {
    let sol = env::var("JITO_TIP_VALUE").ok().and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.001);
    (sol * 1_000_000_000.0) as u64
}

fn pick_jito_tip_account() -> Pubkey {
    let idx = (std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0) as usize) % JITO_TIP_ACCOUNTS.len();
    Pubkey::from_str(JITO_TIP_ACCOUNTS[idx]).expect("valid jito tip account")
}

/// POST a base58-encoded signed transaction to the Jito block engine as a bundle.
async fn submit_jito_bundle(tx_b58: &str, logger: &Logger) -> Result<()> {
    let body = serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "sendBundle", "params": [[tx_b58]]
    });
    let client = Client::builder().timeout(Duration::from_secs(5)).build().unwrap_or_else(|_| Client::new());
    let resp = client.post(jito_block_engine_url()).json(&body).send().await
        .map_err(|e| anyhow!("jito send failed: {}", e))?;
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() || text.contains("\"error\"") {
        return Err(anyhow!("jito bundle rejected ({}): {}", status, text));
    }
    logger.log(format!("Jito bundle accepted: {}", text).green().to_string());
    Ok(())
}

/// Land a transaction via a Jito bundle (adds a Jito tip instruction).
pub async fn new_signed_and_send_jito(
    recent_blockhash: anchor_client::solana_sdk::hash::Hash,
    keypair: &Keypair,
    mut instructions: Vec<Instruction>,
    logger: &Logger,
) -> Result<Vec<String>> {
    let start = Instant::now();
    instructions.push(system_instruction::transfer(&keypair.pubkey(), &pick_jito_tip_account(), jito_tip_lamports()));
    let txn = Transaction::new_signed_with_payer(&instructions, Some(&keypair.pubkey()), &vec![keypair], recent_blockhash);
    let sig = txn.signatures.first().map(|s| s.to_string()).unwrap_or_default();
    let wire = bincode::serialize(&txn).map_err(|e| anyhow!("serialize: {}", e))?;
    let b58 = bs58::encode(&wire).into_string();
    submit_jito_bundle(&b58, logger).await?;
    logger.log(format!("[TXN-ELAPSED(JITO)]: {:?}", start.elapsed()).yellow().to_string());
    Ok(vec![sig])
}

/// SAFE multi-route: build ONE Jito-tipped transaction and broadcast the *same*
/// signed bytes to both the Jito block engine and the normal RPC concurrently.
/// Because both carry the identical signature, at most one can execute — no
/// risk of double-buying — but two independent paths maximize the chance one lands.
pub async fn new_signed_and_send_multi(
    app_state: &crate::common::config::AppState,
    recent_blockhash: anchor_client::solana_sdk::hash::Hash,
    keypair: &Keypair,
    mut instructions: Vec<Instruction>,
    logger: &Logger,
) -> Result<Vec<String>> {
    let start = Instant::now();
    instructions.push(system_instruction::transfer(&keypair.pubkey(), &pick_jito_tip_account(), jito_tip_lamports()));
    let txn = Transaction::new_signed_with_payer(&instructions, Some(&keypair.pubkey()), &vec![keypair], recent_blockhash);
    let sig = txn.signatures.first().map(|s| s.to_string()).unwrap_or_default();
    let wire = bincode::serialize(&txn).map_err(|e| anyhow!("serialize: {}", e))?;
    let b58 = bs58::encode(&wire).into_string();

    let jito = submit_jito_bundle(&b58, logger);
    let rpc = app_state.rpc_nonblocking_client.send_transaction(&txn);
    let (jito_res, rpc_res) = tokio::join!(jito, rpc);

    let jito_ok = jito_res.is_ok();
    let rpc_ok = rpc_res.is_ok();
    if jito_ok || rpc_ok {
        logger.log(format!(
            "[TXN-ELAPSED(MULTI)]: {:?} | jito={} rpc={}", start.elapsed(), jito_ok, rpc_ok
        ).yellow().to_string());
        Ok(vec![sig])
    } else {
        Err(anyhow!("multi-route failed: jito={:?} rpc={:?}", jito_res.err(), rpc_res.err()))
    }
}
