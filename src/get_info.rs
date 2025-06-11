// src/utils.rs

use crate::net::http_client;
use anyhow::{Result, anyhow};
use std::time::Duration;
use std::str::FromStr;
use solana_client::rpc_client::RpcClient;
use solana_sdk::{
    pubkey::Pubkey,
    signature::{read_keypair_file, Signer},
    transaction::Transaction,
};
use reqwest::Client;
use serde_json::Value;
use orca_whirlpools_client::{
    Position, Whirlpool, UpdateFeesAndRewardsBuilder, get_tick_array_address,
};
use crate::params::{PoolConfig, RPC_URL, KEYPAIR_FILENAME};
use crate::types;
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
pub async fn fetch_pool_position_info(pool_cfg: &PoolConfig) -> Result<types::PoolPositionInfo> {
    // Загрузить keypair для подписи транзакции
    let keypair = read_keypair_file(KEYPAIR_FILENAME)
        .map_err(|e| anyhow!("Не удалось прочитать keypair: {}", e))?;
    let owner = keypair.pubkey();

    // Клиенты RPC и HTTP
    let rpc = RpcClient::new(RPC_URL.to_string());
    let http = http_client();

    // Адреса whirlpool и позиции
    let whirl_addr = Pubkey::from_str(pool_cfg.pool_address)?;
    let pos_addr   = Pubkey::from_str(pool_cfg.position_address)?;

    // Считать состояние whirlpool и позиции
    let whirl_acc = rpc.get_account(&whirl_addr)?;
    let whirl: Whirlpool = Whirlpool::from_bytes(&whirl_acc.data)?;
    let pos_acc = rpc.get_account(&pos_addr)?;
    let pos: Position = Position::from_bytes(&pos_acc.data)?;

    // Адреса тик-массивов для обновления сборов
    let span = whirl.tick_spacing as i32 * TICK_ARRAY_SIZE;
    let start_l = pos.tick_lower_index.div_euclid(span) * span;
    let start_u = pos.tick_upper_index.div_euclid(span) * span;
    let (ta_l, _) = get_tick_array_address(&whirl_addr, start_l)?;
    let (ta_u, _) = get_tick_array_address(&whirl_addr, start_u)?;

    // Собрать и отправить транзакцию UpdateFeesAndRewards
    let ix = UpdateFeesAndRewardsBuilder::new()
        .whirlpool(whirl_addr)
        .position(pos_addr)
        .tick_array_lower(ta_l)
        .tick_array_upper(ta_u)
        .instruction();
    let recent = rpc.get_latest_blockhash()?;
    let tx = Transaction::new_signed_with_payer(&[ix], Some(&owner), &[&keypair], recent);
    let _ = rpc.send_and_confirm_transaction(&tx);

    // Перечитать позицию с учётом начисленных сборов
    let pos_acc2 = rpc.get_account(&pos_addr)?;
    let pos2: Position = Position::from_bytes(&pos_acc2.data)?;

    // Pending yield
    let dec_a = pool_cfg.decimal_A as i32;
    let dec_b = pool_cfg.decimal_B as i32;
    let raw_fee_a = pos2.fee_owed_a;
    let raw_fee_b = pos2.fee_owed_b;
    let pending_a = raw_fee_a as f64 / 10f64.powi(dec_a);
    let pending_b = raw_fee_b as f64 / 10f64.powi(dec_b);

    // Текущая цена WSOL через Raydium API
    let url = format!("https://api-v3.raydium.io/mint/price?mints={}", pool_cfg.mint_A);
    let resp = http.get(&url).timeout(Duration::from_secs(10)).send().await?;
    if !resp.status().is_success() {
        return Err(anyhow!("Raydium API HTTP {}", resp.status()));
    }
    let json: Value = resp.json().await?;
    if !json.get("success").and_then(|v| v.as_bool()).unwrap_or(false) {
        return Err(anyhow!("Raydium API: success=false"));
    }
    let data = json["data"].as_object().ok_or_else(|| anyhow!("no data"))?;
    let price_str = data.get(pool_cfg.mint_A)
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("missing price"))?;
    let current_price: f64 = price_str.parse().map_err(|e| anyhow!("parse price: {}", e))?;

    // Эквивалент WSOL в USD и сумма всех pending в USD
    let pending_a_usd = pending_a * current_price;
    let sum = pending_a_usd + pending_b;

    // Объёмы в диапазоне
    let sqrt_p = whirl.sqrt_price as f64 / Q64;
    let sqrt_l = tick_price(pos.tick_lower_index / 2);
    let sqrt_u = tick_price(pos.tick_upper_index / 2);
    let L = pos2.liquidity as f64;
    let (raw_a, raw_b) = compute_amounts(L, sqrt_p, sqrt_l, sqrt_u);
    let amount_a = raw_a / 10f64.powi(dec_a);
    let amount_b = raw_b / 10f64.powi(dec_b);

    // Значения и проценты распределения
    let value_a = amount_a * current_price;
    let value_b = amount_b;
    let total = value_a + value_b;
    let pct_a = value_a / total * 100.0;
    let pct_b = value_b / total * 100.0;

    // Границы диапазона и относительные проценты
    let lower_price = full_price(pos.tick_lower_index, dec_a, dec_b);
    let upper_price = full_price(pos.tick_upper_index, dec_a, dec_b);
    let pct_down = (current_price - lower_price) / current_price * 100.0;
    let pct_up   = (upper_price - current_price) / current_price * 100.0;

    // Символы токенов из имени пула
    let symbols: Vec<&str> = pool_cfg.name.split('/').collect();
    let sym_a = symbols.get(0).copied().unwrap_or(pool_cfg.mint_A);
    let sym_b = symbols.get(1).copied().unwrap_or(pool_cfg.mint_B);

    // Печать в консоль
    println!("--- Pending Yield ---");
    println!(
        "{}: {:.*} (~${:.2})",
        sym_a,
        dec_a as usize,
        pending_a,
        pending_a_usd
    );
    println!("{}: {:.*}", sym_b, dec_b as usize, pending_b);
    println!("Sum (USD): ${:.2}", sum);

    println!("\n--- Distribution ---");
    println!("{}:  {:.*} (~${:.2})  ({:.1}%)",
        sym_a, dec_a as usize, amount_a, value_a, pct_a);
    println!("{}: {:.*} (~${:.2})  ({:.1}%)",
        sym_b, dec_b as usize, amount_b, value_b, pct_b);
    println!("\n--- Price & Range ---");
    println!("Price: ${:.6}", current_price);
    println!(
        "Range: [{:.6} – {:.6}] ({:.2}% ↓ / {:.2}% ↑)",
        lower_price, upper_price, pct_down, pct_up
    );

    Ok(types::PoolPositionInfo {
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
