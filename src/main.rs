mod telegram;
mod params;
mod utils;

use anyhow::{Result, anyhow};
use tokio::time::Duration;
use params::POOLS;
use reqwest::Client;
use serde_json::Value;
use solana_client::rpc_client::RpcClient;
use solana_sdk::{
    pubkey::Pubkey,
    signature::{read_keypair_file, Signer},
    transaction::Transaction,
    program_pack::Pack,
};
use spl_token::state::Mint;
use orca_whirlpools_client::{
    {Position, Whirlpool},
    UpdateFeesAndRewardsBuilder,
    get_tick_array_address,
};
use std::str::FromStr;

// Размер массива тиков Orca CLMM
const TICK_ARRAY_SIZE: i32 = 88;
// Константа Q64 = 2^64
const Q64: f64 = 1.8446744073709552e19;

/// Цена из тика (без учета десятичных):
/// price = 1.0001^tick
fn tick_price(tick: i32) -> f64 {
    1.0001_f64.powi(tick)
}

/// Цена WSOL/USDC с учетом десятичных:
/// price * 10^(dec_a - dec_b)
fn full_price(tick: i32, dec_a: i32, dec_b: i32) -> f64 {
    tick_price(tick) * 10f64.powi(dec_a - dec_b)
}

/// Вычисление балансов токенов из CLMM-liquidity
fn compute_amounts(liquidity: f64, sqrt_p: f64, sqrt_l: f64, sqrt_u: f64) -> (f64, f64) {
    if sqrt_p <= sqrt_l {
        // все токены в A
        let a = liquidity * (sqrt_u - sqrt_l) / (sqrt_l * sqrt_u);
        (a, 0.0)
    } else if sqrt_p < sqrt_u {
        let a = liquidity * (sqrt_u - sqrt_p) / (sqrt_p * sqrt_u);
        let b = liquidity * (sqrt_p - sqrt_l);
        (a, b)
    } else {
        // все токены в B
        (0.0, liquidity * (sqrt_u - sqrt_l))
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let pool_cfg = &POOLS[0];

    // 1) Загрузка ключей и клиентов
    let keypair = read_keypair_file(params::KEYPAIR_FILENAME)
        .map_err(|e| anyhow!("Не удалось прочитать keypair: {}", e))?;
    let owner = keypair.pubkey();
    let rpc   = RpcClient::new(params::RPC_URL.to_string());
    let http  = Client::new();

    // 2) Pubkey’и и decimals из конфига
    let whirl_addr = Pubkey::from_str(pool_cfg.pool_address)?;
    let pos_addr   = Pubkey::from_str(pool_cfg.position_address)?;
    let mint_a_str = pool_cfg.mint_A;
    let dec_a = pool_cfg.decimal_A as i32;
    let dec_b = pool_cfg.decimal_B as i32;

    // 3) Считываем Whirlpool и Position
    let whirl_acc = rpc.get_account(&whirl_addr)?;
    let whirl: Whirlpool = Whirlpool::from_bytes(&whirl_acc.data)?;
    let pos_acc   = rpc.get_account(&pos_addr)?;
    let pos: Position = Position::from_bytes(&pos_acc.data)?;

    // 4) On-chain update fees
    let span = whirl.tick_spacing as i32 * TICK_ARRAY_SIZE;
    let start_l = pos.tick_lower_index.div_euclid(span) * span;
    let start_u = pos.tick_upper_index.div_euclid(span) * span;
    let (ta_l, _) = get_tick_array_address(&whirl_addr, start_l)?;
    let (ta_u, _) = get_tick_array_address(&whirl_addr, start_u)?;
    let ix = UpdateFeesAndRewardsBuilder::new()
        .whirlpool(whirl_addr)
        .position(pos_addr)
        .tick_array_lower(ta_l)
        .tick_array_upper(ta_u)
        .instruction();
    let recent = rpc.get_latest_blockhash()?;
    let tx = Transaction::new_signed_with_payer(&[ix], Some(&owner), &[&keypair], recent);
    let _ = rpc.send_and_confirm_transaction(&tx);

    // 5) Повторно считаем Position для pending fees
    let pos_acc2 = rpc.get_account(&pos_addr)?;
    let pos2: Position = Position::from_bytes(&pos_acc2.data)?;
    let raw_fee_a = pos2.fee_owed_a;
    let raw_fee_b = pos2.fee_owed_b;
    let pending_a = raw_fee_a as f64 / 10f64.powi(dec_a);
    let pending_b = raw_fee_b as f64 / 10f64.powi(dec_b);

    // 6) Текущий sqrt_price из on-chain Q64.64
    let sqrt_p = whirl.sqrt_price as f64 / Q64;

    // 7) Границы sqrt
    let sqrt_l = tick_price(pos.tick_lower_index / 2); // 1.0001^(tick/2)
    let sqrt_u = tick_price(pos.tick_upper_index / 2);

    // 8) Расчёт балансов в атомарных единицах
    let L = pos2.liquidity as f64;
    let (raw_a, raw_b) = compute_amounts(L, sqrt_p, sqrt_l, sqrt_u);
    let amt_sol  = raw_a  / 10f64.powi(dec_a); // WSOL
    let amt_usdc = raw_b  / 10f64.powi(dec_b); // USDC

    // 9) Цена WSOL через Raydium API
    let url = format!("https://api-v3.raydium.io/mint/price?mints={}", mint_a_str);
    let resp = http.get(&url).timeout(Duration::from_secs(10)).send().await?;
    if !resp.status().is_success() {
        return Err(anyhow!("Raydium API HTTP {}", resp.status()));
    }
    let json: Value = resp.json().await?;
    if !json.get("success").and_then(|v| v.as_bool()).unwrap_or(false) {
        return Err(anyhow!("Raydium API: success=false"));
    }
    let data = json["data"].as_object().ok_or_else(|| anyhow!("no data"))?;
    let sol_price_str = data.get(mint_a_str).and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("missing price"))?;
    let sol_price: f64 = sol_price_str.parse().map_err(|e| anyhow!("parse price: {}", e))?;

    // 10) Вычисляем стоимости и доли
    let val_sol  = amt_sol  * sol_price;
    let val_usdc = amt_usdc;
    let total    = val_sol + val_usdc;
    let pct_sol  = val_sol / total * 100.0;
    let pct_usdc = val_usdc / total * 100.0;

    // 11) Границы цен
    let price_l = full_price(pos.tick_lower_index, dec_a, dec_b);
    let price_u = full_price(pos.tick_upper_index, dec_a, dec_b);

    // 12) Проценты отклонения
    let pct_down = (sol_price - price_l)/sol_price*100.0;
    let pct_up   = (price_u - sol_price)/sol_price*100.0;

    // 13) Печать
    println!("--- Pending Yield ---");
    println!("WSOL: {:.9}", pending_a);
    println!("USDC: {:.6}", pending_b);
    println!("\n--- Distribution ---");
    println!("SOL:  {:.9} (~${:.2})  ({:.1}%)", amt_sol, val_sol, pct_sol);
    println!("USDC: {:.6} ( ${:.2})  ({:.1}%)", amt_usdc, val_usdc, pct_usdc);
    println!("\n--- Price & Range ---");
    println!("Price: ${:.6}", sol_price);
    println!(
        "Range: [{:.6} – {:.6}] ({:.2}% ↓ / {:.2}% ↑)",
        price_l, price_u, pct_down, pct_up
    );

    Ok(())
}

