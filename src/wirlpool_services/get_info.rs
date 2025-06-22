// src/utils.rs


use std::time::Duration;
use std::{str::FromStr, result::Result as StdResult};

use anyhow::{anyhow, Result};
use reqwest::Client as HttpClient;
use crate::utils::utils;
use serde_json::Value;
use solana_client::nonblocking::rpc_client::RpcClient;
use orca_whirlpools_core::U128;
use orca_whirlpools_core::tick_index_to_price;

use orca_whirlpools_core::sqrt_price_to_price;
use crate::utils::safe_get_account;
use solana_sdk::{
    pubkey::Pubkey,
    signature::{read_keypair_file, Keypair},
    transaction::Transaction,
};
use solana_sdk::signature::Signer;
use orca_whirlpools_client::{
    get_tick_array_address, Position, UpdateFeesAndRewardsBuilder, Whirlpool,
};
use crate::types::PoolConfig;
use crate::{
    params::{KEYPAIR_FILENAME, RPC_URL},
    types::PoolPositionInfo,
};

const Q64: f64 = 1.8446744073709552e19;

/// Цена из тика
fn tick_price(tick: i32) -> f64 {
    1.0001_f64.powi(tick)
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
pub async fn fetch_pool_position_info(
    pool_cfg: &PoolConfig,
    position_address: Option<&str>,
) -> anyhow::Result<PoolPositionInfo> {

    //------------------------------------------------------------------//
    // 0. RPC, Whirlpool, базовые величины                               //
    //------------------------------------------------------------------//
    let rpc       = utils::init_rpc();
    let whirl_pk  = Pubkey::from_str(pool_cfg.pool_address)?;
    let whirl_acc = safe_get_account(&rpc, &whirl_pk).await?;
    let whirl     = orca_whirlpools_client::Whirlpool::from_bytes(&whirl_acc.data)?;

    let dec_a = pool_cfg.decimal_a as u8;          // SOL = 9
    let dec_b = pool_cfg.decimal_b as u8;          // USDC/RAY/WETH/WBTC …

    // базовая цена «B per A»
    let price_ab = sqrt_price_to_price(U128::from(whirl.sqrt_price), dec_a, dec_b);

    // для отображения ‒ инвертируем всё, кроме SOL/USDC
    let disp_invert   = pool_cfg.name != "SOL/USDC";
    let display_price = if disp_invert { 1.0 / price_ab } else { price_ab };

    //------------------------------------------------------------------//
    // 1. Значения «по-умолчанию», если позиции нет                      //
    //------------------------------------------------------------------//
    let mut info = PoolPositionInfo {
        pending_a:     0.0,
        pending_b:     0.0,
        pending_a_usd: 0.0,
        sum:           0.0,
        amount_a:      0.0,
        amount_b:      0.0,
        value_a:       0.0,
        value_b:       0.0,
        pct_a:         0.0,
        pct_b:         0.0,
        current_price: display_price,
        lower_price:   display_price,
        upper_price:   display_price,
        pct_down:      0.0,
        pct_up:        0.0,
    };

    //------------------------------------------------------------------//
    // 2. Читаем on-chain позицию (если передан её mint)                 //
    //------------------------------------------------------------------//
    if let Some(addr_str) = position_address {
        let pos_pk = Pubkey::from_str(addr_str)?;

        // 2.1 принудительно обновляем fee-счётчики (иначе они «старые»)
        refresh_fees(&rpc, whirl_pk, pos_pk, whirl.tick_spacing as i32).await?;

        // 2.2 читаем аккаунт ещё раз – поля fee_owed_* уже свежие
        let pos_acc = safe_get_account(&rpc, &pos_pk).await?;
        let pos     = Position::from_bytes(&pos_acc.data)?;

        //---------------------- комиссии ------------------------------//
        info.pending_a = pos.fee_owed_a as f64 / 10_f64.powi(dec_a as i32);
        info.pending_b = pos.fee_owed_b as f64 / 10_f64.powi(dec_b as i32);

        // переводим **в доллары** без лишних допущений
        let sol_usd  = get_sol_price_usd().await.unwrap_or(0.0);
        let tokb_usd = if pool_cfg.name == "SOL/USDC" {
            1.0                            // B = USDC
        } else {
            sol_usd / price_ab.max(1e-12)  // $/B  = $/SOL / (B/SOL)
        };

        info.pending_a_usd = info.pending_a * sol_usd;
        let pending_b_usd  = info.pending_b * tokb_usd;
        info.sum           = info.pending_a_usd + pending_b_usd;

        //---------------------- состав позиции ------------------------//
        let sqrt_p = whirl.sqrt_price as f64 / Q64;
        let sqrt_l = tick_price(pos.tick_lower_index / 2);
        let sqrt_u = tick_price(pos.tick_upper_index / 2);
        let (raw_a, raw_b) =
            compute_amounts(pos.liquidity as f64, sqrt_p, sqrt_l, sqrt_u);

        info.amount_a = raw_a / 10_f64.powi(dec_a as i32);
        info.amount_b = raw_b / 10_f64.powi(dec_b as i32);
        info.value_a  = info.amount_a * sol_usd;   // в USD
        info.value_b  = info.amount_b * tokb_usd;
        let total_usd = info.value_a + info.value_b;
        if total_usd > 0.0 {
            info.pct_a = info.value_a / total_usd * 100.0;
            info.pct_b = 100.0 - info.pct_a;
        }

        //---------------------- диапазон ------------------------------//
        let lo_raw = tick_index_to_price(pos.tick_lower_index, dec_a, dec_b);
        let hi_raw = tick_index_to_price(pos.tick_upper_index, dec_a, dec_b);

        if disp_invert {
            info.lower_price = 1.0 / hi_raw;
            info.upper_price = 1.0 / lo_raw;
        } else {
            info.lower_price = lo_raw;
            info.upper_price = hi_raw;
        }

        info.pct_down = (info.current_price - info.lower_price)
                      / info.current_price * 100.0;
        info.pct_up   = (info.upper_price   - info.current_price)
                      / info.current_price * 100.0;
    }

    //------------------------------------------------------------------//
    // 3. Лог (оставили без изменений)                                  //
    //------------------------------------------------------------------//
    println!("--- Pool Position Info for {} ---", pool_cfg.name);
    println!("Price (display): {:.6}", info.current_price);
    println!(
        "Range [{:.6} – {:.6}] (↓{:.2}% ↑{:.2}%)",
        info.lower_price, info.upper_price, info.pct_down, info.pct_up
    );
    println!(
        "Position: A={:.6}, B={:.6}  ({}% / {}%)",
        info.amount_a, info.amount_b, info.pct_a, info.pct_b
    );
    println!(
        "Unclaimed: A={:.10} SOL (≈${:.6}), B={:.10} (≈${:.6})",
        info.pending_a, info.pending_a_usd,
        info.pending_b, info.sum - info.pending_a_usd
    );

    Ok(info)
}
// pub async fn fetch_pool_position_info(
//     pool_cfg: &PoolConfig,
//     position_address: Option<&str>,
// ) -> Result<PoolPositionInfo> {

//     //------------------------------------------------------------------//
//     // 0. RPC, Whirlpool, базовые величины                               //
//     //------------------------------------------------------------------//
//     let rpc       = utils::init_rpc();
//     let whirl_pk  = Pubkey::from_str(pool_cfg.pool_address)?;
//     let whirl_acc = safe_get_account(&rpc, &whirl_pk).await?;
//     let whirl     = Whirlpool::from_bytes(&whirl_acc.data)?;

//     let dec_a = pool_cfg.decimal_a as u8;
//     let dec_b = pool_cfg.decimal_b as u8;

//     // базовая цена (B per A)
//     let price_ab = sqrt_price_to_price(U128::from(whirl.sqrt_price), dec_a, dec_b);

//     // для отображения инвертируем всё, кроме SOL/USDC
//     let disp_invert   = pool_cfg.name != "SOL/USDC";
//     let display_price = if disp_invert { 1.0 / price_ab } else { price_ab };

//     //------------------------------------------------------------------//
//     // 1. Значения «по-умолчанию», если позиции нет                      //
//     //------------------------------------------------------------------//
//     let mut info = PoolPositionInfo {
//         pending_a:      0.0,
//         pending_b:      0.0,
//         pending_a_usd:  0.0,  // для non-USDC – «в токенах B»
//         sum:            0.0,
//         amount_a:       0.0,
//         amount_b:       0.0,
//         value_a:        0.0,
//         value_b:        0.0,
//         pct_a:          0.0,
//         pct_b:          0.0,
//         current_price:  display_price,
//         lower_price:    display_price,
//         upper_price:    display_price,
//         pct_down:       0.0,
//         pct_up:         0.0,
//     };

//     //------------------------------------------------------------------//
//     // 2. Если передан mint позиции – читаем on-chain данные             //
//     //------------------------------------------------------------------//
//     if let Some(addr_str) = position_address {
//         let pos_pk = Pubkey::from_str(addr_str)?;
//         let pos_acc = safe_get_account(&rpc, &pos_pk).await?;
//         let pos     = Position::from_bytes(&pos_acc.data)?;

//         //------------------------ fees ---------------------------------//
//         info.pending_a = pos.fee_owed_a as f64 / 10_f64.powi(dec_a as i32);
//         info.pending_b = pos.fee_owed_b as f64 / 10_f64.powi(dec_b as i32);
//         info.pending_a_usd = info.pending_a * price_ab;   // B-эквивалент
//         info.sum = info.pending_a_usd + info.pending_b;

//         //---------------- liquidity / состав ---------------------------//
//         let sqrt_p = whirl.sqrt_price as f64 / Q64;
//         let sqrt_l = tick_price(pos.tick_lower_index / 2);
//         let sqrt_u = tick_price(pos.tick_upper_index / 2);
//         let (raw_a, raw_b) =
//             compute_amounts(pos.liquidity as f64, sqrt_p, sqrt_l, sqrt_u);

//         info.amount_a = raw_a / 10_f64.powi(dec_a as i32);
//         info.amount_b = raw_b / 10_f64.powi(dec_b as i32);
//         info.value_a  = info.amount_a * price_ab;   // в токенах B
//         info.value_b  = info.amount_b;
//         let total_b   = info.value_a + info.value_b;
//         if total_b > 0.0 {
//             info.pct_a = info.value_a / total_b * 100.0;
//             info.pct_b = 100.0 - info.pct_a;
//         }

//         //---------------- диапазон (B per A) ---------------------------//
//         let lo_raw = tick_index_to_price(pos.tick_lower_index, dec_a, dec_b);
//         let hi_raw = tick_index_to_price(pos.tick_upper_index, dec_a, dec_b);

//         // перевод в «человеческий» вид
//         if disp_invert {
//             info.lower_price = 1.0 / hi_raw;
//             info.upper_price = 1.0 / lo_raw;
//         } else {
//             info.lower_price = lo_raw;
//             info.upper_price = hi_raw;
//         }

//         info.pct_down = (info.current_price - info.lower_price)
//                       / info.current_price * 100.0;
//         info.pct_up   = (info.upper_price   - info.current_price)
//                       / info.current_price * 100.0;
//     }

//     //------------------------------------------------------------------//
//     // 3. Лог в stdout – удобно для отладки                             //
//     //------------------------------------------------------------------//
//     println!("--- Pool Position Info for {} ---", pool_cfg.name);
//     println!("Price (display): {:.6}", info.current_price);
//     println!(
//         "Range [{:.6} – {:.6}] (↓{:.2}% ↑{:.2}%)",
//         info.lower_price, info.upper_price, info.pct_down, info.pct_up
//     );
//     println!(
//         "Position: A={:.6}, B={:.6}  ({}% / {}%)",
//         info.amount_a, info.amount_b, info.pct_a, info.pct_b
//     );
//     println!(
//         "Unclaimed: A={:.10} (≈{:.10} B), B={:.10}",
//         info.pending_a, info.pending_a_usd, info.pending_b
//     );

//     Ok(info)
// }

pub async fn refresh_fees(
    rpc: &RpcClient,
    whirlpool: Pubkey,
    position:  Pubkey,
    tick_spacing: i32,
) -> anyhow::Result<()> {
    const TICK_ARRAY_SIZE: i32 = 88;   // из SDK

    let payer = utils::load_wallet()?;          // ваш helper
    let pos_acc = rpc.get_account(&position).await
    .map_err(|e| anyhow!("get_account failed: {e}"))?;
    let pos     = Position::from_bytes(&pos_acc.data)?;

    let span   = tick_spacing * TICK_ARRAY_SIZE;
    let start_l = pos.tick_lower_index.div_euclid(span) * span;
    let start_u = pos.tick_upper_index.div_euclid(span) * span;

    let (ta_l, _) = get_tick_array_address(&whirlpool, start_l)?;
    let (ta_u, _) = get_tick_array_address(&whirlpool, start_u)?;

    let ix = UpdateFeesAndRewardsBuilder::new()
        .whirlpool(whirlpool)
        .position(position)
        .tick_array_lower(ta_l)
        .tick_array_upper(ta_u)
        .instruction();

    let recent = rpc.get_latest_blockhash().await
    .map_err(|e| anyhow!("get_latest_blockhash failed: {e}"))?;

    let tx     = Transaction::new_signed_with_payer(
        &[ix],
        Some(&payer.pubkey()),
        &[&payer],
        recent,
    );

    rpc.send_and_confirm_transaction(&tx).await
    .map_err(|e| anyhow!("send_and_confirm_transaction failed: {e}"))?;
    Ok(())
}

pub async fn get_sol_price_usd() -> anyhow::Result<f64> {
    use reqwest::Client;
    use serde_json::Value;

    // ---------- 1) CoinGecko -------------------------------------------
    let url_gecko = "https://api.coingecko.com/api/v3/simple/price?ids=solana&vs_currencies=usd";
    if let Ok(resp) = Client::new().get(url_gecko).send().await {
        if resp.status().is_success() {
            let v: Value = resp.json().await?;
            if let Some(p) = v["solana"]["usd"].as_f64() {
                return Ok(p);
            }
        }
    }

    // ---------- 2) Coinbase (fallback) ---------------------------------
    #[derive(serde::Deserialize)]
    struct CbResp { data: CbData }
    #[derive(serde::Deserialize)]
    struct CbData { amount: String }

    let url_cb = "https://api.coinbase.com/v2/prices/SOL-USD/spot";
    let resp: CbResp = Client::new().get(url_cb).send().await?.json().await?;
    let price: f64 = resp.data.amount.parse()?;
    Ok(price)
}