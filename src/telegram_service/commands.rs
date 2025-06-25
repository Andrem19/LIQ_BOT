// src/telegram_service/commands.rs

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use chrono::{Local, Utc};
use futures::future::BoxFuture;
use futures::FutureExt;
use std::sync::atomic::{AtomicI64, Ordering};

pub type CommandFunc = Arc<dyn Fn(Vec<String>) -> BoxFuture<'static, ()> + Send + Sync>;


pub struct Commander {
    /// Зарегистрированные команды
    commands: Arc<Mutex<HashMap<Vec<String>, CommandFunc>>>,
    /// Описания для каждой команды (default: empty)
    descriptions: Arc<Mutex<HashMap<Vec<String>, String>>>,
    /// Вести ли лог в stdout
    logs: bool,
    /// Момент запуска бота (Unix-секунды)
    start_unix: AtomicI64,
    /// Окно «свежести» команды, сек. (по-умолчанию 5 с)
    fresh_window_secs: i64,
}
impl Clone for Commander {
    fn clone(&self) -> Self {
        Commander {
            commands:     Arc::clone(&self.commands),
            descriptions: Arc::clone(&self.descriptions),
            logs:         self.logs,
            start_unix:   AtomicI64::new(self.start_unix.load(Ordering::SeqCst)),
            fresh_window_secs: self.fresh_window_secs,
        }
    }
}

impl Commander {
    /// Создаёт новый экземпляр.
    /// * `logs` — печатать ли в консоль успешные/неуспешные вызовы.
    pub fn new(logs: bool) -> Self {
        Commander {
            commands:     Arc::new(Mutex::new(HashMap::new())),
            descriptions: Arc::new(Mutex::new(HashMap::new())),
            logs,
            start_unix:   AtomicI64::new(Utc::now().timestamp()),
            fresh_window_secs: 15,
        }
    }

    /// Регистрирует команду.
    pub fn add_command<F, Fut>(&self, cmd: &[&str], func: F)
    where
        F: Fn(Vec<String>) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let mut map = self.commands.lock().unwrap();
        map.insert(
            cmd.iter().map(|s| s.to_string()).collect(),
            Arc::new(move |params: Vec<String>| func(params).boxed()),
        );
    }

    /// Регистрирует команду с описанием.
    pub fn add_command_with_help<F, Fut>(&self, cmd: &[&str], help: &str, func: F)
    where
        F: Fn(Vec<String>) -> Fut + Send + Sync + 'static,
        Fut: std::future::Future<Output = ()> + Send + 'static,
    {
        let key = cmd.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let mut cmds = self.commands.lock().unwrap();
        cmds.insert(
            key.clone(),
            Arc::new(move |params: Vec<String>| func(params).boxed()),
        );
        let mut descs = self.descriptions.lock().unwrap();
        descs.insert(key, help.to_string());
    }

    /// Разбирает строку на «ключевые» слова и параметры (`--param`).
    pub fn decode_str(&self, prompt: &str) -> (Vec<String>, Vec<String>) {
        let mut cmd = Vec::new();
        let mut params = Vec::new();

        for el in prompt.split_whitespace() {
            if let Some(idx) = el.find("--") {
                params.push(el[idx + 2..].to_string());
            } else {
                cmd.push(el.to_lowercase());
            }
        }
        (cmd, params)
    }

    /// Выполняет команду *только если* сообщение свежее.
    ///
    /// * `prompt` — сам текст (например, "/close all");
    /// * `msg_unix_time` — `message.date` из Telegram (Unix-секунды).
    pub async fn exec_command(&self, prompt: &str, msg_unix_time: i64) {
        // --- ФИЛЬТР «СВЕЖЕСТИ» --------------------------------------------
        if msg_unix_time < self.start_unix.load(Ordering::SeqCst) {
                if self.logs {
                    println!(
                        "{} Игнорирована (устарела до старта): {}",
                        Local::now().format("%Y-%m-%d %H:%M:%S"),
                        prompt
                    );
                }
                return;
            }
        // ------------------------------------------------------------------

        let (cmd, params) = self.decode_str(prompt);

        // Берём callback без удержания замка дольше необходимого
        let maybe_cb = {
            let map = self.commands.lock().unwrap();
            map.get(&cmd).cloned()
        };

        if let Some(cb) = maybe_cb {
            cb(params).await;

            if self.logs {
                println!(
                    "{} Выполнена: {}",
                    Local::now().format("%Y-%m-%d %H:%M:%S"),
                    cmd.join(" ")
                );
            }
        } else if self.logs {
            println!(
                "{} Не найдена: {}",
                Local::now().format("%Y-%m-%d %H:%M:%S"),
                cmd.join(" ")
            );
        }
    }

    /// Возвращает дерево команд с описаниями.
    pub fn show_tree(&self) -> String {
        let cmds = self.commands.lock().unwrap();
        let descs = self.descriptions.lock().unwrap();
        let mut lines = Vec::new();
        for key in cmds.keys() {
            let cmd_str = key.join(" ");
            let help = descs.get(key).cloned().unwrap_or_default();
            if help.is_empty() {
                lines.push(cmd_str);
            } else {
                lines.push(format!("{} — {}", cmd_str, help));
            }
        }
        lines.join("\n")
    }
}
