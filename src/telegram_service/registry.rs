// src/telegram_service/registry.rs

use tokio::sync::mpsc::UnboundedSender;
use crate::telegram_service::commands::Commander;
use crate::telegram_service::tl_engine::ServiceCommand;
use crate::params::POOL;
use tokio::sync::Notify;
use crate::wirlpool_services::wirlpool::open_with_funds_check_universal;
use crate::wirlpool_services::{
    get_info::fetch_pool_position_info,
    wirlpool::{harvest_whirlpool_position, summarize_harvest_fees,
        close_whirlpool_position, close_all_positions, list_positions_for_owner
    },
};
use orca_whirlpools::PositionOrBundle;
use tokio::time::sleep;
use std::time::Duration;
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;
use std::sync::Arc;

/// Регистрация всех телеграм-команд
pub fn register_commands(commander: Arc<Commander>, tx: UnboundedSender<ServiceCommand>, close_ntf:  Arc<Notify>) {
    let tx = Arc::new(tx);

    {
        let c: Arc<Commander> = Arc::clone(&commander);
        let t = Arc::clone(&tx);
        commander.add_command(&["info"], move |_params| {
            let c2 = Arc::clone(&c);
            let t2: Arc<UnboundedSender<ServiceCommand>> = Arc::clone(&t);
            async move {
                let tree = c2.show_tree();
                let _ = t2.send(ServiceCommand::SendMessage(
                    format!("Доступные команды:\n{}", tree),
                ));
            }
        });
    }

    
    commander.add_command(&["close", "all"], {
        let tx = Arc::clone(&tx);
        let close_ntf = close_ntf.clone();
        move |_params| {
            let tx = Arc::clone(&tx);
            let close_ntf = close_ntf.clone(); 
            async move {
                // мгновенно информируем пользователя
                let _ = tx.send(ServiceCommand::SendMessage(
                    "🔒 Начинаем закрывать ВСЕ позиции…".into(),
                ));
    
                // всё тяжёлое – в фоне
                let tx_bg = Arc::clone(&tx);
                tokio::spawn(async move {
                    if let Err(err) = close_all_positions(300, None).await {
                        let _ = tx_bg.send(ServiceCommand::SendMessage(
                            format!("❌ Ошибка при закрытии позиций: {err:?}"),
                        ));
                        return;
                    }
    
                    let _ = tx_bg.send(ServiceCommand::SendMessage(
                        "✅ Запросы отправлены, ждём подтверждений…".into(),
                    ));
    
                    tokio::time::sleep(Duration::from_secs(2)).await;
                    close_ntf.notify_waiters();
    
                    match list_positions_for_owner(None).await {
                        Ok(positions) if positions.is_empty() => {
                            
                            let _ = tx_bg.send(ServiceCommand::SendMessage(
                                "🎉 Все позиции успешно закрыты.".into(),
                            ));
                        }
                        Ok(positions) => {
                            let mut msg = String::from("⚠️ Остались незакрытые позиции:\n");
                            for p in positions {
                                match p {
                                    PositionOrBundle::Position(hp) =>
                                        msg.push_str(&format!("- mint: {}\n",
                                            hp.data.position_mint)),
                                    PositionOrBundle::PositionBundle(pb) =>
                                        msg.push_str(&format!("- bundle account: {}\n",
                                            pb.address)),
                                }
                            }
                            let _ = tx_bg.send(ServiceCommand::SendMessage(msg));
                        }
                        Err(err) => {
                            let _ = tx_bg.send(ServiceCommand::SendMessage(
                                format!("❌ Ошибка при проверке позиций: {err:?}"),
                            ));
                        }
                    }
                });
            }
        }
    });
    
    

    
    commander.add_command(&["close", "off"], {
        let tx = Arc::clone(&tx);
        move |_params| {
            let tx = Arc::clone(&tx);
            async move {
                // первое сообщение – сразу
                let _ = tx.send(ServiceCommand::SendMessage(
                    "🔒 Закрываем все позиции и выключаемся…".into(),
                ));
    
                // тяжёлую работу + завершение — в фоне
                let tx_bg = Arc::clone(&tx);
                tokio::spawn(async move {
                    if let Err(err) = close_all_positions(300, None).await {
                        let _ = tx_bg.send(ServiceCommand::SendMessage(
                            format!("❌ Ошибка при закрытии позиций: {err:?}"),
                        ));
                    } else {
                        let _ = tx_bg.send(ServiceCommand::SendMessage(
                            "✅ Позиции закрыты, завершаем работу…".into(),
                        ));
                    }
    
                    // даём Telegram-циклу секунду, чтобы реально отправить сообщение
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    std::process::exit(0);
                });
            }
        }
    });
    


    // 1. bal all
    commander.add_command(&["bal", "all"], {
        let tx = Arc::clone(&tx);
        move |_params| {
            let tx = Arc::clone(&tx);
            async move {
                let pool = POOL.clone();
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
    });


    // open fc --pct --usize
    commander.add_command(&["open"], {
        let tx = Arc::clone(&tx);
        move |params| {
            let tx = Arc::clone(&tx);
            async move {
                let pct = params.get(0)
                    .and_then(|v| v.parse::<f64>().ok())
                    .unwrap_or(0.4);
                let initial_amount_usdc = params.get(1)
                    .and_then(|v| v.parse::<f64>().ok())
                    .unwrap_or(100.0);
                let pool = POOL.clone();
                let info_res = fetch_pool_position_info(&pool, None).await;
                if let Ok(info) = info_res {
                    let cp = info.current_price;
                    let lower = cp * (1.0 - pct / 100.0);
                    let upper = cp * (1.0 + pct / 100.0);

                    match open_with_funds_check_universal(lower, upper, initial_amount_usdc, pool.clone(), 200).await {
                        Ok(mint) => {
                            let _ = tx.send(ServiceCommand::SendMessage(
                                format!(
                                    "✅ Открыта новая позиция (with funds check) c диапазоном ±{:.2}% на сумму {:.2} USDC\nNFT mint: {}",
                                    pct, initial_amount_usdc, mint.position_mint
                                )
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
    });

    // 3. harvest all
    commander.add_command(&["harvest", "all"], {
        let tx = Arc::clone(&tx);
        move |_params| {
            let tx = Arc::clone(&tx);
            async move {
                let pool = POOL.clone();
                let mut results = Vec::new();
                for (idx, pos) in [pool.position_1.as_ref(), pool.position_2.as_ref(), pool.position_3.as_ref()].iter().enumerate() {
                    let msg = match pos.and_then(|p| p.position_nft) {
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
    });

    // harvest --usize (по конкретной позиции)
    commander.add_command(&["harvest"], {
        let tx = Arc::clone(&tx);
        move |params| {
            let tx = Arc::clone(&tx);
            async move {
                let pos_num = params.get(0).and_then(|v| v.parse::<usize>().ok()).unwrap_or(1);
                let pool = POOL.clone();
                let pos = match pos_num {
                    1 => pool.position_1.as_ref(),
                    2 => pool.position_2.as_ref(),
                    3 => pool.position_3.as_ref(),
                    _ => None,
                };
                let msg = match pos.and_then(|p| p.position_nft) {
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
    });

    // 4. close all
    commander.add_command(&["close", "full"], {
        let tx = Arc::clone(&tx);
        move |_params| {
            let tx = Arc::clone(&tx);
            async move {
                let pool = POOL.clone();
                let mut results = Vec::new();
                for (idx, pos) in [pool.position_1.as_ref(), pool.position_2.as_ref(), pool.position_3.as_ref()].iter().enumerate() {
                    let msg = match pos.and_then(|p| p.position_nft) {
                        Some(nft_addr) => {
                            let mint_res = Pubkey::from_str(nft_addr);
                            if let Ok(mint) = mint_res {
                                match close_whirlpool_position(mint, 150).await {
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
    });

    // close --usize (конкретная позиция)
    commander.add_command(&["close"], {
        let tx = Arc::clone(&tx);
        move |params| {
            let tx = Arc::clone(&tx);
            async move {
                let pos_num = params.get(0).and_then(|v| v.parse::<usize>().ok()).unwrap_or(1);
                let pool = POOL.clone();
                let pos = match pos_num {
                    1 => pool.position_1.as_ref(),
                    2 => pool.position_2.as_ref(),
                    3 => pool.position_3.as_ref(),
                    _ => None,
                };
                let msg = match pos.and_then(|p| p.position_nft) {
                    Some(nft_addr) => {
                        if let Ok(mint) = Pubkey::from_str(nft_addr) {
                            match close_whirlpool_position(mint, 150).await {
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
    });

    // --- SHORT OPEN: short --uint
    commander.add_command(&["short"], {
        let tx = Arc::clone(&tx);
        move |params| {
            let tx = Arc::clone(&tx);
            async move {
                let amount = params.get(0)
                    .and_then(|v| v.parse::<f64>().ok())
                    .unwrap_or(0.0);
                if amount <= 0.0 {
                    let _ = tx.send(ServiceCommand::SendMessage("Укажите сумму в USDT: short --300".into()));
                    return;
                }
                let mut hl = match crate::exchange::hyperliquid::hl::HL::new_from_env().await {
                    Ok(h) => h,
                    Err(e) => {
                        let _ = tx.send(ServiceCommand::SendMessage(format!("HL init error: {e}")));
                        return;
                    }
                };
                match hl.open_market_order("SOLUSDT", "Sell", amount, false, 0.0).await {
                    Ok((cloid, px)) => {
                        let _ = tx.send(ServiceCommand::SendMessage(format!(
                            "✅ Short открыт на {:.2} USDT\nOrder ID: {}\nЦена: {:.4}", amount, cloid, px
                        )));
                    }
                    Err(e) => {
                        let _ = tx.send(ServiceCommand::SendMessage(format!("Ошибка открытия шорта: {e}")));
                    }
                }
            }
        }
    });

    // --- SHORT CLOSE: short close
    commander.add_command(&["short", "close"], {
        let tx = Arc::clone(&tx);
        move |_params| {
            let tx = Arc::clone(&tx);
            async move {
                let mut hl = match crate::exchange::hyperliquid::hl::HL::new_from_env().await {
                    Ok(h) => h,
                    Err(e) => {
                        let _ = tx.send(ServiceCommand::SendMessage(format!("HL init error: {e}")));
                        return;
                    }
                };
                // 1. Получить позицию
                match hl.get_position("SOLUSDT").await {
                    Ok(Some(pos)) if pos.size > 0.0 => {
                        // 2. Закрыть её (reduce_only = true, amount_coins = pos.size)
                        match hl.open_market_order("SOLUSDT", "Short", 0.0, true, pos.size).await {
                            Ok((cloid, close_px)) => {
                                // 3. Узнать баланс
                                match hl.get_balance().await {
                                    Ok(bal) => {
                                        let _ = tx.send(ServiceCommand::SendMessage(
                                            format!(
                                                "✅ Шорт по SOLUSDT ({:.4} контрактов) успешно закрыт (order {})\nБаланс: ${:.2}",
                                                pos.size, cloid, bal
                                            )
                                        ));
                                    }
                                    Err(e) => {
                                        let _ = tx.send(ServiceCommand::SendMessage(
                                            format!(
                                                "✅ Позиция закрыта, но не удалось получить баланс: {e}"
                                            )
                                        ));
                                    }
                                }
                            }
                            Err(e) => {
                                let _ = tx.send(ServiceCommand::SendMessage(
                                    format!("Ошибка закрытия позиции: {e}")
                                ));
                            }
                        }
                    }
                    Ok(_) => {
                        let _ = tx.send(ServiceCommand::SendMessage("Нет открытого шорта по SOLUSD".into()));
                    }
                    Err(e) => {
                        let _ = tx.send(ServiceCommand::SendMessage(
                            format!("Ошибка получения позиции: {e}")
                        ));
                    }
                }
            }
        }
    });


}
