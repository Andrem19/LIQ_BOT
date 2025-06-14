// src/telegram_service/registry.rs

use tokio::sync::mpsc::UnboundedSender;
use crate::telegram_service::commands::Commander;
use crate::telegram_service::engine::ServiceCommand;
use crate::params::POOLS;
use crate::wirlpool_services::{
    get_info::fetch_pool_position_info,
    wirlpool::{
        open_whirlpool_position, open_with_funds_check,
        harvest_whirlpool_position, summarize_harvest_fees,
        close_whirlpool_position,
    },
};
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;
use std::sync::Arc;

/// Регистрация всех телеграм-команд
pub async fn register_commands(commander: &Commander, tx: UnboundedSender<ServiceCommand>) {
    let tx = Arc::new(tx);

    // 1. bal all
    commander.add_command(&["bal", "all"], {
        let tx = Arc::clone(&tx);
        move |_params| {
            let tx = Arc::clone(&tx);
            async move {
                let pool = POOLS[0].clone();
                let mut results = Vec::new();
                for (idx, pos) in [pool.position_1.as_ref(), pool.position_2.as_ref(), pool.position_3.as_ref()].iter().enumerate() {
                    let msg = match pos.and_then(|p| p.position_address) {
                        Some(addr) => {
                            match fetch_pool_position_info(&pool, Some(addr)).await {
                                Ok(info) => format!(
                                    "Position {}:\n• Pending A: {:.6}\n• Pending B: {:.6}\n• Total: {:.6}\n• Price: {:.6} [{:.6} – {:.6}]",
                                    idx + 1, info.pending_a, info.pending_b, info.sum, info.current_price, info.lower_price, info.upper_price
                                ),
                                Err(e) => format!("Position {}: Ошибка: {}", idx + 1, e),
                            }
                        },
                        None => format!("Position {}: —", idx + 1),
                    };
                    results.push(msg);
                }
                let text = results.join("\n\n");
                let _ = tx.send(ServiceCommand::SendMessage(text));
            }
        }
    }).await;

    // 2. open --usize (открытие позиции)
    commander.add_command(&["open"], {
        let tx = Arc::clone(&tx);
        move |params| {
            let tx = Arc::clone(&tx);
            async move {
                let pct = params.get(0)
                    .and_then(|v| v.parse::<f64>().ok())
                    .unwrap_or(0.4);
                let pool = POOLS[0].clone();
                let info_res = fetch_pool_position_info(&pool, None).await;
                if let Ok(info) = info_res {
                    let cp = info.current_price;
                    let lower = cp * (1.0 - pct / 100.0);
                    let upper = cp * (1.0 + pct / 100.0);
                    
                    match open_whirlpool_position(lower, upper, pool.clone()).await {
                        Ok(mint) => {
                            let _ = tx.send(ServiceCommand::SendMessage(
                                format!("✅ Открыта новая позиция c диапазоном ±{:.2}%\nNFT mint: {}", pct, mint)
                            ));
                        }
                        Err(e) => {
                            let _ = tx.send(ServiceCommand::SendMessage(
                                format!("Ошибка открытия позиции: {}", e)
                            ));
                        }
                    }
                } else {
                    let _ = tx.send(ServiceCommand::SendMessage(
                        format!("Ошибка получения текущей цены: {:?}", info_res.err())
                    ));
                }
            }
        }
    }).await;

    // open fc --usize (открытие позиции с проверкой средств)
    commander.add_command(&["open", "fc"], {
        let tx = Arc::clone(&tx);
        move |params| {
            let tx = Arc::clone(&tx);
            async move {
                let pct = params.get(0)
                    .and_then(|v| v.parse::<f64>().ok())
                    .unwrap_or(0.4);
                let pool = POOLS[0].clone();
                let info_res = fetch_pool_position_info(&pool, None).await;
                if let Ok(info) = info_res {
                    let cp = info.current_price;
                    let lower = cp * (1.0 - pct / 100.0);
                    let upper = cp * (1.0 + pct / 100.0);
                    match open_with_funds_check(lower, upper, pool.clone()).await {
                        Ok(mint) => {
                            let _ = tx.send(ServiceCommand::SendMessage(
                                format!("✅ Открыта новая позиция (with funds check) c диапазоном ±{:.2}%\nNFT mint: {}", pct, mint)
                            ));
                        }
                        Err(e) => {
                            let _ = tx.send(ServiceCommand::SendMessage(
                                format!("Ошибка открытия позиции: {}", e)
                            ));
                        }
                    }
                } else {
                    let _ = tx.send(ServiceCommand::SendMessage(
                        format!("Ошибка получения текущей цены: {:?}", info_res.err())
                    ));
                }
            }
        }
    }).await;

    // 3. harvest all
    commander.add_command(&["harvest", "all"], {
        let tx = Arc::clone(&tx);
        move |_params| {
            let tx = Arc::clone(&tx);
            async move {
                let pool = POOLS[0].clone();
                let mut results = Vec::new();
                for (idx, pos) in [pool.position_1.as_ref(), pool.position_2.as_ref(), pool.position_3.as_ref()].iter().enumerate() {
                    let msg = match pos.and_then(|p| p.position_address) {
                        Some(addr) => {
                            let mint_res = Pubkey::from_str(addr);
                            if let Ok(mint) = mint_res {
                                match harvest_whirlpool_position(mint).await {
                                    Ok(fees) => {
                                        match summarize_harvest_fees(&pool, &fees).await {
                                            Ok(summary) => format!(
                                                "Harvest {}: {:.6} A, {:.6} B, total ${:.2}",
                                                idx + 1, summary.amount_a, summary.amount_b, summary.total_usd
                                            ),
                                            Err(e) => format!("Harvest {}: Ошибка summary: {}", idx + 1, e),
                                        }
                                    }
                                    Err(e) => format!("Harvest {}: Ошибка: {}", idx + 1, e),
                                }
                            } else {
                                format!("Harvest {}: Некорректный mint", idx + 1)
                            }
                        }
                        None => format!("Harvest {}: —", idx + 1),
                    };
                    results.push(msg);
                }
                let text = results.join("\n");
                let _ = tx.send(ServiceCommand::SendMessage(text));
            }
        }
    }).await;

    // harvest --usize (по конкретной позиции)
    commander.add_command(&["harvest"], {
        let tx = Arc::clone(&tx);
        move |params| {
            let tx = Arc::clone(&tx);
            async move {
                let pos_num = params.get(0).and_then(|v| v.parse::<usize>().ok()).unwrap_or(1);
                let pool = POOLS[0].clone();
                let pos = match pos_num {
                    1 => pool.position_1.as_ref(),
                    2 => pool.position_2.as_ref(),
                    3 => pool.position_3.as_ref(),
                    _ => None,
                };
                let msg = match pos.and_then(|p| p.position_address) {
                    Some(addr) => {
                        if let Ok(mint) = Pubkey::from_str(addr) {
                            match harvest_whirlpool_position(mint).await {
                                Ok(fees) => {
                                    match summarize_harvest_fees(&pool, &fees).await {
                                        Ok(summary) => format!(
                                            "Harvest {}: {:.6} A, {:.6} B, total ${:.2}",
                                            pos_num, summary.amount_a, summary.amount_b, summary.total_usd
                                        ),
                                        Err(e) => format!("Harvest {}: Ошибка summary: {}", pos_num, e),
                                    }
                                }
                                Err(e) => format!("Harvest {}: Ошибка: {}", pos_num, e),
                            }
                        } else {
                            format!("Harvest {}: Некорректный mint", pos_num)
                        }
                    }
                    None => format!("Harvest {}: —", pos_num),
                };
                let _ = tx.send(ServiceCommand::SendMessage(msg));
            }
        }
    }).await;

    // 4. close all
    commander.add_command(&["close", "all"], {
        let tx = Arc::clone(&tx);
        move |_params| {
            let tx = Arc::clone(&tx);
            async move {
                let pool = POOLS[0].clone();
                let mut results = Vec::new();
                for (idx, pos) in [pool.position_1.as_ref(), pool.position_2.as_ref(), pool.position_3.as_ref()].iter().enumerate() {
                    let msg = match pos.and_then(|p| p.position_nft) {
                        Some(nft_addr) => {
                            let mint_res = Pubkey::from_str(nft_addr);
                            if let Ok(mint) = mint_res {
                                match close_whirlpool_position(mint).await {
                                    Ok(_) => format!("✅ Closed position {}", idx + 1),
                                    Err(e) => format!("Ошибка закрытия позиции {}: {}", idx + 1, e),
                                }
                            } else {
                                format!("Position {}: некорректный NFT mint", idx + 1)
                            }
                        }
                        None => format!("Position {}: —", idx + 1),
                    };
                    results.push(msg);
                }
                let text = results.join("\n");
                let _ = tx.send(ServiceCommand::SendMessage(text));
            }
        }
    }).await;

    // close --usize (конкретная позиция)
    commander.add_command(&["close"], {
        let tx = Arc::clone(&tx);
        move |params| {
            let tx = Arc::clone(&tx);
            async move {
                let pos_num = params.get(0).and_then(|v| v.parse::<usize>().ok()).unwrap_or(1);
                let pool = POOLS[0].clone();
                let pos = match pos_num {
                    1 => pool.position_1.as_ref(),
                    2 => pool.position_2.as_ref(),
                    3 => pool.position_3.as_ref(),
                    _ => None,
                };
                let msg = match pos.and_then(|p| p.position_nft) {
                    Some(nft_addr) => {
                        if let Ok(mint) = Pubkey::from_str(nft_addr) {
                            match close_whirlpool_position(mint).await {
                                Ok(_) => format!("✅ Closed position {}", pos_num),
                                Err(e) => format!("Ошибка закрытия позиции {}: {}", pos_num, e),
                            }
                        } else {
                            format!("Position {}: некорректный NFT mint", pos_num)
                        }
                    }
                    None => format!("Position {}: —", pos_num),
                };
                let _ = tx.send(ServiceCommand::SendMessage(msg));
            }
        }
    }).await;
}
