// src/telegram_service/engine.rs

use std::{thread, time::Duration};
use dotenv::dotenv;
use std::env;
use std::sync::Arc;
use chrono::Local;
use tokio::runtime::Builder as RtBuilder;
use tokio::runtime::Builder;
use tokio::sync::mpsc::{UnboundedSender, UnboundedReceiver, unbounded_channel};
use crate::telegram_service::{telegram::Telegram, commands::Commander};
use serde::Deserialize;
use tokio::sync::Notify;
use crate::telegram_service::registry::register_commands;
/// Тип команд, которые внешние потоки могут отправлять в Telegram-модуль.
#[derive(Debug)]
pub enum ServiceCommand {
    /// Сообщение через «основной» бот (TELEGRAM_API / CHAT_ID)
    SendMessage(String),
    /// Сообщение через «коллектор»-бот (COLLECTOR_API / COLLECTOR_CHAT_ID)
    SendSignal(String),
}

pub fn start(close_ntf: Arc<Notify>) -> (UnboundedSender<ServiceCommand>, Arc<Commander>) {
    dotenv().ok();

    // --- читаем окружение -----------------------------------------------------
    let token_main   = env::var("COLLECTOR_API").expect("TELEGRAM_API not set");
    let chat_main    = env::var("CHAT_ID").expect("CHAT_ID not set");

    // COLLECTOR_* не обязательны – по-умолчанию используем основные
    let token_signal   = env::var("TELEGRAM_API").unwrap_or_else(|_| token_main.clone());
    let chat_coll    = env::var("CHAT_ID").unwrap_or_else(|_| chat_main.clone());

    // --- обычная инициализация -------------------------------------------------
    let commander = Arc::new(Commander::new(true));
    let (tx, rx)  = unbounded_channel::<ServiceCommand>();

    register_commands(commander.clone(), tx.clone(), close_ntf.clone());

    let cmd = commander.clone();
    // передаём ВСЕ строки токенов / chat-id в thread
    thread::spawn(move || {
        let rt = RtBuilder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("Failed to build Tokio runtime");
        rt.block_on(async move {
            telegram_loop(
                &token_main,
                &chat_main,
                &token_signal,
                &chat_coll,
                commander,
                rx,
            )
            .await;
        });
    });

    (tx, cmd)
}

async fn telegram_loop(
    token_main: &str,
    chat_main:  &str,
    token_signal: &str,
    chat_coll:  &str,
    commander:  Arc<Commander>,
    mut rx:     UnboundedReceiver<ServiceCommand>,
) {
    let now = || Local::now().format("%Y-%m-%d %H:%M:%S");

    // ── два Telegram-клиента ---------------------------------------------------
    let mut tg_main = Telegram::new(token_main, chat_main);
    let mut tg_coll = Telegram::new(token_signal, chat_coll);

    println!("[{}] Telegram clients initialised", now());

    // получаем offset для ПОЛЬЗОВАТЕЛЬСКОГО (main) бота
    let mut offset = {
        match get_updates(&tg_main, 0).await {
            Ok(upds) => upds.iter().map(|u| u.update_id).max().unwrap_or(0) + 1,
            Err(e)   => { eprintln!("[{}] Failed to fetch initial updates: {e}", now()); 0 }
        }
    };

    println!("[{}] Starting poll loop (offset={})", now(), offset);
    let mut bad_sends_main = 0usize;
    let mut bad_sends_coll = 0usize;

    loop {
        // ---------- исходим входящую очередь команд  --------------------------
        while let Ok(cmd) = rx.try_recv() {
            match cmd {
                ServiceCommand::SendMessage(text) => {
                    match tg_main.send(&text).await {
                        Ok(_)  => { println!("[{}] → Sent (main): {}", now(), text); bad_sends_main = 0; }
                        Err(e) => {
                            eprintln!("[{}] ❌ Send error MAIN: {e}", now());
                            bad_sends_main += 1;
                            if bad_sends_main >= 3 {
                                eprintln!("[{}] ↻ Recreating MAIN Telegram client", now());
                                tg_main = Telegram::new(token_main, chat_main);
                                bad_sends_main = 0;
                            }
                        }
                    }
                }
                ServiceCommand::SendSignal(text) => {
                    match tg_coll.send(&text).await {
                        Ok(_)  => { println!("[{}] → Sent (collector): {}", now(), text); bad_sends_coll = 0; }
                        Err(e) => {
                            eprintln!("[{}] ❌ Send error COLLECTOR: {e}", now());
                            bad_sends_coll += 1;
                            if bad_sends_coll >= 3 {
                                eprintln!("[{}] ↻ Recreating COLLECTOR Telegram client", now());
                                tg_coll = Telegram::new(token_signal, chat_coll);
                                bad_sends_coll = 0;
                            }
                        }
                    }
                }
            }
        }

        // ---------- long-poll для ОСНОВНОГО бота (команды) --------------------
        match get_updates(&tg_main, offset).await {
            Ok(upds) => {
                for upd in upds {
                    offset = upd.update_id + 1;
                    if let Some(msg) = upd.message {
                        if msg.chat.id == tg_main.chat_id() {
                            if let Some(txt) = msg.text {
                                println!("[{}] ← Received: {}", now(), txt);
                                commander.exec_command(&txt, msg.date).await;
                            }
                        }
                    }
                }
            }
            Err(e) => {
                eprintln!("[{}] ❌ Polling error: {e}", now());
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        }

        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}





async fn get_updates(telegram: &Telegram, offset: i64) -> anyhow::Result<Vec<Update>> {
    let url = format!(
        "https://api.telegram.org/bot{}/getUpdates?offset={}&timeout=60",
        telegram.token(),
        offset
    );
    let resp = telegram.client().get(&url).send().await?;
    let data: GetUpdatesResponse = resp.json().await?;
    Ok(data.result)
}

#[derive(Debug, Deserialize)]
struct GetUpdatesResponse {
    ok: bool,
    result: Vec<Update>,
}

#[derive(Debug, Deserialize)]
struct Update {
    update_id: i64,
    message: Option<Message>,
}

#[derive(Debug, Deserialize)]
struct Message {
    /// Unix-timestamp (поле `date` в JSON от Telegram)
    date: i64,
    chat: Chat,
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Chat {
    id: i64,
}