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
use crate::telegram_service::registry::register_commands;
/// Тип команд, которые внешние потоки могут отправлять в Telegram-модуль.
#[derive(Debug)]
pub enum ServiceCommand {
    SendMessage(String),
}

pub fn start() -> (UnboundedSender<ServiceCommand>, Arc<Commander>) {
    dotenv::dotenv().ok();

    // 0) прочитаем переменные окружения
    let token   = env::var("TELEGRAM_API").expect("TELEGRAM_API not set");
    let chat_id = env::var("CHAT_ID").expect("CHAT_ID not set");

    // 1) заведём Commander и канал
    let commander = Arc::new(Commander::new(true));
    let (tx, rx)  = unbounded_channel::<ServiceCommand>();

    // 2) зарегистрируем все команды заранее
    register_commands(commander.clone(), tx.clone());
    let cmd = commander.clone();
    // 3) запускаем отдельный поток с собственным runtime
    thread::spawn(move || {
        // создаём мультипоточный runtime
        let rt = RtBuilder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .expect("Failed to build Tokio runtime");

        // переносим rx внутрь runtime и запускаем loop
        rt.block_on(async move {
            telegram_loop(&token, &chat_id, commander, rx).await;
        });
    });
    

    (tx, cmd)
}

/// Вся логика polling + отправки
async fn telegram_loop(
    token:     &str,
    chat_id:   &str,
    commander: Arc<Commander>,
    mut rx:    UnboundedReceiver<ServiceCommand>,
) {
    // простой helper для timestamp
    let now_ts = || Local::now().format("%Y-%m-%d %H:%M:%S");

    // инициализируем клиента
    let mut telegram = Telegram::new(token, chat_id);
    println!("[{}] Telegram client initialized", now_ts());

    // сброс offset'а (чтобы не обрабатывать старые команды)
    let mut offset = {
        match get_updates(&telegram, 0).await {
            Ok(upds) => {
                let mx = upds.iter().map(|u| u.update_id).max().unwrap_or(0);
                mx + 1
            }
            Err(e) => {
                eprintln!("[{}] Failed to fetch initial updates: {e}", now_ts());
                0
            }
        }
    };
    println!("[{}] Starting poll loop (offset={})", now_ts(), offset);

    // счётчик подряд неуспешных send()
    let mut bad_sends = 0;

    loop {
        // 1) отправляем исходящие ServiceCommand
        while let Ok(cmd) = rx.try_recv() {
            if let ServiceCommand::SendMessage(text) = cmd {
                match telegram.send(&text).await {
                    Ok(_) => {
                        println!("[{}] → Sent: {}", now_ts(), text);
                        bad_sends = 0;
                    }
                    Err(err) => {
                        eprintln!("[{}] ❌ Send error: {err}", now_ts());
                        bad_sends += 1;
                        // если три раза подряд не смогли — пересоздаём клиента
                        if bad_sends >= 3 {
                            eprintln!("[{}] ↻ Recreating Telegram client", now_ts());
                            telegram = Telegram::new(token, chat_id);
                            bad_sends = 0;
                        }
                    }
                }
            }
        }

        // 2) polling новых апдейтов
        match get_updates(&telegram, offset).await {
            Ok(upds) => {
                for upd in upds {
                    offset = upd.update_id + 1;
                    if let Some(msg) = upd.message {
                        if msg.chat.id == telegram.chat_id() {
                            if let Some(txt) = msg.text {
                                println!("[{}] ← Received: {}", now_ts(), txt);
                                commander.exec_command(&txt, msg.date).await;
                            }
                        }
                    }
                }
            }
            Err(err) => {
                eprintln!("[{}] ❌ Polling error: {err}", now_ts());
                // если упало — подождём чуть и попробуем снова
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            }
        }

        // 3) небольшой таймаут между циклами
        tokio::time::sleep(std::time::Duration::from_secs(1)).await;
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