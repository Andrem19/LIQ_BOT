// src/telegram_service/engine.rs

use std::{thread, time::Duration};
use dotenv::dotenv;
use std::env;
use tokio::runtime::Builder;
use tokio::sync::mpsc::{UnboundedSender, UnboundedReceiver, unbounded_channel};
use crate::telegram_service::{telegram::Telegram, commands::Commander};
use serde::Deserialize;

/// Тип команд, которые внешние потоки могут отправлять в Telegram-модуль.
#[derive(Debug)]
pub enum ServiceCommand {
    SendMessage(String),
}

/// Запускает Telegram-сервис в отдельном потоке, возвращает
/// (sender, commander) — канал для отправки событий и доступ к регистрации команд.
pub fn start() -> (UnboundedSender<ServiceCommand>, Commander) {
    // Загрузка .env
    dotenv().ok();
    let token = env::var("TELEGRAM_API").expect("TELEGRAM_API must be set in .env");
    let chat_id = env::var("CHAT_ID").expect("CHAT_ID must be set in .env");

    let telegram = Telegram::new(&token, &chat_id);
    let commander = Commander::new(true);
    let commander_thread = commander.clone();

    let (tx, mut rx): (UnboundedSender<ServiceCommand>, UnboundedReceiver<ServiceCommand>) = unbounded_channel();

    thread::spawn(move || {
        // Один runtime на весь поток
        let rt = Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("Failed to build Tokio runtime");

        rt.block_on(async move {
            let mut update_offset: i64 = 0;

            loop {
                // 1) Отправка сообщений из приложения в бот
                while let Ok(cmd) = rx.try_recv() {
                    if let ServiceCommand::SendMessage(text) = cmd {
                        if let Err(e) = telegram.send(&text).await {
                            eprintln!("Error sending Telegram message: {:?}", e);
                        }
                    }
                }

                // 2) Получение команд от бота (polling getUpdates)
                match get_updates(&telegram, update_offset).await {
                    Ok(updates) => {
                        for upd in updates {
                            update_offset = upd.update_id + 1;
                            if let Some(msg) = upd.message {
                                if msg.chat.id == telegram.chat_id() {
                                    if let Some(text) = msg.text {
                                        // Выполнить зарегистрированную команду
                                        commander_thread.exec_command(&text).await;
                                    }
                                }
                            }
                        }
                    }
                    Err(e) => eprintln!("Error fetching updates: {:?}", e),
                }

                // Небольшая пауза между polling
                tokio::time::sleep(Duration::from_secs(1)).await;
            }
        });
    });

    (tx, commander)
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
    chat: Chat,
    text: Option<String>,
}

#[derive(Debug, Deserialize)]
struct Chat {
    id: i64,
}
