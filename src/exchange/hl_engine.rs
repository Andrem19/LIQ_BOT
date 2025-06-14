// src/exchange/engine.rs

use std::{env, sync::Arc};
use dotenv::dotenv;
use tokio::{
    sync::mpsc::{unbounded_channel, UnboundedSender},
    task,
    time::{sleep, Duration},
};
use anyhow::Result;

use crate::types::WorkerCommand;
use crate::exchange::hyperliquid::hl::HL;
use crate::telegram_service::telegram::Telegram;

/// Запускает движок Hyperliquid в фоне и возвращает `UnboundedSender<WorkerCommand>`,
/// по которому можно посылать сигналы `On(cfg)` или `Off`.
///
/// MUST be called *после* инициализации Tokio runtime (в `main.rs` под `#[tokio::main]`).
pub fn start() -> Result<UnboundedSender<WorkerCommand>> {
    // Загрузка .env (HLSECRET, HL_TRADING_ADDRESS, TELEGRAM_API, CHAT_ID)
    dotenv().ok();

    // канал управления воркером
    let (tx, mut rx) = unbounded_channel::<WorkerCommand>();

    // сам воркер
    task::spawn(async move {
        // 1) Инициализируем HL-клиент
        let mut hl = match HL::new_from_env().await {
            Ok(c) => c,
            Err(e) => {
                eprintln!("[exchange][ERROR] HL init failed: {e}");
                return;
            }
        };

        // 2) Инициализируем Telegram-уведомления
        let token = env::var("TELEGRAM_API").expect("TELEGRAM_API not set");
        let chat_id = env::var("CHAT_ID").expect("CHAT_ID not set");
        let tele = Telegram::new(&token, &chat_id);

        // Состояние воркера
        let mut running = false;
        let mut entry_px = 0.0;
        let mut pos_size = 0.0;
        let mut tp_price = 0.0;
        let mut sl_price = 0.0;

        loop {
            tokio::select! {
                // Приход команды On/Off
                Some(cmd) = rx.recv() => {
                    match cmd {
                        WorkerCommand::On(cfg) => {
                            // Если уже работаем — сначала закрываем старую позицию
                            if running {
                                if let Ok(Some(pos)) = hl.get_position("SOLUSDT").await {
                                    let _ = hl.open_market_order("SOLUSDT", "Buy", 0.0, true, pos.size).await;
                                }
                            }
                            // Открываем новый шорт на cfg.amount/2
                            let amount = cfg.amount / 2.0;
                            if let Ok((_, px)) = hl.open_market_order("SOLUSDT", "Sell", amount, false, 0.0).await {
                                entry_px = px;
                                // Узнаём реальный размер
                                if let Ok(Some(pos)) = hl.get_position("SOLUSDT").await {
                                    pos_size = pos.size;
                                    // Берём TP из position_1.upper, SL из position_3.lower
                                    if let Some(p1) = cfg.position_1.clone() {
                                        tp_price = p1.upper_price;
                                    }
                                    if let Some(p3) = cfg.position_3.clone() {
                                        sl_price = p3.lower_price;
                                    }
                                    // Переводим в относительные проценты
                                    let tp_perc = if entry_px>0.0 { (tp_price/entry_px)-1.0 } else { 0.0 };
                                    let sl_perc = if entry_px>0.0 { 1.0-(sl_price/entry_px) } else { 0.0 };
                                    // Ставим TP и SL
                                    let _ = hl.open_tp("SOLUSDT", "Sell", pos_size, entry_px, tp_perc).await;
                                    let _ = hl.open_sl("SOLUSDT", "Sell", pos_size, entry_px, sl_perc).await;
                                    running = true;
                                }
                            }
                        }
                        WorkerCommand::Off => {
                            if running {
                                // Принудительно закрываем
                                if let Ok(Some(pos)) = hl.get_position("SOLUSDT").await {
                                    let _ = hl.open_market_order("SOLUSDT", "Buy", 0.0, true, pos.size).await;
                                    // Шлём остаток в Telegram
                                    if let Ok(bal) = hl.get_balance().await {
                                        let msg = format!("🚫 Forced close. Balance: ${:.2}", bal);
                                        let _ = tele.send(&msg).await;
                                    }
                                }
                                running = false;
                            }
                        }
                    }
                },

                // Каждые 5 с, если в режиме `On`, проверяем цену
                _ = sleep(Duration::from_secs(5)), if running => {
                    if let Ok(price) = hl.get_last_price("SOLUSDT").await {
                        if price >= tp_price || price <= sl_price {
                            // Закрываем по тейку или стопу
                            if let Ok(Some(pos)) = hl.get_position("SOLUSDT").await {
                                let _ = hl.open_market_order("SOLUSDT", "Buy", 0.0, true, pos.size).await;
                                if let Ok(bal) = hl.get_balance().await {
                                    let msg = format!("🔔 Auto-closed at {:.2}. Balance: ${:.2}", price, bal);
                                    let _ = tele.send(&msg).await;
                                }
                            }
                            running = false;
                        }
                    }
                }
            }
        }
    });

    Ok(tx)
}
