// src/main.rs

mod params;
mod utils;
mod telegram;

use anyhow::Result;
use std::str::FromStr;
use tokio::select;
use tokio::time::{interval, Instant};
use solana_client::rpc_client::RpcClient;
use solana_sdk::signature::Keypair;
use solana_sdk::pubkey::Pubkey;
use raydium_amm_v3::util::tick_math::tick_to_price;

use crate::params::{POOLS, CHECK_INTERVAL, REPORT_INTERVAL, RANGE_HALF};
use crate::utils::{
    load_keypair, create_rpc_client, create_program_client,
    get_pool_state, compute_tick_range, get_token_balance,
    withdraw_and_collect, deposit_liquidity, swap_token, send_tx,
};
use telegram::Telegram;

#[tokio::main]
async fn main() -> Result<()> {
    // 1) Загрузка ключа, RPC и Telegram
    let keypair = load_keypair()?;
    let rpc_client = create_rpc_client();
    let telegram = Telegram::new(
        &std::env::var("TELEGRAM_TOKEN")?,
        &std::env::var("TELEGRAM_CHAT_ID")?,
    );

    // 2) Для каждого пула заводим свою задачу
    let mut handles = Vec::new();
    for cfg in POOLS.iter().cloned() {
        let keypair = keypair.clone();
        let rpc = rpc_client.clone();
        let tele = telegram.clone();
        handles.push(tokio::spawn(async move {
            if let Err(e) = monitor_pool(cfg, keypair, rpc, tele).await {
                eprintln!("Ошибка в пуле {}: {:?}", cfg.name, e);
            }
        }));
    }

    // 3) Ждём, чтобы main не завершился
    futures::future::join_all(handles).await;
    Ok(())
}

/// Основной цикл мониторинга и ребалансировки для одного пула
async fn monitor_pool(
    cfg: params::PoolConfig,
    keypair: Keypair,
    rpc_client: RpcClient,
    telegram: Telegram,
) -> Result<()> {
    let program = create_program_client(&keypair);

    // Парсим адреса
    let pool_pubkey = Pubkey::from_str(cfg.pool_address)?;
    let usdc_mint  = Pubkey::from_str(cfg.usdc_mint_address)?;
    let wsol_mint  = Pubkey::from_str(cfg.wsol_mint_address)?;
    let initial_value = cfg.initial_amount_usdc;

    // Состояние позиции
    let mut has_position = false;
    let mut lower_tick    = 0;
    let mut upper_tick    = 0;
    let mut last_price    = 0.0;
    let mut last_spacing  = 0;

    let mut check_int  = interval(CHECK_INTERVAL);
    let mut report_int = interval(REPORT_INTERVAL);

    loop {
        select! {
            // 1) Интервал проверки цены
            _ = check_int.tick() => {
                let ( _state, price, spacing ) = get_pool_state(&program, &pool_pubkey)?;
                last_price   = price;
                last_spacing = spacing;

                let (new_low, new_high) = compute_tick_range(price, spacing, RANGE_HALF)?;

                if !has_position {
                    // Initial deposit
                    let amt_usdc = (cfg.initial_amount_usdc * 1e6) as u128;
                    let amt_wsol = ((cfg.initial_amount_usdc / price) * 1e9) as u128;
                    let ix = deposit_liquidity(&program, &pool_pubkey, new_low, new_high, amt_usdc, amt_wsol);
                    let sig = send_tx(&rpc_client, &keypair, &[ix])?;
                    println!("{}: initial deposit @ {:.6}, tx={}", cfg.name, price, sig);
                    lower_tick = new_low;
                    upper_tick = new_high;
                    has_position = true;
                } else {
                    // Rebalance only if price out of old range
                    let old_low_price  = tick_to_price(lower_tick, last_spacing)?;
                    let old_high_price = tick_to_price(upper_tick, last_spacing)?;
                    if price < old_low_price || price > old_high_price {
                        println!("{}: price {:.6} outside [{:.6}–{:.6}], rebalancing...", cfg.name, price, old_low_price, old_high_price);

                        // 1) withdraw + collect
                        let mut ix = withdraw_and_collect(&program, &pool_pubkey, lower_tick, upper_tick);

                        // 2) get balances
                        let bal_usdc = get_token_balance(&rpc_client, &keypair.pubkey(), &usdc_mint)? as u128;
                        let bal_wsol = get_token_balance(&rpc_client, &keypair.pubkey(), &wsol_mint)? as u128;

                        // 3) calculate 50/50
                        let total_usdc_val = bal_usdc as f64 / 1e6 + bal_wsol as f64 / 1e9 * price;
                        let tgt_usdc = (total_usdc_val / 2.0 * 1e6).round() as u128;
                        let tgt_wsol = ((total_usdc_val / 2.0) / price * 1e9).round() as u128;

                        // 4) swap excess
                        if bal_usdc > tgt_usdc {
                            ix.push(swap_token(&program, &pool_pubkey, bal_usdc - tgt_usdc));
                        } else if bal_wsol > tgt_wsol {
                            ix.push(swap_token(&program, &pool_pubkey, bal_wsol - tgt_wsol));
                        }

                        // 5) new deposit
                        ix.push(deposit_liquidity(&program, &pool_pubkey, new_low, new_high, tgt_usdc, tgt_wsol));

                        let sig = send_tx(&rpc_client, &keypair, &ix)?;
                        println!("{}: rebalanced @ {:.6}, tx={}", cfg.name, price, sig);

                        lower_tick = new_low;
                        upper_tick = new_high;
                    }
                }
            }

            // 2) Интервал отчёта
            _ = report_int.tick() => {
                let bal_usdc = get_token_balance(&rpc_client, &keypair.pubkey(), &usdc_mint)? as u128;
                let bal_wsol = get_token_balance(&rpc_client, &keypair.pubkey(), &wsol_mint)? as u128;
                let total_usdc_val = bal_usdc as f64 / 1e6 + bal_wsol as f64 / 1e9 * last_price;
                let profit = total_usdc_val - initial_value;
                let low_p  = tick_to_price(lower_tick, last_spacing)?;
                let high_p = tick_to_price(upper_tick, last_spacing)?;
                telegram.send_pool_report(
                    &cfg.name,
                    last_price,
                    low_p,
                    high_p,
                    bal_usdc,
                    bal_wsol,
                    profit,
                ).await?;
            }
        }
    }
}
