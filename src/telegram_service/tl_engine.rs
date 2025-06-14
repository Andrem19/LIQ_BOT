// src/telegram_service/engine.rs

use std::{thread, time::Duration};
use dotenv::dotenv;
use std::env;
use std::sync::Arc;
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

/// Запускает Telegram-сервис в отдельном потоке, возвращает
/// (sender, commander) — канал для отправки событий и доступ к регистрации команд.
pub fn start() -> (UnboundedSender<ServiceCommand>, Arc<Commander>) {
    dotenv().ok();
    let token = env::var("TELEGRAM_API").unwrap();
    let chat_id = env::var("CHAT_ID").unwrap();
    let telegram = Telegram::new(&token, &chat_id);

    // создаём общий Commander
    let commander = Arc::new(Commander::new(true));

    // настраиваем канал для отправки сообщений
    let (tx, mut rx) = unbounded_channel();

    // регистрируем все команды
    register_commands(Arc::clone(&commander), tx.clone());

    // запускаем polling loop в отдельном потоке
    let tele_clone = telegram.clone();
    let cmd_clone = Arc::clone(&commander);
    thread::spawn(move || {
        let rt = Builder::new_current_thread().enable_all().build().unwrap();
        rt.block_on(async move {
            let mut offset = 0;
            loop {
                // сначала отправляем исходящие ServiceCommand
                while let Ok(cmd) = rx.try_recv() {
                    if let ServiceCommand::SendMessage(text) = cmd {
                        tele_clone.send(&text).await.ok();
                    }
                }
                // затем забираем обновления из Telegram
                let updates = get_updates(&tele_clone, offset).await.unwrap_or_default();
                for upd in updates {
                    offset = upd.update_id + 1;
                    if let Some(msg) = upd.message {
                        if msg.chat.id == tele_clone.chat_id() {
                            if let Some(txt) = msg.text {
                                cmd_clone.exec_command(&txt).await;
                            }
                        }
                    }
                }
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
