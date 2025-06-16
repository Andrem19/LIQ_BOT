//! Swap через агрегатор Jupiter (mainnet-beta).
//! С учётом исправления тела запроса к /swap.

use std::str::FromStr;

use anyhow::{anyhow, bail, Result};
use base64::{engine::general_purpose::STANDARD as b64, Engine as _};
use bincode::deserialize;
use serde_json::{self, Value};
use solana_client::{
    rpc_client::RpcClient,
    rpc_config::RpcSendTransactionConfig,
};
use solana_sdk::{
    commitment_config::CommitmentConfig,
    pubkey::Pubkey,
    signature::{read_keypair_file, Keypair, Signer},
    transaction::Transaction,
};

use spl_associated_token_account::get_associated_token_address;

use crate::wirlpool_services::net::http_client;
use crate::params::{KEYPAIR_FILENAME, RPC_URL};
use crate::types::PoolConfig;

/// Итог свопа
pub struct SwapResult {
    pub balance_sell: f64,
    pub balance_buy:  f64,
}

/// Получить баланс токена (u64) по ATA.
fn get_token_balance_u64(rpc: &RpcClient, ata: &Pubkey) -> Result<u64> {
    Ok(rpc
        .get_token_account_balance(ata)?
        .amount
        .parse::<u64>()?)
}

/// Отправка legacy-транзакции, подписанной `signer`.
fn send_signed_tx(
    rpc: &RpcClient,
    mut tx: Transaction,
    signer: &Keypair,
) -> Result<()> {
    let bh = tx.message.recent_blockhash;
    tx.sign(&[signer], bh);

    rpc.send_and_confirm_transaction_with_spinner_and_config(
        &tx,
        CommitmentConfig::confirmed(),
        RpcSendTransactionConfig {
            skip_preflight: false,
            ..RpcSendTransactionConfig::default()
        },
    )
    .map_err(|e| anyhow!("send tx: {e}"))?;
    Ok(())
}

/// Основной API свопа через Jupiter.
pub async fn execute_swap(
    _pool_cfg: &PoolConfig,  // для совместимости
    sell_mint: &str,         // что продаём
    buy_mint:  &str,         // что покупаем
    amount_usd: f64,         // если WSOL — сумма в USD, иначе — кол-во USDC
) -> Result<SwapResult> {
    // 0) Кошелёк и клиенты
    let payer: Keypair = read_keypair_file(KEYPAIR_FILENAME)
        .map_err(|e| anyhow!("keypair: {e}"))?;
    let wallet = payer.pubkey();
    let rpc    = RpcClient::new(RPC_URL.to_string());
    let http   = http_client();

    // 1) Поддерживаем только WSOL и USDC
    let (in_mint, in_dec) = match sell_mint {
        "So11111111111111111111111111111111111111112" => (Pubkey::from_str(sell_mint)?, 9),
        "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v" => (Pubkey::from_str(sell_mint)?, 6),
        _ => bail!("sell_mint не поддерживается"),
    };
    let (out_mint, out_dec) = match buy_mint {
        "So11111111111111111111111111111111111111112" => (Pubkey::from_str(buy_mint)?, 9),
        "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v" => (Pubkey::from_str(buy_mint)?, 6),
        _ => bail!("buy_mint не поддерживается"),
    };

    // 2) Считаем amount_in в атомах
    let amount_in_atoms = if sell_mint == "So11111111111111111111111111111111111111112" {
        // Цена SOL
        let resp = http
            .get("https://lite-api.jup.ag/price/v2?ids=So11111111111111111111111111111111111111112")
            .send().await?;
        let status = resp.status();
        let body   = resp.text().await?;
        println!("{:?}", body);
        println!("DEBUG price status={} body={}", status, body);
        let price_json: Value = serde_json::from_str(&body)
            .map_err(|e| anyhow!("Price JSON parse error: {} (body={})", e, body))?;
        let price_str = price_json["data"]["So11111111111111111111111111111111111111112"]["price"]
        .as_str()
        .ok_or_else(|| anyhow!("no SOL price string in response"))?;
        let sol_price: f64 = price_str.parse()
            .map_err(|e| anyhow!("parse SOL price error: {}", e))?;

        // 4) считаем, сколько атомов WSOL нужно под amount_usd
        let sol_qty = amount_usd / sol_price;
        (sol_qty * 10f64.powi(in_dec as i32)) as u64
    } else {
        // Для USDC: amount_usd = кол-во токена
        (amount_usd * 10f64.powi(in_dec as i32)) as u64
    };

    // 3) Quote
    let quote_url = format!(
        "https://quote-api.jup.ag/v6/quote?inputMint={}&outputMint={}&amount={}&slippageBps=50&onlyDirectRoutes=true",
        in_mint, out_mint, amount_in_atoms
    );
    let resp = http.get(&quote_url).send().await?;
    let status = resp.status();
    let body   = resp.text().await?;
    println!("DEBUG quote status={} body={}", status, body);
    let quote_json: Value = serde_json::from_str(&body)
        .map_err(|e| anyhow!("Quote JSON parse error: {} (body={})", e, body))?;
    // будем передавать весь ответ в поле quoteResponse
    let quote_response = quote_json.clone();

    // 4) Build swap (legacy tx) с правильным полем quoteResponse
    let swap_body = serde_json::json!({
        "quoteResponse": quote_response,
        "userPublicKey": wallet.to_string(),
        "wrapAndUnwrapSol": true,
        "asLegacyTransaction": true
    });

    let resp = http
        .post("https://quote-api.jup.ag/v6/swap")
        .json(&swap_body)
        .send()
        .await?;
    let status = resp.status();
    let body   = resp.text().await?;
    println!("DEBUG swap status={} body={}", status, body);

    // Теперь разбор
    let swap_json: Value = serde_json::from_str(&body)
        .map_err(|e| anyhow!("Swap JSON parse error: {} (body={})", e, body))?;
    let swap_tx_b64 = swap_json["swapTransaction"]
        .as_str()
        .ok_or_else(|| anyhow!("Jupiter: нет swapTransaction in response"))?;

    // 5) Декодим, подписываем и отправляем
    let tx_bytes = b64.decode(swap_tx_b64)?;
    let tx: Transaction = deserialize(&tx_bytes)
        .map_err(|e| anyhow!("bincode deserialize error: {}", e))?;
    let send_res = send_signed_tx(&rpc, tx, &payer);
    if let Err(err) = send_res {
        let msg = err.to_string();
        // если это именно наша «виртуальная» ошибка от Jupiter wrap/close
        if msg.contains("could not find account") {
            // просто логируем и продолжаем дальше — SOL у нас уже на балансе
            eprintln!("⚠️  warning: Jupiter wrapper error ignored: {}", msg);
        } else {
            // а остальные — прокидываем дальше
            return Err(err);
        }
    }

    // 6) Финальные балансы
    let ata_in  = get_associated_token_address(&wallet, &in_mint);
    let ata_out = get_associated_token_address(&wallet, &out_mint);
    let bal_in  = get_token_balance_u64(&rpc, &ata_in)?  as f64 / 10f64.powi(in_dec as i32);
    let bal_out = get_token_balance_u64(&rpc, &ata_out)? as f64 / 10f64.powi(out_dec as i32);

    Ok(SwapResult {
        balance_sell: bal_in,
        balance_buy:  bal_out,
    })
}
