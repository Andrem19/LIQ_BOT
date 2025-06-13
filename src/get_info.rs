// src/utils.rs


use std::time::Duration;
use std::{str::FromStr, result::Result as StdResult};

use anyhow::{anyhow, Result};
use reqwest::Client as HttpClient;
use serde_json::Value;
use solana_client::rpc_client::RpcClient;
use solana_sdk::{
    pubkey::Pubkey,
    signature::{read_keypair_file, Keypair},
    transaction::Transaction,
};
use solana_sdk::signature::Signer;
use orca_whirlpools_client::{
    get_tick_array_address, Position, UpdateFeesAndRewardsBuilder, Whirlpool,
};
use crate::{
    params::{KEYPAIR_FILENAME, RPC_URL},
    types::PoolPositionInfo,
};
const TICK_ARRAY_SIZE: i32 = 88;
const Q64: f64 = 1.8446744073709552e19;

/// Цена из тика
fn tick_price(tick: i32) -> f64 {
    1.0001_f64.powi(tick)
}

/// Полная цена с учётом децималей токенов
fn full_price(tick: i32, dec_a: i32, dec_b: i32) -> f64 {
    tick_price(tick) * 10f64.powi(dec_a - dec_b)
}

/// Рассчёт объёмов токенов внутри текущего диапазона
fn compute_amounts(liquidity: f64, sqrt_p: f64, sqrt_l: f64, sqrt_u: f64) -> (f64, f64) {
    if sqrt_p <= sqrt_l {
        let a = liquidity * (sqrt_u - sqrt_l) / (sqrt_l * sqrt_u);
        (a, 0.0)
    } else if sqrt_p < sqrt_u {
        let a = liquidity * (sqrt_u - sqrt_p) / (sqrt_p * sqrt_u);
        let b = liquidity * (sqrt_p - sqrt_l);
        (a, b)
    } else {
        (0.0, liquidity * (sqrt_u - sqrt_l))
    }
}

/// Получить и распечатать всю информацию по открытой позиции.
///
/// Принимает конфиг пула и возвращает struct с теми же полями, что выводятся в консоль.
pub async fn fetch_pool_position_info(pool_cfg: &crate::params::PoolConfig) -> Result<PoolPositionInfo> {
    // --- 1) Читать keypair и RPC/HTTP клиенты
    let keypair = read_keypair_file(KEYPAIR_FILENAME)
        .map_err(|e| anyhow!("Не удалось прочитать keypair: {}", e))?;
    let owner = keypair.pubkey();
    let rpc = RpcClient::new(RPC_URL.to_string());
    let http = HttpClient::new();

    // --- 2) Считать Whirlpool, чтобы знать tick_spacing и Q64
    let whirl_addr = Pubkey::from_str(pool_cfg.pool_address)?;
    let whirl_acc = rpc
        .get_account(&whirl_addr)
        .map_err(|e| anyhow!("RPC get_account whirlpool: {}", e))?;
    let whirl: Whirlpool = Whirlpool::from_bytes(&whirl_acc.data)?;

    // --- 3) Всегда получаем текущий price через Raydium API
    let url = format!("https://api-v3.raydium.io/mint/price?mints={}", pool_cfg.mint_A);
    let resp = http
        .get(&url)
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .map_err(|e| anyhow!("HTTP запрос Raydium: {}", e))?;
    if !resp.status().is_success() {
        return Err(anyhow!("Raydium API HTTP {}", resp.status()));
    }
    let json: Value = resp
        .json()
        .await
        .map_err(|e| anyhow!("Не удалось распарсить JSON Raydium: {}", e))?;
    if !json.get("success").and_then(|v| v.as_bool()).unwrap_or(false) {
        return Err(anyhow!("Raydium API: success=false"));
    }
    let data = json
        .get("data")
        .and_then(|v| v.as_object())
        .ok_or_else(|| anyhow!("Raydium API: missing data field"))?;
    let price_str = data
        .get(pool_cfg.mint_A)
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("Raydium API: missing mint price"))?;
    let current_price: f64 = price_str
        .parse()
        .map_err(|e| anyhow!("Не удалось распарсить цену: {}", e))?;

    // --- 4) Подготовить дефолтные значения — если позиции нет, они так и останутся
    let mut pending_a = 0.0;
    let mut pending_b = 0.0;
    let mut pending_a_usd = 0.0;
    let mut sum = 0.0;
    let mut amount_a = 0.0;
    let mut amount_b = 0.0;
    let mut value_a = 0.0;
    let mut value_b = 0.0;
    let mut pct_a = 0.0;
    let mut pct_b = 0.0;
    let mut lower_price = current_price;
    let mut upper_price = current_price;
    let mut pct_down = 0.0;
    let mut pct_up = 0.0;

    // --- 5) Если у нас задан address позиции, пытаемся её обновить
    if let Some(addr_str) = pool_cfg.position_address {
        if let Ok(pos_addr) = Pubkey::from_str(addr_str) {
            if let Ok(pos_acc) = rpc.get_account(&pos_addr) {
                // --- 5.1) Считать старую позицию и посчитать накопленные fees
                let pos: Position = Position::from_bytes(&pos_acc.data)?;
                let span = whirl.tick_spacing as i32 * TICK_ARRAY_SIZE;
                let start_l = pos.tick_lower_index.div_euclid(span) * span;
                let start_u = pos.tick_upper_index.div_euclid(span) * span;

                let (ta_l, _) = get_tick_array_address(&whirl_addr, start_l)?;
                let (ta_u, _) = get_tick_array_address(&whirl_addr, start_u)?;

                // Собираем и отправляем UpdateFeesAndRewards
                let ix = UpdateFeesAndRewardsBuilder::new()
                    .whirlpool(whirl_addr)
                    .position(pos_addr)
                    .tick_array_lower(ta_l)
                    .tick_array_upper(ta_u)
                    .instruction();
                let recent = rpc.get_latest_blockhash()?;
                let tx = Transaction::new_signed_with_payer(&[ix], Some(&owner), &[&keypair], recent);
                let _ = rpc.send_and_confirm_transaction(&tx);

                // Перечитать позицию с учётом fees
                let pos2_acc = rpc.get_account(&pos_addr)?;
                let pos2: Position = Position::from_bytes(&pos2_acc.data)?;

                // --- 5.2) Fee owing в токенах и в USD
                let dec_a = pool_cfg.decimal_A as i32;
                let dec_b = pool_cfg.decimal_B as i32;
                pending_a = pos2.fee_owed_a as f64 / 10f64.powi(dec_a);
                pending_b = pos2.fee_owed_b as f64 / 10f64.powi(dec_b);
                pending_a_usd = pending_a * current_price;
                sum = pending_a_usd + pending_b;

                // --- 5.3) Текущий состав позиции в диапазоне
                let sqrt_p = whirl.sqrt_price as f64 / Q64;
                let sqrt_l = tick_price(pos.tick_lower_index / 2);
                let sqrt_u = tick_price(pos.tick_upper_index / 2);
                let L = pos2.liquidity as f64;
                let (raw_a, raw_b) = compute_amounts(L, sqrt_p, sqrt_l, sqrt_u);
                amount_a = raw_a / 10f64.powi(dec_a);
                amount_b = raw_b / 10f64.powi(dec_b);
                value_a = amount_a * current_price;
                value_b = amount_b;
                let total = value_a + value_b;
                if total > 0.0 {
                    pct_a = value_a / total * 100.0;
                    pct_b = value_b / total * 100.0;
                }

                // --- 5.4) Границы диапазона и проценты от текущей цены
                lower_price = full_price(pos.tick_lower_index, dec_a, dec_b);
                upper_price = full_price(pos.tick_upper_index, dec_a, dec_b);
                pct_down = (current_price - lower_price) / current_price * 100.0;
                pct_up = (upper_price - current_price) / current_price * 100.0;
            }
        }
    }

    // --- 6) Печать в консоль
    println!("--- Pool Position Info for {} ---", pool_cfg.name);
    println!("Price: ${:.6}", current_price);
    println!("Pending A: {:.6} (~${:.6}), Pending B: {:.6}", pending_a, pending_a_usd, pending_b);
    println!(
        "Position: A={:.6} (~${:.6}), B={:.6} ({}% / {}%), Range [{:.6}–{:.6}] (↓{:.2}% ↑{:.2}%)",
        amount_a, value_a, amount_b, pct_a, pct_b, lower_price, upper_price, pct_down, pct_up
    );

    // --- 7) Вернуть собранную структуру
    Ok(PoolPositionInfo {
        pending_a,
        pending_b,
        pending_a_usd,
        sum,
        amount_a,
        amount_b,
        value_a,
        value_b,
        pct_a,
        pct_b,
        current_price,
        lower_price,
        upper_price,
        pct_down,
        pct_up,
    })
}
