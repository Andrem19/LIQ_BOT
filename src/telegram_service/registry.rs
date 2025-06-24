// src/telegram_service/registry.rs

use solana_sdk::signature::Keypair;
use tokio::sync::mpsc::UnboundedSender;
use crate::telegram_service::commands::Commander;
use crate::telegram_service::tl_engine::ServiceCommand;
use crate::params::{WETH, WBTC, WSOL, USDC};
use tokio::sync::Notify;
use chrono::Utc;
use crate::wirlpool_services::wirlpool::position_mode;
use crate::wirlpool_services::wirlpool::open_with_funds_check_universal;
use crate::wirlpool_services::{
    get_info::fetch_pool_position_info,
    wirlpool::{harvest_whirlpool_position, summarize_harvest_fees,
        close_whirlpool_position, close_all_positions, list_positions_for_owner
    },
};
use crate::wirlpool_services::wirlpool::Mode;
use orca_whirlpools_core::tick_index_to_price;
use orca_tx_sender::Signer;
use crate::database::triggers;
use anyhow::anyhow;
use orca_whirlpools::PositionOrBundle;
use tokio::time::sleep;
use crate::utils::{self, sweep_dust_to_usdc};
use std::time::Duration;
use solana_sdk::pubkey::Pubkey;
use std::str::FromStr;
use std::sync::Arc;
use crate::utils::utils::{init_rpc, load_wallet};
use crate::database::triggers::Trigger;
use crate:: wirlpool_services::wirlpool::{increase_liquidity_partial, decrease_liquidity_partial, refresh_balances};

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

    commander.add_command(&["on"], {
        let tx = Arc::clone(&tx);
        move |_params| {
            let tx = Arc::clone(&tx);
            async move {
                // Выключаем флаг в БД
                let mut t = Trigger {
                    name: "auto_trade".into(),
                    state: false,
                    position: "opening".into(),
                };
                match triggers::upsert_trigger(&t).await {
                    Ok(_) => {
                        let _ = tx.send(ServiceCommand::SendMessage(
                            "✅ Trigger `auto_trade` on".into(),
                        ));
                    }
                    Err(e) => {
                        let _ = tx.send(ServiceCommand::SendMessage(
                            format!("❌ Failed to on trigger: {}", e),
                        ));
                    }
                }
            }
        }
    });

    commander.add_command(&["off"], {
        let tx = Arc::clone(&tx);
        move |_params| {
            let tx = Arc::clone(&tx);
            async move {
                let mut t = Trigger {
                    name: "auto_trade".into(),
                    state: true,
                    position: "opening".into(),
                };
                match triggers::upsert_trigger(&t).await {
                    Ok(_) => {
                        let _ = tx.send(ServiceCommand::SendMessage(
                            "✅ Trigger `auto_trade` enabled".into(),
                        ));
                    }
                    Err(e) => {
                        let _ = tx.send(ServiceCommand::SendMessage(
                            format!("❌ Failed to enable trigger: {}", e),
                        ));
                    }
                }
            }
        }
    });

    commander.add_command(&["safe"], {
        let tx = Arc::clone(&tx);
        move |_params| {
            let tx = Arc::clone(&tx);
            async move {
                match utils::swap_excess_to_usdc(WSOL, 9, 0.10).await {
                    Ok(report) => { let _ = tx.send(ServiceCommand::SendMessage(report)); }
                    Err(e)     => { let _ = tx.send(ServiceCommand::SendMessage(format!("Error: {e}"))); }
                }
            }
        }
    });

    commander.add_command(&["swap", "dust"], {
        let tx = Arc::clone(&tx);
        move |_params| {
            let tx = Arc::clone(&tx);

            // «пыль», которую хотим обменять на USDC.
            // Можно свободно добавлять новые (mint, decimals).
            const DUST_TOKENS: [(&str, u8); 2] = [
                (WETH, 8),
                (WBTC, 8),
            ];

            async move {
                let _ = tx.send(ServiceCommand::SendMessage(
                    "🔄 Ищу «пыль» (WETH, WBTC)…".into(),
                ));

                match sweep_dust_to_usdc(&DUST_TOKENS).await {
                    Ok(report) => {
                        let _ = tx.send(ServiceCommand::SendMessage(report));
                    }
                    Err(err) => {
                        let _ = tx.send(ServiceCommand::SendMessage(
                            format!("❌ Ошибка при свипе пыли: {err:?}"),
                        ));
                    }
                }
            }
        }
    });

    commander.add_command(&["close", "on"], {
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

                            let mut t = Trigger {
                                name: "auto_trade".into(),
                                state: true,
                                position: "opening".into(),
                            };
                            let t = triggers::upsert_trigger(&t).await;
                            tokio::time::sleep(Duration::from_secs(10)).await;

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
                            tokio::time::sleep(Duration::from_secs(10)).await;
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
                        let rpc = &utils::utils::init_rpc();
                        let payer = match utils::utils::load_wallet() {
                            Ok(kp) => kp,
                            Err(e) => {
                                let _ = tx_bg.send(ServiceCommand::SendMessage(
                                    format!("❌ Не удалось загрузить кошелёк: {e}"),
                                ));
                                return; // прекращаем работу в этом фоне
                            }
                        };
                        let wallet: Pubkey  = payer.pubkey();
                        const TOKENS: [(&str, u8); 2] = [
                            (WSOL, 9),
                            (USDC, 6),
                        ];
                        let balances = match utils::balances_for_mints(&rpc, &wallet, &TOKENS).await {
                            Ok(v) => v,
                            Err(e) => {
                                let _ = tx_bg.send(ServiceCommand::SendMessage(
                                    format!("❌ Ошибка при получении балансов: {e}"),
                                ));
                                return;
                            }
                        };

                        let mut report = String::from("✅ Позиции закрыты. Текущие балансы:\n");
                        if balances.is_empty() {
                            report.push_str("  — все остатки равны нулю.\n");
                        } else {
                            for (mint, _dec, bal) in balances {
                                report.push_str(&format!("  • {}: {:.6}\n", mint, bal));
                            }
                        }
                        let _ = tx_bg.send(ServiceCommand::SendMessage(report));
                    }
    
                    // даём Telegram-циклу секунду, чтобы реально отправить сообщение
                    tokio::time::sleep(Duration::from_secs(10)).await;
                    std::process::exit(0);
                });
            }
        }
    });
    

    commander.add_command(&["inc"], {
        let tx = Arc::clone(&tx);
    
        move |params| {
            let tx = Arc::clone(&tx);
    
            // эта обёртка ВОЗВРАЩАЕТ Send-future
            async move {
                // клонируем, потому что params придут по &'a Vec<String>
                let params_owned = params.clone();
                // Handle текущего runtime (нужен внутри blocking-потока)
                let handle = tokio::runtime::Handle::current();
    
                // запускаем "несендовую" логику в blocking-пуле
                let _ = tokio::task::spawn_blocking(move || {
                    handle.block_on(run_inc_command(params_owned, tx))
                })
                .await;               // `JoinHandle` — Send, всё ОК
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


#[allow(clippy::too_many_lines)]
pub async fn run_inc_command(
    params: Vec<String>,
    tx:     std::sync::Arc<tokio::sync::mpsc::UnboundedSender<ServiceCommand>>,
) {
    use crate::params::{WSOL, USDC, OVR};
    use crate::types::PoolConfig;
    use crate::utils::utils::{init_rpc, load_wallet};

    // ── 0. Парсим аргументы ────────────────────────────────────────────────
    if params.len() < 3 {
        let _ = tx.send(ServiceCommand::SendMessage(
            "Использование: inc --<from> --<to> --<pct>".into(),
        ));
        return;
    }
    let from_idx = params[0].parse::<usize>().unwrap_or(0);
    let to_idx   = params[1].parse::<usize>().unwrap_or(0);
    let pct      = params[2].parse::<f64>().unwrap_or(0.0);

    if !(1..=3).contains(&from_idx) || !(1..=3).contains(&to_idx) || from_idx == to_idx {
        let _ = tx.send(ServiceCommand::SendMessage(
            "Номера позиций должны быть 1..3 и различаться".into(),
        ));
        return;
    }
    if !(0.0 < pct && pct <= 100.0) {
        let _ = tx.send(ServiceCommand::SendMessage(
            "Процент должен быть в диапазоне (0;100]".into(),
        ));
        return;
    }

    // ── 1. Берём три позиции владельца ─────────────────────────────────────
    let list = match list_positions_for_owner(None).await {
        Ok(v) if v.len() == 3 => v,
        Ok(v) => {
            let _ = tx.send(ServiceCommand::SendMessage(
                format!("Ожидалось 3 позиции, найдено {}", v.len()),
            ));
            return;
        }
        Err(e) => {
            let _ = tx.send(ServiceCommand::SendMessage(
                format!("Не удалось получить позиции: {e}"),
            ));
            return;
        }
    };

    // ── 2. Сортируем «сверху → вниз» (по верхнему ценовому пределу) ────────
    let mut info = Vec::<(Pubkey, f64)>::new();
    for p in &list {
        if let PositionOrBundle::Position(hp) = p {
            let up = upper_price(hp.data.tick_upper_index, 9, 6); // SOL/USDC
            info.push((hp.data.position_mint, up));
        }
    }
    info.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());

    let mint_from = info[from_idx - 1].0;
    let mint_to   = info[to_idx   - 1].0;

    // ── 3. Приготовим PoolConfig (нужен increase_…) ────────────────────────
    let whirl_pk = if let PositionOrBundle::Position(hp) = list.first().unwrap() {
        hp.data.whirlpool
    } else { unreachable!() };

    let pool_cfg = PoolConfig {
        amount: 0.0,
        program: "".to_string(),
        name: "SOL/USDC".to_string(),
        pool_address: whirl_pk.to_string(),
        position_1: None, position_2: None, position_3: None,
        mint_a: WSOL.to_string(), mint_b: USDC.to_string(),
        decimal_a: 9, decimal_b: 6,

        date_opened:           Utc::now(),     // или любая другая заглушечная дата
        is_closed:             false,          // позиция ещё не закрыта
        commission_collected_1: 0.0,            // пока комиссий нет
        commission_collected_2: 0.0,
        commission_collected_3: 0.0,
        total_value_open:      0.0,            // либо исходная TVL
        total_value_current:   0.0,            // либо текущее TVL
    };

    // ── 4. Замеряем балансы «до» ───────────────────────────────────────────
    let rpc    = init_rpc();
    let wallet = load_wallet().unwrap();
    let dec_b  = 6u8;

    let (sol_before, usdc_before) = refresh_balances(
        &rpc, &wallet.pubkey(), &Pubkey::from_str(USDC).unwrap(), dec_b,
    )
    .await
    .expect("refresh_balances failed");

    // ── 5. Снимаем часть ликвидности из исходной позиции ───────────────────
    let _ = tx.send(ServiceCommand::SendMessage(
        format!("🔄 Снимаю {pct:.2}% ликвидности из позиции {from_idx}"),
    ));
    if let Err(e) = decrease_liquidity_partial(mint_from, pct, 500).await {
        let _ = tx.send(ServiceCommand::SendMessage(format!("❌ decrease_liquidity: {e}")));
        return;
    }

    // ── 6. Замеряем балансы «после» и считаем дельту ───────────────────────
    let (sol_after, usdc_after) = refresh_balances(
        &rpc, &wallet.pubkey(), &Pubkey::from_str(USDC).unwrap(), dec_b,
    )
    .await
    .expect("refresh_balances failed");

    let freed_sol  = (sol_after  - sol_before ).max(0.0);
    let freed_usdc = (usdc_after - usdc_before).max(0.0);

    if freed_sol + freed_usdc < 1e-9 {
        let _ = tx.send(ServiceCommand::SendMessage(
            "⚠️ Ничего не освободилось – операция прекращена".into(),
        ));
        return;
    }
    let _ = tx.send(ServiceCommand::SendMessage(format!(
        "➕ Свободно ≈ {:.6} SOL  /  {:.6} USDC. Добавляю в позицию {to_idx}",
        freed_sol, freed_usdc
    )));

    // ── 7. Увеличиваем ликвидность в целевой позиции ───────────────────────
    if let Err(e) = increase_liquidity_partial(
        mint_to,
        freed_sol,
        freed_usdc,
        &pool_cfg,
        500,
    )
    .await
    {
        let _ = tx.send(ServiceCommand::SendMessage(format!("❌ increase_liquidity: {e}")));
        return;
    }

    let _ = tx.send(ServiceCommand::SendMessage("✅ Перебалансировка завершена".into()));
}

fn upper_price(tick_u: i32, dec_a: u8, dec_b: u8) -> f64 {
    tick_index_to_price(tick_u, dec_a, dec_b)
}

