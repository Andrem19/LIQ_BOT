// src/telegram_service/registry.rs

use tokio::sync::mpsc::UnboundedSender;
use crate::telegram_service::commands::Commander;
use crate::telegram_service::tl_engine::ServiceCommand;
use crate::params::{WETH, WBTC, WSOL, USDC};
use tokio::sync::Notify;
use chrono::Utc;
use crate::dex_services::{
    wirlpool::{close_all_positions, list_positions_for_owner
    },
};
use crate::strategies::limit_order;
use crate::types::PoolConfig;
use crate::dex_services::swap::execute_swap_tokens;
use crate::dex_services::get_info::get_sol_price_usd;
use spl_associated_token_account::get_associated_token_address;
use solana_sdk::pubkey::Pubkey;
use crate::database::general_settings::{update_pct_list_2, update_pct_number};
use orca_whirlpools_core::tick_index_to_price;
use orca_tx_sender::Signer;
use crate::database::triggers;
use orca_whirlpools::PositionOrBundle;
use crate::utils::{self, sweep_dust_to_usdc};
use std::time::Duration;
use std::str::FromStr;
use std::sync::Arc;
use crate::database::general_settings::update_info_interval;
use crate::database::general_settings::{update_amount, get_general_settings};
use crate::utils::utils::{init_rpc, load_wallet};
use crate::database::triggers::Trigger;
use crate:: dex_services::wirlpool::{increase_liquidity_partial, decrease_liquidity_partial, refresh_balances};

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
    let limit_help = "<float lower> <float upper> — установить нижний и/или верхний лимиты";
    commander.add_command_with_help(&["limit"], limit_help, {
        let tx = Arc::clone(&tx);
        move |params| {
            let tx = Arc::clone(&tx);
            async move {
                // 1) Проверяем количество аргументов
                if params.len() != 2 {
                    let _ = tx.send(ServiceCommand::SendMessage(
                        "❌ Usage: limit <lower> <upper>".into(),
                    ));
                    return;
                }

                // 2) Парсим оба аргумента в f64 (в случае ошибки получаем 0.0)
                let lower = params[0].parse::<f64>().unwrap_or(0.0);
                let upper = params[1].parse::<f64>().unwrap_or(0.0);

                // 3) Проверяем, что хотя бы один лимит задан (>0)
                if lower <= 0.0 && upper <= 0.0 {
                    let _ = tx.send(ServiceCommand::SendMessage(
                        "❌ At least one of <lower> or <upper> must be > 0".into(),
                    ));
                    return;
                }

                // 4) Вызываем нужную функцию
                let result = if lower > 0.0 && upper > 0.0 {
                    // оба лимита
                    limit_order::set_both_limits(lower, upper).await
                } else if lower > 0.0 {
                    // только нижний
                    limit_order::set_low_limit(lower).await
                } else {
                    // только верхний
                    limit_order::set_high_limit(upper).await
                };

                // 5) Отправляем ответ пользователю
                match result {
                    Ok(_) => {
                        let _ = tx.send(ServiceCommand::SendMessage(
                            format!(
                                "✅ Limits set: lower = {}, upper = {}",
                                lower, upper
                            )
                        ));
                    }
                    Err(e) => {
                        let _ = tx.send(ServiceCommand::SendMessage(
                            format!("❌ Failed to set limits: {}", e)
                        ));
                    }
                }
            }
        }
    });

    commander.add_command(&["auto", "off"], {
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
                            "✅ Trigger `auto_trade` off".into(),
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

    commander.add_command(&["auto", "on"], {
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
                            "✅ Trigger `auto_trade` on".into(),
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

    {
        let tx = Arc::clone(&tx);
        commander.add_command(&["limit", "off"], {
            let tx = Arc::clone(&tx);
            move |_params| {
                let tx = Arc::clone(&tx);
                async move {
                    // Вызываем наш асинхронный switcher
                    match triggers::limit_switcher(false, Some(&tx)).await {
                        Ok(_) => {
                            let _ = tx.send(ServiceCommand::SendMessage(
                                "✅ Trigger `limit` disabled".into(),
                            ));
                        }
                        Err(e) => {
                            let _ = tx.send(ServiceCommand::SendMessage(
                                format!("❌ Failed to disable `limit` trigger: {}", e),
                            ));
                        }
                    }
                }
            }
        });
    }

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

    // ─────────── Команда su <SOL> — обменять SOL → USDC ───────────────────
    let su_help = "<float> — swap that many SOL to USDC";
    commander.add_command_with_help(&["su"], su_help, {
        let tx = Arc::clone(&tx);
        move |params| {
            let tx = Arc::clone(&tx);
            async move {
                // 1) проверяем аргумент
                if params.len() != 1 {
                    let _ = tx.send(ServiceCommand::SendMessage(
                        "❌ Usage: su <amount_SOL>".into()
                    ));
                    return;
                }
                let amount = match params[0].parse::<f64>() {
                    Ok(v) if v > 0.0 => v,
                    _ => {
                        let _ = tx.send(ServiceCommand::SendMessage(
                            format!("❌ Invalid SOL amount: {}", params[0])
                        ));
                        return;
                    }
                };

                // 2) проверяем баланс SOL
                let rpc = init_rpc();
                let wallet = match load_wallet() {
                    Ok(w) => w,
                    Err(e) => {
                        let _ = tx.send(ServiceCommand::SendMessage(
                            format!("❌ Wallet load failed: {}", e)
                        ));
                        return;
                    }
                };
                let pk = wallet.pubkey();
                let lam = match rpc.get_balance(&pk).await {
                    Ok(b) => b,
                    Err(e) => {
                        let _ = tx.send(ServiceCommand::SendMessage(
                            format!("❌ RPC get_balance failed: {}", e)
                        ));
                        return;
                    }
                };
                let sol_bal = lam as f64 / 1e9;
                if sol_bal < amount {
                    let _ = tx.send(ServiceCommand::SendMessage(
                        format!("❌ Insufficient SOL: have {:.6}, need {:.6}", sol_bal, amount)
                    ));
                    return;
                }

                // 3) выполняем swap
                match execute_swap_tokens(WSOL, USDC, amount).await {
                    Ok(res) => {
                        let _ = tx.send(ServiceCommand::SendMessage(
                            format!(
                                "✅ Swapped {:.6} SOL → USDC\n\
                                 New SOL balance: {:.6}\n\
                                 New USDC balance: {:.6}",
                                amount, res.balance_sell, res.balance_buy
                            )
                        ));
                    }
                    Err(e) => {
                        let _ = tx.send(ServiceCommand::SendMessage(
                            format!("❌ Swap failed: {}", e)
                        ));
                    }
                }
            }
        }
    });

    // ─────────── Команда us <USDC> — обменять USDC → SOL ───────────────────
    let us_help = "<float> — swap that many USDC to SOL";
    commander.add_command_with_help(&["us"], us_help, {
        let tx = Arc::clone(&tx);
        move |params| {
            let tx = Arc::clone(&tx);
            async move {
                // 1) проверяем аргумент
                if params.len() != 1 {
                    let _ = tx.send(ServiceCommand::SendMessage(
                        "❌ Usage: us <amount_USDC>".into()
                    ));
                    return;
                }
                let amount = match params[0].parse::<f64>() {
                    Ok(v) if v > 0.0 => v,
                    _ => {
                        let _ = tx.send(ServiceCommand::SendMessage(
                            format!("❌ Invalid USDC amount: {}", params[0])
                        ));
                        return;
                    }
                };

                // 2) проверяем баланс USDC
                let rpc = init_rpc();
                let wallet = match load_wallet() {
                    Ok(w) => w,
                    Err(e) => {
                        let _ = tx.send(ServiceCommand::SendMessage(
                            format!("❌ Wallet load failed: {}", e)
                        ));
                        return;
                    }
                };
                let pk = wallet.pubkey();
                let mint = Pubkey::from_str(USDC).unwrap();
                let ata = get_associated_token_address(&pk, &mint);
                let usdc_bal = match rpc.get_token_account_balance(&ata).await {
                    Ok(bal) => bal.amount.parse::<f64>().unwrap_or(0.0) / 1e6,
                    Err(_)    => 0.0,
                };
                if usdc_bal < amount {
                    let _ = tx.send(ServiceCommand::SendMessage(
                        format!("❌ Insufficient USDC: have {:.6}, need {:.6}", usdc_bal, amount)
                    ));
                    return;
                }

                // 3) выполняем swap
                match execute_swap_tokens(USDC, WSOL, amount).await {
                    Ok(res) => {
                        let _ = tx.send(ServiceCommand::SendMessage(
                            format!(
                                "✅ Swapped {:.6} USDC → SOL\n\
                                 New USDC balance: {:.6}\n\
                                 New SOL balance: {:.6}",
                                amount, res.balance_sell, res.balance_buy
                            )
                        ));
                    }
                    Err(e) => {
                        let _ = tx.send(ServiceCommand::SendMessage(
                            format!("❌ Swap failed: {}", e)
                        ));
                    }
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
                let _ = triggers::auto_trade_switch(true, Some(&tx)).await;
                let _ =triggers::opening_switcher(true, Some(&tx)).await;
                // мгновенно информируем пользователя
                let _ = tx.send(ServiceCommand::SendMessage(
                    "🔒 Начинаем закрывать ВСЕ позиции…".into(),
                ));
                let mut t = Trigger {
                    name: "auto_trade".into(),
                    state: true,
                    position: "opening".into(),
                };
    
                // всё тяжёлое – в фоне
                let tx_bg = Arc::clone(&tx);
                tokio::spawn(async move {
                    if let Err(err) = close_all_positions(300, None).await {
                        let t = triggers::auto_trade_switch(true, Some(&tx)).await;
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
                            tokio::time::sleep(Duration::from_secs(10)).await;
                            match utils::swap_excess_to_usdc(WSOL, 9, 0.10).await {
                                Ok(report) => { let _ = tx.send(ServiceCommand::SendMessage(report)); }
                                Err(e)     => { let _ = tx.send(ServiceCommand::SendMessage(format!("Error: {e}"))); }
                            }
                            
                            let _ = tx_bg.send(ServiceCommand::SendMessage(
                                "🎉 Все позиции успешно закрыты.".into(),
                            ));


                            let t = triggers::auto_trade_switch(true, Some(&tx)).await;
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
                            tokio::time::sleep(Duration::from_secs(10)).await;
                            match utils::swap_excess_to_usdc(WSOL, 9, 0.10).await {
                                Ok(report) => { let _ = tx.send(ServiceCommand::SendMessage(report)); }
                                Err(e)     => { let _ = tx.send(ServiceCommand::SendMessage(format!("Error: {e}"))); }
                            }
                            
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
                    tokio::time::sleep(Duration::from_secs(10)).await;
                    match utils::swap_excess_to_usdc(WSOL, 9, 0.10).await {
                        Ok(report) => { let _ = tx.send(ServiceCommand::SendMessage(report)); }
                        Err(e)     => { let _ = tx.send(ServiceCommand::SendMessage(format!("Error: {e}"))); }
                    }
    
                    // даём Telegram-циклу секунду, чтобы реально отправить сообщение
                    tokio::time::sleep(Duration::from_secs(5)).await;
                    std::process::exit(0);
                });
            }
        }
    });

    // ─────────── Команда int <n> — установить info_interval ─────────────────
    let int_help = "<integer> — интервал отчётов в минутах";
    commander.add_command_with_help(&["int"], int_help, {
        let tx = Arc::clone(&tx);
        move |params| {
            let tx = Arc::clone(&tx);
            async move {
                // 1) проверяем, что есть ровно один аргумент
                if params.len() != 1 {
                    let _ = tx.send(ServiceCommand::SendMessage(
                        "❌ Usage: int <integer>".into()
                    ));
                    return;
                }
                // 2) парсим в u16
                let val: u16 = match params[0].parse::<i16>() {
                    Ok(v) if v > 0 => v as u16,
                    _ => {
                        let _ = tx.send(ServiceCommand::SendMessage(
                            format!("❌ Неверный интервал: {}", params[0])
                        ));
                        return;
                    }
                };
                // 3) сохраняем в БД
                if let Err(e) = update_info_interval(val).await {
                    let _ = tx.send(ServiceCommand::SendMessage(
                        format!("❌ Не удалось обновить info_interval: {}", e)
                    ));
                    return;
                }
                // 4) сообщаем об успехе
                let _ = tx.send(ServiceCommand::SendMessage(
                    format!("✅ info_interval успешно установлен в {} мин.", val)
                ));
            }
        }
    });
    


    let balance_help = "показывает баланс WSOL и USDC в кошельке";
    commander.add_command_with_help(&["bal"], balance_help, {
        let tx = Arc::clone(&tx);
        move |_params| {
            let tx = Arc::clone(&tx);
            async move {
                match utils::fetch_wallet_balance_info().await {
                    Ok(info) => {
                        let msg = format!(
                            "🏦 Баланс кошелька:\n\
                             ► SOL: {:.4} (~${:.2})\n\
                             ► USDC: {:.4} (~${:.2})\n\
                             ► Всего: ≈ ${:.2}",
                            info.sol_balance, info.sol_in_usd,
                            info.usdc_balance, info.usdc_in_usd,
                            info.total_usd
                        );
                        let _ = tx.send(ServiceCommand::SendMessage(msg));
                    }
                    Err(e) => {
                        let _ = tx.send(ServiceCommand::SendMessage(
                            format!("❌ Ошибка получения баланса: {}", e)
                        ));
                    }
                }
            }
        }
    });

    // ─────────── Команда pcts — переключить pct_number 1⇄2 ─────────────────
    let pcts_help = "toggle pct_number between 1 and 2";
    commander.add_command_with_help(&["pcts"], pcts_help, {
        let tx = Arc::clone(&tx);
        move |_params| {
            let tx = Arc::clone(&tx);
            async move {
                // 1) читаем текущие настройки
                match get_general_settings().await {
                    Ok(Some(cfg)) => {
                        // 2) вычисляем новый номер
                        let new = if cfg.pct_number == 1 { 2 } else { 1 };
                        // 3) сохраняем
                        if let Err(e) = update_pct_number(new).await {
                            let _ = tx.send(ServiceCommand::SendMessage(
                                format!("❌ Failed to update pct_number: {}", e)
                            ));
                        } else {
                            let _ = tx.send(ServiceCommand::SendMessage(
                                format!("✅ pct_number switched to {}", new)
                            ));
                        }
                    }
                    Ok(None) => {
                        let _ = tx.send(ServiceCommand::SendMessage(
                            "❌ General settings not found in DB".into()
                        ));
                    }
                    Err(e) => {
                        let _ = tx.send(ServiceCommand::SendMessage(
                            format!("❌ Error reading general settings: {}", e)
                        ));
                    }
                }
            }
        }
    });

    // ─────────── Команда pct <f1> <f2> <f3> <f4> ──────────────────────────
    let pct_help = "<float1>-верх средней <float2>-низ средней <float3>-низ нижней <float4>-верх верхней";
    commander.add_command_with_help(&["pct"], pct_help, {
        let tx = Arc::clone(&tx);
        move |params| {
            let tx = Arc::clone(&tx);
            async move {
                // проверяем число аргументов
                if params.len() != 4 {
                    let _ = tx.send(ServiceCommand::SendMessage(
                        "❌ Usage: pct <float1> <float2> <float3> <float4>".into()
                    ));
                    return;
                }

                // парсим иконвертируем в [f64;4]
                let mut arr = [0.0f64; 4];
                for (i, p) in params.iter().enumerate().take(4) {
                    match p.parse::<f32>() {
                        Ok(v) => arr[i] = v as f64,
                        Err(_) => {
                            let _ = tx.send(ServiceCommand::SendMessage(
                                format!("❌ Invalid float at position {}: {}", i+1, p)
                            ));
                            return;
                        }
                    }
                }

                // сохраняем в БД
                if let Err(e) = update_pct_list_2(arr).await {
                    let _ = tx.send(ServiceCommand::SendMessage(
                        format!("❌ Failed to update pct_list_2: {}", e)
                    ));
                    return;
                }
                // переключаем active pct_number = 2
                if let Err(e) = update_pct_number(2).await {
                    let _ = tx.send(ServiceCommand::SendMessage(
                        format!("❌ Failed to set pct_number: {}", e)
                    ));
                    return;
                }

                let _ = tx.send(ServiceCommand::SendMessage(
                    format!("✅ pct_list_2 set to {:?}, pct_number = 2", arr)
                ));
            }
        }
    });

    // ─────────── Команда amount <float> ────────────────────────────────
    let amount_help = "<float> — установить базовый amount";
    commander.add_command_with_help(&["amount"], amount_help, {
        let tx = Arc::clone(&tx);
        move |params| {
            let tx = Arc::clone(&tx);
            async move {
                // проверяем, что ровно один аргумент
                if params.len() != 1 {
                    let _ = tx.send(ServiceCommand::SendMessage(
                        "❌ Usage: amount <float>".into()
                    ));
                    return;
                }
                // парсим в f64
                let val: f64 = match params[0].parse::<f64>() {
                    Ok(v) => v,
                    Err(_) => {
                        let _ = tx.send(ServiceCommand::SendMessage(
                            format!("❌ Неверное число: {}", params[0])
                        ));
                        return;
                    }
                };
                // сохраняем в БД
                if let Err(e) = update_amount(val).await {
                    let _ = tx.send(ServiceCommand::SendMessage(
                        format!("❌ Не удалось обновить amount: {}", e)
                    ));
                    return;
                }
                let _ = tx.send(ServiceCommand::SendMessage(
                    format!("✅ amount успешно установлен в {}", val)
                ));
            }
        }
    });

    // ─────────── Команда mliq <pos> <pct> — вывести часть ликвидности ─────────────
    let mliq_help = "<int> — номер позиции (1–3), <float> — % ликвидности для вывода";
    commander.add_command_with_help(&["mliq"], mliq_help, {
        let tx = Arc::clone(&tx);
        move |params| {
            let tx = Arc::clone(&tx);
            async move {
                // 1) Проверяем аргументы
                if params.len() != 2 {
                    let _ = tx.send(ServiceCommand::SendMessage(
                        "❌ Usage: mliq <position_index> <percent>".into()
                    ));
                    return;
                }
                let pos_idx = match params[0].parse::<usize>() {
                    Ok(n) if (1..=3).contains(&n) => n,
                    _ => {
                        let _ = tx.send(ServiceCommand::SendMessage(
                            format!("❌ Позиция должна быть 1, 2 или 3: got {}", params[0])
                        ));
                        return;
                    }
                };
                let pct = match params[1].parse::<f64>() {
                    Ok(v) if (0.0..=100.0).contains(&v) => v,
                    _ => {
                        let _ = tx.send(ServiceCommand::SendMessage(
                            format!("❌ Процент должен быть в диапазоне 0–100: got {}", params[1])
                        ));
                        return;
                    }
                };
                // 2) Загружаем RPC и позиции
                let rpc = init_rpc();
                let list = match list_positions_for_owner(None).await {
                    Ok(v) if !v.is_empty() => v,
                    Ok(_) => {
                        let _ = tx.send(ServiceCommand::SendMessage(
                            "⚠️ У вас нет открытых позиций.".into()
                        ));
                        return;
                    }
                    Err(e) => {
                        let _ = tx.send(ServiceCommand::SendMessage(
                            format!("❌ Не удалось получить позиции: {}", e)
                        ));
                        return;
                    }
                };
                // 3) Собираем вектор (mint, upper_price) и сортируем
                let mut info = Vec::new();
                for p in &list {
                    if let PositionOrBundle::Position(hp) = p {
                        let up = tick_index_to_price(
                            hp.data.tick_upper_index,
                            /* dec_a = */ 9,
                            /* dec_b = */ 6,
                        );
                        info.push((hp.data.position_mint, up));
                    }
                }
                info.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
                if pos_idx > info.len() {
                    let _ = tx.send(ServiceCommand::SendMessage(
                        format!("❌ Всего {} позиций, позиция {} отсутствует", info.len(), pos_idx)
                    ));
                    return;
                }
                let mint = info[pos_idx - 1].0;
                let _ = tx.send(ServiceCommand::SendMessage(
                    format!("🔄 Снимаю {:.2}% ликвидности из позиции {}", pct, pos_idx)
                ));
                // 4) Вызываем decrease_liquidity_partial
                if let Err(e) = decrease_liquidity_partial(mint, pct, 500).await {
                    let _ = tx.send(ServiceCommand::SendMessage(
                        format!("❌ Не удалось снять ликвидность: {}", e)
                    ));
                } else {
                    let _ = tx.send(ServiceCommand::SendMessage(
                        "✅ Ликвидность снята".into()
                    ));
                }
            }
        }
    });
    
    // ─────────── Команда pliq <pos> <usd> — добавить ликвидность на сумму в USD ────────────
    let pliq_help = "<int> — номер позиции (1–3), <float> — сумма в USD для добавления ликвидности";
    commander.add_command_with_help(&["pliq"], pliq_help, {
        let tx = Arc::clone(&tx);
        move |params| {
            let tx = Arc::clone(&tx);
            async move {
                // 1) Разбор аргументов ──────────────────────────────────────
                if params.len() != 2 {
                    let _ = tx.send(ServiceCommand::SendMessage(
                        "❌ Usage: pliq <position_index> <usd_amount>".into()
                    ));
                    return;
                }
                let pos_idx = match params[0].parse::<usize>() {
                    Ok(n) if (1..=3).contains(&n) => n,
                    _ => {
                        let _ = tx.send(ServiceCommand::SendMessage(
                            format!("❌ Position must be 1, 2 or 3: got {}", params[0])
                        ));
                        return;
                    }
                };
                let usd_amount = match params[1].parse::<f64>() {
                    Ok(v) if v > 0.0 => v,
                    _ => {
                        let _ = tx.send(ServiceCommand::SendMessage(
                            format!("❌ USD amount must be positive: got {}", params[1])
                        ));
                        return;
                    }
                };

                // 2) Список позиций владельца ───────────────────────────────
                let list = match list_positions_for_owner(None).await {
                    Ok(v) if !v.is_empty() => v,
                    Ok(_) => {
                        let _ = tx.send(ServiceCommand::SendMessage(
                            "⚠️ У вас нет открытых позиций.".into()
                        ));
                        return;
                    }
                    Err(e) => {
                        let _ = tx.send(ServiceCommand::SendMessage(
                            format!("❌ Failed to fetch positions: {e}")
                        ));
                        return;
                    }
                };

                // 3) Сортируем «сверху → вниз» по upper-price и берём нужную позицию
                let mut info = Vec::<(Pubkey, f64)>::new();
                for p in &list {
                    if let PositionOrBundle::Position(hp) = p {
                        let up = tick_index_to_price(hp.data.tick_upper_index, 9, 6);
                        info.push((hp.data.position_mint, up));
                    }
                }
                info.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());

                if pos_idx > info.len() {
                    let _ = tx.send(ServiceCommand::SendMessage(
                        format!("❌ Only {} positions; index {} is out of range", info.len(), pos_idx)
                    ));
                    return;
                }
                let mint = info[pos_idx - 1].0;

                // 4) Быстрая проверка, хватает ли общего капитала под указанный бюджет
                let rpc    = init_rpc();
                let wallet = match load_wallet() {
                    Ok(w) => w,
                    Err(e) => {
                        let _ = tx.send(ServiceCommand::SendMessage(
                            format!("❌ Wallet load failed: {e}")
                        ));
                        return;
                    }
                };
                let pk = wallet.pubkey();

                // свободный SOL
                let sol_free = rpc.get_balance(&pk).await.unwrap_or(0) as f64 / 1e9;
                // свободный USDC
                let mint_b  = Pubkey::from_str(USDC).unwrap();
                let ata_b   = get_associated_token_address(&pk, &mint_b);
                let usdc_free = rpc
                    .get_token_account_balance(&ata_b).await
                    .ok()
                    .and_then(|ui| ui.amount.parse::<u64>().ok())
                    .map(|atoms| atoms as f64 / 1e6)
                    .unwrap_or(0.0);

                // цена SOL/USDC
                let sol_usd = match get_sol_price_usd().await {
                    Ok(p) => p,
                    Err(e) => {
                        let _ = tx.send(ServiceCommand::SendMessage(
                            format!("❌ Failed to fetch SOL/USD price: {e}")
                        ));
                        return;
                    }
                };

                // запас 12 % уже внутри increase_liquidity_partial, но сделаем грубый pre-check
                let total_usd_free = usdc_free + (sol_free.max(0.0) - 0.07).max(0.0) * sol_usd;
                if total_usd_free < usd_amount * 1.05 {
                    let _ = tx.send(ServiceCommand::SendMessage(
                        format!(
                            "⚠️ Warning: you seem to have only ≈ ${:.2} free (needed ${:.2}). \
                            Function will try to swap, but operation may fail.",
                            total_usd_free, usd_amount
                        )
                    ));
                }

                // 5) Формируем PoolConfig (SOL/USDC Whirlpool)
                let pool_cfg = PoolConfig {
                    program:      "whirlpool".to_owned(),
                    name:         "SOL/USDC".to_owned(),
                    pool_address: std::env::var("SOLUSDC_POOL").expect("SOLUSDC_POOL not set"),
                    mint_a:       WSOL.to_owned(),
                    mint_b:       USDC.to_owned(),
                    decimal_a:    9,
                    decimal_b:    6,
                    amount:       0.0,
                    position_1:   None,
                    position_2:   None,
                    position_3:   None,
                    date_opened:           Utc::now(),
                    is_closed:             false,
                    commission_collected_1: 0.0,
                    commission_collected_2: 0.0,
                    commission_collected_3: 0.0,
                    total_value_open:      0.0,
                    total_value_current:   0.0,
                    wallet_balance: 0.0
                };

                let _ = tx.send(ServiceCommand::SendMessage(
                    format!("🔄 Добавляю ${:.2} в позицию {}", usd_amount, pos_idx)
                ));

                // 6) Добавляем ликвидность (теперь функция принимает *только* USD-бюджет)
                match increase_liquidity_partial(mint, usd_amount, &pool_cfg, 500).await {
                    Ok(_) => {
                        let _ = tx.send(ServiceCommand::SendMessage(
                            "✅ Ликвидность добавлена".into()
                        ));
                    }
                    Err(e) => {
                        let _ = tx.send(ServiceCommand::SendMessage(
                            format!("❌ increase_liquidity failed: {e}")
                        ));
                    }
                }
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
        wallet_balance: 0.0
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

    // ── 7. Конвертируем освобождённое в USD‐бюджет ──────────────────────
    let sol_price_usd = match get_sol_price_usd().await {
        Ok(p) => p,
        Err(e) => {
            let _ = tx.send(ServiceCommand::SendMessage(
                format!("Не удалось получить цену SOL: {e}"),
            ));
            return;
        }
    };
    let usd_budget = freed_usdc + freed_sol * sol_price_usd;

    // ── 8. Увеличиваем ликвидность в целевой позиции ────────────────────
    if let Err(e) = increase_liquidity_partial(
        mint_to,
        usd_budget,          // ← теперь передаём общий бюджет в USD
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

